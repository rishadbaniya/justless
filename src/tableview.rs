//! Universal lossless compactor for CLI tables (ASCII grid, PostgreSQL aligned,
//! Unicode box-drawing / aligned, and standalone Markdown pipe tables).
//!
//! Two-tier output:
//! - **Tier 1 (lossless / bijective):** when the compacted table fits within
//!   `budget`, strips decorative borders and alignment padding, hoists 100%
//!   constant columns into the header (`col=VAL`, omitted from data rows) and
//!   at least 70% majority-default columns (`col?=VAL`, `.` on matching rows
//!   with literal `.` escaped as `\.`), emits compact TSV data rows, and
//!   verifies a full bijective round-trip via `decode_table`.
//! - **Tier 2 (bounded preview):** when Tier 1 exceeds `budget`, emits the same
//!   hoisted header (plus DuckDB column types when present), head + tail rows
//!   that fit inside `budget`, an explicit elision line naming the exact
//!   1-based `[stdout:A-B]` line range of the omitted middle rows, and any
//!   preserved footer lines.
//!
//! Fails closed (`None`) on prose, diffs, logs, code, wrapped multi-line cells,
//! irregular column counts, failed round-trip verification, or insufficient
//! byte savings.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt::Write as _;

/// Largest stdout payload inspected by this module (matches `jsonview::LIMIT`).
#[allow(dead_code)]
pub const LIMIT: usize = 8 * 1024 * 1024;
/// Reject inputs with single lines wider than this (matches `logview::MAX_LINE_BYTES`).
const MAX_LINE_BYTES: usize = 256 * 1024;
/// Minimum columns required to treat output as a table.
const MIN_COLS: usize = 2;
/// Minimum data rows for ASCII grid, `psql` aligned, and Unicode tables.
const MIN_ROWS: usize = 4;
/// Minimum data rows for standalone Markdown pipe tables.
const MIN_MARKDOWN_ROWS: usize = 6;
/// Maximum leading title or trailing footer metadata lines outside the table.
const MAX_META_LINES: usize = 2;
/// Minimum absolute byte savings when relative savings are below `MIN_SAVED_PCT`.
const MIN_SAVED_BYTES: usize = 120;
/// Minimum relative byte savings percentage when absolute savings are below `MIN_SAVED_BYTES`.
const MIN_SAVED_PCT: usize = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableKind {
    AsciiGrid,
    PsqlAligned,
    UnicodeBox,
    UnicodeAligned,
    MarkdownPipe,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceLine<'a> {
    /// 1-based line number in the raw `stdout` input.
    line_no: usize,
    /// Line text with any trailing `\r` stripped.
    text: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedRow {
    line_no: usize,
    cells: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedTable {
    kind: TableKind,
    titles: Vec<String>,
    headers: Vec<String>,
    col_types: Option<Vec<String>>,
    rows: Vec<ParsedRow>,
    footers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DecodedTable {
    titles: Vec<String>,
    headers: Vec<String>,
    col_types: Option<Vec<String>>,
    rows: Vec<Vec<String>>,
    footers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ColMode {
    Plain,
    Constant(String),
    Default(String),
}

struct CompiledTable {
    varying: Vec<usize>,
    has_const: bool,
    has_default: bool,
    has_types: bool,
    prefix_block: String,
    encoded_rows: Vec<String>,
    footer_block: String,
}

/// Compact a CLI table from already-redacted `stdout` to fit within `budget`.
///
/// Returns `Some(compacted)` only when `stdout` parses unambiguously as a
/// single table, Tier 1 verifies a 100% bijective round-trip, the chosen tier
/// fits within `budget`, and the result saves at least 15% or 120 bytes vs raw
/// `stdout`.
#[allow(dead_code)]
pub fn format(stdout: &str, budget: usize) -> Option<String> {
    if budget == 0 || stdout.is_empty() || stdout.len() > LIMIT {
        return None;
    }
    let table = parse_table(stdout)?;
    let compiled = compile_table(&table);
    let tier1 = render_tier1(&table, &compiled);

    // Every compacted schema and row encoding must pass bijective round-trip
    // verification before either Tier 1 or Tier 2 may be emitted.
    if !verify_round_trip(&table, &tier1) {
        return None;
    }

    if tier1.len() <= budget {
        return saves_enough(tier1.len(), stdout.len()).then_some(tier1);
    }

    let tier2 = render_tier2(&table, &compiled, budget)?;
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
// Input scanning & strict table parsers
// ---------------------------------------------------------------------------

fn parse_table(stdout: &str) -> Option<ParsedTable> {
    // Reject NUL, ANSI escape sequences, and unexpected control characters so
    // color-coded output or binary payloads fail closed.
    if stdout
        .bytes()
        .any(|b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r'))
    {
        return None;
    }

    let raw = stdout.strip_suffix('\n').unwrap_or(stdout);
    let mut lines: Vec<SourceLine<'_>> = Vec::new();
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

    // Trim outer blank lines while preserving original 1-based line numbers.
    let first = lines.iter().position(|l| !l.text.trim().is_empty())?;
    let last = lines.iter().rposition(|l| !l.text.trim().is_empty())?;
    let slice = &lines[first..=last];
    if slice.len() < MIN_ROWS + 2 {
        return None;
    }

    parse_unicode_box(slice)
        .or_else(|| parse_ascii_grid(slice))
        .or_else(|| parse_markdown_pipe(slice))
        .or_else(|| parse_psql_aligned(slice))
        .or_else(|| parse_unicode_aligned(slice))
}

fn leading_spaces(s: &str) -> usize {
    s.bytes().take_while(|&b| b == b' ').count()
}

fn is_blank(s: &str) -> bool {
    s.trim().is_empty()
}

/// Validate an optional 1-2 line title/preamble line before a table.
fn is_valid_title_line(line: &str, allow_md_heading: bool) -> bool {
    let t = line.trim();
    if t.is_empty() || t.len() > 160 {
        return false;
    }
    if !allow_md_heading && t.starts_with('#') {
        return false;
    }
    // Reject prose sentences, bullets, fences, diffs, code, and structured logs.
    if t.starts_with("```")
        || t.starts_with("~~~")
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("> ")
        || t.starts_with("diff ")
        || t.starts_with("--- ")
        || t.starts_with("+++ ")
        || t.starts_with("@@ ")
        || t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with("*/")
        || t.starts_with("<!--")
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
        "def ",
        "class ",
        "import ",
        "from ",
        "return ",
        "match ",
        "error:",
        "Error:",
        "ERROR",
        "WARN",
        "INFO",
        "DEBUG",
        "TRACE",
        "Traceback",
    ] {
        if t.starts_with(prefix) {
            return false;
        }
    }
    for bad in [
        "{", "}", ";", "=>", "->", "::", "||", "&&", "==", "!=", "|", "│",
    ] {
        if t.contains(bad) {
            return false;
        }
    }
    if t.len() > 80 && t.ends_with('.') {
        return false;
    }
    true
}

/// Validate an optional trailing footer line after a table (row counts, timing).
fn is_valid_footer_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.len() > 160 {
        return false;
    }
    // Reject code/diff/pipe markers.
    if t.contains('|') || t.contains('│') || t.contains('{') || t.contains('}') || t.contains("=>")
    {
        return false;
    }

    // psql: `(42 rows)` / `(1 row)` / `(0 rows)`
    if let Some(inner) = t.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        let mut parts = inner.split_whitespace();
        if let (Some(num), Some(unit), None) = (parts.next(), parts.next(), parts.next())
            && !num.is_empty()
            && num.bytes().all(|b| b.is_ascii_digit())
            && matches!(unit, "row" | "rows")
        {
            return true;
        }
    }

    let lower = t.to_ascii_lowercase();

    // mysql: `42 rows in set (0.01 sec)` / `1 row in set (0.00 sec)` / `Empty set (0.00 sec)`
    if lower.starts_with("empty set") {
        return true;
    }

    // duckdb / mysql / generic leading row count:
    // `50 rows`, `50 rows (40 shown)`, `50 rows (40 shown)   4 columns`,
    // `42 rows in set (0.01 sec)`
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    if tokens.len() >= 2
        && tokens[0].bytes().all(|b| b.is_ascii_digit())
        && matches!(tokens[1], "row" | "rows")
    {
        return true;
    }

    // Timing or summary footers:
    // `Time: 1.234 ms`, `Run Time: real 0.001 user 0.000 sys 0.000`,
    // `Total: 42`, `Showing 1-50 of 50`, `Rows: 42`, etc.
    for prefix in [
        "time:",
        "run time:",
        "elapsed:",
        "query time:",
        "execution time:",
        "total:",
        "total ",
        "showing ",
        "fetched ",
        "returned ",
        "rows:",
        "count:",
    ] {
        if lower.starts_with(prefix) {
            return true;
        }
    }

    false
}

/// Collect and validate optional trailing footer lines after a table's final line.
fn collect_footers(tail: &[SourceLine<'_>], existing: &[String]) -> Option<Vec<String>> {
    let mut footers = existing.to_vec();
    for line in tail {
        if is_blank(line.text) {
            continue;
        }
        if footers.len() >= MAX_META_LINES || !is_valid_footer_line(line.text) {
            return None;
        }
        footers.push(line.text.trim().to_owned());
    }
    Some(footers)
}

/// Detect DuckDB / Polars / SQL column type names in a 2nd boxed header row.
fn is_column_type(raw: &str) -> bool {
    let t = raw.trim();
    if t.is_empty() || t.len() > 80 {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    let mut s = lower.as_str();

    // Strip repeated trailing array suffixes: `[]`, `[3]`, etc.
    while let Some(prefix) = s.strip_suffix(']') {
        if let Some(open) = prefix.rfind('[') {
            let inside = prefix[open + 1..].trim();
            if inside.is_empty() || inside.bytes().all(|b| b.is_ascii_digit()) {
                s = prefix[..open].trim_end();
                continue;
            }
        }
        break;
    }

    // Check parameterized types: `decimal(10, 2)`, `struct(...)`, `datetime[μs]`, `list[str]`.
    let base = if let Some(open) = s.find('(') {
        if !s.ends_with(')') {
            return false;
        }
        s[..open].trim()
    } else if let Some(open) = s.find('[') {
        if !s.ends_with(']') {
            return false;
        }
        s[..open].trim()
    } else {
        s
    };

    matches!(
        base,
        "any"
            | "array"
            | "bigint"
            | "binary"
            | "bit"
            | "blob"
            | "bool"
            | "boolean"
            | "bpchar"
            | "bytea"
            | "cat"
            | "categorical"
            | "char"
            | "character"
            | "character varying"
            | "date"
            | "datetime"
            | "dec"
            | "decimal"
            | "double"
            | "double precision"
            | "duration"
            | "enum"
            | "f32"
            | "f64"
            | "float"
            | "float4"
            | "float8"
            | "hugeint"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "int"
            | "int1"
            | "int2"
            | "int4"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "int128"
            | "integer"
            | "interval"
            | "json"
            | "jsonb"
            | "list"
            | "map"
            | "null"
            | "numeric"
            | "object"
            | "real"
            | "smallint"
            | "sql_null"
            | "str"
            | "string"
            | "struct"
            | "text"
            | "time"
            | "time with time zone"
            | "time without time zone"
            | "timestamp"
            | "timestamp with time zone"
            | "timestamp without time zone"
            | "timestamp_ms"
            | "timestamp_ns"
            | "timestamp_s"
            | "timestamptz"
            | "timetz"
            | "tinyint"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "ubigint"
            | "uhugeint"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uint128"
            | "uinteger"
            | "union"
            | "usmallint"
            | "utinyint"
            | "uuid"
            | "varbinary"
            | "varchar"
    )
}

/// Approximate terminal display width for a scalar char (0 for combining marks,
/// 2 for East Asian Wide/Fullwidth & common emojis, 1 otherwise). Used only as
/// a fallback when a row contains both wide UTF-8 characters and a literal cell
/// delimiter (`|` / `│`).
fn char_display_width(c: char) -> usize {
    let cp = c as u32;
    if matches!(
        cp,
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200B..=0x200D
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
    ) {
        return 0;
    }
    if matches!(
        cp,
        0x1100..=0x115F
            | 0x2329..=0x232A
            | 0x2E80..=0x303E
            | 0x3040..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF01..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

fn slice_by_offset_mode(
    line: &str,
    delim: char,
    joints: &[usize],
    boxed: bool,
    use_display_width: bool,
) -> Option<(Vec<String>, Vec<String>)> {
    let mut joint_bytes = Vec::with_capacity(joints.len());
    let mut col = 0usize;
    let mut j_idx = 0usize;
    for (byte_idx, c) in line.char_indices() {
        let w = if use_display_width {
            char_display_width(c)
        } else {
            1
        };
        if j_idx < joints.len() && col == joints[j_idx] && w > 0 {
            if c != delim {
                return None;
            }
            joint_bytes.push(byte_idx);
            j_idx += 1;
        }
        col += w;
    }
    if j_idx != joints.len() {
        return None;
    }
    let delim_len = delim.len_utf8();
    if boxed {
        if col != *joints.last()? + 1 {
            return None;
        }
        let mut raw = Vec::with_capacity(joints.len() - 1);
        let mut trimmed = Vec::with_capacity(joints.len() - 1);
        for pair in joint_bytes.windows(2) {
            let piece = &line[pair[0] + delim_len..pair[1]];
            raw.push(piece.to_owned());
            trimmed.push(piece.trim_matches(' ').to_owned());
        }
        Some((raw, trimmed))
    } else {
        let mut raw = Vec::with_capacity(joints.len() + 1);
        let mut trimmed = Vec::with_capacity(joints.len() + 1);
        let mut start_byte = 0;
        for &end_byte in &joint_bytes {
            let piece = &line[start_byte..end_byte];
            raw.push(piece.to_owned());
            trimmed.push(piece.trim_matches(' ').to_owned());
            start_byte = end_byte + delim_len;
        }
        let last = &line[start_byte..];
        raw.push(last.to_owned());
        trimmed.push(last.trim_matches(' ').to_owned());
        Some((raw, trimmed))
    }
}

/// Slice a line into cells using exact border/separator column boundary offsets
/// when a cell contains a literal delimiter (e.g. `a | b`), verifying every
/// boundary char matches `delim`.
fn slice_by_char_joints(
    line: &str,
    delim: char,
    joints: &[usize],
    boxed: bool,
) -> Option<(Vec<String>, Vec<String>)> {
    slice_by_offset_mode(line, delim, joints, boxed, false)
        .or_else(|| slice_by_offset_mode(line, delim, joints, boxed, true))
}

/// Split a boxed row (`| a | b |` or `│ a │ b │`) into exact `ncols` trimmed cells.
fn split_boxed_row(line: &str, delim: char, joints: &[usize], ncols: usize) -> Option<Vec<String>> {
    let inner = line.strip_prefix(delim)?.strip_suffix(delim)?;
    let pieces: Vec<&str> = inner.split(delim).collect();
    if pieces.len() == ncols {
        return Some(
            pieces
                .into_iter()
                .map(|p| p.trim_matches(' ').to_owned())
                .collect(),
        );
    }
    if pieces.len() > ncols {
        return slice_by_char_joints(line, delim, joints, true).map(|(_, trimmed)| trimmed);
    }
    None
}

/// Split an unboxed aligned row (` a | b | c `) into raw & trimmed cells.
fn split_unboxed_row(
    line: &str,
    delim: char,
    joints: &[usize],
    ncols: usize,
) -> Option<(Vec<String>, Vec<String>)> {
    let pieces: Vec<&str> = line.split(delim).collect();
    if pieces.len() == ncols {
        let raw: Vec<String> = pieces.iter().map(|&p| p.to_owned()).collect();
        let trimmed: Vec<String> = pieces
            .into_iter()
            .map(|p| p.trim_matches(' ').to_owned())
            .collect();
        return Some((raw, trimmed));
    }
    if pieces.len() > ncols {
        return slice_by_char_joints(line, delim, joints, false);
    }
    None
}

// --- Parser 1: ASCII Grid (`mysql`, `aws --output table`, `tabulate`) ---

#[derive(Clone, Debug, PartialEq, Eq)]
struct AsciiBorder {
    joints: Vec<usize>,
    width: usize,
}

impl AsciiBorder {
    fn ncols(&self) -> usize {
        self.joints.len().saturating_sub(1)
    }
}

fn parse_ascii_border(s: &str) -> Option<AsciiBorder> {
    if s.len() < 3 || !s.starts_with('+') || !s.ends_with('+') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut joints = vec![0usize];
    let mut seg_fill: Option<u8> = None;
    let mut seg_len = 0usize;

    for (i, &b) in bytes.iter().enumerate().skip(1) {
        if b == b'+' {
            if seg_len == 0 {
                return None;
            }
            joints.push(i);
            seg_len = 0;
        } else if b == b'-' || b == b'=' {
            if let Some(fill) = seg_fill {
                if fill != b {
                    return None;
                }
            } else {
                seg_fill = Some(b);
            }
            seg_len += 1;
        } else {
            return None;
        }
    }

    if joints.len() < 2 {
        return None;
    }
    Some(AsciiBorder {
        joints,
        width: s.len(),
    })
}

fn is_aws_dash_banner(s: &str, width: usize) -> bool {
    s.len() == width && width >= 5 && s.bytes().all(|b| b == b'-')
}

fn parse_ascii_grid(lines: &[SourceLine<'_>]) -> Option<ParsedTable> {
    let mut titles = Vec::new();
    let mut idx = 0usize;

    // Locate the opening multi-column ASCII border (optionally preceded by up to
    // 2 title lines or an AWS single-cell title banner).
    while idx < lines.len() {
        let raw = lines[idx].text;
        if is_blank(raw) {
            idx += 1;
            continue;
        }
        let trimmed = raw.trim_end_matches(' ');
        let indent = leading_spaces(trimmed);
        let body = &trimmed[indent..];

        if let Some(border) = parse_ascii_border(body) {
            if border.ncols() >= MIN_COLS {
                break;
            }
            // Single-cell title box (`+------+` / `| Title |` / `+--+--+`)
            if border.ncols() == 1 && idx + 2 < lines.len() {
                let mid = lines[idx + 1].text.trim_end_matches(' ');
                let next = lines[idx + 2].text.trim_end_matches(' ');
                if leading_spaces(mid) == indent
                    && leading_spaces(next) == indent
                    && let Some(next_border) = parse_ascii_border(&next[indent..])
                    && next_border.ncols() >= MIN_COLS
                    && next_border.width == border.width
                    && let Some(title_inner) = mid[indent..]
                        .strip_prefix('|')
                        .and_then(|s| s.strip_suffix('|'))
                {
                    let title = title_inner.trim();
                    if !title.is_empty() && titles.len() < MAX_META_LINES {
                        titles.push(title.to_owned());
                        idx += 2;
                        break;
                    }
                }
            }
            return None;
        }

        // AWS CLI `--output table` top title banner uses `-----` (all dashes)
        // of the exact same width as the `+---+---+` line two lines below.
        if idx + 2 < lines.len() {
            let mid = lines[idx + 1].text.trim_end_matches(' ');
            let next = lines[idx + 2].text.trim_end_matches(' ');
            if leading_spaces(mid) == indent
                && leading_spaces(next) == indent
                && let Some(next_border) = parse_ascii_border(&next[indent..])
                && next_border.ncols() >= MIN_COLS
                && is_aws_dash_banner(body, next_border.width)
                && let Some(title_inner) = mid[indent..]
                    .strip_prefix('|')
                    .and_then(|s| s.strip_suffix('|'))
            {
                let title = title_inner.trim();
                if !title.is_empty() && titles.len() < MAX_META_LINES {
                    titles.push(title.to_owned());
                    idx += 2;
                    break;
                }
            }
        }

        if titles.len() >= MAX_META_LINES || !is_valid_title_line(raw, false) {
            return None;
        }
        titles.push(raw.trim().to_owned());
        idx += 1;
    }

    if idx + 4 >= lines.len() {
        return None;
    }
    let top_trimmed = lines[idx].text.trim_end_matches(' ');
    let indent = leading_spaces(top_trimmed);
    let top = parse_ascii_border(&top_trimmed[indent..])?;
    let ncols = top.ncols();
    if ncols < MIN_COLS {
        return None;
    }
    idx += 1;

    // Header row 1
    let h1_trimmed = lines.get(idx)?.text.trim_end_matches(' ');
    if leading_spaces(h1_trimmed) != indent {
        return None;
    }
    let headers = split_boxed_row(&h1_trimmed[indent..], '|', &top.joints, ncols)?;
    if headers.iter().all(|h| h.is_empty()) {
        return None;
    }
    idx += 1;

    // Optional DuckDB-style column type row before header divider
    let mut col_types = None;
    let next_trimmed = lines.get(idx)?.text.trim_end_matches(' ');
    if leading_spaces(next_trimmed) != indent {
        return None;
    }
    let next_body = &next_trimmed[indent..];
    if let Some(div) = parse_ascii_border(next_body) {
        if div.joints != top.joints {
            return None;
        }
        idx += 1;
    } else {
        let maybe_types = split_boxed_row(next_body, '|', &top.joints, ncols)?;
        if !maybe_types.iter().all(|c| is_column_type(c)) {
            return None;
        }
        col_types = Some(maybe_types);
        idx += 1;
        let div_trimmed = lines.get(idx)?.text.trim_end_matches(' ');
        if leading_spaces(div_trimmed) != indent {
            return None;
        }
        let div = parse_ascii_border(&div_trimmed[indent..])?;
        if div.joints != top.joints {
            return None;
        }
        idx += 1;
    }

    // Data rows + optional per-row dividers + closing bottom border
    let mut rows: Vec<ParsedRow> = Vec::new();
    let mut used_row_dividers = false;
    let mut rows_since_divider = 0usize;
    let mut closed = false;

    while idx < lines.len() {
        let raw = lines[idx].text;
        let trimmed = raw.trim_end_matches(' ');
        if leading_spaces(trimmed) != indent {
            return None;
        }
        let body = &trimmed[indent..];

        if let Some(border) = parse_ascii_border(body) {
            if border.joints != top.joints {
                return None;
            }
            if rows_since_divider == 0 {
                return None;
            }
            // Determine whether another `|...|` data row immediately follows.
            let has_next_data = lines.get(idx + 1).is_some_and(|next| {
                let nt = next.text.trim_end_matches(' ');
                leading_spaces(nt) == indent
                    && nt[indent..].starts_with('|')
                    && nt[indent..].ends_with('|')
            });
            if !has_next_data {
                if used_row_dividers && rows_since_divider != 1 {
                    return None;
                }
                closed = true;
                idx += 1;
                break;
            }
            // Internal row divider: every data block must be strictly 1 line
            // (rejects wrapped multi-line cells).
            if rows_since_divider != 1 || (!used_row_dividers && rows.len() != 1) {
                return None;
            }
            used_row_dividers = true;
            rows_since_divider = 0;
            idx += 1;
            continue;
        }

        if !body.starts_with('|') || !body.ends_with('|') {
            return None;
        }
        if used_row_dividers && rows_since_divider >= 1 {
            // Multiple lines between row dividers -> wrapped multi-line cell.
            return None;
        }
        let cells = split_boxed_row(body, '|', &top.joints, ncols)?;
        rows.push(ParsedRow {
            line_no: lines[idx].line_no,
            cells,
        });
        rows_since_divider += 1;
        idx += 1;
    }

    if !closed || rows.len() < MIN_ROWS {
        return None;
    }

    let footers = collect_footers(&lines[idx..], &[])?;
    Some(ParsedTable {
        kind: TableKind::AsciiGrid,
        titles,
        headers,
        col_types,
        rows,
        footers,
    })
}

// --- Parser 2: Unicode Box-Drawing (`duckdb`, `sqlite3 -box`, `rich`, `cli-table`) ---

#[derive(Clone, Debug, PartialEq, Eq)]
struct UnicodeBorder {
    joints: Vec<usize>, // char indices
    char_width: usize,
}

impl UnicodeBorder {
    fn ncols(&self) -> usize {
        self.joints.len().saturating_sub(1)
    }
}

fn is_h_bar(c: char) -> bool {
    matches!(c, '─' | '━' | '═')
}

fn is_v_bar(c: char) -> bool {
    matches!(c, '│' | '┃' | '║')
}

fn is_top_left(c: char) -> bool {
    matches!(c, '┌' | '╭' | '┏' | '╔' | '╒' | '╓')
}

fn is_top_right(c: char) -> bool {
    matches!(c, '┐' | '╮' | '┓' | '╗' | '╕' | '╖')
}

fn is_top_joint(c: char) -> bool {
    matches!(c, '┬' | '┳' | '╦' | '╤' | '╥')
}

fn is_mid_left(c: char) -> bool {
    matches!(c, '├' | '┣' | '╠' | '╞' | '╟' | '┡' | '┢' | '┠' | '┝')
}

fn is_mid_right(c: char) -> bool {
    matches!(c, '┤' | '┫' | '╣' | '╡' | '╢' | '┩' | '┪' | '┨' | '┥')
}

fn is_mid_joint(c: char) -> bool {
    matches!(
        c,
        '┼' | '╋' | '╬' | '╪' | '╫' | '╇' | '╈' | '╉' | '╊' | '╂' | '┿'
    )
}

fn is_bot_left(c: char) -> bool {
    matches!(c, '└' | '╰' | '┗' | '╚' | '╘' | '╙')
}

fn is_bot_right(c: char) -> bool {
    matches!(c, '┘' | '╯' | '┛' | '╝' | '╛' | '╜')
}

fn is_bot_joint(c: char) -> bool {
    matches!(c, '┴' | '┻' | '╩' | '╧' | '╨')
}

fn parse_unicode_border(
    s: &str,
    left_ok: fn(char) -> bool,
    joint_ok: fn(char) -> bool,
    right_ok: fn(char) -> bool,
) -> Option<UnicodeBorder> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 || !left_ok(chars[0]) || !right_ok(*chars.last()?) {
        return None;
    }
    let mut joints = vec![0usize];
    let mut seg_len = 0usize;
    for (i, &c) in chars.iter().enumerate().skip(1).take(chars.len() - 2) {
        if joint_ok(c) {
            if seg_len == 0 {
                return None;
            }
            joints.push(i);
            seg_len = 0;
        } else if is_h_bar(c) {
            seg_len += 1;
        } else {
            return None;
        }
    }
    if seg_len == 0 {
        return None;
    }
    joints.push(chars.len() - 1);
    Some(UnicodeBorder {
        joints,
        char_width: chars.len(),
    })
}

fn strip_unicode_vbar_outer(s: &str) -> Option<(char, &str)> {
    let mut chars = s.chars();
    let first = chars.next()?;
    let last = chars.next_back()?;
    if first == last && is_v_bar(first) {
        let inner = &s[first.len_utf8()..s.len() - last.len_utf8()];
        Some((first, inner))
    } else {
        None
    }
}

fn parse_unicode_box(lines: &[SourceLine<'_>]) -> Option<ParsedTable> {
    let mut titles = Vec::new();
    let mut idx = 0usize;

    while idx < lines.len() {
        let raw = lines[idx].text;
        if is_blank(raw) {
            idx += 1;
            continue;
        }
        let trimmed = raw.trim_end_matches(' ');
        let indent = leading_spaces(trimmed);
        let body = &trimmed[indent..];

        if let Some(border) = parse_unicode_border(body, is_top_left, is_top_joint, is_top_right) {
            if border.ncols() >= MIN_COLS {
                break;
            }
            // Optional single-cell title box (`┌─────┐` / `│ Title │` / `├──┬──┤`)
            if border.ncols() == 1 && idx + 2 < lines.len() {
                let mid = lines[idx + 1].text.trim_end_matches(' ');
                let next = lines[idx + 2].text.trim_end_matches(' ');
                if leading_spaces(mid) == indent
                    && leading_spaces(next) == indent
                    && let Some((_, title_inner)) = strip_unicode_vbar_outer(&mid[indent..])
                    && let Some(next_border) = parse_unicode_border(
                        &next[indent..],
                        is_mid_left,
                        is_top_joint,
                        is_mid_right,
                    )
                    .or_else(|| {
                        parse_unicode_border(
                            &next[indent..],
                            is_top_left,
                            is_top_joint,
                            is_top_right,
                        )
                    })
                    && next_border.ncols() >= MIN_COLS
                    && next_border.char_width == border.char_width
                {
                    let title = title_inner.trim();
                    if !title.is_empty() && titles.len() < MAX_META_LINES {
                        titles.push(title.to_owned());
                        idx += 2;
                        break;
                    }
                }
            }
            return None;
        }

        if titles.len() >= MAX_META_LINES || !is_valid_title_line(raw, false) {
            return None;
        }
        titles.push(raw.trim().to_owned());
        idx += 1;
    }

    if idx + 4 >= lines.len() {
        return None;
    }
    let top_trimmed = lines[idx].text.trim_end_matches(' ');
    let indent = leading_spaces(top_trimmed);
    let top_body = &top_trimmed[indent..];
    let top = parse_unicode_border(top_body, is_top_left, is_top_joint, is_top_right)
        .or_else(|| parse_unicode_border(top_body, is_mid_left, is_top_joint, is_mid_right))?;
    let ncols = top.ncols();
    if ncols < MIN_COLS {
        return None;
    }
    idx += 1;

    // Header row 1
    let h1_trimmed = lines.get(idx)?.text.trim_end_matches(' ');
    if leading_spaces(h1_trimmed) != indent {
        return None;
    }
    let (vbar, _) = strip_unicode_vbar_outer(&h1_trimmed[indent..])?;
    let headers = split_boxed_row(&h1_trimmed[indent..], vbar, &top.joints, ncols)?;
    if headers.iter().all(|h| h.is_empty()) {
        return None;
    }
    idx += 1;

    // Optional DuckDB column type row before `├─┼─┤` header divider
    let mut col_types = None;
    let next_trimmed = lines.get(idx)?.text.trim_end_matches(' ');
    if leading_spaces(next_trimmed) != indent {
        return None;
    }
    let next_body = &next_trimmed[indent..];
    if let Some(div) = parse_unicode_border(next_body, is_mid_left, is_mid_joint, is_mid_right) {
        if div.joints != top.joints {
            return None;
        }
        idx += 1;
    } else {
        let (row_bar, _) = strip_unicode_vbar_outer(next_body)?;
        if row_bar != vbar {
            return None;
        }
        let maybe_types = split_boxed_row(next_body, vbar, &top.joints, ncols)?;
        if !maybe_types.iter().all(|c| is_column_type(c)) {
            return None;
        }
        col_types = Some(maybe_types);
        idx += 1;

        let div_trimmed = lines.get(idx)?.text.trim_end_matches(' ');
        if leading_spaces(div_trimmed) != indent {
            return None;
        }
        let div = parse_unicode_border(
            &div_trimmed[indent..],
            is_mid_left,
            is_mid_joint,
            is_mid_right,
        )?;
        if div.joints != top.joints {
            return None;
        }
        idx += 1;
    }

    // Data rows + optional per-row dividers + bottom border (or DuckDB merged footer band)
    let mut rows: Vec<ParsedRow> = Vec::new();
    let mut box_footers: Vec<String> = Vec::new();
    let mut used_row_dividers = false;
    let mut rows_since_divider = 0usize;
    let mut closed = false;

    while idx < lines.len() {
        let raw = lines[idx].text;
        let trimmed = raw.trim_end_matches(' ');
        if leading_spaces(trimmed) != indent {
            return None;
        }
        let body = &trimmed[indent..];

        // Standard bottom border (`└─┴─┘`)
        if let Some(bot) = parse_unicode_border(body, is_bot_left, is_bot_joint, is_bot_right) {
            if bot.joints != top.joints || rows_since_divider == 0 {
                return None;
            }
            if used_row_dividers && rows_since_divider != 1 {
                return None;
            }
            closed = true;
            idx += 1;
            break;
        }

        // DuckDB merged bottom footer band (`├─┴─┴─┤` + 1..=2 single-cell footer lines + `└─────┘`)
        if let Some(foot_div) = parse_unicode_border(body, is_mid_left, is_bot_joint, is_mid_right)
            && foot_div.joints == top.joints
        {
            if rows_since_divider == 0 || (used_row_dividers && rows_since_divider != 1) {
                return None;
            }
            idx += 1;
            let mut inner_closed = false;
            while idx < lines.len() {
                let ft = lines[idx].text.trim_end_matches(' ');
                if leading_spaces(ft) != indent {
                    return None;
                }
                let fb = &ft[indent..];
                if let Some(bot) = parse_unicode_border(fb, is_bot_left, is_bot_joint, is_bot_right)
                    && bot.ncols() == 1
                    && bot.char_width == top.char_width
                {
                    inner_closed = true;
                    idx += 1;
                    break;
                }
                let (bar, inner) = strip_unicode_vbar_outer(fb)?;
                if bar != vbar || box_footers.len() >= MAX_META_LINES {
                    return None;
                }
                let text = inner.trim();
                if !is_valid_footer_line(text) {
                    return None;
                }
                box_footers.push(text.to_owned());
                idx += 1;
            }
            if !inner_closed || box_footers.is_empty() {
                return None;
            }
            closed = true;
            break;
        }

        // Internal row divider (`├─┼─┤`)
        if let Some(mid) = parse_unicode_border(body, is_mid_left, is_mid_joint, is_mid_right) {
            if mid.joints != top.joints {
                return None;
            }
            if rows_since_divider != 1 || (!used_row_dividers && rows.len() != 1) {
                return None;
            }
            used_row_dividers = true;
            rows_since_divider = 0;
            idx += 1;
            continue;
        }

        // Data row (`│ ... │`)
        let (row_bar, _) = strip_unicode_vbar_outer(body)?;
        if row_bar != vbar {
            return None;
        }
        if used_row_dividers && rows_since_divider >= 1 {
            return None;
        }
        let cells = split_boxed_row(body, vbar, &top.joints, ncols)?;
        rows.push(ParsedRow {
            line_no: lines[idx].line_no,
            cells,
        });
        rows_since_divider += 1;
        idx += 1;
    }

    if !closed || rows.len() < MIN_ROWS {
        return None;
    }

    let footers = collect_footers(&lines[idx..], &box_footers)?;
    Some(ParsedTable {
        kind: TableKind::UnicodeBox,
        titles,
        headers,
        col_types,
        rows,
        footers,
    })
}

// --- Parser 3: PostgreSQL Aligned & Unicode Aligned ---

fn parse_aligned_separator(
    s: &str,
    dash_ok: fn(char) -> bool,
    joint_ok: fn(char) -> bool,
) -> Option<Vec<usize>> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 {
        return None;
    }
    if joint_ok(chars[0]) || joint_ok(*chars.last()?) {
        return None;
    }
    let mut joints = Vec::new();
    let mut seg_dashes = 0usize;
    let mut total_dashes = 0usize;
    for (i, &c) in chars.iter().enumerate() {
        if joint_ok(c) {
            if seg_dashes == 0 {
                return None;
            }
            joints.push(i);
            seg_dashes = 0;
        } else if dash_ok(c) {
            seg_dashes += 1;
            total_dashes += 1;
        } else {
            return None;
        }
    }
    if seg_dashes == 0 {
        return None;
    }
    let ncols = joints.len() + 1;
    if ncols < MIN_COLS || total_dashes < ncols * 2 {
        return None;
    }
    Some(joints)
}

fn parse_aligned_table(
    lines: &[SourceLine<'_>],
    kind: TableKind,
    delim: char,
    dash_ok: fn(char) -> bool,
    joint_ok: fn(char) -> bool,
) -> Option<ParsedTable> {
    // Locate separator line within the first few lines (allowing 0..=2 title lines
    // and optional blank line before header).
    let max_sep_idx = (MAX_META_LINES + 3).min(lines.len().saturating_sub(MIN_ROWS));
    let mut found = None;
    for (sep_idx, line) in lines.iter().enumerate().take(max_sep_idx + 1).skip(1) {
        let trimmed = line.text.trim_end_matches(' ');
        let indent = leading_spaces(trimmed);
        if let Some(joints) = parse_aligned_separator(&trimmed[indent..], dash_ok, joint_ok) {
            found = Some((sep_idx, indent, joints));
            break;
        }
    }
    let (sep_idx, indent, joints) = found?;
    let ncols = joints.len() + 1;

    // Header must sit directly above the separator line. Column 1 may be
    // centered or right-aligned (leading spaces >= separator indent).
    let header_idx = sep_idx - 1;
    let header_raw = lines[header_idx].text.trim_end_matches(' ');
    if is_blank(header_raw) || leading_spaces(header_raw) < indent {
        return None;
    }
    let header_body = &header_raw[indent..];
    // Reject outer-delimited lines (handled by ASCII grid / Markdown / Unicode box).
    let ht = header_body.trim_matches(' ');
    if ht.starts_with(delim) && ht.ends_with(delim) {
        return None;
    }
    let (_, headers) = split_unboxed_row(header_body, delim, &joints, ncols)?;
    if headers.iter().all(|h| h.is_empty()) {
        return None;
    }

    // Any leading non-blank lines before header must be clean title lines
    // without column delimiters (rejects multi-line headers or stray tables).
    let mut titles = Vec::new();
    for line in &lines[..header_idx] {
        if is_blank(line.text) {
            continue;
        }
        if titles.len() >= MAX_META_LINES || !is_valid_title_line(line.text, false) {
            return None;
        }
        titles.push(line.text.trim().to_owned());
    }

    // Parse data rows until blank line or footer.
    let mut rows: Vec<ParsedRow> = Vec::new();
    let mut raw_rows: Vec<Vec<String>> = Vec::new();
    let mut idx = sep_idx + 1;

    while idx < lines.len() {
        let raw = lines[idx].text;
        if is_blank(raw) {
            break;
        }
        let trimmed = raw.trim_end_matches(' ');
        if leading_spaces(trimmed) < indent {
            // Might be an unindented footer like `(50 rows)` or `Time: ...`.
            if is_valid_footer_line(raw) {
                break;
            }
            return None;
        }
        let body = &trimmed[indent..];
        if !body.contains(delim) {
            if is_valid_footer_line(raw) {
                break;
            }
            return None;
        }
        let (raw_cells, cells) = split_unboxed_row(body, delim, &joints, ncols)?;
        raw_rows.push(raw_cells);
        rows.push(ParsedRow {
            line_no: lines[idx].line_no,
            cells,
        });
        idx += 1;
    }

    if rows.len() < MIN_ROWS {
        return None;
    }

    // Detect psql multi-line / wrapped cell continuation markers (`+` or `↵`
    // sitting flush against `|` or at line end before a blank-prefix row).
    if has_psql_continuation(&raw_rows, &rows) {
        return None;
    }

    let footers = collect_footers(&lines[idx..], &[])?;
    Some(ParsedTable {
        kind,
        titles,
        headers,
        col_types: None,
        rows,
        footers,
    })
}

fn has_psql_continuation(raw_rows: &[Vec<String>], rows: &[ParsedRow]) -> bool {
    let ncols = rows[0].cells.len();
    for col in 0..ncols.saturating_sub(1) {
        let any_space_pad = raw_rows.iter().any(|r| r[col].ends_with(' '));
        if !any_space_pad {
            continue;
        }
        for (i, r) in raw_rows.iter().enumerate() {
            if (r[col].ends_with('+') || r[col].ends_with('↵'))
                && i + 1 < rows.len()
                && rows[i + 1]
                    .cells
                    .iter()
                    .enumerate()
                    .any(|(c, v)| c != col && v.is_empty())
            {
                return true;
            }
        }
    }
    let last_col = ncols - 1;
    for (i, r) in raw_rows.iter().enumerate() {
        let last = &r[last_col];
        if (last.ends_with(" +") || last.ends_with('↵'))
            && i + 1 < rows.len()
            && rows[i + 1].cells[..last_col].iter().all(|v| v.is_empty())
        {
            return true;
        }
    }
    false
}

fn parse_psql_aligned(lines: &[SourceLine<'_>]) -> Option<ParsedTable> {
    parse_aligned_table(
        lines,
        TableKind::PsqlAligned,
        '|',
        |c| c == '-',
        |c| c == '+',
    )
}

fn parse_unicode_aligned(lines: &[SourceLine<'_>]) -> Option<ParsedTable> {
    parse_aligned_table(
        lines,
        TableKind::UnicodeAligned,
        '│',
        is_h_bar,
        is_mid_joint,
    )
}

// --- Parser 4: Standalone Markdown Pipe Table ---

fn parse_markdown_separator(trimmed: &str) -> Option<(usize, Vec<usize>)> {
    if trimmed.len() < 5 || !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return None;
    }
    let mut joints = Vec::new();
    for (ci, c) in trimmed.chars().enumerate() {
        if c == '|' {
            joints.push(ci);
        }
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let parts: Vec<&str> = inner.split('|').collect();
    if parts.len() < MIN_COLS {
        return None;
    }
    let mut total_dashes = 0usize;
    for seg in &parts {
        let t = seg.trim_matches(' ');
        if t.is_empty() {
            return None;
        }
        let core = t
            .strip_prefix(':')
            .unwrap_or(t)
            .strip_suffix(':')
            .unwrap_or_else(|| t.strip_prefix(':').unwrap_or(t));
        if core.is_empty() || !core.bytes().all(|b| b == b'-') {
            return None;
        }
        total_dashes += core.len();
    }
    if total_dashes < parts.len() * 2 {
        return None;
    }
    Some((parts.len(), joints))
}

/// Split a Markdown pipe row on unescaped `|`, falling back to column-aligned
/// `|` offsets when a cell contains an unescaped literal `|`.
fn split_markdown_row(trimmed: &str, sep_joints: &[usize], ncols: usize) -> Option<Vec<String>> {
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') || trimmed.len() < 3 {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut pieces: Vec<&str> = Vec::with_capacity(ncols);
    let mut start = 0usize;
    let bytes = inner.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'|' {
            // Count consecutive preceding backslashes to distinguish `\|` from `\\|`.
            let mut bs = 0usize;
            let mut j = i;
            while j > start && bytes[j - 1] == b'\\' {
                bs += 1;
                j -= 1;
            }
            if bs.is_multiple_of(2) {
                pieces.push(&inner[start..i]);
                start = i + 1;
            }
        }
        i += 1;
    }
    pieces.push(&inner[start..]);

    if pieces.len() == ncols {
        return Some(
            pieces
                .into_iter()
                .map(|p| p.trim_matches(' ').to_owned())
                .collect(),
        );
    }
    if pieces.len() > ncols {
        return slice_by_char_joints(trimmed, '|', sep_joints, true).map(|(_, trimmed)| trimmed);
    }
    None
}

fn parse_markdown_pipe(lines: &[SourceLine<'_>]) -> Option<ParsedTable> {
    let mut titles = Vec::new();
    let mut idx = 0usize;

    // Allow at most 1 short heading/title line before a standalone Markdown table.
    while idx < lines.len() {
        let raw = lines[idx].text;
        if is_blank(raw) {
            idx += 1;
            continue;
        }
        let trimmed = raw.trim_matches(' ');
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            break;
        }
        if !titles.is_empty() || !is_valid_title_line(raw, true) {
            return None;
        }
        titles.push(raw.trim().to_owned());
        idx += 1;
    }

    if idx + 1 + MIN_MARKDOWN_ROWS > lines.len() {
        return None;
    }

    let sep_trimmed = lines[idx + 1].text.trim_matches(' ');
    let (ncols, sep_joints) = parse_markdown_separator(sep_trimmed)?;
    let header_trimmed = lines[idx].text.trim_matches(' ');
    let headers = split_markdown_row(header_trimmed, &sep_joints, ncols)?;
    if headers.iter().all(|h| h.is_empty()) {
        return None;
    }

    let mut rows: Vec<ParsedRow> = Vec::new();
    let mut cur = idx + 2;
    while cur < lines.len() {
        let raw = lines[cur].text;
        if is_blank(raw) {
            break;
        }
        let trimmed = raw.trim_matches(' ');
        if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
            if is_valid_footer_line(raw) {
                break;
            }
            return None;
        }
        // Reject a second separator row inside the body (concatenated tables).
        if parse_markdown_separator(trimmed).is_some() {
            return None;
        }
        let cells = split_markdown_row(trimmed, &sep_joints, ncols)?;
        rows.push(ParsedRow {
            line_no: lines[cur].line_no,
            cells,
        });
        cur += 1;
    }

    if rows.len() < MIN_MARKDOWN_ROWS {
        return None;
    }

    let footers = collect_footers(&lines[cur..], &[])?;
    Some(ParsedTable {
        kind: TableKind::MarkdownPipe,
        titles,
        headers,
        col_types: None,
        rows,
        footers,
    })
}

// ---------------------------------------------------------------------------
// Escaping, hoisting, Tier 1 & Tier 2 rendering
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

fn unescape_cell(s: &str) -> Option<String> {
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
    Some(out)
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

fn encode_col_spec(name: &str, mode: &ColMode) -> String {
    let ename = escape_col_name(name);
    match mode {
        ColMode::Plain => ename.into_owned(),
        ColMode::Constant(val) => format!("{ename}={}", escape_cell(val)),
        ColMode::Default(val) => format!("{ename}?={}", escape_cell(val)),
    }
}

fn decode_col_spec(spec: &str) -> Option<(String, ColMode)> {
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
                return Some((name, ColMode::Constant(val)));
            }
            '?' => {
                let rest = spec[idx + 1..].strip_prefix('=')?;
                let val = unescape_cell(rest)?;
                return Some((name, ColMode::Default(val)));
            }
            '\t' | '\n' | '\r' => return None,
            other => name.push(other),
        }
    }
    Some((name, ColMode::Plain))
}

fn encode_varying_cell<'a>(cell: &'a str, mode: &ColMode) -> Cow<'a, str> {
    match mode {
        ColMode::Plain => escape_cell(cell),
        ColMode::Default(default_val) => {
            if cell == default_val {
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

fn decode_varying_cell(encoded: &str, mode: &ColMode) -> Option<String> {
    match mode {
        ColMode::Plain => unescape_cell(encoded),
        ColMode::Default(default_val) => {
            if encoded == "." {
                Some(default_val.clone())
            } else if encoded == "\\." {
                Some(String::from("."))
            } else {
                unescape_cell(encoded)
            }
        }
        ColMode::Constant(_) => unreachable!("constant columns are omitted from rows"),
    }
}

fn plan_column_mode(rows: &[ParsedRow], col: usize) -> ColMode {
    let n = rows.len();
    let first = &rows[0].cells[col];
    if rows.iter().all(|r| &r.cells[col] == first) {
        return ColMode::Constant(first.clone());
    }

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for r in rows {
        *counts.entry(r.cells[col].as_str()).or_insert(0) += 1;
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

    let dot_outliers = rows.iter().filter(|r| r.cells[col] == ".").count();
    let header_cost = 2 + escaped_len; // `?=VAL`
    let row_savings = best_count * (escaped_len - 1);
    if row_savings > header_cost + dot_outliers {
        ColMode::Default(best_val.to_owned())
    } else {
        ColMode::Plain
    }
}

fn legend_suffix(has_tsv: bool, has_types: bool, has_const: bool, has_default: bool) -> String {
    let mut s = String::new();
    if has_tsv {
        s.push_str(", TSV");
    }
    if has_types {
        s.push_str(", typed");
    }
    match (has_const, has_default) {
        (true, true) => s.push_str("; col=V const, col?=V default (.=V)"),
        (true, false) => s.push_str("; col=V const"),
        (false, true) => s.push_str("; col?=V default (.=V)"),
        (false, false) => {}
    }
    s
}

fn compile_table(table: &ParsedTable) -> CompiledTable {
    let ncols = table.headers.len();
    let modes: Vec<ColMode> = (0..ncols)
        .map(|c| plan_column_mode(&table.rows, c))
        .collect();
    let varying: Vec<usize> = (0..ncols)
        .filter(|&c| !matches!(modes[c], ColMode::Constant(_)))
        .collect();
    let has_const = modes.iter().any(|m| matches!(m, ColMode::Constant(_)));
    let has_default = modes.iter().any(|m| matches!(m, ColMode::Default(_)));
    let has_types = table.col_types.is_some();

    let mut prefix_block = String::new();
    for title in &table.titles {
        prefix_block.push_str("title: ");
        prefix_block.push_str(&escape_cell(title));
        prefix_block.push('\n');
    }
    prefix_block.push_str("cols: ");
    for (c, name) in table.headers.iter().enumerate() {
        if c > 0 {
            prefix_block.push('\t');
        }
        prefix_block.push_str(&encode_col_spec(name, &modes[c]));
    }
    prefix_block.push('\n');
    if let Some(types) = &table.col_types {
        prefix_block.push_str("types: ");
        for (c, ty) in types.iter().enumerate() {
            if c > 0 {
                prefix_block.push('\t');
            }
            prefix_block.push_str(&escape_cell(ty));
        }
        prefix_block.push('\n');
    }

    let encoded_rows: Vec<String> = if varying.is_empty() {
        Vec::new()
    } else {
        table
            .rows
            .iter()
            .map(|row| {
                let mut line = String::new();
                for (i, &c) in varying.iter().enumerate() {
                    if i > 0 {
                        line.push('\t');
                    }
                    line.push_str(&encode_varying_cell(&row.cells[c], &modes[c]));
                }
                line
            })
            .collect()
    };

    let mut footer_block = String::new();
    for footer in &table.footers {
        footer_block.push_str(footer);
        footer_block.push('\n');
    }

    CompiledTable {
        varying,
        has_const,
        has_default,
        has_types,
        prefix_block,
        encoded_rows,
        footer_block,
    }
}

fn render_tier1(table: &ParsedTable, compiled: &CompiledTable) -> String {
    let has_tsv = !compiled.varying.is_empty();
    let suffix = legend_suffix(
        has_tsv,
        compiled.has_types,
        compiled.has_const,
        compiled.has_default,
    );
    let mut out = format!("[table: {} rows{suffix}]\n", table.rows.len());
    out.push_str(&compiled.prefix_block);
    for row in &compiled.encoded_rows {
        out.push_str(row);
        out.push('\n');
    }
    out.push_str(&compiled.footer_block);
    out
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

fn render_tier2(table: &ParsedTable, compiled: &CompiledTable, budget: usize) -> Option<String> {
    let n = table.rows.len();
    if compiled.varying.is_empty() || n < 2 {
        return None;
    }

    let suffix = legend_suffix(
        true,
        compiled.has_types,
        compiled.has_const,
        compiled.has_default,
    );
    let fixed_bytes = compiled.prefix_block.len() + compiled.footer_block.len();

    let total_for = |head_k: usize, tail_m: usize, head_bytes: usize, tail_bytes: usize| -> usize {
        let shown = head_k + tail_m;
        let omitted = n - shown;
        let first_line = table.rows[head_k].line_no;
        let last_line = table.rows[n - tail_m - 1].line_no;
        let legend_len =
            "[table preview:  of  rows]\n".len() + digits(shown) + digits(n) + suffix.len();
        let elision_len = elision_line(omitted, first_line, last_line).len();
        legend_len + fixed_bytes + head_bytes + elision_len + tail_bytes
    };

    // Start with (1 head, 1 tail) if it fits, else (1 head, 0 tail).
    let row0_bytes = compiled.encoded_rows[0].len() + 1;
    let last_row_bytes = compiled.encoded_rows[n - 1].len() + 1;

    let (mut head_k, mut tail_m, mut head_bytes, mut tail_bytes) =
        if n >= 3 && total_for(1, 1, row0_bytes, last_row_bytes) <= budget {
            (1, 1, row0_bytes, last_row_bytes)
        } else if total_for(1, 0, row0_bytes, 0) <= budget {
            (1, 0, row0_bytes, 0)
        } else {
            return None;
        };

    // Greedily grow head and tail in ~2:1 ratio while keeping >= 1 row omitted.
    while head_k + tail_m + 1 < n {
        let prefer_head = head_k <= tail_m * 2;
        let next_head_bytes = compiled.encoded_rows[head_k].len() + 1;
        let next_tail_bytes = compiled.encoded_rows[n - tail_m - 1].len() + 1;

        let try_head = |hk: usize, tm: usize, hb: usize, tb: usize| {
            total_for(hk + 1, tm, hb + next_head_bytes, tb) <= budget
        };
        let try_tail = |hk: usize, tm: usize, hb: usize, tb: usize| {
            total_for(hk, tm + 1, hb, tb + next_tail_bytes) <= budget
        };

        if prefer_head {
            if try_head(head_k, tail_m, head_bytes, tail_bytes) {
                head_k += 1;
                head_bytes += next_head_bytes;
            } else if try_tail(head_k, tail_m, head_bytes, tail_bytes) {
                tail_m += 1;
                tail_bytes += next_tail_bytes;
            } else {
                break;
            }
        } else if try_tail(head_k, tail_m, head_bytes, tail_bytes) {
            tail_m += 1;
            tail_bytes += next_tail_bytes;
        } else if try_head(head_k, tail_m, head_bytes, tail_bytes) {
            head_k += 1;
            head_bytes += next_head_bytes;
        } else {
            break;
        }
    }

    let shown = head_k + tail_m;
    let omitted = n - shown;
    let first_line = table.rows[head_k].line_no;
    let last_line = table.rows[n - tail_m - 1].line_no;

    let mut out = String::with_capacity(budget.min(fixed_bytes + head_bytes + tail_bytes + 128));
    let _ = writeln!(out, "[table preview: {shown} of {n} rows{suffix}]");
    out.push_str(&compiled.prefix_block);
    for row in &compiled.encoded_rows[..head_k] {
        out.push_str(row);
        out.push('\n');
    }
    out.push_str(&elision_line(omitted, first_line, last_line));
    for row in &compiled.encoded_rows[n - tail_m..] {
        out.push_str(row);
        out.push('\n');
    }
    out.push_str(&compiled.footer_block);

    (out.len() <= budget).then_some(out)
}

fn digits(mut n: usize) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

// ---------------------------------------------------------------------------
// Bijective Tier 1 decoder & round-trip verification
// ---------------------------------------------------------------------------

fn decode_table(view: &str) -> Option<DecodedTable> {
    let mut lines = view.strip_suffix('\n')?.split('\n').peekable();

    // 1. Legend line
    let legend = lines.next()?;
    let inner = legend.strip_prefix("[table: ")?.strip_suffix(']')?;
    let (count_str, mut rest) = inner.split_once(" rows")?;
    if count_str.is_empty() || !count_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let row_count: usize = count_str.parse().ok()?;
    if row_count < MIN_ROWS {
        return None;
    }
    let has_tsv = if let Some(after) = rest.strip_prefix(", TSV") {
        rest = after;
        true
    } else {
        false
    };
    let has_types = if let Some(after) = rest.strip_prefix(", typed") {
        rest = after;
        true
    } else {
        false
    };
    if !matches!(
        rest,
        "" | "; col=V const" | "; col?=V default (.=V)" | "; col=V const, col?=V default (.=V)"
    ) {
        return None;
    }

    // 2. Optional `title: ` lines
    let mut titles = Vec::new();
    while let Some(next) = lines.peek() {
        let Some(title_raw) = next.strip_prefix("title: ") else {
            break;
        };
        lines.next();
        if titles.len() >= MAX_META_LINES {
            return None;
        }
        titles.push(unescape_cell(title_raw)?);
    }

    // 3. Schema line (`cols: ...`)
    let cols_line = lines.next()?.strip_prefix("cols: ")?;
    let mut headers = Vec::new();
    let mut modes = Vec::new();
    for spec in cols_line.split('\t') {
        let (name, mode) = decode_col_spec(spec)?;
        headers.push(name);
        modes.push(mode);
    }
    let ncols = headers.len();
    if ncols < MIN_COLS {
        return None;
    }

    let varying: Vec<usize> = (0..ncols)
        .filter(|&c| !matches!(modes[c], ColMode::Constant(_)))
        .collect();
    if has_tsv != !varying.is_empty() {
        return None;
    }

    // 4. Optional `types: ` line (present iff legend declared `typed`)
    let col_types = if has_types {
        let types_line = lines.next()?.strip_prefix("types: ")?;
        let parsed: Option<Vec<String>> = types_line.split('\t').map(unescape_cell).collect();
        let parsed = parsed?;
        if parsed.len() != ncols {
            return None;
        }
        Some(parsed)
    } else {
        None
    };

    // 5. Exact `row_count` data rows
    let mut rows = Vec::with_capacity(row_count);
    if varying.is_empty() {
        let const_row: Vec<String> = modes
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
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() != varying.len() {
                return None;
            }
            let mut full_row = Vec::with_capacity(ncols);
            let mut v_cursor = 0usize;
            for mode in &modes {
                match mode {
                    ColMode::Constant(val) => full_row.push(val.clone()),
                    ColMode::Plain | ColMode::Default(_) => {
                        full_row.push(decode_varying_cell(parts[v_cursor], mode)?);
                        v_cursor += 1;
                    }
                }
            }
            rows.push(full_row);
        }
    }

    // 6. Remaining lines are exact footers
    let mut footers = Vec::new();
    for line in lines {
        if footers.len() >= MAX_META_LINES {
            return None;
        }
        footers.push(line.to_owned());
    }

    Some(DecodedTable {
        titles,
        headers,
        col_types,
        rows,
        footers,
    })
}

fn verify_round_trip(table: &ParsedTable, rendered: &str) -> bool {
    let Some(decoded) = decode_table(rendered) else {
        return false;
    };
    if decoded.titles != table.titles
        || decoded.headers != table.headers
        || decoded.col_types != table.col_types
        || decoded.footers != table.footers
        || decoded.rows.len() != table.rows.len()
    {
        return false;
    }
    decoded
        .rows
        .iter()
        .zip(&table.rows)
        .all(|(got, orig)| got == &orig.cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_psql_table(row_count: usize) -> String {
        let mut out = String::new();
        out.push_str("  id  |  region   |    env     |     host     |  status   | latency_ms \n");
        out.push_str("------+-----------+------------+--------------+-----------+------------\n");
        for i in 1..=row_count {
            let status = if i % 10 == 0 {
                "degraded"
            } else if i % 15 == 0 {
                "stopped"
            } else {
                "running"
            };
            let _ = writeln!(
                out,
                " {:>4} | {:<9} | {:<10} | {:<12} | {:<9} | {:>10} ",
                i,
                "us-east-1",
                "production",
                format!("web-srv-{i:04}"),
                status,
                12 + (i * 7) % 180
            );
        }
        let _ = writeln!(out, "({row_count} rows)");
        out.push_str("Time: 1.842 ms\n");
        out
    }

    fn build_mysql_table(row_count: usize) -> String {
        let border = "+------+-----------+--------------+-----------+----------+\n";
        let mut out = String::from(border);
        out.push_str("| id   | region    | host         | status    | cpu_pct  |\n");
        out.push_str(border);
        for i in 1..=row_count {
            let status = if i % 8 == 0 { "stopped" } else { "running" };
            let _ = writeln!(
                out,
                "| {:<4} | {:<9} | {:<12} | {:<9} | {:>8} |",
                i,
                "us-east-1",
                format!("db-node-{i:03}"),
                status,
                format!("{:.1}", 10.0 + (i as f64 * 1.7) % 75.0)
            );
        }
        out.push_str(border);
        let _ = writeln!(out, "{row_count} rows in set (0.01 sec)");
        out
    }

    fn build_duckdb_table(row_count: usize) -> String {
        let mut out = String::new();
        out.push_str("┌───────┬───────────┬────────────┬──────────┐\n");
        out.push_str("│  id   │  region   │    host    │  status  │\n");
        out.push_str("│ int32 │  varchar  │  varchar   │ varchar  │\n");
        out.push_str("├───────┼───────────┼────────────┼──────────┤\n");
        for i in 1..=row_count {
            let status = if i % 9 == 0 { "pending" } else { "active" };
            let _ = writeln!(
                out,
                "│ {:>5} │ {:<9} │ {:<10} │ {:<8} │",
                i,
                "eu-west-1",
                format!("worker-{i:03}"),
                status
            );
        }
        out.push_str("├───────┴───────────┴────────────┴──────────┤\n");
        let footer = format!("{row_count} rows ({row_count} shown)");
        let _ = writeln!(out, "│ {:<41} │", footer);
        out.push_str("└───────────────────────────────────────────┘\n");
        out
    }

    fn build_markdown_table(row_count: usize) -> String {
        let mut out = String::new();
        out.push_str("| id | region | service | status | p99_ms |\n");
        out.push_str("| ---: | --- | --- | :---: | ---: |\n");
        for i in 1..=row_count {
            let status = if i % 7 == 0 { "warn" } else { "healthy" };
            let _ = writeln!(
                out,
                "| {i:>4} | us-west-2 | {:<12} | {:<7} | {:>6} |",
                format!("svc-api-{i:02}"),
                status,
                20 + i * 3
            );
        }
        out
    }

    fn assert_bijective(raw: &str, out: &str) {
        let parsed = parse_table(raw).expect("input should parse");
        let decoded = decode_table(out).expect("Tier 1 output should decode");
        assert_eq!(decoded.titles, parsed.titles);
        assert_eq!(decoded.headers, parsed.headers);
        assert_eq!(decoded.col_types, parsed.col_types);
        assert_eq!(decoded.footers, parsed.footers);
        assert_eq!(decoded.rows.len(), parsed.rows.len());
        for (i, (got, orig)) in decoded.rows.iter().zip(&parsed.rows).enumerate() {
            assert_eq!(got, &orig.cells, "row {i} mismatch");
        }
    }

    #[test]
    fn psql_50_rows_tier1_lossless_and_round_trip() {
        let raw = build_psql_table(50);
        let out = format(&raw, 16 * 1024).expect("50-row psql table should compact");
        assert!(out.starts_with("[table: 50 rows, TSV; col=V const, col?=V default (.=V)]\n"));
        assert!(out.contains("region=us-east-1"));
        assert!(out.contains("env=production"));
        assert!(out.contains("status?=running"));
        assert!(out.ends_with("(50 rows)\nTime: 1.842 ms\n"));
        assert_bijective(&raw, &out);

        let pct = (raw.len() - out.len()) * 100 / raw.len();
        assert!(
            pct >= 65,
            "expected >=65% byte reduction on 50-row psql, got {pct}% ({} -> {})",
            raw.len(),
            out.len()
        );
    }

    #[test]
    fn psql_500_rows_tier1_and_tier2_exact_line_range() {
        let raw = build_psql_table(500);

        // Tier 1: large budget -> lossless full table
        let tier1 = format(&raw, 64 * 1024).expect("500-row psql Tier 1 should compact");
        assert!(tier1.starts_with("[table: 500 rows, TSV;"));
        assert_bijective(&raw, &tier1);
        let t1_pct = (raw.len() - tier1.len()) * 100 / raw.len();
        assert!(
            t1_pct >= 65,
            "expected >=65% byte reduction in Tier 1, got {t1_pct}% ({} -> {})",
            raw.len(),
            tier1.len()
        );

        // Tier 2: bounded budget -> head + elision with exact [stdout:A-B] + tail + footers
        let budget = 1_800;
        let tier2 = format(&raw, budget).expect("500-row psql Tier 2 should compact");
        assert!(
            tier2.len() <= budget,
            "Tier 2 exceeded budget: {} > {budget}",
            tier2.len()
        );
        assert!(tier2.starts_with("[table preview: "));
        assert!(tier2.contains(" of 500 rows, TSV;"));
        assert!(tier2.contains("region=us-east-1"));
        assert!(tier2.contains("status?=running"));
        assert!(tier2.ends_with("(500 rows)\nTime: 1.842 ms\n"));

        // Verify exact 1-based line numbers in [stdout:A-B] against raw stdout lines.
        let elision = tier2
            .lines()
            .find(|l| l.starts_with("... ") && l.contains("[stdout:"))
            .expect("Tier 2 must include [stdout:A-B] elision line");
        let after = elision.split("[stdout:").nth(1).unwrap();
        let range = after.split(']').next().unwrap();
        let (a, b): (usize, usize) = range
            .split_once('-')
            .map(|(x, y)| (x.parse().unwrap(), y.parse().unwrap()))
            .unwrap();

        let raw_lines: Vec<&str> = raw.lines().collect();
        // Line 1 is header, line 2 is separator, line 3 is row 1 (`id=1`), so line L is row `L-2`.
        let first_omitted_id = a - 2;
        let last_omitted_id = b - 2;
        assert!(
            raw_lines[a - 1]
                .trim_start()
                .starts_with(&format!("{first_omitted_id} |")),
            "line {a} should be row {first_omitted_id}, got: {}",
            raw_lines[a - 1]
        );
        assert!(
            raw_lines[b - 1]
                .trim_start()
                .starts_with(&format!("{last_omitted_id} |")),
            "line {b} should be row {last_omitted_id}, got: {}",
            raw_lines[b - 1]
        );
        // The row immediately before A must be in head, and the row immediately after B must be in tail.
        let prev_id = first_omitted_id - 1;
        let next_id = last_omitted_id + 1;
        assert!(tier2.contains(&format!("\n{prev_id}\tweb-srv-{prev_id:04}\t")));
        assert!(tier2.contains(&format!("\n{next_id}\tweb-srv-{next_id:04}\t")));
        assert!(!tier2.contains(&format!(
            "\n{first_omitted_id}\tweb-srv-{first_omitted_id:04}\t"
        )));
        assert!(!tier2.contains(&format!(
            "\n{last_omitted_id}\tweb-srv-{last_omitted_id:04}\t"
        )));

        let omitted_count: usize = elision
            .strip_prefix("... ")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(omitted_count, b - a + 1);
    }

    #[test]
    fn mysql_and_aws_ascii_grid_tables() {
        let mysql = build_mysql_table(30);
        let out = format(&mysql, 16 * 1024).expect("mysql ASCII grid should compact");
        assert!(out.contains("region=us-east-1"));
        assert!(out.contains("status?=running"));
        assert!(out.ends_with("30 rows in set (0.01 sec)\n"));
        assert_bijective(&mysql, &out);
        let mysql_pct = (mysql.len() - out.len()) * 100 / mysql.len();
        assert!(
            mysql_pct >= 60,
            "expected >=60% reduction on mysql, got {mysql_pct}% ({} -> {})",
            mysql.len(),
            out.len()
        );

        // AWS `--output table` with top title banner and per-row borders.
        let aws = "\
----------------------------------------------------------
|                   DescribeInstances                    |
+------------+-----------+---------------+---------------+
| InstanceId |  Region   |    Status     |     Type      |
+------------+-----------+---------------+---------------+
| i-0001a    | us-east-1 | running       | t3.micro      |
+------------+-----------+---------------+---------------+
| i-0002b    | us-east-1 | running       | t3.small      |
+------------+-----------+---------------+---------------+
| i-0003c    | us-east-1 | stopped       | t3.micro      |
+------------+-----------+---------------+---------------+
| i-0004d    | us-east-1 | running       | t3.medium     |
+------------+-----------+---------------+---------------+
| i-0005e    | us-east-1 | running       | t3.micro      |
+------------+-----------+---------------+---------------+
| i-0006f    | us-east-1 | running       | t3.large      |
+------------+-----------+---------------+---------------+
";
        let aws_out = format(aws, 8192).expect("AWS table should compact");
        assert!(aws_out.contains("title: DescribeInstances\n"));
        assert!(aws_out.contains("Region=us-east-1"));
        assert!(aws_out.contains("Status?=running"));
        assert_bijective(aws, &aws_out);
        assert!(aws_out.len() * 100 / aws.len() <= 35);

        // Verify Tier 2 line numbers on a row-divided ASCII grid account for
        // interleaved `+---+---+` border lines accurately!
        let aws_t2 = format(aws, 240).expect("AWS Tier 2 should fit 240 B");
        assert!(aws_t2.len() <= 240);
        // Data row 1 is line 6, row 2 is line 8, row 3 is line 10, row 4 is line 12,
        // row 5 is line 14, row 6 is line 16.
        let elision = aws_t2
            .lines()
            .find(|l| l.contains("[stdout:"))
            .expect("must have elision line");
        let after = elision.split("[stdout:").nth(1).unwrap();
        let range = after.split(']').next().unwrap();
        let (a, b): (usize, usize) = if let Some((x, y)) = range.split_once('-') {
            (x.parse().unwrap(), y.parse().unwrap())
        } else {
            let v: usize = range.parse().unwrap();
            (v, v)
        };
        let aws_lines: Vec<&str> = aws.lines().collect();
        assert!(aws_lines[a - 1].starts_with("| i-000"));
        assert!(aws_lines[b - 1].starts_with("| i-000"));
    }

    #[test]
    fn duckdb_sqlite_rich_and_unicode_aligned_tables() {
        // DuckDB box with column-type row + merged bottom footer band
        let duck = build_duckdb_table(40);
        let out = format(&duck, 16 * 1024).expect("DuckDB table should compact");
        assert!(out.starts_with("[table: 40 rows, TSV, typed;"));
        assert!(out.contains("types: int32\tvarchar\tvarchar\tvarchar\n"));
        assert!(out.contains("region=eu-west-1"));
        assert!(out.contains("status?=active"));
        assert!(out.ends_with("40 rows (40 shown)\n"));
        assert_bijective(&duck, &out);

        // DuckDB Tier 2 preserves `types:` and merged footer
        let duck_t2 = format(&duck, 450).expect("DuckDB Tier 2 should compact");
        assert!(duck_t2.len() <= 450);
        assert!(duck_t2.contains("types: int32\tvarchar\tvarchar\tvarchar\n"));
        assert!(duck_t2.contains("[stdout:"));
        assert!(duck_t2.ends_with("40 rows (40 shown)\n"));

        // Rich rounded Unicode box (`╭─┬─╮` / `╰─┴─╯`) without type row
        let rich = "\
╭────┬───────────┬────────────┬─────────╮
│ id │ region    │ service    │ state   │
├────┼───────────┼────────────┼─────────┤
│  1 │ ap-east-1 │ auth-api   │ healthy │
│  2 │ ap-east-1 │ billing    │ healthy │
│  3 │ ap-east-1 │ checkout   │ healthy │
│  4 │ ap-east-1 │ search-api │ healthy │
│  5 │ ap-east-1 │ mailer     │ failing │
│  6 │ ap-east-1 │ gateway    │ healthy │
╰────┴───────────┴────────────┴─────────╯
";
        let rich_out = format(rich, 8192).expect("Rich rounded table should compact");
        assert!(!rich_out.contains("types: "));
        assert_bijective(rich, &rich_out);

        // psql Unicode linestyle (`border = 1`, no outer borders, centered header)
        let ualigned = "  id │  region   │    host    │ status  \n\
─────┼───────────┼────────────┼─────────\n\
   1 │ us-east-1 │ api-001    │ running\n\
   2 │ us-east-1 │ api-002    │ running\n\
   3 │ us-east-1 │ api-003    │ running\n\
   4 │ us-east-1 │ api-004    │ running\n\
   5 │ us-east-1 │ api-005    │ stopped\n\
(5 rows)\n";
        let ua_out = format(ualigned, 8192).expect("Unicode aligned table should compact");
        assert_eq!(
            parse_table(ualigned).unwrap().kind,
            TableKind::UnicodeAligned
        );
        assert_bijective(ualigned, &ua_out);
    }

    #[test]
    fn markdown_pipe_table_24_rows_and_minimum_row_threshold() {
        let md = build_markdown_table(24);
        let out = format(&md, 16 * 1024).expect("24-row markdown table should compact");
        assert!(out.contains("region=us-west-2"));
        assert!(out.contains("status?=healthy"));
        assert_bijective(&md, &out);
        let pct = (md.len() - out.len()) * 100 / md.len();
        assert!(
            pct >= 50,
            "expected >=50% reduction on markdown, got {pct}%"
        );

        // Fewer than 6 rows must fail closed for Markdown pipe tables.
        let small_md = build_markdown_table(5);
        assert!(format(&small_md, 16 * 1024).is_none());
    }

    #[test]
    fn adversarial_cells_escaping_unicode_nulls_and_grammar_safety() {
        // 1. ASCII grid containing literal `|`, tabs, `.` under majority default,
        // `\.`, backslashes, empty cells, NULL, Unicode UTF-8, and tricky column names!
        let mut raw = String::new();
        let border = "+------+----------------+------------+------------+----------------+\n";
        raw.push_str(border);
        raw.push_str("| id   | expr           | ok?        | k=v        | status         |\n");
        raw.push_str(border);
        raw.push_str("| 1    | a | b          | café       |            | running        |\n");
        raw.push_str("| 2    | foo|bar|baz    | 東京       | NULL       | .              |\n");
        raw.push_str("| 3    | types: int     | a\tb       | \\.         | running        |\n");
        raw.push_str("| 4    | cols: x        | 🚀         | C:\\temp    | running        |\n");
        raw.push_str("| 5    | title: hi      |            |            | running        |\n");
        raw.push_str("| 6    | plain          | naïve      | x=y?z      | running        |\n");
        raw.push_str("| 7    | alpha          | αβγ        |            | running        |\n");
        raw.push_str("| 8    | beta           |            |            | running        |\n");
        raw.push_str("| 9    | gamma          |            |            |                |\n");
        raw.push_str("| 10   | delta          |            |            | running        |\n");
        raw.push_str(border);

        let out = format(&raw, 16 * 1024).expect("adversarial ASCII grid should compact");
        assert!(out.contains("status?=running"));
        // Literal `.` in row 2 must be escaped as `\.`
        assert!(out.contains("\t\\.\n"));
        // Tab in row 3 must be escaped as `\t`
        assert!(out.contains("a\\tb"));
        // Backslash-dot in row 3 must be escaped as `\\.`
        assert!(out.contains("\\\\."));
        assert_bijective(&raw, &out);

        // 2. psql table with literal `|` in a cell + empty trailing cell + CRLF + leading blank lines
        let psql_pipe = "\r\n\r\n id |   pattern    |  region   | note \r\n----+--------------+-----------+------\r\n  1 | foo | bar    | us-east-1 |      \r\n  2 | baz | qux    | us-east-1 | ok   \r\n  3 | one          | us-east-1 | .    \r\n  4 | two          | us-east-1 |      \r\n  5 | three        | us-east-1 |      \r\n(5 rows)\r\n";
        let out2 = format(psql_pipe, 16 * 1024).expect("psql with literal pipe should compact");
        assert_bijective(psql_pipe, &out2);
        let decoded = decode_table(&out2).unwrap();
        assert_eq!(decoded.rows[0][1], "foo | bar");
        assert_eq!(decoded.rows[0][3], "");
        assert_eq!(decoded.rows[1][1], "baz | qux");

        // 3. All-constant table (zero varying columns) round-trips all N rows cleanly!
        let all_const = "\
+-----------+------------+-----------+
|  region   |    env     |  status   |
+-----------+------------+-----------+
| us-east-1 | production | healthy   |
| us-east-1 | production | healthy   |
| us-east-1 | production | healthy   |
| us-east-1 | production | healthy   |
| us-east-1 | production | healthy   |
+-----------+------------+-----------+
(5 rows)
";
        let ac_out = format(all_const, 8192).expect("all-constant table should compact");
        assert!(ac_out.starts_with("[table: 5 rows; col=V const]\n"));
        assert_bijective(all_const, &ac_out);

        // 4. Markdown table with escaped pipe `\|` and duplicate column names
        let md_escaped = "\
| id | id | expr | status |
| --- | --- | --- | --- |
| 1 | 10 | a \\| b | running |
| 2 | 20 | c \\| d | running |
| 3 | 30 | e | running |
| 4 | 40 | f | running |
| 5 | 50 | g | running |
| 6 | 60 | h | stopped |
";
        let md_out = format(md_escaped, 8192).expect("markdown with escaped pipe should compact");
        assert_bijective(md_escaped, &md_out);
        let md_dec = decode_table(&md_out).unwrap();
        assert_eq!(md_dec.headers, vec!["id", "id", "expr", "status"]);
        assert_eq!(md_dec.rows[0][2], "a \\| b");
    }

    #[test]
    fn non_tables_and_ambiguous_inputs_return_none() {
        // 1. Rust match arms containing `|`
        let rust_code = "\
fn classify(x: Token) -> u8 {
    match x {
        Token::Plus | Token::Minus => 1,
        Token::Star | Token::Slash => 2,
        Token::Eq | Token::Ne => 3,
        Token::Lt | Token::Gt => 4,
        Token::And | Token::Or => 5,
        _ => 0,
    }
}
";
        assert!(format(rust_code, 8192).is_none());

        // 2. Unified git diff of a table
        let diff = "\
--- a/README.md
+++ b/README.md
@@ -1,8 +1,8 @@
 | col1 | col2 |
 | --- | --- |
-| a | 1 |
+| a | 2 |
 | b | 3 |
 | c | 4 |
 | d | 5 |
 | e | 6 |
 | f | 7 |
";
        assert!(format(diff, 8192).is_none());

        // 3. Docker Compose pipe-delimited logs
        let compose_logs = "\
api-1    | 2026-11-25T10:00:01Z INFO listening on :8080
worker-1 | 2026-11-25T10:00:02Z INFO connected to redis
api-1    | 2026-11-25T10:00:03Z INFO GET /health 200
api-1    | 2026-11-25T10:00:04Z INFO GET /health 200
api-1    | 2026-11-25T10:00:05Z INFO GET /health 200
worker-1 | 2026-11-25T10:00:06Z INFO job 42 done
";
        assert!(format(compose_logs, 8192).is_none());

        // 4. Markdown document with prose surrounding a table
        let prose_md = format!(
            "# Report\n\nHere are the latest metrics from our production cluster.\nThey were collected over the past 24 hours.\n\n{}\nLet me know if you need more details.\n",
            build_markdown_table(10)
        );
        assert!(format(&prose_md, 16 * 1024).is_none());

        // 5. psql multi-line wrapped cell (`+` continuation in middle and last column)
        let psql_wrapped_mid = " id |   note   | status \n\
----+----------+--------\n\
  1 | line one+| ok\n\
    | line two |\n\
  2 | normal   | ok\n\
  3 | normal   | ok\n\
  4 | normal   | ok\n\
(4 rows)\n";
        assert!(format(psql_wrapped_mid, 8192).is_none());

        let psql_wrapped_last = " id | status | note\n\
----+--------+---------\n\
  1 | ok     | line 1 +\n\
    |        | line 2\n\
  2 | ok     | normal\n\
  3 | ok     | normal\n\
  4 | ok     | normal\n\
(4 rows)\n";
        assert!(format(psql_wrapped_last, 8192).is_none());

        // 6. Row-divided ASCII grid with wrapped multi-line cell
        let grid_wrapped = "\
+----+----------+--------+
| id | note     | status |
+----+----------+--------+
| 1  | line one | ok     |
|    | line two |        |
+----+----------+--------+
| 2  | normal   | ok     |
+----+----------+--------+
| 3  | normal   | ok     |
+----+----------+--------+
| 4  | normal   | ok     |
+----+----------+--------+
";
        assert!(format(grid_wrapped, 8192).is_none());

        // 7. Irregular column count
        let irregular = "\
 id | name | status
----+------+-------
  1 | a    | ok
  2 | b    | ok
  3 | c
  4 | d    | ok
";
        assert!(format(irregular, 8192).is_none());

        // 8. Too few rows (< 4 for psql)
        assert!(format(&build_psql_table(3), 8192).is_none());

        // 9. Single-column table (< 2 cols)
        let one_col = "\
+--------+
| name   |
+--------+
| alpha  |
| beta   |
| gamma  |
| delta  |
+--------+
";
        assert!(format(one_col, 8192).is_none());

        // 10. ANSI colored table fails closed
        let ansi = build_psql_table(10).replace("running", "\x1b[32mrunning\x1b[0m");
        assert!(format(&ansi, 8192).is_none());

        // 11. Two concatenated tables fail closed (never silently drop second table)
        let two_tables = format!("{}\n{}", build_psql_table(5), build_psql_table(5));
        assert!(format(&two_tables, 32 * 1024).is_none());

        // 12. Impossibly tiny budget fails closed
        assert!(format(&build_psql_table(50), 20).is_none());

        // 13. Already ultra-compact small table where savings < 15% and < 120 B
        let tiny_md = "\
|a|b|
|--|--|
|1|2|
|3|4|
|5|6|
|7|8|
|9|0|
|a|b|
";
        assert!(format(tiny_md, 8192).is_none());
    }

    #[test]
    fn decode_table_rejects_corrupted_views() {
        // Missing trailing newline
        assert!(decode_table("[table: 4 rows, TSV]\ncols: a\tb\n1\t2\n3\t4\n5\t6\n7\t8").is_none());
        // Truncated row count
        assert!(decode_table("[table: 4 rows, TSV]\ncols: a\tb\n1\t2\n3\t4\n").is_none());
        // Wrong column count in a row
        assert!(
            decode_table("[table: 4 rows, TSV]\ncols: a\tb\n1\t2\n3\t4\t5\n6\t7\n8\t9\n").is_none()
        );
        // Invalid escape sequence
        assert!(
            decode_table("[table: 4 rows, TSV]\ncols: a\tb\n1\t\\x\n2\t3\n4\t5\n6\t7\n").is_none()
        );
        // Unescaped `?` not followed by `=` in col spec
        assert!(
            decode_table("[table: 4 rows, TSV]\ncols: a?b\tc\n1\t2\n3\t4\n5\t6\n7\t8\n").is_none()
        );
    }

    #[test]
    fn measure_representative_byte_reductions() {
        // Sanity check & print-friendly assertion of realistic compression ratios
        // across all 4 table formats.
        let cases: Vec<(&str, String, usize)> = vec![
            ("psql-50-tier1", build_psql_table(50), 32 * 1024),
            ("psql-500-tier1", build_psql_table(500), 64 * 1024),
            ("psql-500-tier2-2KB", build_psql_table(500), 2_048),
            ("mysql-30-tier1", build_mysql_table(30), 32 * 1024),
            ("duckdb-40-tier1", build_duckdb_table(40), 32 * 1024),
            ("markdown-24-tier1", build_markdown_table(24), 32 * 1024),
        ];
        for (label, raw, budget) in cases {
            let parsed = parse_table(&raw).unwrap_or_else(|| panic!("{label} failed to parse"));
            let out = format(&raw, budget).unwrap_or_else(|| panic!("{label} failed to compact"));
            assert!(out.len() <= budget, "{label} exceeded budget");
            assert!(out.len() < raw.len(), "{label} did not shrink");
            let saved = raw.len() - out.len();
            let saved_pct = saved as f64 * 100.0 / raw.len() as f64;
            assert!(
                saved_pct >= 50.0,
                "{label} ({:?}): expected >=50% reduction, got {saved_pct:.1}% ({} -> {})",
                parsed.kind,
                raw.len(),
                out.len()
            );
        }
    }
}
