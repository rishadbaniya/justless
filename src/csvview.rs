//! Universal zero-semantic-loss compactor for RFC 4180 CSV and TSV output.
//!
//! Two-tier output:
//! - **Tier 1 (bijective lossless):** when the compacted table fits within
//!   `budget`, hoists 100% constant columns into the header (`col=VAL`, omitted
//!   from data rows) and `>= 70%` majority-default columns (`col?=VAL`, `.` on
//!   matching rows with literal `.` escaped as `\.`), strips redundant RFC 4180
//!   quoting into compact TSV rows, and round-trip verifies via `decode_csv`
//!   that decoding reproduces the exact header and exact cell matrix.
//! - **Tier 2 (bounded schema + summary preview):** when raw input and Tier 1
//!   exceed `budget`, emits a concise `[CSV <rows> rows × <cols> cols · exact
//!   data in full log]` header (or `TSV`), a per-column schema summary (inferred
//!   type `int`/`float`/`bool`/`date`/`str`, constant/majority values, null or
//!   empty counts, numeric `min..max` ranges, and distinct counts + top values
//!   for low-cardinality categoricals), plus head K + tail M sample rows and an
//!   exact 1-based `[stdout:A-B]` elision line for the omitted middle rows.
//!
//! Fails closed (`None`) on prose, source code, logs, JSON, CLI tables, NUL or
//! control bytes, malformed RFC 4180 quoting, inconsistent column counts,
//! implausible headers, failed round-trip verification, or insufficient byte
//! savings.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Largest stdout payload inspected by this module (2 MiB, matching `exec::TEXT_SHAPE_LIMIT`).
pub const LIMIT: usize = 2 * 1024 * 1024;
/// Reject inputs with single lines wider than this (matches `tableview::MAX_LINE_BYTES`).
const MAX_LINE_BYTES: usize = 256 * 1024;
/// Minimum columns required to treat output as CSV/TSV.
const MIN_COLS: usize = 2;
/// Maximum columns accepted so pathological wide lines fail closed cheaply.
const MAX_COLS: usize = 512;
/// Minimum data rows required (excluding the header row).
const MIN_ROWS: usize = 8;
/// Minimum absolute byte savings when relative savings are below `MIN_SAVED_PCT`.
const MIN_SAVED_BYTES: usize = 120;
/// Minimum relative byte savings percentage when absolute savings are below `MIN_SAVED_BYTES`.
const MIN_SAVED_PCT: usize = 15;
/// Maximum head sample rows emitted in Tier 2.
const MAX_TIER2_HEAD: usize = 8;
/// Maximum tail sample rows emitted in Tier 2.
const MAX_TIER2_TAIL: usize = 4;
/// Maximum display characters for a value snippet inside the Tier 2 schema summary.
const MAX_SCHEMA_VAL_CHARS: usize = 40;
/// Cardinality threshold for enumerating top categorical values inline in Tier 2.
const MAX_CATEGORICAL_DISTINCT: usize = 12;
/// Maximum number of top categorical values printed for one column in Tier 2.
const MAX_TOP_VALUES: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delim {
    Comma,
    Tab,
}

