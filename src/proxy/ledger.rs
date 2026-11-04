//! The request ledger and the tool-result archive. A background worker archives
//! results and appends one JSON line per completed request, so neither can delay or
//! fail a request. `justless stats` reads the ledger back.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::plan::{self, Request};
use crate::history::{human_bytes, now_ms, utc_date};
use crate::observations::{ToolCall, archive_tool_use, valid_tool_use_id};
use crate::workspace::{Workspace, lock_state};

/// Results larger than this are not archived.
const MAX_ARCHIVED: u64 = 32 * 1024 * 1024;
const MAX_LOG: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cleared {
    pub results: u64,
    pub bytes: u64,
    pub est_tokens: u64,
}

/// One completed request. It holds no content and no header values; the only
/// identifiers are Claude Code's session, agent and request-class hints.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub ts: u64,
    pub session: Option<String>,
    pub agent: Option<String>,
    pub class: Option<String>,
    pub role: Option<String>,
    pub path: String,
    pub model: Option<String>,
    pub status: u16,
    pub mode: String,
    pub reason: Option<String>,
    pub uses: Option<u64>,
    pub boundary: Option<u64>,
    pub trigger: Option<u64>,
    pub keep: Option<u64>,
    #[serde(rename = "move")]
    pub movement: Option<String>,
    pub newly_cleared: Option<Cleared>,
    pub rewrite_est: Option<u64>,
    pub rewrite_api: Option<u64>,
    pub api_cleared_uses: Option<u64>,
    pub api_cleared_tokens: Option<u64>,
    pub mismatch: bool,
    pub repeats: u64,
    pub observed: Option<bool>,
    pub input: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub output: Option<u64>,
    pub archived: u64,
    pub archived_bytes: u64,
    pub archive_skipped: u64,
    /// Set on the request that first left a conversation to the client's clearing.
    pub client_managed_new: bool,
    pub ms: u64,
}

/// Running totals for the summary `wrap` prints at exit.
#[derive(Clone, Debug, Default)]
pub struct Totals {
    /// Every request, probes included. None means the client bypassed the proxy.
    pub seen: u64,
    /// Messages and token-count requests.
    pub requests: u64,
    pub moves: u64,
    pub cold_moves: u64,
    /// Results the proxy's moves asked the API to clear.
    pub cleared_results: u64,
    pub tokens_not_sent: u64,
    /// Requests where the API cleared a different number of calls than planned.
    pub mismatches: u64,
    pub archived: u64,
    pub client_managed: u64,
}

pub enum Job {
    Done(Box<(Record, Option<Request>)>),
    Flush(Sender<()>),
}

/// Appends startup lines, fallbacks and errors to `.justless/proxy/proxy.log`. It
/// never holds bodies or header values.
#[derive(Clone)]
pub struct Log(Arc<Mutex<PathBuf>>);

impl Log {
    pub fn open(path: PathBuf) -> Self {
        // Keep only the last megabyte from earlier runs.
        if let Ok(mut file) = File::open(&path)
            && let Ok(meta) = file.metadata()
            && meta.len() > MAX_LOG
            && file.seek(SeekFrom::Start(meta.len() - MAX_LOG)).is_ok()
        {
            let mut tail = Vec::new();
            if file.read_to_end(&mut tail).is_ok() {
                let start = tail.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
                let _ = fs::write(&path, &tail[start..]);
            }
        }
        Self(Arc::new(Mutex::new(path)))
    }

    pub fn line(&self, text: &str) {
        let path = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&*path) {
            let _ = writeln!(file, "{} {text}", now_ms());
        }
    }
}

/// The call summary kept with an archived result, on one line of at most 160
/// characters: `PATTERN in PATH` for searches, `FILE:OFFSET+LIMIT` for partial
/// reads, else the first path, command or query in the input, else its JSON.
pub fn call_summary(tool: &str, input: &Value) -> String {
    const FIELDS: [&str; 9] = [
        "file_path",
        "path",
        "command",
        "pattern",
        "url",
        "query",
        "notebook_path",
        "description",
        "prompt",
    ];
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    let number = |key: &str| input.get(key).and_then(Value::as_u64);
    let specific = match tool {
        "Grep" | "Glob" => text("pattern").map(|pattern| match text("path") {
            Some(path) => format!("{pattern} in {path}"),
            None => pattern.to_owned(),
        }),
        "Read" => text("file_path").map(|file| {
            let mut summary = file.to_owned();
            if let Some(offset) = number("offset") {
                summary.push_str(&format!(":{offset}"));
            }
            if let Some(limit) = number("limit") {
                summary.push_str(&format!("+{limit}"));
            }
            summary
        }),
        _ => None,
    };
    let text = specific
        .or_else(|| FIELDS.iter().find_map(|key| text(key)).map(str::to_owned))
        .unwrap_or_else(|| {
            if input.is_null() {
                String::new()
            } else {
                serde_json::to_string(input).unwrap_or_default()
            }
        });
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(160) {
        Some((cut, _)) => format!("{}…", &collapsed[..cut]),
        None => collapsed,
    }
}

