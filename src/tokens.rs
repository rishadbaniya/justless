//! Token figures for reports, without a tokenizer and without network access.
//!
//! Every compaction decision uses exact bytes; tokens appear only in reports.
//! Claude's tokenizer is not public, so reports convert bytes to tokens with one
//! measured ratio per content type: code, JSON, prose and logs.
//! `scripts/token_calibration.py` measured the ratios once with Anthropic's
//! token-counting endpoint on this repository's own sources, Markdown, JSON and
//! command output (docs/benchmarks.md). Figures derived from them are estimates.
//!
//! Claude Code also records the usage the provider reported for every request in
//! its session transcripts. [`sessions`] sums it for a workspace. That is total
//! usage, not savings, and only model labels and numbers leave this module.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The model whose token counts the ratios come from.
pub(crate) const MODEL: &str = "claude-sonnet-5-5";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Content {
    Code,
    Json,
    Prose,
    Logs,
}

/// One content type's measurement: `bytes` of UTF-8 in `samples` pieces of
/// `sources` files or command outputs counted as `tokens`.
struct Calibration {
    name: &'static str,
    sources: u32,
    samples: u32,
    bytes: u64,
    tokens: u64,
}

/// Measured on 2026-11-19 at commit e7d0926 with `MODEL`, the per-request
/// overhead subtracted. Indexed by [`Content`].
const CALIBRATION: [Calibration; 4] = [
    Calibration {
        name: "code",
        sources: 152,
        samples: 152,
        bytes: 1_499_977,
        tokens: 584_751,
    },
    Calibration {
        name: "json",
        sources: 10,
        samples: 13,
        bytes: 123_392,
        tokens: 55_462,
    },
    Calibration {
        name: "prose",
        sources: 15,
        samples: 22,
        bytes: 267_944,
        tokens: 96_407,
    },
    Calibration {
        name: "logs",
        sources: 33,
        samples: 34,
        bytes: 139_440,
        tokens: 63_191,
    },
];

const _: () = {
    let mut i = 0;
    while i < CALIBRATION.len() {
        let c = &CALIBRATION[i];
        assert!(c.bytes > 0 && c.tokens > 0 && c.tokens <= c.bytes);
        i += 1;
    }
};

impl Content {
    const ALL: [Content; 4] = [Content::Code, Content::Json, Content::Prose, Content::Logs];

    #[inline]
    const fn calibration(self) -> &'static Calibration {
        &CALIBRATION[self as usize]
    }

    #[inline]
    pub(crate) const fn name(self) -> &'static str {
        self.calibration().name
    }

    #[inline]
    pub(crate) const fn bytes_per_token(self) -> f64 {
        let c = self.calibration();
        c.bytes as f64 / c.tokens as f64
    }

    /// Estimated tokens in `bytes` of this content, rounded up.
    /// Exact integer ceiling arithmetic in `u128` avoids overflow and
    /// floating-point precision loss across all `u64` byte sizes.
    #[inline]
    pub(crate) const fn tokens(self, bytes: u64) -> u64 {
        if bytes == 0 {
            return 0;
        }
        let c = self.calibration();
        let tokens = (bytes as u128 * c.tokens as u128).div_ceil(c.bytes as u128);
        if tokens > u64::MAX as u128 {
            u64::MAX
        } else {
            tokens as u64
        }
    }

    /// [`Content::tokens`] for net savings, which can be negative.
    #[inline]
    pub(crate) const fn tokens_signed(self, bytes: i64) -> i64 {
        let raw = self.tokens(bytes.unsigned_abs());
        let tokens = if raw > i64::MAX as u64 {
            i64::MAX
        } else {
            raw as i64
        };
        if bytes < 0 { -tokens } else { tokens }
    }
}

/// How reports describe the estimate.
pub(crate) fn method() -> String {
    let mut out = format!("UTF-8 bytes / bytes per token by content type, measured with {MODEL}: ");
    for (i, c) in Content::ALL.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{} {:.2}", c.name(), c.bytes_per_token());
    }
    out
}