impl Delim {
    fn as_byte(self) -> u8 {
        match self {
            Self::Comma => b',',
            Self::Tab => b'\t',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Comma => "CSV",
            Self::Tab => "TSV",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceLine<'a> {
    /// 1-based line number in the raw `stdout` input.
    line_no: usize,
    /// Line text with any trailing `\r` stripped.
    text: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedRow<'a> {
    line_no: usize,
    cells: Vec<Cow<'a, str>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedCsv<'a> {
    delim: Delim,
    headers: Vec<Cow<'a, str>>,
    rows: Vec<ParsedRow<'a>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DecodedCsv<'a> {
    delim: Delim,
    headers: Vec<Cow<'a, str>>,
    rows: Vec<Vec<Cow<'a, str>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ColMode<'a> {
    Plain,
    Constant(Cow<'a, str>),
    Default(Cow<'a, str>),
}

struct ColumnPlan<'a> {
    modes: Vec<ColMode<'a>>,
    varying: Vec<usize>,
    has_const: bool,
    has_default: bool,
    cols_line: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MissingKind {
    Present,
    Empty,
    NullToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColType {
    Int,
    Float,
    Bool,
    Date,
    Str,
}

impl ColType {
    fn label(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::Date => "date",
            Self::Str => "str",
        }
    }
}

/// Compact RFC 4180 CSV or TSV `stdout` to fit within `budget`.
///
/// Returns `Some(compacted)` only when `stdout` parses unambiguously as a
/// single CSV or TSV table, passes strict header and consistency validation,
/// fits within `budget`, passes bijective round-trip verification (Tier 1) or
/// verified sample-row encoding (Tier 2), and saves at least 15% or 120 bytes
/// compared to raw `stdout`.
pub fn format(stdout: &str, budget: usize) -> Option<String> {
    if budget == 0 || stdout.is_empty() || stdout.len() > LIMIT {
        return None;
    }

    let csv = parse_csv(stdout)?;
    let plan = build_column_plan(&csv);

    if let Some(tier1) = try_render_tier1(&csv, &plan, budget, stdout.len()) {
        if !verify_round_trip(&csv, &tier1) {
            return None;
        }
        return saves_enough(tier1.len(), stdout.len()).then_some(tier1);
    }

    // Never drop rows via Tier 2 when the raw CSV already fits in `budget`.
    if stdout.len() <= budget {
        return None;
    }

    let tier2 = render_tier2(&csv, &plan, budget)?;
    (tier2.len() <= budget && saves_enough(tier2.len(), stdout.len())).then_some(tier2)
}

fn saves_enough(rendered_len: usize, raw_len: usize) -> bool {
    if rendered_len >= raw_len {
        return false;
    }
    let saved = raw_len - rendered_len;
    saved >= MIN_SAVED_BYTES || saved.saturating_mul(100) >= raw_len.saturating_mul(MIN_SAVED_PCT)
}

// ---------------------------------------------------------------------------
// Strict RFC 4180 CSV & TSV parser and validation
// ---------------------------------------------------------------------------

fn parse_csv(stdout: &str) -> Option<ParsedCsv<'_>> {
    // Strip a leading UTF-8 BOM (common in Excel/BI CSV exports) before
    // parsing so the first column header is never corrupted and quoted headers
    // like `\u{feff}"id",...` parse cleanly.
    let input = stdout.strip_prefix('\u{feff}').unwrap_or(stdout);

    // Reject NUL, ANSI escape sequences, and unexpected control characters.
    // Fast-path ESC via SIMD `str::contains` before scanning remaining bytes.
    if input.contains('\x1b')
        || input
            .bytes()
            .any(|b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r'))
    {
        return None;
    }

    let raw = input.strip_suffix('\n').unwrap_or(input);
    let line_count = raw.bytes().filter(|&b| b == b'\n').count() + 1;
    if line_count < MIN_ROWS + 1 {
        return None;
    }
    let mut lines: Vec<SourceLine<'_>> = Vec::with_capacity(line_count);
    for (idx, piece) in raw.split('\n').enumerate() {
        let text = piece.strip_suffix('\r').unwrap_or(piece);
        if text.len() > MAX_LINE_BYTES {
            return None;
        }
        lines.push(SourceLine {
            line_no: idx + 1,
            text,
        });
    }

    // Trim leading/trailing blank lines (spaces only; keep `\t` lines intact for TSV).
    let is_outer_blank = |s: &str| s.is_empty() || s.bytes().all(|b| b == b' ');
    let first = lines.iter().position(|l| !is_outer_blank(l.text))?;
    let last = lines.iter().rposition(|l| !is_outer_blank(l.text))?;
    let slice = &lines[first..=last];
    if slice.len() < MIN_ROWS + 1 {
        return None;
    }

    // Reject internal blank lines (prose paragraphs, code blocks, multi-section output).
    if slice.iter().any(|l| is_outer_blank(l.text)) {
        return None;
    }

    let header_raw = slice[0].text;
    let has_tab = header_raw.contains('\t');
    let has_comma = header_raw.contains(',');
    if !has_tab && !has_comma {
        return None;
    }

    // Try TSV first when the header contains tabs (TSV values often contain commas);
    // fall back to CSV when tabs are incidental or absent.
    if has_tab && let Some(parsed) = parse_with_delim(slice, Delim::Tab) {
        return Some(parsed);
    }
    if has_comma {
        return parse_with_delim(slice, Delim::Comma);
    }
    None
}

fn parse_with_delim<'a>(slice: &[SourceLine<'a>], delim: Delim) -> Option<ParsedCsv<'a>> {
    let delim_byte = delim.as_byte();
    let headers = parse_rfc4180_line(slice[0].text, delim_byte, 0)?;
    let ncols = headers.len();
    if !(MIN_COLS..=MAX_COLS).contains(&ncols) {
        return None;
    }
    if !is_plausible_header(&headers, delim) {
        return None;
    }

    let mut rows = Vec::with_capacity(slice.len() - 1);
    for line in &slice[1..] {
        let cells = parse_rfc4180_line(line.text, delim_byte, ncols)?;
        if cells.len() != ncols {
            return None;
        }
        rows.push(ParsedRow {
            line_no: line.line_no,
            cells,
        });
    }

    if rows.len() < MIN_ROWS {
        return None;
    }

    if !is_plausible_table(&headers, &rows, delim) {
        return None;
    }

    Some(ParsedCsv {
        delim,
        headers,
        rows,
    })
}

/// Strict single-line RFC 4180 parser parameterized by `delim` (`b','` or `b'\t'`).
///
/// Rejects unclosed quotes, trailing non-delimiter text after a closing quote,
/// and unescaped double quotes inside unquoted fields. Borrows unquoted and
/// escape-free quoted fields directly from `line` without heap allocation.
fn parse_rfc4180_line<'a>(
    line: &'a str,
    delim: u8,
    expected_cols: usize,
) -> Option<Vec<Cow<'a, str>>> {
    let bytes = line.as_bytes();
    let mut fields = Vec::with_capacity(if expected_cols > 0 { expected_cols } else { 8 });
    let mut i = 0usize;
    loop {
        if expected_cols > 0 && fields.len() >= expected_cols {
            return None;
        }
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let mut owned: Option<String> = None;
            let mut closed = false;
            let mut chunk_start = i;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        let buf = match &mut owned {
                            Some(s) => {
                                s.push_str(&line[chunk_start..i]);
                                s
                            }
                            None => {
                                let mut s = String::with_capacity((i - chunk_start) + 16);
                                s.push_str(&line[chunk_start..i]);
                                owned.insert(s)
                            }
                        };
                        buf.push('"');
                        i += 2;
                        chunk_start = i;
                    } else {
                        let cell = match owned {
                            Some(mut s) => {
                                s.push_str(&line[chunk_start..i]);
                                Cow::Owned(s)
                            }
                            None => Cow::Borrowed(&line[chunk_start..i]),
                        };
                        fields.push(cell);
                        i += 1;
                        closed = true;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if !closed {
                return None;
            }
            if i == bytes.len() {
                break;
            }
            if bytes[i] != delim {
                return None;
            }
            i += 1;
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != delim {
                if bytes[i] == b'"' {
                    return None;
                }
                i += 1;
            }
            fields.push(Cow::Borrowed(&line[start..i]));
            if i == bytes.len() {
                break;
            }
            i += 1;
        }
    }
    Some(fields)
}

fn has_balanced_delimiters(s: &str) -> bool {
    let mut parens = 0i32;
    let mut brackets = 0i32;
    for c in s.chars() {
        match c {
            '(' => parens += 1,
            ')' => {
                parens -= 1;
                if parens < 0 {
                    return false;
                }
            }
            '[' => brackets += 1,
            ']' => {
                brackets -= 1;
                if brackets < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    parens == 0 && brackets == 0
}

fn is_prose_stopword(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "we"
            | "they"
            | "he"
            | "she"
            | "it"
            | "is"
            | "are"
            | "was"
            | "were"
            | "be"
            | "been"
            | "have"
            | "has"
            | "had"
            | "will"
            | "would"
            | "should"
            | "could"
            | "must"
            | "can"
            | "that"
            | "which"
            | "while"
            | "when"
            | "where"
            | "because"
            | "although"
            | "however"
            | "therefore"
            | "into"
            | "onto"
            | "their"
            | "there"
            | "these"
            | "those"
            | "very"
            | "just"
            | "also"
            | "only"
            | "then"
            | "than"
    )
}

fn is_plausible_header(headers: &[Cow<'_, str>], delim: Delim) -> bool {
    let mut seen: HashSet<&str> = HashSet::with_capacity(headers.len());
    let mut alpha_cols = 0usize;
    let mut year_cols = 0usize;
    let mut total_words = 0usize;

    for (idx, raw) in headers.iter().enumerate() {
        let t = raw.trim();
        if t.is_empty() || t.len() > 64 {
            return false;
        }
        // Reject heavy indentation or alignment padding (e.g., code blocks, formatted tables).
        if raw.len() - t.len() > 2 {
            return false;
        }
        if delim == Delim::Comma && raw.contains('\t') {
            return false;
        }
        if !seen.insert(t) {
            return false;
        }

        // Reject syntax characters characteristic of code, JSON, logfmt, and CLI tables.
        for bad in [
            "{", "}", ";", "=", ":", "|", "│", "`", "=>", "->", "||", "&&", "//", "/*", "*/",
            "<!--", "\\",
        ] {
            if t.contains(bad) {
                return false;
            }
        }

        // Reject sentence-ending punctuation and markdown/comment/log prefixes.
        if t.ends_with('.') || t.ends_with('!') || t.ends_with(';') || t.ends_with(':') {
            return false;
        }
        if t.starts_with("# ")
            || t.starts_with("---")
            || t.starts_with("+++")
            || t.starts_with("@@")
            || t.starts_with("```")
            || t.starts_with("~~~")
            || t.starts_with("- ")
            || t.starts_with("* ")
            || t.starts_with("> ")
            || t.starts_with("+--")
            || t.starts_with('[')
        {
            return false;
        }

        for prefix in [
            "fn ",
            "pub ",
            "let ",
            "use ",
            "impl ",
            "struct ",
            "enum ",
            "trait ",
            "mod ",
            "const ",
            "static ",
            "match ",
            "if ",
            "else ",
            "for ",
            "while ",
            "loop ",
            "return ",
            "def ",
            "class ",
            "import ",
            "from ",
            "async ",
            "await ",
            "function ",
            "var ",
            "package ",
            "func ",
            "SELECT ",
            "INSERT ",
            "UPDATE ",
            "DELETE ",
            "INFO ",
            "WARN ",
            "WARNING ",
            "ERROR ",
            "DEBUG ",
            "TRACE ",
            "FATAL ",
            "Traceback",
        ] {
            if t.starts_with(prefix) {
                return false;
            }
        }

        if matches!(
            t,
            "INFO" | "WARN" | "WARNING" | "ERROR" | "DEBUG" | "TRACE" | "FATAL"
        ) {
            return false;
        }

        if !has_balanced_delimiters(t) {
            return false;
        }

        // Reject emails and IPv4 addresses masquerading as column headers.
        if t.contains('@') && t.split('@').nth(1).is_some_and(|dom| dom.contains('.')) {
            return false;
        }
        if looks_like_ipv4(t) {
            return false;
        }

        let mut word_count = 0usize;
        let mut has_stopword = false;
        for w in t.split_whitespace() {
            word_count += 1;
            if is_prose_stopword(w) {
                has_stopword = true;
            }
        }
        if word_count > 5 || (headers.len() == 2 && word_count > 4) {
            return false;
        }
        if word_count >= 3 && has_stopword {
            return false;
        }
        total_words += word_count;

        // Reject header cells that look like data values (booleans, nulls, dates, floats,
        // or non-year integers).
        if is_bool_value(t) || is_null_token(t) || is_date_value(t) {
            return false;
        }
        if let Some(n) = parse_int_value(t) {
            if idx > 0 && (1900..=2100).contains(&n) && t.len() == 4 {
                year_cols += 1;
            } else {
                return false;
            }
        } else if parse_float_value(t).is_some() {
            return false;
        } else if t.chars().any(char::is_alphabetic) {
            alpha_cols += 1;
        }
    }

    if alpha_cols == 0 {
        return false;
    }
    if (alpha_cols + year_cols) * 2 < headers.len() {
        return false;
    }
    if headers.len() == 2 && total_words > 6 {
        return false;
    }

    true
}

fn looks_like_ipv4(s: &str) -> bool {
    let mut parts = s.split('.');
    let mut count = 0;
    for p in &mut parts {
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        if p.parse::<u8>().is_err() {
            return false;
        }
        count += 1;
    }
    count == 4
}

fn is_plausible_table(headers: &[Cow<'_, str>], rows: &[ParsedRow<'_>], delim: Delim) -> bool {
    let n = rows.len();
    let ncols = headers.len();

    // If a header cell equals >= 50% of data cells in its own column, line 1 is almost
    // certainly a headerless data row (e.g., `alice,us-east,prod` followed by `bob,us-east,prod`).
    for (c, header) in headers.iter().enumerate() {
        let header_str = header.as_ref();
        let matches = rows
            .iter()
            .filter(|r| r.cells[c].as_ref() == header_str)
            .count();
        if matches.saturating_mul(2) >= n {
            return false;
        }
    }

    // Reject code blocks where >= 50% of lines terminate with statement syntax (`;`, `{`, `}`).
    let code_endings = rows
        .iter()
        .filter(|r| {
            let last = r.cells[ncols - 1].trim_end();
            last.ends_with(';') || last.ends_with('{') || last.ends_with('}')
        })
        .count();
    if code_endings.saturating_mul(2) >= n {
        return false;
    }

    // Reject 2-column comma-separated English prose where rows are punctuated multi-word clauses.
    if delim == Delim::Comma && ncols == 2 {
        let prose_rows = rows
            .iter()
            .filter(|r| {
                let c0 = r.cells[0].trim();
                let c1 = r.cells[1].as_ref();
                let c1_trimmed = c1.trim();
                let space_after_comma = c1.starts_with(' ');
                let punctuated = c1_trimmed.ends_with('.')
                    || c1_trimmed.ends_with('!')
                    || c1_trimmed.ends_with('?');
                let words0 = c0.split_whitespace().count();
                let words1 = c1_trimmed.split_whitespace().count();
                space_after_comma
                    && ((punctuated && words0 + words1 >= 5) || (words0 >= 3 && words1 >= 4))
            })
            .count();
        if prose_rows.saturating_mul(2) >= n {
            return false;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Escaping, column planning, Tier 1 bijective encoding & decoding
// ---------------------------------------------------------------------------

fn escape_cell(s: &str) -> Cow<'_, str> {
    if !s
        .bytes()
        .any(|b| matches!(b, b'\\' | b'\t' | b'\n' | b'\r'))
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    Cow::Owned(out)
}

fn unescape_cell(s: &str) -> Option<Cow<'_, str>> {
    if !s
        .bytes()
        .any(|b| matches!(b, b'\\' | b'\t' | b'\n' | b'\r'))
    {
        return Some(Cow::Borrowed(s));
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                '\\' => out.push('\\'),
                't' => out.push('\t'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                _ => return None,
            }
        } else if matches!(c, '\t' | '\n' | '\r') {
            return None;
        } else {
            out.push(c);
        }
    }
    Some(Cow::Owned(out))
}

fn escape_col_name(s: &str) -> Cow<'_, str> {
    if !s
        .bytes()
        .any(|b| matches!(b, b'\\' | b'\t' | b'\n' | b'\r' | b'=' | b'?'))
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '=' => out.push_str("\\="),
            '?' => out.push_str("\\?"),
            other => out.push(other),
        }
    }
    Cow::Owned(out)
}

fn write_col_spec(name: &str, mode: &ColMode<'_>, out: &mut String) {
    out.push_str(&escape_col_name(name));
    match mode {
        ColMode::Plain => {}
        ColMode::Constant(val) => {
            out.push('=');
            out.push_str(&escape_cell(val));
        }
        ColMode::Default(val) => {
            out.push_str("?=");
            out.push_str(&escape_cell(val));
        }
    }
}

fn decode_col_spec(spec: &str) -> Option<(Cow<'_, str>, ColMode<'_>)> {
    if !spec
        .bytes()
        .any(|b| matches!(b, b'\\' | b'\t' | b'\n' | b'\r' | b'=' | b'?'))
    {
        return Some((Cow::Borrowed(spec), ColMode::Plain));
    }
    let mut name = String::with_capacity(spec.len());
    let mut chars = spec.char_indices();
    while let Some((idx, c)) = chars.next() {
        match c {
            '\\' => match chars.next()?.1 {
                '\\' => name.push('\\'),
                't' => name.push('\t'),
                'n' => name.push('\n'),
                'r' => name.push('\r'),
                '=' => name.push('='),
                '?' => name.push('?'),
                _ => return None,
            },
            '=' => {
                let val = unescape_cell(&spec[idx + 1..])?;
                return Some((Cow::Owned(name), ColMode::Constant(val)));
            }
            '?' => {
                let rest = spec[idx + 1..].strip_prefix('=')?;
                let val = unescape_cell(rest)?;
                return Some((Cow::Owned(name), ColMode::Default(val)));
            }
            '\t' | '\n' | '\r' => return None,
            other => name.push(other),
        }
    }
    Some((Cow::Owned(name), ColMode::Plain))
}

fn encode_varying_cell<'a>(cell: &'a str, mode: &ColMode<'_>) -> Cow<'a, str> {
    match mode {
        ColMode::Plain => escape_cell(cell),
        ColMode::Default(default_val) => {
            if cell == default_val.as_ref() {
                Cow::Borrowed(".")
            } else if cell == "." {
                Cow::Borrowed("\\.")
            } else {
                escape_cell(cell)
            }
        }
        ColMode::Constant(_) => unreachable!("constant columns are omitted from rows"),
    }
}

fn decode_varying_cell<'a>(encoded: &'a str, mode: &ColMode<'a>) -> Option<Cow<'a, str>> {
    match mode {
        ColMode::Plain => unescape_cell(encoded),
        ColMode::Default(default_val) => {
            if encoded == "." {
                Some(default_val.clone())
            } else if encoded == "\\." {
                Some(Cow::Borrowed("."))
            } else {
                unescape_cell(encoded)
            }
        }
        ColMode::Constant(_) => unreachable!("constant columns are omitted from rows"),
    }
}

fn plan_column_mode<'a>(rows: &[ParsedRow<'a>], col: usize) -> ColMode<'a> {
    let n = rows.len();
    let first = &rows[0].cells[col];
    if rows[1..]
        .iter()
        .all(|r| r.cells[col].as_ref() == first.as_ref())
    {
        return ColMode::Constant(first.clone());
    }

    let mut counts: HashMap<&str, usize> = HashMap::with_capacity(n.min(64));
    for r in rows {
        *counts.entry(r.cells[col].as_ref()).or_insert(0) += 1;
    }
    let Some((&best_val, &best_count)) = counts
        .iter()
        .max_by_key(|&(&val, &count)| (count, val.len(), Reverse(val)))
    else {
        return ColMode::Plain;
    };

    // Require >= 70% frequency.
    if best_count.saturating_mul(10) < n.saturating_mul(7) {
        return ColMode::Plain;
    }

    let escaped_len = escape_cell(best_val).len();
    // Replacing a <= 1-byte value with `.` cannot save bytes.
    if escaped_len <= 1 {
        return ColMode::Plain;
    }

    let dot_outliers = counts.get(".").copied().unwrap_or(0);
    let header_cost = 2 + escaped_len; // `?=VAL`
    let row_savings = best_count * (escaped_len - 1);
    if row_savings > header_cost + dot_outliers {
        let best_cow = rows
            .iter()
            .find(|r| r.cells[col].as_ref() == best_val)
            .map(|r| r.cells[col].clone())
            .unwrap_or_else(|| Cow::Owned(best_val.to_owned()));
        ColMode::Default(best_cow)
    } else {
        ColMode::Plain
    }
}

