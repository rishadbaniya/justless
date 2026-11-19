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
use std::collections::{BTreeMap, BTreeSet, HashSet};
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

impl Content {
    const ALL: [Content; 4] = [Content::Code, Content::Json, Content::Prose, Content::Logs];

    fn calibration(self) -> &'static Calibration {
        &CALIBRATION[self as usize]
    }

    pub(crate) fn name(self) -> &'static str {
        self.calibration().name
    }

    pub(crate) fn bytes_per_token(self) -> f64 {
        let c = self.calibration();
        c.bytes as f64 / c.tokens as f64
    }

    /// Estimated tokens in `bytes` of this content, rounded up.
    pub(crate) fn tokens(self, bytes: u64) -> u64 {
        let c = self.calibration();
        let tokens = (u128::from(bytes) * u128::from(c.tokens)).div_ceil(u128::from(c.bytes));
        u64::try_from(tokens).unwrap_or(u64::MAX)
    }

    /// [`Content::tokens`] for net savings, which can be negative.
    pub(crate) fn tokens_signed(self, bytes: i64) -> i64 {
        let tokens = i64::try_from(self.tokens(bytes.unsigned_abs())).unwrap_or(i64::MAX);
        if bytes < 0 { -tokens } else { tokens }
    }
}

/// How reports describe the estimate.
pub(crate) fn method() -> String {
    let ratios: Vec<_> = Content::ALL
        .iter()
        .map(|c| format!("{} {:.2}", c.name(), c.bytes_per_token()))
        .collect();
    format!(
        "UTF-8 bytes / bytes per token by content type, measured with {MODEL}: {}",
        ratios.join(", ")
    )
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

    pub(crate) fn saved_tokens(&self) -> u64 {
        let tokens = Content::ALL
            .iter()
            .map(|&c| c.tokens_signed(self.saved[c as usize]))
            .fold(0, i64::saturating_add);
        tokens.max(0).unsigned_abs()
    }

    pub(crate) fn net_tokens(&self) -> i64 {
        Content::ALL
            .iter()
            .map(|&c| c.tokens_signed(self.net[c as usize]))
            .fold(0, i64::saturating_add)
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
        let mut used: Vec<_> = Content::ALL
            .into_iter()
            .filter(|&c| self.saved[c as usize] > 0 || self.net[c as usize] != 0)
            .collect();
        if used.is_empty() {
            used.push(Content::Logs);
        }
        used.sort_by_key(|&c| std::cmp::Reverse(self.saved[c as usize]));
        used.iter()
            .enumerate()
            .map(|(i, c)| {
                let unit = if i == 0 { " bytes per token" } else { "" };
                format!("{:.2}{unit} for {}", c.bytes_per_token(), c.name())
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Bytes of a run's archived streams read to decide its content type.
const SNIFF: u64 = 8192;

/// The content type of a run's output, from the start of its archived stdout and
/// then stderr. Without an archive, command output counts as logs.
pub(crate) fn run_content(directory: &Path) -> Content {
    let mut sample = Vec::new();
    for stream in ["stdout", "stderr"] {
        let left = SNIFF.saturating_sub(sample.len() as u64);
        if left == 0 {
            break;
        }
        if let Ok(file) = File::open(directory.join(stream)) {
            let _ = file.take(left).read_to_end(&mut sample);
        }
    }
    classify(&sample)
}

/// JSON when the text opens a JSON value; code when two lines in five look like
/// source or a patch; prose when half read as sentences or Markdown; else logs.
/// A line that starts with a location or a level, as compilers and linters
/// print them, counts as neither code nor prose.
pub(crate) fn classify(sample: &[u8]) -> Content {
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
    let level = first
        .trim_start_matches('[')
        .split([':', '[', ']'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    first
        .split(':')
        .nth(1)
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        || (LEVELS.contains(&level.as_str())
            && (first.ends_with([':', ']']) || first.starts_with('[')))
        || ["-->", "=", "^"].contains(&first)
        // A diagnostic's gutter, not a Markdown table row.
        || (first == "|" && !line.ends_with('|'))
        || (first.bytes().all(|b| b.is_ascii_digit()) && words.next() == Some("|"))
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
    let visible = line.chars().filter(|c| !c.is_whitespace()).count();
    let letters = line.chars().filter(|c| c.is_alphabetic()).count();
    line.split_whitespace().count() >= 6 && letters * 10 >= visible * 7
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

impl Usage {
    fn add(&mut self, counts: &Counts) {
        self.input_tokens += counts.input_tokens.unwrap_or(0);
        self.cache_creation_input_tokens += counts.cache_creation_input_tokens.unwrap_or(0);
        self.cache_read_input_tokens += counts.cache_read_input_tokens.unwrap_or(0);
        self.output_tokens += counts.output_tokens.unwrap_or(0);
    }

    fn merge(&mut self, other: &Usage) {
        self.requests += other.requests;
        self.input_tokens += other.input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.output_tokens += other.output_tokens;
    }

    fn tokens(&self) -> u64 {
        self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
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
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    #[serde(rename = "isSidechain", default)]
    sidechain: bool,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Counts>,
}

#[derive(Deserialize)]
struct Counts {
    #[serde(rename = "type")]
    kind: Option<String>,
    model: Option<String>,
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    iterations: Option<Vec<Counts>>,
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
    let mut seen = HashSet::new();
    let mut models: BTreeMap<String, Usage> = BTreeMap::new();
    let mut sessions = BTreeSet::new();
    let mut subagent_requests = 0;
    for file in &files {
        let Ok(handle) = File::open(file) else {
            continue;
        };
        for line in BufReader::new(handle).lines() {
            let Ok(line) = line else {
                break;
            };
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Line>(&line) else {
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
            let model = model.unwrap_or_else(|| "unknown".to_owned());
            if model == "<synthetic>"
                || entry
                    .timestamp
                    .as_deref()
                    .is_some_and(|time| time < since_iso.as_str())
            {
                continue;
            }
            if let Some(key) = id.or(entry.request_id)
                && !seen.insert(key)
            {
                continue;
            }
            // <session>.jsonl and everything under <session>/ belong to one session.
            if let Some(first) = file
                .strip_prefix(&directory)
                .ok()
                .and_then(|relative| relative.components().next())
            {
                let name = first.as_os_str().to_string_lossy();
                sessions.insert(name.strip_suffix(".jsonl").unwrap_or(&name).to_owned());
            }
            let mut calls = 1;
            models.entry(model.clone()).or_default().requests += 1;
            match usage.iterations.as_deref() {
                Some(parts) if !parts.is_empty() => {
                    for part in parts {
                        let owner = part.model.clone().unwrap_or_else(|| model.clone());
                        let usage = models.entry(owner).or_default();
                        usage.add(part);
                        if part.kind.as_deref() == Some("advisor_message") {
                            usage.requests += 1;
                            calls += 1;
                        }
                    }
                }
                _ => models.entry(model).or_default().add(&usage),
            }
            if entry.sidechain {
                subagent_requests += calls;
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
}