/// Saved bytes by content type, and the tokens they are estimated to be. A run
/// whose preview outgrew its output subtracts from its type's savings, as it
/// does from the byte totals.
#[derive(Default)]
pub(crate) struct Estimate {
    saved: [i64; 4],
    net: [i64; 4],
}

impl Estimate {
    pub(crate) fn add(&mut self, content: Content, saved: i64, net: i64) {
        let i = content as usize;
        self.saved[i] = self.saved[i].saturating_add(saved);
        self.net[i] = self.net[i].saturating_add(net);
    }

    /// Adds another estimate's bytes, as when several projects' totals are summed.
    pub(crate) fn merge(&mut self, other: &Estimate) {
        for c in Content::ALL {
            self.add(c, other.saved[c as usize], other.net[c as usize]);
        }
    }

    pub(crate) fn saved_tokens(&self) -> u64 {
        let mut total = 0i128;
        for c in Content::ALL {
            total += i128::from(c.tokens_signed(self.saved[c as usize]));
        }
        if total <= 0 {
            0
        } else {
            u64::try_from(total).unwrap_or(u64::MAX)
        }
    }

    pub(crate) fn net_tokens(&self) -> i64 {
        let mut total = 0i128;
        for c in Content::ALL {
            total += i128::from(c.tokens_signed(self.net[c as usize]));
        }
        total.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }

    /// Each type's bytes and calibration, so a reader can redo the arithmetic.
    pub(crate) fn json(&self) -> Value {
        let mut types = serde_json::Map::new();
        for c in Content::ALL {
            let cal = c.calibration();
            types.insert(
                c.name().to_owned(),
                json!({
                    "saved_bytes": self.saved[c as usize],
                    "net_saved_bytes": self.net[c as usize],
                    "bytes_per_token": (c.bytes_per_token() * 1000.0).round() / 1000.0,
                    "sample_sources": cal.sources,
                    "samples": cal.samples,
                    "sample_bytes": cal.bytes,
                    "sample_tokens": cal.tokens,
                }),
            );
        }
        Value::Object(types)
    }

    /// The ratios applied, most saved bytes first, such as
    /// `2.21 bytes per token for logs, 2.57 for code`.
    pub(crate) fn basis(&self) -> String {
        let mut used = [Content::Logs; 4];
        let mut count = 0usize;
        for c in Content::ALL {
            if self.saved[c as usize] > 0 || self.net[c as usize] != 0 {
                used[count] = c;
                count += 1;
            }
        }
        if count == 0 {
            count = 1;
        }
        let slice = &mut used[..count];
        slice.sort_by_key(|&c| std::cmp::Reverse(self.saved[c as usize]));
        let mut out = String::with_capacity(count * 24);
        for (i, c) in slice.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let unit = if i == 0 { " bytes per token" } else { "" };
            let _ = write!(out, "{:.2}{unit} for {}", c.bytes_per_token(), c.name());
        }
        out
    }
}

/// Bytes of a run's archived streams read to decide its content type.
const SNIFF: u64 = 8192;