fn build_column_plan<'a>(csv: &ParsedCsv<'a>) -> ColumnPlan<'a> {
    let ncols = csv.headers.len();
    let modes: Vec<ColMode<'a>> = (0..ncols).map(|c| plan_column_mode(&csv.rows, c)).collect();
    let varying: Vec<usize> = (0..ncols)
        .filter(|&c| !matches!(modes[c], ColMode::Constant(_)))
        .collect();
    let has_const = modes.iter().any(|m| matches!(m, ColMode::Constant(_)));
    let has_default = modes.iter().any(|m| matches!(m, ColMode::Default(_)));

    let est_cols_len: usize = 7
        + csv.headers.iter().map(|h| h.len() + 1).sum::<usize>()
        + modes
            .iter()
            .map(|m| match m {
                ColMode::Plain => 0,
                ColMode::Constant(v) => v.len() + 1,
                ColMode::Default(v) => v.len() + 2,
            })
            .sum::<usize>();
    let mut cols_line = String::with_capacity(est_cols_len);
    cols_line.push_str("cols: ");
    for (c, name) in csv.headers.iter().enumerate() {
        if c > 0 {
            cols_line.push('\t');
        }
        write_col_spec(name, &modes[c], &mut cols_line);
    }
    cols_line.push('\n');

    ColumnPlan {
        modes,
        varying,
        has_const,
        has_default,
        cols_line,
    }
}

fn encode_row(row: &ParsedRow<'_>, plan: &ColumnPlan<'_>) -> String {
    let est: usize = plan.varying.iter().map(|&c| row.cells[c].len() + 1).sum();
    let mut line = String::with_capacity(est);
    for (i, &c) in plan.varying.iter().enumerate() {
        if i > 0 {
            line.push('\t');
        }
        line.push_str(&encode_varying_cell(&row.cells[c], &plan.modes[c]));
    }
    line
}

fn decode_encoded_row<'a>(line: &'a str, plan: &ColumnPlan<'a>) -> Option<Vec<Cow<'a, str>>> {
    let mut parts = line.split('\t');
    let mut full_row = Vec::with_capacity(plan.modes.len());
    for mode in &plan.modes {
        match mode {
            ColMode::Constant(val) => full_row.push(val.clone()),
            ColMode::Plain | ColMode::Default(_) => {
                let part = parts.next()?;
                full_row.push(decode_varying_cell(part, mode)?);
            }
        }
    }
    if parts.next().is_some() {
        return None;
    }
    Some(full_row)
}