pub struct Worker {
    ws: Workspace,
    archive: bool,
    seen: HashSet<String>,
    log: Log,
    totals: Arc<Mutex<Totals>>,
}

impl Worker {
    pub fn new(ws: Workspace, archive: bool, log: Log, totals: Arc<Mutex<Totals>>) -> Self {
        Self {
            ws,
            archive,
            seen: HashSet::new(),
            log,
            totals,
        }
    }

    pub fn run(mut self, jobs: Receiver<Job>) {
        for job in jobs {
            match job {
                Job::Done(done) => {
                    let (mut record, request) = *done;
                    if self.archive
                        && let Some(request) = request
                    {
                        self.archive(&request, &mut record);
                    }
                    self.count(&record);
                    if let Err(error) = append(&self.ws, &record) {
                        self.log.line(&format!("ledger: {error:#}"));
                    }
                }
                Job::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    }

    fn archive(&mut self, request: &Request, record: &mut Record) {
        for tool_use in &request.uses {
            let Some(at) = tool_use.result else {
                continue;
            };
            if self.seen.contains(&tool_use.id) || !valid_tool_use_id(&tool_use.id) {
                continue;
            }
            if tool_use.result_bytes > MAX_ARCHIVED {
                record.archive_skipped += 1;
                self.seen.insert(tool_use.id.clone());
                continue;
            }
            let block = request.block(at);
            let content = block.get("content").unwrap_or(&Value::Null);
            let input = &request.block(tool_use.call)["input"];
            let call = ToolCall {
                id: &tool_use.id,
                tool: &tool_use.name,
                summary: &call_summary(&tool_use.name, input),
                input,
            };
            match archive_tool_use(&self.ws, &call, content, block["is_error"] == true) {
                Ok(created) => {
                    self.seen.insert(tool_use.id.clone());
                    if created {
                        record.archived += 1;
                        record.archived_bytes += tool_use.result_bytes;
                    }
                }
                Err(error) => self.log.line(&format!("archive: {error:#}")),
            }
        }
    }

    fn count(&self, record: &Record) {
        let mut totals = self.totals.lock().unwrap_or_else(|e| e.into_inner());
        totals.seen += 1;
        totals.requests += u64::from(record.path != "other");
        totals.mismatches += u64::from(record.mismatch);
        if let Some(cleared) = &record.newly_cleared {
            totals.moves += 1;
            totals.cold_moves += u64::from(record.movement.as_deref() == Some("cold"));
            totals.cleared_results += cleared.results;
        }
        totals.tokens_not_sent += record.api_cleared_tokens.unwrap_or(0);
        totals.archived += record.archived;
        totals.client_managed += u64::from(record.client_managed_new);
    }
}

fn ledger_dir(ws: &Workspace) -> PathBuf {
    ws.state.join("proxy")
}

fn append(ws: &Workspace, record: &Record) -> Result<()> {
    let dir = ledger_dir(ws);
    let _guard = lock_state(&dir.join("ledger.lock"))?;
    let month = &utc_date(record.ts)[..7];
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("requests-{month}.jsonl")))?;
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(())
}

/// Totals over the ledger since a time, for `justless stats`.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    requests: u64,
    cleared_requests: u64,
    client_managed_conversations: u64,
    fallbacks: u64,
    moves: Moves,
    newly_cleared: Cleared,
    warm_rewrite_tokens: u64,
    tokens_not_sent: u64,
    mismatches: u64,
    repeats: u64,
    unobserved: u64,
    archived_results: u64,
    archived_bytes: u64,
    usage: Usage,
    token_estimate: &'static str,
    token_source: &'static str,
}

#[derive(Debug, Default, Serialize)]
struct Moves {
    cold: u64,
    payback: u64,
}