/// The content type of a run's output, from the start of its archived stdout and
/// then stderr. Without an archive, command output counts as logs.
pub(crate) fn run_content(directory: &Path) -> Content {
    let mut sample = [0u8; SNIFF as usize];
    let mut len = 0usize;
    for stream in ["stdout", "stderr"] {
        if len >= sample.len() {
            break;
        }
        let Ok(mut file) = File::open(directory.join(stream)) else {
            continue;
        };
        while len < sample.len() {
            match file.read(&mut sample[len..]) {
                Ok(0) => break,
                Ok(n) => len += n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }
    classify(&sample[..len])
}

/// JSON when the text opens a JSON value; code when two lines in five look like
/// source or a patch; prose when half read as sentences or Markdown; else logs.
/// A line that starts with a location or a level, as compilers and linters
/// print them, counts as neither code nor prose.
pub(crate) fn classify(sample: &[u8]) -> Content {
    if sample.is_empty() {
        return Content::Logs;
    }
    let text = String::from_utf8_lossy(sample);
    let trimmed = text.trim_start();
    if trimmed.starts_with(['{', '[']) {
        let mut values =
            serde_json::Deserializer::from_str(trimmed).into_iter::<serde::de::IgnoredAny>();
        match values.next() {
            Some(Ok(_)) => return Content::Json,
            // The sample ends inside the first value.
            Some(Err(error)) if error.is_eof() => return Content::Json,
            _ => {}
        }
    }
    let (mut lines, mut code, mut prose) = (0usize, 0usize, 0usize);
    for line in text.lines().take(300) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        lines += 1;
        if log_line(line) {
            continue;
        } else if markdown_line(line) {
            prose += 1;
        } else if code_line(line) {
            code += 1;
        } else if prose_line(line) {
            prose += 1;
        }
    }
    if lines > 0 && code * 5 >= lines * 2 {
        Content::Code
    } else if lines > 0 && prose * 2 >= lines {
        Content::Prose
    } else {
        Content::Logs
    }
}

/// `src/lib.rs:1:21`, `error[E0308]:`, `warning:`, `[INFO]`, ` --> `, `= note:` or
/// a numbered source line under a diagnostic, `12 | let x = 1;`.
fn log_line(line: &str) -> bool {
    const LEVELS: [&str; 10] = [
        "error", "warning", "warn", "note", "help", "info", "debug", "trace", "fatal", "panic",
    ];
    let mut words = line.split_whitespace();
    let first = words.next().unwrap_or("");
    let is_level = (first.ends_with([':', ']']) || first.starts_with('[')) && {
        let level = first
            .trim_start_matches('[')
            .split([':', '[', ']'])
            .next()
            .unwrap_or("");
        LEVELS.iter().any(|l| level.eq_ignore_ascii_case(l))
    };
    first
        .split(':')
        .nth(1)
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        || is_level
        || ["-->", "=", "^"].contains(&first)
        // A diagnostic's gutter, not a Markdown table row.
        || (first == "|" && !line.ends_with('|'))
        || (!first.is_empty()
            && first.bytes().all(|b| b.is_ascii_digit())
            && words.next() == Some("|"))
}

fn code_line(line: &str) -> bool {
    const STARTS: [&str; 34] = [
        "fn ",
        "pub ",
        "let ",
        "use ",
        "impl ",
        "struct ",
        "enum ",
        "mod ",
        "match ",
        "def ",
        "class ",
        "import ",
        "from ",
        "return ",
        "const ",
        "function ",
        "var ",
        "if ",
        "for ",
        "while ",
        "else ",
        "end ",
        "#include",
        "package ",
        "func ",
        "local ",
        "//",
        "/*",
        "#[",
        "#!",
        "diff --git",
        "@@ ",
        "+++ ",
        "--- ",
    ];
    if ["end", "else", "fi", "done", "esac"].contains(&line)
        || line.ends_with(" then")
        || line.ends_with(" do")
    {
        return true;
    }
    let mut words = line.split_whitespace();
    let first = words.next().unwrap_or("");
    // An assignment such as `name = "justless"` or `total := 0`.
    let assignment = first
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        && matches!(words.next(), Some("=" | ":="));
    line.ends_with(['{', '}', ';', '(', ')', ',', '['])
        || assignment
        || (line.starts_with('[') && line.ends_with(']'))
        || STARTS.iter().any(|start| line.starts_with(start))
}

/// A Markdown heading, list item, quote or table row. A removed patch line such
/// as `-    let x = 1;` is not a list item.
fn markdown_line(line: &str) -> bool {
    if line.len() > 1 && line.starts_with('|') && line.ends_with('|') {
        return true;
    }
    ["#### ", "### ", "## ", "# ", "- ", "* ", "> "]
        .iter()
        .find_map(|start| line.strip_prefix(start))
        .is_some_and(|body| !body.starts_with(char::is_whitespace) && !body.is_empty())
}

/// A line of at least six words whose other characters are mostly letters.
fn prose_line(line: &str) -> bool {
    let (mut words, mut visible, mut letters) = (0usize, 0usize, 0usize);
    let mut in_word = false;
    for c in line.chars() {
        if c.is_whitespace() {
            in_word = false;
        } else {
            if !in_word {
                words += 1;
                in_word = true;
            }
            visible += 1;
            if c.is_alphabetic() {
                letters += 1;
            }
        }
    }
    words >= 6 && letters.saturating_mul(10) >= visible.saturating_mul(7)
}

/// Provider-reported token counts.
#[derive(Default, Clone, Serialize)]
pub(crate) struct Usage {
    /// Responses billed to the model: one per request, plus one per advisor call.
    pub requests: u64,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

/// Transcripts are not ours, so their counts are added without overflowing.
impl Usage {
    fn add(&mut self, counts: &Counts) {
        let add = |total: &mut u64, count: Option<u64>| {
            *total = total.saturating_add(count.unwrap_or(0));
        };
        add(&mut self.input_tokens, counts.input_tokens);
        add(
            &mut self.cache_creation_input_tokens,
            counts.cache_creation_input_tokens,
        );
        add(
            &mut self.cache_read_input_tokens,
            counts.cache_read_input_tokens,
        );
        add(&mut self.output_tokens, counts.output_tokens);
    }

    fn merge(&mut self, other: &Usage) {
        self.requests = self.requests.saturating_add(other.requests);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }

    fn tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.output_tokens)
    }
}