fn tier1_legend_suffix(
    delim: Delim,
    has_varying: bool,
    has_const: bool,
    has_default: bool,
) -> &'static str {
    match (delim == Delim::Comma && has_varying, has_const, has_default) {
        (true, true, true) => " · TSV · col=V const, col?=V default (.=V)",
        (true, true, false) => " · TSV · col=V const",
        (true, false, true) => " · TSV · col?=V default (.=V)",
        (true, false, false) => " · TSV",
        (false, true, true) => " · col=V const, col?=V default (.=V)",
        (false, true, false) => " · col=V const",
        (false, false, true) => " · col?=V default (.=V)",
        (false, false, false) => "",
    }
}

/// Attempts to render the full Tier 1 bijective lossless view, bailing out early
/// as soon as the output would exceed `budget`.
fn try_render_tier1(
    csv: &ParsedCsv<'_>,
    plan: &ColumnPlan<'_>,
    budget: usize,
    raw_len: usize,
) -> Option<String> {
    let has_varying = !plan.varying.is_empty();
    let suffix = tier1_legend_suffix(csv.delim, has_varying, plan.has_const, plan.has_default);
    let est_cap = if has_varying {
        raw_len.min(budget).max(plan.cols_line.len() + 96)
    } else {
        plan.cols_line.len() + 96
    };
    let mut out = String::with_capacity(est_cap);
    let _ = writeln!(
        out,
        "[{} {} rows × {} cols{suffix}]",
        csv.delim.label(),
        csv.rows.len(),
        csv.headers.len()
    );
    out.push_str(&plan.cols_line);
    if out.len() > budget {
        return None;
    }
    if has_varying {
        for row in &csv.rows {
            for (i, &c) in plan.varying.iter().enumerate() {
                if i > 0 {
                    out.push('\t');
                }
                out.push_str(&encode_varying_cell(&row.cells[c], &plan.modes[c]));
            }
            out.push('\n');
            if out.len() > budget {
                return None;
            }
        }
    }
    Some(out)
}

fn decode_csv(view: &str) -> Option<DecodedCsv<'_>> {
    let mut lines = view.strip_suffix('\n')?.split('\n');

    // 1. Legend line: `[CSV 50 rows × 6 cols · TSV · col=V const, col?=V default (.=V)]`
    let legend = lines.next()?;
    let inner = legend.strip_prefix('[')?.strip_suffix(']')?;
    let (delim, rest) = if let Some(r) = inner.strip_prefix("CSV ") {
        (Delim::Comma, r)
    } else {
        (Delim::Tab, inner.strip_prefix("TSV ")?)
    };
    let (rows_str, rest) = rest.split_once(" rows × ")?;
    if rows_str.is_empty() || !rows_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let row_count: usize = rows_str.parse().ok()?;
    if row_count < MIN_ROWS {
        return None;
    }
    let (cols_str, mut rest) = rest.split_once(" cols")?;
    if cols_str.is_empty() || !cols_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let col_count: usize = cols_str.parse().ok()?;
    if !(MIN_COLS..=MAX_COLS).contains(&col_count) {
        return None;
    }

    let has_tsv_flag = if let Some(after) = rest.strip_prefix(" · TSV") {
        rest = after;
        true
    } else {
        false
    };
    let (expect_const, expect_default) = match rest {
        "" => (false, false),
        " · col=V const" => (true, false),
        " · col?=V default (.=V)" => (false, true),
        " · col=V const, col?=V default (.=V)" => (true, true),
        _ => return None,
    };

    // 2. Schema line (`cols: ...`)
    let cols_line = lines.next()?.strip_prefix("cols: ")?;
    let mut headers = Vec::with_capacity(col_count);
    let mut modes = Vec::with_capacity(col_count);
    for spec in cols_line.split('\t') {
        let (name, mode) = decode_col_spec(spec)?;
        headers.push(name);
        modes.push(mode);
    }
    if headers.len() != col_count {
        return None;
    }

    let has_const = modes.iter().any(|m| matches!(m, ColMode::Constant(_)));
    let has_default = modes.iter().any(|m| matches!(m, ColMode::Default(_)));
    if has_const != expect_const || has_default != expect_default {
        return None;
    }

    let has_varying = modes.iter().any(|m| !matches!(m, ColMode::Constant(_)));
    let expect_tsv_flag = delim == Delim::Comma && has_varying;
    if has_tsv_flag != expect_tsv_flag {
        return None;
    }

    // 3. Exact `row_count` data rows
    let mut rows = Vec::with_capacity(row_count);
    if !has_varying {
        let const_row: Vec<Cow<'_, str>> = modes
            .iter()
            .map(|m| match m {
                ColMode::Constant(v) => v.clone(),
                _ => unreachable!(),
            })
            .collect();
        for _ in 0..row_count {
            rows.push(const_row.clone());
        }
    } else {
        for _ in 0..row_count {
            let line = lines.next()?;
            let mut parts = line.split('\t');
            let mut full_row = Vec::with_capacity(col_count);
            for mode in &modes {
                match mode {
                    ColMode::Constant(val) => full_row.push(val.clone()),
                    ColMode::Plain | ColMode::Default(_) => {
                        let part = parts.next()?;
                        full_row.push(decode_varying_cell(part, mode)?);
                    }
                }
            }
            if parts.next().is_some() {
                return None;
            }
            rows.push(full_row);
        }
    }

    // Reject trailing garbage lines.
    if lines.next().is_some() {
        return None;
    }

    Some(DecodedCsv {
        delim,
        headers,
        rows,
    })
}

fn verify_round_trip(csv: &ParsedCsv<'_>, rendered: &str) -> bool {
    let Some(decoded) = decode_csv(rendered) else {
        return false;
    };
    if decoded.delim != csv.delim
        || decoded.headers != csv.headers
        || decoded.rows.len() != csv.rows.len()
    {
        return false;
    }
    decoded
        .rows
        .iter()
        .zip(&csv.rows)
        .all(|(dec, orig)| dec == &orig.cells)
}

// ---------------------------------------------------------------------------
// Type inference & Tier 2 bounded schema + summary preview
// ---------------------------------------------------------------------------

fn classify_missing(cell: &str) -> MissingKind {
    let t = cell.trim();
    if t.is_empty() {
        MissingKind::Empty
    } else if is_null_token(t) {
        MissingKind::NullToken
    } else {
        MissingKind::Present
    }
}

fn is_null_token(t: &str) -> bool {
    t == "\\N"
        || t.eq_ignore_ascii_case("null")
        || t.eq_ignore_ascii_case("na")
        || t.eq_ignore_ascii_case("n/a")
        || t.eq_ignore_ascii_case("none")
        || t.eq_ignore_ascii_case("nan")
}

fn is_bool_value(t: &str) -> bool {
    t.eq_ignore_ascii_case("true")
        || t.eq_ignore_ascii_case("false")
        || t.eq_ignore_ascii_case("yes")
        || t.eq_ignore_ascii_case("no")
}

fn parse_int_value(t: &str) -> Option<i64> {
    if t.is_empty() {
        return None;
    }
    let digits = t
        .strip_prefix('-')
        .or_else(|| t.strip_prefix('+'))
        .unwrap_or(t);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Preserve leading-zero identifiers (ZIP codes, padded IDs) as strings.
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    t.parse::<i64>().ok()
}