#[derive(Debug, Default, Serialize)]
struct Usage {
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
}

fn ledger_files(dir: &Path) -> Result<Option<Vec<PathBuf>>> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut files = Vec::new();
    for entry in listing {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.starts_with("requests-") && name.ends_with(".jsonl") {
            files.push(path);
        }
    }
    files.sort();
    Ok((!files.is_empty()).then_some(files))
}

/// `None` when the proxy never ran in this project.
pub fn report(ws: &Workspace, since_ms: u64) -> Result<Option<Report>> {
    let Some(files) = ledger_files(&ledger_dir(ws))? else {
        return Ok(None);
    };
    let mut report = Report {
        token_estimate: "UTF-8 bytes / 4",
        token_source: "API usage",
        ..Default::default()
    };
    // Monthly files that end before the window are skipped unread.
    let first = format!("requests-{}.jsonl", &utc_date(since_ms)[..7]);
    for path in files {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name < first.as_str())
        {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        for record in text
            .lines()
            .filter_map(|line| serde_json::from_str::<Record>(line).ok())
            .filter(|record| record.ts >= since_ms)
        {
            report.add(&record);
        }
    }
    Ok(Some(report))
}

impl Report {
    fn add(&mut self, record: &Record) {
        self.requests += 1;
        self.cleared_requests += u64::from(record.mode == "cleared");
        self.client_managed_conversations += u64::from(record.client_managed_new);
        self.fallbacks += u64::from(record.mode == "fallback");
        if let Some(cleared) = &record.newly_cleared {
            self.newly_cleared.results += cleared.results;
            self.newly_cleared.bytes += cleared.bytes;
            self.newly_cleared.est_tokens += cleared.est_tokens;
            match record.movement.as_deref() {
                Some("cold") => self.moves.cold += 1,
                _ => {
                    self.moves.payback += 1;
                    self.warm_rewrite_tokens += record.rewrite_api.unwrap_or(0);
                }
            }
        }
        self.tokens_not_sent += record.api_cleared_tokens.unwrap_or(0);
        self.mismatches += u64::from(record.mismatch);
        self.repeats += record.repeats;
        self.unobserved += u64::from(record.observed == Some(false));
        self.archived_results += record.archived;
        self.archived_bytes += record.archived_bytes;
        self.usage.input += record.input.unwrap_or(0);
        self.usage.cache_read += record.cache_read.unwrap_or(0);
        self.usage.cache_write += record.cache_write.unwrap_or(0);
        self.usage.output += record.output.unwrap_or(0);
    }

    pub fn json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!(null))
    }

    /// `justless` starts the commands it names. See [`Workspace::justless`].
    pub fn print(&self, days: u64, justless: &str) {
        let plural = |n: u64, one: &str, many: &str| {
            format!("{} {}", thousands(n), if n == 1 { one } else { many })
        };
        println!("\nRequest boundary (justless proxy), last {days} day(s)");
        println!(
            "  {} · clearing requested on {} · {} left to Claude Code's own clearing · {}",
            plural(self.requests, "request", "requests"),
            thousands(self.cleared_requests),
            plural(
                self.client_managed_conversations,
                "conversation",
                "conversations"
            ),
            plural(self.fallbacks, "refusal", "refusals"),
        );
        let moves = self.moves.cold + self.moves.payback;
        if moves == 0 {
            println!("  No old tool results cleared yet");
        } else {
            let mut kinds = Vec::new();
            if self.moves.cold > 0 {
                kinds.push(format!(
                    "{} on an expired cache",
                    thousands(self.moves.cold)
                ));
            }
            if self.moves.payback > 0 {
                kinds.push(format!(
                    "{} on a warm cache ({} tokens written to cache on {}, reported by the API)",
                    thousands(self.moves.payback),
                    short(self.warm_rewrite_tokens),
                    if self.moves.payback == 1 {
                        "that request"
                    } else {
                        "those requests"
                    }
                ));
            }
            println!(
                "  {} ({}, about {} tokens estimated) in {}: {}",
                plural(
                    self.newly_cleared.results,
                    "old tool result marked for clearing",
                    "old tool results marked for clearing"
                ),
                human_bytes(self.newly_cleared.bytes),
                short(self.newly_cleared.est_tokens),
                plural(moves, "clear", "clears"),
                kinds.join(", ")
            );
        }
        println!(
            "  {} input tokens not sent to the model, summed over requests as reported by the API",
            short(self.tokens_not_sent)
        );
        if self.repeats > 0 {
            println!(
                "  {} a cleared call with no file change in between",
                plural(self.repeats, "read repeated", "reads repeated")
            );
        }
        if self.mismatches > 0 {
            println!(
                "  {} where the API cleared a different number of calls than planned",
                plural(self.mismatches, "request", "requests")
            );
        }
        if self.unobserved > 0 {
            println!(
                "  {} relayed without being counted",
                plural(
                    self.unobserved,
                    "compressed response",
                    "compressed responses"
                )
            );
        }
        println!(
            "  {} ({}). `{justless} recall --list TEXT` finds one.",
            plural(
                self.archived_results,
                "tool result archived",
                "tool results archived"
            ),
            human_bytes(self.archived_bytes)
        );
    }
}