#[derive(Serialize)]
pub(crate) struct ModelUsage {
    pub model: String,
    #[serde(flatten)]
    pub usage: Usage,
}

/// What Claude Code's sessions for a workspace recorded.
#[derive(Default, Serialize)]
pub(crate) struct Sessions {
    #[serde(skip)]
    pub directory: PathBuf,
    pub sessions: u64,
    /// Requests that subagents made, their advisor calls included, part of
    /// `requests`.
    pub subagent_requests: u64,
    #[serde(flatten)]
    pub total: Usage,
    /// Most tokens first.
    pub models: Vec<ModelUsage>,
}

/// The fields read from a transcript line. The rest, content included, is
/// skipped without being kept.
#[derive(Deserialize)]
struct Line<'a> {
    #[serde(borrow)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(rename = "requestId", borrow)]
    request_id: Option<Cow<'a, str>>,
    #[serde(rename = "isSidechain", default)]
    sidechain: bool,
    #[serde(borrow)]
    message: Option<Message<'a>>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    model: Option<Cow<'a, str>>,
    #[serde(borrow)]
    usage: Option<Counts<'a>>,
}

#[derive(Deserialize)]
struct Counts<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    model: Option<Cow<'a, str>>,
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    #[serde(borrow)]
    iterations: Option<Vec<Counts<'a>>>,
}

#[inline]
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    let mut pos = 0;
    while let Some(offset) = haystack[pos..].iter().position(|&b| b == first) {
        let start = pos + offset + 1;
        if haystack.get(start..start + rest.len()) == Some(rest) {
            return true;
        }
        pos = start;
    }
    false
}

fn with_model(models: &mut BTreeMap<String, Usage>, model: &str, update: impl FnOnce(&mut Usage)) {
    if let Some(usage) = models.get_mut(model) {
        update(usage);
    } else {
        let mut usage = Usage::default();
        update(&mut usage);
        models.insert(model.to_owned(), usage);
    }
}

/// An RFC 3339 UTC timestamp with milliseconds, as Claude Code writes them, so
/// that the two compare as strings.
fn iso(ms: u64) -> String {
    let day = ms % 86_400_000;
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        crate::history::utc_date(ms),
        day / 3_600_000,
        day / 60_000 % 60,
        day / 1000 % 60,
        day % 1000
    )
}

/// Usage that Claude Code recorded at or after `since_ms` in the sessions it keeps
/// for `root`, subagent transcripts included. A response is written once per
/// content block and a resumed session copies earlier ones, so each message counts
/// once. When a response lists `iterations`, they are its billed parts: the
/// model's own and those of an advisor, each under its model. `None` when the
/// Claude Code configuration directory cannot be located.
pub(crate) fn sessions(root: &Path, since_ms: u64) -> Option<Sessions> {
    Some(sessions_in(
        crate::discover::transcript_dir(root)?,
        since_ms,
    ))
}