fn parse_float_value(t: &str) -> Option<f64> {
    if t.is_empty() {
        return None;
    }
    let unsigned = t
        .strip_prefix('-')
        .or_else(|| t.strip_prefix('+'))
        .unwrap_or(t);
    let b = unsigned.as_bytes();
    if b.is_empty() {
        return None;
    }
    // Reject leading-zero multi-digit integer part like `001.5` or `0042`.
    if b.len() >= 2 && b[0] == b'0' && b[1].is_ascii_digit() {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn parse_2_digits(b: &[u8]) -> Option<u32> {
    if b.len() == 2 && b[0].is_ascii_digit() && b[1].is_ascii_digit() {
        Some(u32::from(b[0] - b'0') * 10 + u32::from(b[1] - b'0'))
    } else {
        None
    }
}

fn is_date_value(t: &str) -> bool {
    let b = t.as_bytes();
    if b.len() < 10 || b.len() > 35 {
        return false;
    }
    if !b[0..4].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let sep = b[4];
    if sep != b'-' && sep != b'/' {
        return false;
    }
    let Some(month) = parse_2_digits(&b[5..7]) else {
        return false;
    };
    if !(1..=12).contains(&month) || b[7] != sep {
        return false;
    }
    let Some(day) = parse_2_digits(&b[8..10]) else {
        return false;
    };
    if !(1..=31).contains(&day) {
        return false;
    }
    if b.len() == 10 {
        return true;
    }
    if sep != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return false;
    }
    // `HH:MM`
    if b.len() < 16 || b[13] != b':' {
        return false;
    }
    let Some(hour) = parse_2_digits(&b[11..13]) else {
        return false;
    };
    let Some(minute) = parse_2_digits(&b[14..16]) else {
        return false;
    };
    if hour > 23 || minute > 59 {
        return false;
    }
    let mut i = 16usize;
    // Optional `:SS`
    if i < b.len() && b[i] == b':' {
        if i + 3 > b.len() {
            return false;
        }
        let Some(sec) = parse_2_digits(&b[i + 1..i + 3]) else {
            return false;
        };
        if sec > 60 {
            return false;
        }
        i += 3;
        // Optional `.fractional`
        if i < b.len() && (b[i] == b'.' || b[i] == b',') {
            i += 1;
            let frac_start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == frac_start || i - frac_start > 9 {
                return false;
            }
        }
    }
    if i == b.len() {
        return true;
    }
    // Optional timezone `Z` / `z` / `[+-]HH:?MM`
    if (b[i] == b'Z' || b[i] == b'z') && i + 1 == b.len() {
        return true;
    }
    if b[i] == b'+' || b[i] == b'-' {
        let rest = &b[i + 1..];
        let (hh, mm) = if rest.len() == 5 && rest[2] == b':' {
            (&rest[0..2], &rest[3..5])
        } else if rest.len() == 4 {
            (&rest[0..2], &rest[2..4])
        } else {
            return false;
        };
        let Some(tz_h) = parse_2_digits(hh) else {
            return false;
        };
        let Some(tz_m) = parse_2_digits(mm) else {
            return false;
        };
        return tz_h <= 23 && tz_m <= 59;
    }
    false
}

fn sanitize_schema_val(s: &str) -> String {
    let mut cleaned = String::with_capacity(s.len().min(MAX_SCHEMA_VAL_CHARS + 4));
    let mut truncated = false;
    for (count, c) in s.chars().enumerate() {
        if count >= MAX_SCHEMA_VAL_CHARS {
            truncated = true;
            break;
        }
        match c {
            '\t' => cleaned.push_str("\\t"),
            '\n' => cleaned.push_str("\\n"),
            '\r' => cleaned.push_str("\\r"),
            other => cleaned.push(other),
        }
    }
    if truncated {
        cleaned.push('…');
    }
    cleaned
}

fn format_column_summary(csv: &ParsedCsv<'_>, col: usize) -> String {
    let n = csv.rows.len();
    let name = csv.headers[col].as_ref();

    let mut counts: HashMap<&str, usize> = HashMap::with_capacity(n.min(64));
    let mut empty_count = 0usize;
    let mut null_token_count = 0usize;
    let mut non_missing = 0usize;

    let mut all_int = true;
    let mut all_float = true;
    let mut all_bool_tf = true;
    let mut all_bool_yn = true;
    let mut all_date = true;

    let mut min_int = i64::MAX;
    let mut max_int = i64::MIN;
    let mut min_float: Option<(f64, &str)> = None;
    let mut max_float: Option<(f64, &str)> = None;
    let mut min_date: Option<&str> = None;
    let mut max_date: Option<&str> = None;
    let mut date_sep: Option<u8> = None;
    let mut date_same_sep = true;

    for r in &csv.rows {
        let cell = r.cells[col].as_ref();
        *counts.entry(cell).or_insert(0) += 1;
        match classify_missing(cell) {
            MissingKind::Empty => {
                empty_count += 1;
                continue;
            }
            MissingKind::NullToken => {
                null_token_count += 1;
                continue;
            }
            MissingKind::Present => {}
        }
        non_missing += 1;
        let t = cell.trim();

        if all_bool_tf && !(t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false")) {
            all_bool_tf = false;
        }
        if all_bool_yn && !(t.eq_ignore_ascii_case("yes") || t.eq_ignore_ascii_case("no")) {
            all_bool_yn = false;
        }
        if all_int {
            if let Some(v) = parse_int_value(t) {
                min_int = min_int.min(v);
                max_int = max_int.max(v);
            } else {
                all_int = false;
            }
        }
        if all_float {
            if let Some(v) = parse_float_value(t) {
                if min_float.is_none_or(|(cur, _)| v.total_cmp(&cur).is_lt()) {
                    min_float = Some((v, t));
                }
                if max_float.is_none_or(|(cur, _)| v.total_cmp(&cur).is_gt()) {
                    max_float = Some((v, t));
                }
            } else {
                all_float = false;
            }
        }
        if all_date {
            if is_date_value(t) {
                let sep = t.as_bytes()[4];
                if let Some(prev) = date_sep {
                    if prev != sep {
                        date_same_sep = false;
                    }
                } else {
                    date_sep = Some(sep);
                }
                if min_date.is_none_or(|cur| t < cur) {
                    min_date = Some(t);
                }
                if max_date.is_none_or(|cur| t > cur) {
                    max_date = Some(t);
                }
            } else {
                all_date = false;
            }
        }
    }

    let col_type = if non_missing == 0 {
        ColType::Str
    } else if all_bool_tf || all_bool_yn {
        ColType::Bool
    } else if all_int {
        ColType::Int
    } else if all_float {
        ColType::Float
    } else if all_date {
        ColType::Date
    } else {
        ColType::Str
    };

    let missing_total = empty_count + null_token_count;
    let missing_tag = if missing_total == 0 {
        None
    } else if null_token_count == 0 {
        Some(format!("empty={empty_count}"))
    } else {
        Some(format!("null={missing_total}"))
    };

    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let distinct = ranked.len();
    let (top_val, top_count) = ranked[0];

    let safe_name = sanitize_schema_val(name);
    let mut line = format!("  {safe_name} ({}): ", col_type.label());
    let mut parts: Vec<String> = Vec::with_capacity(4);

    if distinct == 1 {
        if top_val.is_empty() {
            parts.push(format!("{safe_name}= (all empty)"));
        } else {
            parts.push(format!("{safe_name}={}", sanitize_schema_val(top_val)));
        }
    } else {
        let is_majority =
            !top_val.is_empty() && top_count.saturating_mul(10) >= n.saturating_mul(7);
        if is_majority {
            let pct = ((top_count * 100 + n / 2) / n).min(99);
            parts.push(format!(
                "{safe_name}?={} {pct}%",
                sanitize_schema_val(top_val)
            ));
        }

        match col_type {
            ColType::Int => {
                if non_missing > 0 {
                    parts.push(format!("{min_int}..{max_int}"));
                }
                if distinct <= MAX_CATEGORICAL_DISTINCT && !is_majority {
                    parts.push(format!("{distinct} distinct"));
                }
            }
            ColType::Float => {
                if let (Some((_, lo)), Some((_, hi))) = (min_float, max_float) {
                    parts.push(format!("{lo}..{hi}"));
                }
            }
            ColType::Date => {
                if date_same_sep && let (Some(lo), Some(hi)) = (min_date, max_date) {
                    parts.push(format!("{lo}..{hi}"));
                } else {
                    parts.push(format!("{distinct} distinct"));
                }
            }
            ColType::Bool => {
                if !is_majority {
                    parts.push(format_top_values(&ranked, MAX_TOP_VALUES, true));
                }
            }
            ColType::Str => {
                if distinct <= MAX_CATEGORICAL_DISTINCT && distinct < n {
                    parts.push(format!(
                        "{distinct} distinct [{}]",
                        format_top_values(&ranked, MAX_TOP_VALUES, false)
                    ));
                } else if distinct == n {
                    parts.push(format!("{distinct} unique"));
                } else if ranked[0].1 >= 2 {
                    parts.push(format!(
                        "{distinct} distinct, top [{}]",
                        format_top_values(&ranked, 3, false)
                    ));
                } else {
                    parts.push(format!("{distinct} distinct"));
                }
            }
        }

        // For majority columns with low cardinality, also surface the distribution.
        if is_majority && col_type != ColType::Str && distinct <= MAX_CATEGORICAL_DISTINCT {
            parts.push(format!(
                "{distinct} distinct [{}]",
                format_top_values(&ranked, MAX_TOP_VALUES, false)
            ));
        }

        if let Some(tag) = missing_tag {
            parts.push(tag);
        }
    }

    line.push_str(&parts.join(", "));
    line.push('\n');
    line
}

fn format_top_values(ranked: &[(&str, usize)], limit: usize, skip_empty: bool) -> String {
    let mut items: Vec<String> = Vec::with_capacity(limit + 1);
    let mut shown = 0usize;
    let mut total_eligible = 0usize;
    for &(val, count) in ranked {
        if skip_empty && val.is_empty() {
            continue;
        }
        total_eligible += 1;
        if shown < limit {
            let label = if val.is_empty() {
                String::from("<empty>")
            } else {
                sanitize_schema_val(val)
            };
            items.push(format!("{label}:{count}"));
            shown += 1;
        }
    }
    if total_eligible > shown {
        items.push(format!("+{} more", total_eligible - shown));
    }
    items.join(", ")
}

fn digits(mut n: usize) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

fn stdout_ref(first: usize, last: usize) -> String {
    if first == last {
        format!("[stdout:{first}]")
    } else {
        format!("[stdout:{first}-{last}]")
    }
}

fn elision_line(omitted: usize, first_line: usize, last_line: usize) -> String {
    let unit = if omitted == 1 { "row" } else { "rows" };
    format!(
        "... {omitted} {unit} omitted {} ...\n",
        stdout_ref(first_line, last_line)
    )
}

fn elision_line_len(omitted: usize, first_line: usize, last_line: usize) -> usize {
    let unit_len = if omitted == 1 { 3 } else { 4 };
    let ref_len = if first_line == last_line {
        "[stdout:]".len() + digits(first_line)
    } else {
        "[stdout:-]".len() + digits(first_line) + digits(last_line)
    };
    "...  ".len() + digits(omitted) + unit_len + " omitted ".len() + ref_len + " ...\n".len()
}

fn render_tier2(csv: &ParsedCsv<'_>, plan: &ColumnPlan<'_>, budget: usize) -> Option<String> {
    let n = csv.rows.len();
    if plan.varying.is_empty() || n < 2 {
        return None;
    }

    let header_line = format!(
        "[{} {} rows × {} cols · exact data in full log]\n",
        csv.delim.label(),
        n,
        csv.headers.len()
    );

    let sample_header = match (plan.has_const, plan.has_default) {
        (true, true) => "sample (TSV; col=V const, col?=V default (.=V)):\n",
        (true, false) => "sample (TSV; col=V const):\n",
        (false, true) => "sample (TSV; col?=V default (.=V)):\n",
        (false, false) => "sample (TSV):\n",
    };

    // Lazily encode only the candidate head and tail sample rows, verifying every
    // encoded sample row round-trips back to its exact original cells.
    let max_head = MAX_TIER2_HEAD.min(n - 1);
    let max_tail = MAX_TIER2_TAIL.min(n - 1);

    let mut head_rows = Vec::with_capacity(max_head);
    for row in &csv.rows[..max_head] {
        let enc = encode_row(row, plan);
        if decode_encoded_row(&enc, plan).as_deref() != Some(row.cells.as_slice()) {
            return None;
        }
        head_rows.push(enc);
    }

    let mut tail_rows = Vec::with_capacity(max_tail);
    for row in &csv.rows[n - max_tail..] {
        let enc = encode_row(row, plan);
        if decode_encoded_row(&enc, plan).as_deref() != Some(row.cells.as_slice()) {
            return None;
        }
        tail_rows.push(enc);
    }

    let sample_prefix_len = sample_header.len() + plan.cols_line.len();
    let row0_bytes = head_rows[0].len() + 1;
    let last_row_bytes = tail_rows[max_tail - 1].len() + 1;

    let min_elision_1_1 = if n >= 3 {
        elision_line_len(n - 2, csv.rows[1].line_no, csv.rows[n - 2].line_no)
    } else {
        usize::MAX / 4
    };
    let min_elision_1_0 = elision_line_len(n - 1, csv.rows[1].line_no, csv.rows[n - 1].line_no);

    let min_sample_1_1 = sample_prefix_len + row0_bytes + min_elision_1_1 + last_row_bytes;
    let min_sample_1_0 = sample_prefix_len + row0_bytes + min_elision_1_0;
    let min_sample_needed = if n >= 3 && header_line.len() + min_sample_1_1 <= budget {
        min_sample_1_1
    } else if header_line.len() + min_sample_1_0 <= budget {
        min_sample_1_0
    } else {
        return None;
    };

    // Build per-column schema lines, staying within remaining budget so sample rows always fit.
    let schema_budget = budget.saturating_sub(header_line.len() + min_sample_needed);
    let schema_lines: Vec<String> = (0..csv.headers.len())
        .map(|c| format_column_summary(csv, c))
        .collect();
    let total_schema_bytes: usize =
        "schema:\n".len() + schema_lines.iter().map(String::len).sum::<usize>();
    let schema_block = if total_schema_bytes <= schema_budget {
        let mut s = String::with_capacity(total_schema_bytes);
        s.push_str("schema:\n");
        for l in &schema_lines {
            s.push_str(l);
        }
        s
    } else if schema_budget > "schema:\n".len() + 32 {
        let mut s = String::with_capacity(schema_budget);
        s.push_str("schema:\n");
        let mut included = 0usize;
        for (idx, l) in schema_lines.iter().enumerate() {
            let remaining_cols = schema_lines.len() - idx - 1;
            let more_len = if remaining_cols > 0 {
                "  ... + more columns\n".len() + digits(remaining_cols)
            } else {
                0
            };
            if s.len() + l.len() + more_len <= schema_budget {
                s.push_str(l);
                included += 1;
            } else {
                let omitted_cols = schema_lines.len() - included;
                let _ = writeln!(s, "  ... +{omitted_cols} more columns");
                break;
            }
        }
        if included == 0 {
            s.clear();
        }
        s
    } else {
        String::new()
    };

    let fixed_bytes = header_line.len() + schema_block.len() + sample_prefix_len;

    let total_for = |head_k: usize, tail_m: usize, head_bytes: usize, tail_bytes: usize| -> usize {
        let omitted = n - (head_k + tail_m);
        let first_line = csv.rows[head_k].line_no;
        let last_line = csv.rows[n - tail_m - 1].line_no;
        let elision_len = elision_line_len(omitted, first_line, last_line);
        fixed_bytes + head_bytes + elision_len + tail_bytes
    };

    let (mut head_k, mut tail_m, mut head_bytes, mut tail_bytes) =
        if n >= 3 && total_for(1, 1, row0_bytes, last_row_bytes) <= budget {
            (1, 1, row0_bytes, last_row_bytes)
        } else if total_for(1, 0, row0_bytes, 0) <= budget {
            (1, 0, row0_bytes, 0)
        } else {
            return None;
        };

    // Greedily grow head and tail in ~2:1 ratio up to (MAX_TIER2_HEAD, MAX_TIER2_TAIL)
    // while keeping >= 1 row omitted.
    while head_k + tail_m + 1 < n && (head_k < max_head || tail_m < max_tail) {
        let can_head = head_k < max_head;
        let can_tail = tail_m < max_tail;
        let prefer_head = can_head && (!can_tail || head_k <= tail_m * 2);

        let try_head = |hk: usize, tm: usize, hb: usize, tb: usize| -> Option<usize> {
            if hk >= max_head {
                return None;
            }
            let next_bytes = head_rows[hk].len() + 1;
            (total_for(hk + 1, tm, hb + next_bytes, tb) <= budget).then_some(next_bytes)
        };
        let try_tail = |hk: usize, tm: usize, hb: usize, tb: usize| -> Option<usize> {
            if tm >= max_tail {
                return None;
            }
            let next_bytes = tail_rows[max_tail - tm - 1].len() + 1;
            (total_for(hk, tm + 1, hb, tb + next_bytes) <= budget).then_some(next_bytes)
        };

        if prefer_head {
            if let Some(add) = try_head(head_k, tail_m, head_bytes, tail_bytes) {
                head_k += 1;
                head_bytes += add;
            } else if let Some(add) = try_tail(head_k, tail_m, head_bytes, tail_bytes) {
                tail_m += 1;
                tail_bytes += add;
            } else {
                break;
            }
        } else if let Some(add) = try_tail(head_k, tail_m, head_bytes, tail_bytes) {
            tail_m += 1;
            tail_bytes += add;
        } else if let Some(add) = try_head(head_k, tail_m, head_bytes, tail_bytes) {
            head_k += 1;
            head_bytes += add;
        } else {
            break;
        }
    }

    let omitted = n - (head_k + tail_m);
    let first_line = csv.rows[head_k].line_no;
    let last_line = csv.rows[n - tail_m - 1].line_no;

    let mut out = String::with_capacity(budget.min(fixed_bytes + head_bytes + tail_bytes + 96));
    out.push_str(&header_line);
    out.push_str(&schema_block);
    out.push_str(sample_header);
    out.push_str(&plan.cols_line);
    for row in &head_rows[..head_k] {
        out.push_str(row);
        out.push('\n');
    }
    out.push_str(&elision_line(omitted, first_line, last_line));
    for row in &tail_rows[max_tail - tail_m..] {
        out.push_str(row);
        out.push('\n');
    }

    (out.len() <= budget).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier1_realistic_50_row_csv_round_trips_losslessly() {
        let mut csv = String::from(
            "\"id\",\"service\",\"env\",\"region\",\"status\",\"customer\",\"latency_ms\",\"amount_usd\"\n",
        );
        let regions = ["us-east-1", "eu-west-1", "us-west-2", "ap-south-1"];
        for i in 1..=50 {
            let status = match i {
                12 => "timeout",
                27 => ".",
                39 => "error",
                45 => "",
                _ => "ok",
            };
            let customer = if i % 7 == 0 {
                format!("\"Smith, Alice \"\"VIP\"\" #{i}\"")
            } else {
                format!("\"customer_{i:03}\"")
            };
            let region = regions[i % regions.len()];
            let latency = 10 + (i * 7) % 180;
            let amount = format!("{:.2}", 9.5 + (i as f64) * 3.25);
            let _ = writeln!(
                csv,
                "\"{i}\",\"orders-api\",\"production\",\"{region}\",\"{status}\",{customer},\"{latency}\",\"{amount}\""
            );
        }

        let parsed = parse_csv(&csv).expect("should parse 50-row RFC 4180 CSV");
        assert_eq!(parsed.delim, Delim::Comma);
        assert_eq!(parsed.headers.len(), 8);
        assert_eq!(parsed.rows.len(), 50);

        let compacted = format(&csv, 8192).expect("expected Tier 1 compaction");
        assert!(
            compacted
                .starts_with("[CSV 50 rows × 8 cols · TSV · col=V const, col?=V default (.=V)]\n"),
            "{compacted}"
        );
        assert!(
            !compacted.contains("exact data in full log"),
            "50-row CSV should use lossless Tier 1:\n{compacted}"
        );
        assert!(
            compacted.contains("service=orders-api") && compacted.contains("env=production"),
            "expected hoisted constants:\n{compacted}"
        );
        assert!(
            compacted.contains("status?=ok"),
            "expected hoisted majority default:\n{compacted}"
        );
        assert!(
            compacted.contains("\\."),
            "expected literal `.` escaped as `\\.`:\n{compacted}"
        );
        assert!(
            compacted.contains("Smith, Alice \"VIP\" #7"),
            "expected stripped RFC 4180 quoting:\n{compacted}"
        );
        assert!(
            compacted.len() * 10 < csv.len() * 6,
            "expected >40% lossless reduction: raw={}, compacted={}",
            csv.len(),
            compacted.len()
        );

        let decoded = decode_csv(&compacted).expect("Tier 1 must decode bijectively");
        assert_eq!(decoded.delim, Delim::Comma);
        assert_eq!(decoded.headers, parsed.headers);
        assert_eq!(decoded.rows.len(), 50);
        for (i, (dec, orig)) in decoded.rows.iter().zip(&parsed.rows).enumerate() {
            assert_eq!(dec, &orig.cells, "row {i} mismatch");
        }
    }

    #[test]
    fn tier2_realistic_1000_row_100kb_csv_achieves_over_95_percent_reduction() {
        let mut csv = String::from(
            "id,timestamp,service,env,region,status,active,latency_ms,amount_usd,request_note\n",
        );
        let regions = ["us-east-1", "eu-west-1", "us-west-2", "ap-south-1"];
        for i in 1..=1000usize {
            let hour = (i / 60) % 24;
            let min = i % 60;
            let region = regions[i % regions.len()];
            let status = if i % 25 == 0 {
                "timeout"
            } else if i % 50 == 1 {
                "err"
            } else {
                "ok"
            };
            let active = if i % 3 == 0 { "false" } else { "true" };
            let latency = if i % 100 == 0 {
                String::new()
            } else {
                format!("{}", 4 + (i * 19) % 1847)
            };
            let amount = format!("{:.2}", 0.99 + ((i % 200) as f64) * 2.50);
            let note = format!("checkout_trace_span_{i:06}_shard_{}", i % 16);
            let _ = writeln!(
                csv,
                "{i},2026-01-01T{hour:02}:{min:02}:00Z,checkout,prod,{region},{status},{active},{latency},{amount},{note}"
            );
        }
        assert!(
            csv.len() >= 95_000,
            "expected ~100 KB CSV fixture, got {} bytes",
            csv.len()
        );

        let view = format(&csv, 4096).expect("expected Tier 2 schema + summary preview");
        assert!(view.len() <= 4096, "view.len()={}", view.len());
        assert!(
            view.len() * 20 < csv.len(),
            "expected >95% reduction: raw={}, view={}\n{view}",
            csv.len(),
            view.len()
        );

        assert!(
            view.starts_with("[CSV 1000 rows × 10 cols · exact data in full log]\n"),
            "{view}"
        );
        // Column types
        assert!(view.contains("id (int): 1..1000"), "{view}");
        assert!(
            view.contains("timestamp (date): 2026-01-01T00:01:00Z..2026-01-01T16:40:00Z"),
            "{view}"
        );
        assert!(view.contains("service (str): service=checkout"), "{view}");
        assert!(view.contains("env (str): env=prod"), "{view}");
        assert!(view.contains("region (str): 4 distinct ["), "{view}");
        assert!(view.contains("status (str): status?=ok 94%"), "{view}");
        assert!(view.contains("active (bool):"), "{view}");
        assert!(
            view.contains("latency_ms (int):") && view.contains("empty=10"),
            "{view}"
        );
        assert!(view.contains("amount_usd (float): 0.99..498.49"), "{view}");

        // Sample rows + exact 1-based line range `[stdout:A-B]`
        // Header is line 1; 8 head rows are lines 2..=9; 4 tail rows are lines 998..=1001.
        // Omitted middle rows are 988 rows at lines 10..=997.
        assert!(
            view.contains("... 988 rows omitted [stdout:10-997] ..."),
            "{view}"
        );
        let raw_lines: Vec<&str> = csv.lines().collect();
        assert!(raw_lines[9].starts_with("9,"));
        assert!(raw_lines[996].starts_with("996,"));
        assert!(
            view.contains("1000\t"),
            "expected last row in tail:\n{view}"
        );

        // Even with a generous budget, Tier 2 stays concise (>95% reduction).
        let generous = format(&csv, 16_384).expect("expected concise Tier 2");
        assert!(
            generous.len() * 20 < csv.len(),
            "expected >95% reduction even with 16 KB budget: raw={}, view={}",
            csv.len(),
            generous.len()
        );
    }

    #[test]
    fn tsv_with_constants_majority_quoted_commas_escapes_empties_and_literal_dot() {
        let mut tsv = String::from("id\tcluster\ttier\tlocation\tquote_note\textra\n");
        for i in 1..=20 {
            let tier = match i {
                3 => ".",
                7 => "\\.",
                11 => "",
                15 => "enterprise",
                _ => "standard",
            };
            let location = match i % 3 {
                0 => "\"Austin, TX\"",
                1 => "\"London, UK\"",
                _ => "\"Tokyo, JP\"",
            };
            let note = match i {
                4 => "\"Say \"\"hello\"\" now\"",
                8 => "\"col\twith\ttab\"",
                12 => "",
                _ => "plain_note",
            };
            let extra = if i % 5 == 0 { "" } else { "v1" };
            let _ = writeln!(
                tsv,
                "{i}\tprod-us-east-1\t{tier}\t{location}\t{note}\t{extra}"
            );
        }

        let parsed = parse_csv(&tsv).expect("should parse TSV");
        assert_eq!(parsed.delim, Delim::Tab);
        assert_eq!(parsed.rows.len(), 20);
        assert_eq!(parsed.rows[0].cells[3], "London, UK");
        assert_eq!(parsed.rows[3].cells[4], "Say \"hello\" now");
        assert_eq!(parsed.rows[7].cells[4], "col\twith\ttab");
        assert_eq!(parsed.rows[4].cells[5], "");

        let view = format(&tsv, 8192).expect("expected Tier 1 TSV compaction");
        assert!(view.starts_with("[TSV 20 rows × 6 cols"), "{view}");
        assert!(view.contains("cluster=prod-us-east-1"), "{view}");
        assert!(view.contains("tier?=standard"), "{view}");
        assert!(view.contains("\\."), "{view}");
        assert!(view.contains("\\\\."), "{view}");
        assert!(view.contains("col\\twith\\ttab"), "{view}");

        let decoded = decode_csv(&view).expect("TSV Tier 1 must decode bijectively");
        assert_eq!(decoded.delim, Delim::Tab);
        assert_eq!(decoded.headers, parsed.headers);
        for (i, (dec, orig)) in decoded.rows.iter().zip(&parsed.rows).enumerate() {
            assert_eq!(dec, &orig.cells, "TSV row {i} mismatch");
        }
    }

    #[test]
    fn edge_cases_all_constant_single_varying_with_empties_and_crlf() {
        // 1. All constant columns -> 0 data lines, exact bijective decode.
        let mut all_const = String::from("service,env,region\n");
        for _ in 0..15 {
            all_const.push_str("billing-service,production,us-east-1\n");
        }
        let view = format(&all_const, 4096).expect("all-constant CSV compacts");
        assert!(
            view.starts_with("[CSV 15 rows × 3 cols · col=V const]\n"),
            "{view}"
        );
        assert_eq!(view.lines().count(), 2, "{view}");
        let decoded = decode_csv(&view).expect("all-constant decodes");
        assert_eq!(decoded.rows.len(), 15);
        assert!(
            decoded
                .rows
                .iter()
                .all(|r| r == &["billing-service", "production", "us-east-1"])
        );

        // 2. Single varying column with empty first, middle, and last cells + CRLF.
        let mut one_var = String::from("service,env,notes\r\n");
        for i in 0..12 {
            let note = match i {
                0 | 5 | 11 => "",
                _ => "note_text",
            };
            let _ = write!(one_var, "billing-service,production,{note}\r\n");
        }
        let view = format(&one_var, 4096).expect("single-varying CSV compacts");
        let decoded = decode_csv(&view).expect("single-varying CSV decodes");
        assert_eq!(decoded.rows.len(), 12);
        assert_eq!(decoded.rows[0][2], "");
        assert_eq!(decoded.rows[5][2], "");
        assert_eq!(decoded.rows[11][2], "");
        assert_eq!(decoded.rows[1][2], "note_text");
    }

    #[test]
    fn adversarial_non_csv_inputs_return_none() {
        // 1. Rust source code with uniform commas
        let rust_code = "\
fn add(a: i32, b: i32) -> i32 {
    let (x, y) = (a, b);
    let (p, q) = (x + 1, y + 1);
    let (r, s) = (p + 2, q + 2);
    let (t, u) = (r + 3, s + 3);
    let (v, w) = (t + 4, u + 4);
    let (m, n) = (v + 5, w + 5);
    let (j, k) = (m + 6, n + 6);
    let (d, e) = (j + 7, k + 7);
    d + e
}
";
        assert!(format(rust_code, 4096).is_none());

        // 2. Rust enum variants with trailing commas
        let rust_enum = "\
    Alpha,
    Beta,
    Gamma,
    Delta,
    Epsilon,
    Zeta,
    Eta,
    Theta,
    Iota,
    Kappa,
";
        assert!(format(rust_enum, 4096).is_none());

        // 3. English prose with commas (punctuated AND unpunctuated)
        let prose_punct = "\
In the beginning, we built a small local utility.
After careful testing, the parser handled every edge case.
When a file arrives, the engine checks its format first.
If the structure fails, the compactor falls back cleanly.
Because false positives hurt, strict rules guard every path.
During normal builds, logs remain untouched by CSV mode.
Before emitting output, round-trip checks verify every cell.
Even under load, memory usage stays strictly bounded here.
For large exports, schema summaries replace raw repetition.
Across every language, exact logs stay archived on disk.
";
        assert!(format(prose_punct, 4096).is_none());

        let prose_no_punct = "\
When the server starts up, the worker pool warms quickly
If the queue grows deep, the scheduler scales out cleanly
While the cache stays hot, the latency remains very low
Before the deploy finishes, the health probe checks ports
After the traffic shifts, the old replicas drain safely
Unless the disk fills up, the archives keep every byte
Until the job completes, the monitor watches for timeouts
Whenever a retry triggers, the backoff doubles each step
Although the payload varies, the schema stays quite small
Because the client retries, the request eventually succeeds
";
        assert!(format(prose_no_punct, 4096).is_none());

        // 4. Structured logs with commas / logfmt
        let mut logs = String::new();
        for i in 1..=20 {
            let _ = writeln!(
                logs,
                "2026-04-01T12:00:{i:02}Z INFO worker, service=api, status=ok, req={i}"
            );
        }
        assert!(format(&logs, 4096).is_none());

        // 5. Irregular column counts (row 6 has an extra column)
        let mut irregular = String::from("id,service,env,status\n");
        for i in 1..=15 {
            if i == 6 {
                let _ = writeln!(irregular, "{i},payments,production,ok,extra_col");
            } else {
                let _ = writeln!(irregular, "{i},payments,production,ok");
            }
        }
        assert!(format(&irregular, 4096).is_none());

        // 6. Malformed RFC 4180 quoting (stray quote / unclosed quote)
        let mut bad_quote = String::from("id,service,env,note\n");
        for i in 1..=12 {
            if i == 5 {
                let _ = writeln!(bad_quote, "{i},payments,production,bad \"quote\" here");
            } else {
                let _ = writeln!(bad_quote, "{i},payments,production,ok");
            }
        }
        assert!(format(&bad_quote, 4096).is_none());

        // 7. JSON / NDJSON
        let mut ndjson = String::new();
        for i in 1..=15 {
            let _ = writeln!(ndjson, r#"{{"id":{i},"service":"payments","env":"prod"}}"#);
        }
        assert!(format(&ndjson, 4096).is_none());

        // 8. CLI table (Markdown pipe table)
        let mut md = String::from("| id | service | env |\n|---|---|---|\n");
        for i in 1..=12 {
            let _ = writeln!(md, "| {i} | payments | prod |");
        }
        assert!(format(&md, 4096).is_none());

        // 9. Headerless numeric dump & headerless string dump
        let mut numeric_no_header = String::new();
        for i in 1..=20 {
            let _ = writeln!(numeric_no_header, "{i},payments,production,200");
        }
        assert!(format(&numeric_no_header, 4096).is_none());

        let mut string_no_header = String::new();
        for i in 1..=20 {
            let _ = writeln!(string_no_header, "user_{i},payments,production");
        }
        assert!(format(&string_no_header, 4096).is_none());

        // 10. NUL byte & ANSI escape sequences
        let with_nul = "id,service,env\n".to_owned() + &"1,payments,production\n".repeat(12) + "\0";
        assert!(format(&with_nul, 4096).is_none());
        let with_ansi =
            "id,service,env\n".to_owned() + &"\x1b[32m1\x1b[0m,payments,production\n".repeat(12);
        assert!(format(&with_ansi, 4096).is_none());

        // 11. Too few rows (< 8) and single column (< 2)
        let too_short = "id,service,env\n".to_owned() + &"1,payments,production\n".repeat(7);
        assert!(format(&too_short, 4096).is_none());
        let single_col = "service\n".to_owned() + &"payments-service-long-name\n".repeat(20);
        assert!(format(&single_col, 4096).is_none());
    }

    #[test]
    fn utf8_bom_csv_unquoted_and_quoted_headers_round_trip_cleanly() {
        // 1. Unquoted Excel UTF-8 BOM CSV with CRLF
        let mut unquoted = String::from("\u{feff}id,service,env,region,status\r\n");
        for i in 1..=20 {
            let status = if i == 10 { "timeout" } else { "ok" };
            let _ = write!(
                unquoted,
                "{i},billing-api,production,us-east-1,{status}\r\n"
            );
        }
        let parsed = parse_csv(&unquoted).expect("BOM-prefixed unquoted CSV should parse");
        assert_eq!(parsed.headers[0], "id", "BOM must not corrupt first header");
        assert!(!parsed.headers[0].contains('\u{feff}'));
        let view = format(&unquoted, 8192).expect("BOM-prefixed unquoted CSV should compact");
        assert!(view.contains("cols: id\tservice=billing-api"), "{view}");
        let decoded = decode_csv(&view).expect("BOM CSV view must decode bijectively");
        assert_eq!(decoded.headers, parsed.headers);
        assert_eq!(decoded.rows.len(), 20);
        for (dec, orig) in decoded.rows.iter().zip(&parsed.rows) {
            assert_eq!(dec, &orig.cells);
        }

        // 2. RFC 4180 quoted Excel UTF-8 BOM CSV (previously failed to parse when BOM preceded `"`)
        let mut quoted =
            String::from("\u{feff}\"id\",\"service\",\"env\",\"customer\",\"status\"\r\n");
        for i in 1..=20 {
            let status = if i == 5 { "error" } else { "ok" };
            let customer = if i % 4 == 0 {
                format!("\"Doe, Jane \"\"#{i}\"\"\"")
            } else {
                format!("\"cust_{i:02}\"")
            };
            let _ = write!(
                quoted,
                "\"{i}\",\"orders-api\",\"production\",{customer},\"{status}\"\r\n"
            );
        }
        let parsed_q = parse_csv(&quoted).expect("BOM-prefixed quoted CSV should parse");
        assert_eq!(parsed_q.headers[0], "id");
        let view_q = format(&quoted, 8192).expect("BOM-prefixed quoted CSV should compact");
        assert!(view_q.contains("cols: id\tservice=orders-api"), "{view_q}");
        let decoded_q = decode_csv(&view_q).expect("BOM quoted CSV must decode bijectively");
        assert_eq!(decoded_q.headers, parsed_q.headers);
        for (dec, orig) in decoded_q.rows.iter().zip(&parsed_q.rows) {
            assert_eq!(dec, &orig.cells);
        }
    }
}