/// 1204 as `1,204`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A token count with three significant digits: `812`, `81.2K`, `100K`, `2.91M`.
pub fn short(n: u64) -> String {
    let scaled = |value: f64, suffix: &str| {
        let text = if value >= 100.0 {
            format!("{value:.0}")
        } else if value >= 10.0 {
            format!("{value:.1}")
        } else {
            format!("{value:.2}")
        };
        let text = if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text
        };
        format!("{text}{suffix}")
    };
    match n {
        0..1000 => n.to_string(),
        1000..999_500 => scaled(n as f64 / 1e3, "K"),
        _ => scaled(n as f64 / 1e6, "M"),
    }
}

impl From<&plan::Move> for Cleared {
    fn from(movement: &plan::Move) -> Self {
        Self {
            results: movement.results,
            bytes: movement.bytes,
            est_tokens: movement.tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_counts() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1204), "1,204");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(short(812), "812");
        assert_eq!(short(81_233), "81.2K");
        assert_eq!(short(100_000), "100K");
        assert_eq!(short(2_910_000), "2.91M");
        assert_eq!(short(1_500_000), "1.5M");
        assert_eq!(short(999_999), "1M");
    }

    #[test]
    fn reports_skip_months_before_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace {
            root: dir.path().to_owned(),
            cwd: dir.path().to_owned(),
            state: dir.path().join(".justless"),
            origin: dir.path().to_owned(),
        };
        let proxy = ledger_dir(&ws);
        fs::create_dir_all(&proxy).unwrap();
        let line = |ts: u64| {
            let record = Record {
                ts,
                path: "messages".into(),
                ..Default::default()
            };
            serde_json::to_string(&record).unwrap() + "\n"
        };
        let january = 1_767_571_200_000;
        let november = 1_793_836_800_000;
        fs::write(proxy.join("requests-2026-01.jsonl"), line(january)).unwrap();
        // An unreadable older month is never opened.
        fs::write(proxy.join("requests-2025-12.jsonl"), b"\xff not json").unwrap();
        fs::write(proxy.join("requests-2026-11.jsonl"), line(november)).unwrap();
        assert_eq!(report(&ws, november - 1).unwrap().unwrap().requests, 1);
        assert_eq!(report(&ws, january - 1).unwrap().unwrap().requests, 2);
    }

    #[test]
    fn summarizes_calls_on_one_short_line() {
        assert_eq!(
            call_summary("Bash", &json!({"command":"cargo  test\n --locked"})),
            "cargo test --locked"
        );
        assert_eq!(
            call_summary("Edit", &json!({"description":"d","file_path":"src/a.rs"})),
            "src/a.rs"
        );
        assert_eq!(call_summary("x", &json!({"n":1})), "{\"n\":1}");
        let long = call_summary("Task", &json!({"prompt":"é".repeat(300)}));
        assert_eq!(long.chars().count(), 161);
        // Searches lead with their pattern; partial reads show their range.
        assert_eq!(
            call_summary("Grep", &json!({"pattern":"parse_trade","path":"/repo/src"})),
            "parse_trade in /repo/src"
        );
        assert_eq!(
            call_summary("Glob", &json!({"pattern":"**/*_test.rs"})),
            "**/*_test.rs"
        );
        assert_eq!(
            call_summary(
                "Read",
                &json!({"file_path":"/repo/src/lib.rs","offset":900,"limit":200})
            ),
            "/repo/src/lib.rs:900+200"
        );
        assert_eq!(
            call_summary("Read", &json!({"file_path":"/repo/src/lib.rs"})),
            "/repo/src/lib.rs"
        );
    }
}