/// [`sessions`] for the transcripts under `directory`.
fn sessions_in(directory: PathBuf, since_ms: u64) -> Sessions {
    let mut files = Vec::new();
    let since = UNIX_EPOCH + Duration::from_millis(since_ms);
    crate::discover::transcripts(&directory, since, 6, &mut files);
    files.sort();
    let since_iso = iso(since_ms);
    let mut seen: HashSet<String> = HashSet::new();
    let mut models: BTreeMap<String, Usage> = BTreeMap::new();
    let mut sessions = BTreeSet::new();
    let mut subagent_requests = 0u64;
    let mut bytes = Vec::new();
    for file in &files {
        let Ok(handle) = File::open(file) else {
            continue;
        };
        let mut reader = BufReader::new(handle);
        let mut file_counted = false;
        loop {
            bytes.clear();
            // The end of the file or a read error; `read_until` retries interruptions.
            if !matches!(reader.read_until(b'\n', &mut bytes), Ok(1..)) {
                break;
            }
            if !contains_bytes(&bytes, b"\"usage\"") {
                continue;
            }
            // A tool result may hold bytes that are not UTF-8; they affect only
            // their own line.
            let line = String::from_utf8_lossy(&bytes);
            let Ok(entry) = serde_json::from_str::<Line<'_>>(&line) else {
                continue;
            };
            let Some(Message {
                id,
                model,
                usage: Some(usage),
            }) = entry.message
            else {
                continue;
            };
            let model = model.as_deref().unwrap_or("unknown");
            if model == "<synthetic>"
                || entry
                    .timestamp
                    .as_deref()
                    .is_some_and(|time| time < since_iso.as_str())
            {
                continue;
            }
            if let Some(key) = id.or(entry.request_id) {
                if seen.contains(key.as_ref()) {
                    continue;
                }
                seen.insert(key.into_owned());
            }
            // <session>.jsonl and everything under <session>/ belong to one session.
            if !file_counted {
                file_counted = true;
                if let Some(first) = file
                    .strip_prefix(&directory)
                    .ok()
                    .and_then(|relative| relative.components().next())
                {
                    let name = first.as_os_str().to_string_lossy();
                    let session = name.strip_suffix(".jsonl").unwrap_or(&name);
                    if !sessions.contains(session) {
                        sessions.insert(session.to_owned());
                    }
                }
            }
            let mut calls = 1u64;
            match usage.iterations.as_deref() {
                Some(parts) if !parts.is_empty() => {
                    with_model(&mut models, model, |u| {
                        u.requests = u.requests.saturating_add(1);
                    });
                    for part in parts {
                        let owner = part.model.as_deref().unwrap_or(model);
                        with_model(&mut models, owner, |u| {
                            u.add(part);
                            if part.kind.as_deref() == Some("advisor_message") {
                                u.requests = u.requests.saturating_add(1);
                                calls = calls.saturating_add(1);
                            }
                        });
                    }
                }
                _ => with_model(&mut models, model, |u| {
                    u.requests = u.requests.saturating_add(1);
                    u.add(&usage);
                }),
            }
            if entry.sidechain {
                subagent_requests = subagent_requests.saturating_add(calls);
            }
        }
    }
    let mut report = Sessions {
        directory,
        sessions: sessions.len() as u64,
        subagent_requests,
        ..Sessions::default()
    };
    for usage in models.values() {
        report.total.merge(usage);
    }
    report.models = models
        .into_iter()
        .map(|(model, usage)| ModelUsage { model, usage })
        .collect();
    report.models.sort_by(|a, b| {
        b.usage
            .tokens()
            .cmp(&a.usage.tokens())
            .then(a.model.cmp(&b.model))
    });
    report
}

/// `stats --sessions` in JSON.
pub(crate) fn sessions_json(found: Option<&Sessions>, days: u64) -> Value {
    let Some(found) = found else {
        return Value::Null;
    };
    let mut value = serde_json::to_value(found).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.insert("days".into(), json!(days));
        map.insert(
            "note".into(),
            json!("provider-reported usage of every request, not Justless savings"),
        );
    }
    value
}

fn short(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

/// `stats --sessions` as text: a block apart from the savings.
pub(crate) fn print_sessions(found: Option<&Sessions>, days: u64) {
    println!(
        "\nClaude Code usage in this workspace's sessions, last {days} day(s), as the provider reported it. This is total usage, not savings."
    );
    let Some(found) = found else {
        println!("  Cannot locate the Claude Code configuration directory.");
        return;
    };
    if found.total.requests == 0 {
        println!(
            "  No requests recorded under {}.",
            found.directory.display()
        );
        return;
    }
    println!(
        "  {} session(s) · {} request(s), {} from subagents",
        found.sessions, found.total.requests, found.subagent_requests
    );
    let wide = found
        .models
        .iter()
        .map(|m| m.model.chars().count().min(28))
        .max()
        .unwrap_or(0)
        .max(5);
    println!(
        "  {:<wide$}  {:>8}  {:>8}  {:>11}  {:>10}  {:>8}",
        "model", "requests", "input", "cache write", "cache read", "output"
    );
    let rows = found
        .models
        .iter()
        .map(|m| (crate::summary::short(&m.model, 28), &m.usage))
        .chain(std::iter::once(("total".to_owned(), &found.total)));
    for (label, u) in rows {
        println!(
            "  {label:<wide$}  {:>8}  {:>8}  {:>11}  {:>10}  {:>8}",
            u.requests,
            short(u.input_tokens),
            short(u.cache_creation_input_tokens),
            short(u.cache_read_input_tokens),
            short(u.output_tokens)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &str) -> Vec<u8> {
        let mut bytes = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap();
        bytes.truncate(SNIFF as usize);
        bytes
    }

    #[test]
    fn estimates_use_each_types_measured_ratio() {
        assert_eq!(Content::Code.tokens(1_499_977), 584_751);
        assert_eq!(Content::Logs.tokens(1), 1);
        assert_eq!(Content::Prose.tokens_signed(-267_944), -96_407);
        let mut estimate = Estimate::default();
        estimate.add(Content::Logs, 139_440, -139_440);
        estimate.add(Content::Json, 123_392, 0);
        assert_eq!(estimate.saved_tokens(), 63_191 + 55_462);
        assert_eq!(estimate.net_tokens(), -63_191);
        assert_eq!(
            estimate.basis(),
            "2.21 bytes per token for logs, 2.22 for json"
        );
        assert!(method().ends_with("code 2.57, json 2.22, prose 2.78, logs 2.21"));
    }

    #[test]
    fn classifies_fixture_sources_documents_data_and_tool_output() {
        for path in [
            "fixtures/lookup/src/net/retry.rs",
            "fixtures/mixed/frontend/orders.ts",
            "fixtures/mixed/backend/orders.py",
            "fixtures/failing/src/pricing.rs",
        ] {
            assert_eq!(classify(&read(path)), Content::Code, "{path}");
        }
        for path in [
            "fixtures/affected/docs/pricing.md",
            "fixtures/lookup/docs/operations.md",
        ] {
            assert_eq!(classify(&read(path)), Content::Prose, "{path}");
        }
        for path in [
            "fixtures/lookup/package-lock.json",
            "fixtures/affected/ledger/fixtures/rates.json",
        ] {
            assert_eq!(classify(&read(path)), Content::Json, "{path}");
        }
        for path in ["mypy.txt", "ruff.txt", "golangci.txt", "cargo_targets.txt"] {
            let path = format!("fixtures/lint/{path}");
            assert_eq!(classify(&read(&path)), Content::Logs, "{path}");
        }
        assert_eq!(classify(b"[1, 2"), Content::Json);
        assert_eq!(classify(b"[justless run-abc] exit 0\n"), Content::Logs);
        // Search matches start with a location, even over source.
        assert_eq!(classify(b"src/a.rs:3:    let x = f(y);\n"), Content::Logs);
        assert_eq!(classify(b""), Content::Logs);
    }

    #[test]
    fn session_usage_counts_each_response_once_by_model_and_skips_content() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = home.path().join("-work-shop");
        std::fs::create_dir_all(dir.join("s1/subagents")).unwrap();
        let line = |id: &str, time: &str, sidechain: bool, usage: Value| {
            json!({"type": "assistant", "timestamp": time, "isSidechain": sidechain,
                   "requestId": format!("req-{id}"),
                   "message": {"id": id, "model": "claude-opus-5-5", "usage": usage,
                               "content": [{"type": "text", "text": "SECRET-CONTENT"}]}})
            .to_string()
        };
        let plain = json!({"input_tokens": 2, "cache_creation_input_tokens": 100,
                           "cache_read_input_tokens": 1000, "output_tokens": 50});
        let advised = json!({"input_tokens": 4, "cache_creation_input_tokens": 10,
            "cache_read_input_tokens": 500, "output_tokens": 30, "iterations": [
                {"type": "message", "input_tokens": 4, "cache_creation_input_tokens": 10,
                 "cache_read_input_tokens": 500, "output_tokens": 30},
                {"type": "advisor_message", "model": "claude-fable-5-2", "input_tokens": 600,
                 "output_tokens": 70}]});
        let main = [
            line("m1", "2026-11-19T10:00:00.000Z", false, plain.clone()),
            // The same response again, for its second content block.
            line("m1", "2026-11-19T10:00:00.000Z", false, plain.clone()),
            line("m2", "2026-11-19T10:01:00.000Z", false, advised),
            line("old", "2020-01-01T00:00:00.000Z", false, plain.clone()),
            line("syn", "2026-11-19T10:02:00.000Z", false, plain.clone())
                .replace("claude-opus-5-5", "<synthetic>"),
            r#"{"type":"user","message":{"role":"user","content":"usage"}}"#.to_owned(),
        ];
        std::fs::write(dir.join("s1.jsonl"), main.join("\n")).unwrap();
        std::fs::write(
            dir.join("s1/subagents/agent-a.jsonl"),
            line("a1", "2026-11-19T10:03:00.000Z", true, plain.clone()),
        )
        .unwrap();
        // A resumed session copies the first response.
        std::fs::write(
            dir.join("s2.jsonl"),
            line("m1", "2026-11-19T10:00:00.000Z", false, plain),
        )
        .unwrap();

        let since = 1_763_000_000_000; // 2025-11-13
        let found = sessions_in(dir, since);
        assert_eq!((found.sessions, found.subagent_requests), (1, 1));
        let usage = |model: &str| {
            &found
                .models
                .iter()
                .find(|m| m.model == model)
                .unwrap()
                .usage
        };
        let opus = usage("claude-opus-5-5");
        assert_eq!(
            (
                opus.requests,
                opus.input_tokens,
                opus.cache_creation_input_tokens
            ),
            (3, 2 + 4 + 2, 100 + 10 + 100)
        );
        assert_eq!(
            (opus.cache_read_input_tokens, opus.output_tokens),
            (2500, 130)
        );
        let fable = usage("claude-fable-5-2");
        assert_eq!(
            (fable.requests, fable.input_tokens, fable.output_tokens),
            (1, 600, 70)
        );
        assert_eq!(found.total.requests, 4);
        let json = sessions_json(Some(&found), 7).to_string();
        assert!(!json.contains("SECRET") && json.contains("\"cache_read_input_tokens\":2500"));
    }

    fn response(id: &str, model: &str, usage: Value) -> String {
        json!({"type": "assistant", "timestamp": "2026-11-19T10:00:00.000Z",
               "message": {"id": id, "model": model, "usage": usage,
                           "content": [{"type": "text", "text": "TEXT"}]}})
        .to_string()
    }

    #[test]
    fn session_usage_reads_past_lines_that_are_not_utf8() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = home.path().join("-work-shop");
        std::fs::create_dir_all(&dir).unwrap();
        let usage = json!({"input_tokens": 10, "output_tokens": 5});
        let mut bytes = Vec::new();
        for (id, text) in [
            ("m1", &b"TEXT"[..]),
            ("m2", b"bad \xff byte"),
            ("m3", b"TEXT"),
        ] {
            let line = response(id, "claude-opus-5-5", usage.clone());
            let (before, after) = line.split_once("TEXT").unwrap();
            bytes.extend_from_slice(before.as_bytes());
            bytes.extend_from_slice(text);
            bytes.extend_from_slice(after.as_bytes());
            bytes.push(b'\n');
            // A tool result that is not UTF-8 between two responses.
            bytes.extend_from_slice(b"{\"type\":\"user\",\"content\":\"\xfe\xff\"}\n");
        }
        bytes.extend_from_slice(b"not json, \"usage\"\n");
        std::fs::write(dir.join("s1.jsonl"), bytes).unwrap();
        let found = sessions_in(dir, 0);
        assert_eq!(
            (found.total.requests, found.total.input_tokens),
            (3, 30),
            "every response counts"
        );
    }

    #[test]
    fn session_usage_saturates_instead_of_overflowing() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = home.path().join("-work-shop");
        std::fs::create_dir_all(&dir).unwrap();
        let huge = json!({"input_tokens": u64::MAX, "cache_read_input_tokens": u64::MAX,
                          "output_tokens": 1});
        let lines = [
            response("m1", "claude-opus-5-5", huge.clone()),
            response("m2", "claude-opus-5-5", huge.clone()),
            response("m3", "claude-fable-5-2", huge),
        ];
        std::fs::write(dir.join("s1.jsonl"), lines.join("\n")).unwrap();
        let found = sessions_in(dir, 0);
        assert_eq!(found.total.requests, 3);
        assert_eq!(found.total.input_tokens, u64::MAX);
        assert_eq!(found.total.output_tokens, 3);
        assert_eq!(found.total.tokens(), u64::MAX);
        assert_eq!(found.models[0].usage.tokens(), u64::MAX);
    }

    #[test]
    fn byte_token_conversions_handle_zero_one_multi_gb_and_extremes_without_overflow() {
        const GIB: u64 = 1 << 30;
        let samples = [
            0u64,
            1,
            2,
            3,
            4096,
            GIB,
            4 * GIB,
            64 * GIB,
            1u64 << 53,
            i64::MAX as u64,
            u64::MAX,
        ];
        for content in Content::ALL {
            let cal = content.calibration();
            assert!(content.bytes_per_token().is_finite() && content.bytes_per_token() > 1.0);
            assert_eq!(content.tokens(0), 0);
            assert_eq!(content.tokens(1), 1);
            assert_eq!(content.tokens_signed(0), 0);
            assert_eq!(content.tokens_signed(1), 1);
            assert_eq!(content.tokens_signed(-1), -1);

            let mut prev = 0u64;
            for bytes in samples {
                let expected = u64::try_from(
                    (u128::from(bytes) * u128::from(cal.tokens)).div_ceil(u128::from(cal.bytes)),
                )
                .unwrap();
                let got = content.tokens(bytes);
                assert_eq!(got, expected, "{:?} at {bytes} bytes", content);
                assert!(got >= prev, "{:?} must be monotonic at {bytes}", content);
                prev = got;
            }

            for signed in [0i64, 1, 4 * GIB as i64, 64 * GIB as i64, i64::MAX] {
                let pos = content.tokens_signed(signed);
                let neg = content.tokens_signed(-signed);
                assert_eq!(pos, -neg, "{:?} symmetry at {signed}", content);
            }
            let min_tokens = content.tokens_signed(i64::MIN);
            assert!(min_tokens < 0);
            assert_eq!(
                min_tokens.unsigned_abs(),
                content.tokens(i64::MIN.unsigned_abs())
            );
        }

        // Huge opposing estimates across content types cancel cleanly in i128
        // without premature i64 saturation.
        let mut estimate = Estimate::default();
        estimate.add(Content::Code, i64::MAX, i64::MAX);
        estimate.add(Content::Logs, i64::MAX, i64::MAX);
        estimate.add(Content::Json, i64::MAX, i64::MAX);
        estimate.add(Content::Prose, -i64::MAX, -i64::MAX);
        let expected_net = i128::from(Content::Code.tokens_signed(i64::MAX))
            + i128::from(Content::Logs.tokens_signed(i64::MAX))
            + i128::from(Content::Json.tokens_signed(i64::MAX))
            + i128::from(Content::Prose.tokens_signed(-i64::MAX));
        assert_eq!(estimate.net_tokens(), expected_net as i64);
        assert_eq!(estimate.saved_tokens(), expected_net as u64);
    }
}
