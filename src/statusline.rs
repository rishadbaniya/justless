//! Claude Code status line: the current session's Justless savings on one line.
//!
//! The contract, from <https://code.claude.com/docs/en/statusline>:
//!
//! - Settings name it as `"statusLine": {"type": "command", "command": "…"}` in
//!   user or project settings. `padding`, `refreshInterval` and
//!   `hideVimModeIndicator` are optional.
//! - The command runs in a shell (Git Bash or PowerShell on Windows) when a
//!   session starts or resumes and again after each new assistant message, after
//!   `/compact` and on a few UI changes. Updates are debounced at 300 ms, and a
//!   new update cancels a command still running.
//! - The command receives session JSON on stdin, including `session_id`,
//!   `transcript_path`, `cwd`, `workspace.current_dir`, `workspace.project_dir`
//!   and `cost.total_duration_ms`. Fields can be absent or null early on.
//! - Claude Code shows what the command prints to stdout, one row per line, once
//!   it exits 0. A non-zero exit or empty output blanks the row. ANSI colors are
//!   allowed, but plain single-line text renders most reliably. `COLUMNS` holds
//!   the terminal width.
//!
//! Settings precedence, from <https://code.claude.com/docs/en/settings#settings-precedence>:
//! managed, command line, `.claude/settings.local.json`, `.claude/settings.json`,
//! then the user's `~/.claude/settings.json` (under `CLAUDE_CONFIG_DIR` when set).
//! A key set higher overrides the same key lower down. The local file holds
//! personal overrides for one project, and Claude Code skips a settings file with
//! invalid JSON after reporting it.
//!
//! Hooks, from <https://code.claude.com/docs/en/hooks>, run in the session's
//! current directory with `CLAUDE_PROJECT_DIR` set to the project root where the
//! session started.
use std::borrow::Cow;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use clap::Command;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::history::{net_bytes, now_ms};
use crate::tokens::Content;
use crate::workspace::{Workspace, lock_state, private_dir, write_text};

/// The command `init --statusline` installs. Other setup code matches it to tell
/// a Justless status line from the user's own.
pub(crate) const COMMAND: &str = "justless statusline";
/// What the status line prints when it cannot read the project.
const FALLBACK: &str = "justless";
/// How much of a transcript is read to find when its session began.
const TRANSCRIPT_SCAN: u64 = 1 << 20;
/// A run record is written once, when its command finishes. Records modified
/// this long before the session began are skipped without being read.
const RECORD_SLACK_MS: u64 = 60_000;
/// Session ledgers and root lists untouched for this long are removed when a new
/// one starts.
const LEDGER_DAYS: u64 = 30;
/// The settings file `init --statusline` writes, relative to the project root.
const LOCAL: &str = ".claude/settings.local.json";
/// How much of a session's root list is read.
const ROOTS_SCAN: u64 = 64 << 10;

pub fn command() -> Command<'static> {
    Command::new("statusline").about(
        "Print this session's estimated savings for Claude Code's status line (reads session JSON on stdin)",
    )
}

/// Prints one line and never fails: Claude Code blanks the status line on a
/// non-zero exit, so any error or panic prints the bare fallback instead.
pub fn run(root: Option<&Path>) {
    let input = read_input();
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|columns| columns.trim().parse::<usize>().ok())
        .filter(|&columns| columns > 0);
    let line = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        line(root, &input, now_ms(), width)
    }))
    .ok()
    .flatten()
    .unwrap_or_else(|| FALLBACK.into());
    let _ = writeln!(io::stdout().lock(), "{line}");
}

fn read_input() -> Value {
    let stdin = io::stdin();
    // Run by hand in a terminal, there is no session JSON to wait for.
    if stdin.is_terminal() {
        return Value::Null;
    }
    let mut bytes = Vec::new();
    if stdin.lock().take(1 << 20).read_to_end(&mut bytes).is_err() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// What this session's hooks kept out of context, and what came back later.
#[derive(Default)]
struct Tally {
    runs: u64,
    run_raw: u64,
    run_shown: u64,
    /// Bytes that later `log`, `search`, `context --run` and excerpt requests
    /// printed from the counted runs, measured or estimated as in `stats`.
    run_replayed: u64,
    reads: u64,
    read_raw: u64,
    read_shown: u64,
    searches: u64,
    search_raw: u64,
    search_shown: u64,
    /// Bytes of archived tool results that `justless recall` printed during the
    /// session, such as the original of a grouped Grep result.
    recalled: u64,
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

fn line(root: Option<&Path>, input: &Value, now: u64, width: Option<usize>) -> Option<String> {
    let digest = input["session_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(session_digest);
    let workspaces = workspaces(root, input, digest.as_deref());
    if workspaces.is_empty() {
        return None;
    }
    // Short-circuit cleanly when no discovered project has a `.justless/` state
    // directory: nothing can be tallied, so avoid reading transcripts or state.
    if !workspaces.iter().any(|(_, has_state)| *has_state) {
        return Some(render(&Tally::default(), width));
    }
    let start = session_start(input, now);
    let ledger_name = digest.as_deref().map(|d| format!("{d}.jsonl"));
    let mut tally = Tally::default();
    let mut path_buf = PathBuf::new();
    let mut read_buf = Vec::new();
    for (workspace, has_state) in &workspaces {
        if !has_state {
            continue;
        }
        if let Some(start) = start {
            tally.add_runs(workspace, start, &mut path_buf, &mut read_buf);
            let recalls = crate::history::archive_recalls(&workspace.state, start);
            tally.recalled = tally
                .recalled
                .saturating_add(recalls.bytes.saturating_sub(recalls.proxy_bytes));
        }
        if let Some(ledger_name) = &ledger_name {
            tally.add_tools(workspace, ledger_name, &mut path_buf, &mut read_buf);
        }
    }
    Some(render(&tally, width))
}

/// The projects whose runs this session may have recorded, paired with whether
/// their `.justless/` state directory exists. Hooks and routed commands store
/// runs in the project around their working directory, which can be a nested
/// repository the session moved into. So the session's current and launch
/// directories are read, and so is every nested project that this session's
/// runs and hooks listed in them, after the session has left it.
fn workspaces(root: Option<&Path>, input: &Value, digest: Option<&str>) -> Vec<(Workspace, bool)> {
    let mut starts: [&Path; 3] = [Path::new(""); 3];
    let mut start_count = 0;
    for raw in [
        &input["workspace"]["current_dir"],
        &input["cwd"],
        &input["workspace"]["project_dir"],
    ] {
        if let Some(dir) = raw.as_str().map(Path::new).filter(|dir| dir.is_absolute())
            && !starts[..start_count].contains(&dir)
        {
            starts[start_count] = dir;
            start_count += 1;
        }
    }
    let mut found: Vec<(Workspace, bool)> = Vec::new();
    if root.is_some() || start_count == 0 {
        if let Ok(workspace) = Workspace::discover(root) {
            let has_state = workspace.state.is_dir();
            found.push((workspace, has_state));
        }
    } else {
        for &start in &starts[..start_count] {
            if let Ok(workspace) = Workspace::discover_from(start)
                && !found.iter().any(|(known, _)| known.root == workspace.root)
            {
                let has_state = workspace.state.is_dir();
                found.push((workspace, has_state));
            }
        }
    }
    let Some(digest) = digest else {
        return found;
    };
    let initial = found.len();
    if !found[..initial].iter().any(|(_, has_state)| *has_state) {
        return found;
    }
    let roots_name = format!("{digest}.roots");
    let mut path_buf = PathBuf::new();
    let mut listed = String::new();
    for index in 0..initial {
        if !found[index].1 {
            continue;
        }
        path_buf.clear();
        path_buf.push(&found[index].0.state);
        path_buf.push("sessions");
        path_buf.push(&roots_name);
        let Ok(file) = File::open(&path_buf) else {
            continue;
        };
        listed.clear();
        if file.take(ROOTS_SCAN).read_to_string(&mut listed).is_err() {
            continue;
        }
        for line in listed.lines() {
            if line.is_empty() {
                continue;
            }
            let nested = Path::new(line);
            // A list names only projects inside the one that holds it.
            if nested == found[index].0.root
                || !nested.starts_with(&found[index].0.root)
                || found.iter().any(|(known, _)| known.root == nested)
            {
                continue;
            }
            if let Ok(workspace) = Workspace::discover(Some(nested))
                && !found.iter().any(|(known, _)| known.root == workspace.root)
            {
                let has_state = workspace.state.is_dir();
                found.push((workspace, has_state));
            }
        }
    }
    found
}

/// When the session began, in Unix milliseconds: the first timestamped record of
/// its transcript, else the transcript's creation time, else now minus the
/// session's running time. `None` when the input names none of them.
fn session_start(input: &Value, now: u64) -> Option<u64> {
    let transcript = input["transcript_path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .map(Path::new);
    transcript.and_then(transcript_start).or_else(|| {
        let duration = &input["cost"]["total_duration_ms"];
        let running = duration.as_u64().or_else(|| {
            let ms = duration.as_f64()?;
            (ms.is_finite() && ms >= 0.0).then_some(ms as u64)
        })?;
        Some(now.saturating_sub(running))
    })
}

/// The first top-level `timestamp` in a Claude Code JSONL transcript, falling
/// back to the transcript file's creation time. Its first few records, such as
/// the permission mode, carry none.
fn transcript_start(path: &Path) -> Option<u64> {
    #[derive(Deserialize)]
    struct Stamp<'a> {
        #[serde(borrow)]
        timestamp: Option<Cow<'a, str>>,
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => return fs::metadata(path).ok()?.created().ok().map(millis),
    };
    let mut reader = BufReader::new(file.take(TRANSCRIPT_SCAN));
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if !contains_bytes(&line, b"timestamp") {
            continue;
        }
        if let Ok(Stamp {
            timestamp: Some(text),
        }) = serde_json::from_slice::<Stamp<'_>>(&line)
            && let Some(ms) = parse_timestamp(&text)
        {
            return Some(ms);
        }
    }
    reader
        .into_inner()
        .into_inner()
        .metadata()
        .ok()?
        .created()
        .ok()
        .map(millis)
}

/// Parses an RFC 3339 timestamp such as `2026-11-17T15:42:21.427Z` into Unix
/// milliseconds.
fn parse_timestamp(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |from: usize, to: usize| -> Option<i64> {
        let digits = text.get(from..to)?;
        digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse().ok())?
    };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    let mut fraction = 0;
    if let Some(after) = rest.strip_prefix('.') {
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let kept = &after[..digits.min(3)];
        fraction = kept.parse::<i64>().ok()? * 10_i64.pow(3 - kept.len() as u32);
        rest = &after[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let zone = rest.get(1..)?;
            let zb = zone.as_bytes();
            if zb.len() != 5
                || zb[2] != b':'
                || !zb[0].is_ascii_digit()
                || !zb[1].is_ascii_digit()
                || !zb[3].is_ascii_digit()
                || !zb[4].is_ascii_digit()
            {
                return None;
            }
            let hours = i64::from((zb[0] - b'0') * 10 + (zb[1] - b'0'));
            let minutes = i64::from((zb[3] - b'0') * 10 + (zb[4] - b'0'));
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 3600 + minutes * 60)
        }
    };
    // Days from the civil date, for the proleptic Gregorian calendar.
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds.checked_mul(1000)? + fraction).ok()
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Tally {
    /// Adds hook-routed runs that finished after `start`. Run records do not
    /// name their session, so a concurrent session in the same project counts
    /// here too.
    fn add_runs(
        &mut self,
        workspace: &Workspace,
        start: u64,
        path: &mut PathBuf,
        buf: &mut Vec<u8>,
    ) {
        /// The fields a savings total needs, read without decoding the whole record.
        #[derive(Deserialize)]
        struct Cost {
            #[serde(default)]
            completed_ms: u64,
            #[serde(default)]
            raw_bytes: u64,
            #[serde(default)]
            preview_bytes: u64,
            #[serde(default)]
            footer_bytes: u64,
            #[serde(default)]
            hook: bool,
        }
        path.clear();
        path.push(&workspace.state);
        path.push("runs");
        let Ok(entries) = fs::read_dir(&*path) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|ft| ft.is_file()) {
                continue;
            }
            path.push(entry.file_name());
            path.push("record.json");
            let matched = (|| {
                let metadata = fs::metadata(&*path).ok()?;
                let modified = metadata.modified().map_or(0, millis);
                if modified.saturating_add(RECORD_SLACK_MS) < start {
                    return None;
                }
                buf.clear();
                File::open(&*path).ok()?.read_to_end(buf).ok()?;
                let cost: Cost = serde_json::from_slice(buf).ok()?;
                // Records written before completion times existed use their modification time.
                let completed = if cost.completed_ms > 0 {
                    cost.completed_ms
                } else {
                    modified
                };
                (cost.hook && completed >= start).then_some(cost)
            })();
            path.pop();
            if let Some(cost) = matched {
                self.runs = self.runs.saturating_add(1);
                self.run_raw = self.run_raw.saturating_add(cost.raw_bytes);
                self.run_shown = self
                    .run_shown
                    .saturating_add(cost.preview_bytes.saturating_add(cost.footer_bytes));
                self.run_replayed = self
                    .run_replayed
                    .saturating_add(crate::history::replayed(path, true).bytes());
            }
            path.pop();
        }
    }

    /// Adds the Read, Grep and Glob results this session's hooks replaced.
    fn add_tools(
        &mut self,
        workspace: &Workspace,
        ledger_name: &str,
        path: &mut PathBuf,
        buf: &mut Vec<u8>,
    ) {
        path.clear();
        path.push(&workspace.state);
        path.push("sessions");
        path.push(ledger_name);
        let Ok(file) = File::open(&*path) else {
            return;
        };
        let mut reader = BufReader::new(file.take(8 << 20));
        loop {
            buf.clear();
            match reader.read_until(b'\n', buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let Ok(entry) = serde_json::from_slice::<Replacement<'_>>(buf) else {
                continue;
            };
            if entry.tool == "Read" {
                self.reads = self.reads.saturating_add(1);
                self.read_raw = self.read_raw.saturating_add(entry.raw);
                self.read_shown = self.read_shown.saturating_add(entry.shown);
            } else {
                self.searches = self.searches.saturating_add(1);
                self.search_raw = self.search_raw.saturating_add(entry.raw);
                self.search_shown = self.search_shown.saturating_add(entry.shown);
            }
        }
    }

    fn total_raw(&self) -> u64 {
        self.run_raw
            .saturating_add(self.read_raw)
            .saturating_add(self.search_raw)
    }

    /// Savings net of what came back later, as in `stats`: everything the agent
    /// received counts, footers, replays and recalls included. Negative when the
    /// replays cost more than the previews saved.
    fn net_bytes(&self) -> i64 {
        let runs = net_bytes(self.run_raw, self.run_shown, self.run_replayed);
        let reads = net_bytes(self.read_raw, self.read_shown, 0);
        let searches = net_bytes(self.search_raw, self.search_shown, self.recalled);
        runs.saturating_add(reads).saturating_add(searches)
    }

    /// Estimated tokens saved at first preview, before replays and recalls:
    /// `Read` source bytes use the calibrated code ratio (2.57 B/tok) and
    /// command/search bytes use the logs ratio (2.21 B/tok), matching `stats`.
    fn initial_tokens(&self) -> i64 {
        let code = net_bytes(self.read_raw, self.read_shown, 0);
        let logs = net_bytes(self.run_raw, self.run_shown, 0).saturating_add(net_bytes(
            self.search_raw,
            self.search_shown,
            0,
        ));
        Content::Code
            .tokens_signed(code)
            .saturating_add(Content::Logs.tokens_signed(logs))
    }

    /// Estimated net tokens saved, after replays and recalls.
    fn net_tokens(&self) -> i64 {
        let code = net_bytes(self.read_raw, self.read_shown, 0);
        let logs = net_bytes(self.run_raw, self.run_shown, self.run_replayed)
            .saturating_add(net_bytes(self.search_raw, self.search_shown, self.recalled));
        Content::Code
            .tokens_signed(code)
            .saturating_add(Content::Logs.tokens_signed(logs))
    }

    /// Rounded whole-percent net byte reduction when the session saved bytes.
    #[cfg(test)]
    fn net_saved_percent(&self) -> Option<u64> {
        self.net_saved_percent_with(self.net_tokens())
    }

    fn net_saved_percent_with(&self, net_tokens: i64) -> Option<u64> {
        let raw = self.total_raw();
        let net = self.net_bytes();
        if raw == 0 || net <= 0 || net_tokens < 0 {
            return None;
        }
        let pct = (net as f64 * 100.0 / raw as f64).round();
        Some((pct as u64).min(100))
    }
}

/// Writes `n` with the noun in the singular or plural into `out`.
fn write_count(out: &mut String, n: u64, one: &str, many: &str) {
    let _ = write!(out, "{n} {}", if n == 1 { one } else { many });
}

/// Writes `850`, `41.2k` or `1.3M` into `out` without extra allocations.
fn write_short_number(out: &mut String, n: u64) {
    let write_scaled = |out: &mut String, value: f64, unit: char| {
        let start = out.len();
        let _ = write!(out, "{value:.1}");
        if out[start..].ends_with(".0") {
            out.truncate(out.len() - 2);
        }
        out.push(unit);
    };
    match n {
        0..1000 => {
            let _ = write!(out, "{n}");
        }
        1000..999_950 => write_scaled(out, n as f64 / 1e3, 'k'),
        _ => write_scaled(out, n as f64 / 1e6, 'M'),
    }
}

/// `850`, `41.2k` or `1.3M`.
#[cfg(test)]
fn short_number(n: u64) -> String {
    let mut out = String::new();
    write_short_number(&mut out, n);
    out
}

/// The status line, shortened from the end to fit `width` columns.
fn render(tally: &Tally, width: Option<usize>) -> String {
    if tally.runs == 0 && tally.reads == 0 && tally.searches == 0 {
        return "justless · ready".into();
    }
    let net = tally.net_tokens();
    let mut saved_plain = String::with_capacity(32);
    saved_plain.push_str("net ≈");
    write_short_number(&mut saved_plain, net.unsigned_abs());
    saved_plain.push_str(if net < 0 {
        " extra tokens"
    } else {
        " tokens saved"
    });
    let saved_pct = tally
        .net_saved_percent_with(net)
        .map(|pct| format!("{saved_plain} ({pct}%)"));
    let saved = saved_pct.as_deref().unwrap_or(&saved_plain);

    let mut counts: [(u64, &str, &str); 3] = [(0, "", ""); 3];
    let mut count_len = 0;
    if tally.runs > 0 {
        counts[count_len] = (tally.runs, "run", "runs");
        count_len += 1;
    }
    if tally.reads > 0 {
        counts[count_len] = (tally.reads, "read", "reads");
        count_len += 1;
    }
    if tally.searches > 0 {
        counts[count_len] = (tally.searches, "search", "searches");
        count_len += 1;
    }

    let fits = |text: &str| width.is_none_or(|limit| text.chars().count() <= limit);
    let mut candidate = String::with_capacity(128);

    if tally.run_replayed > 0 || tally.recalled > 0 {
        let initial = tally.initial_tokens();
        let mut initial_part = String::with_capacity(48);
        initial_part.push_str("justless · initially ≈");
        write_short_number(&mut initial_part, initial.unsigned_abs());
        initial_part.push_str(if initial < 0 {
            " tokens extra"
        } else {
            " tokens saved"
        });

        candidate.push_str(&initial_part);
        candidate.push_str(" · ");
        candidate.push_str(saved);
        let both_len = candidate.len();
        candidate.push_str(" this session");
        for &(n, one, many) in &counts[..count_len] {
            candidate.push_str(" · ");
            write_count(&mut candidate, n, one, many);
        }
        if fits(&candidate) {
            return candidate;
        }
        candidate.truncate(both_len);
        if fits(&candidate) {
            return candidate;
        }
        if saved_pct.is_some() {
            candidate.clear();
            candidate.push_str(&initial_part);
            candidate.push_str(" · ");
            candidate.push_str(&saved_plain);
            if fits(&candidate) {
                return candidate;
            }
        }
    }

    for kept in (0..=count_len).rev() {
        candidate.clear();
        candidate.push_str("justless · ");
        candidate.push_str(saved);
        candidate.push_str(" this session");
        for &(n, one, many) in &counts[..kept] {
            candidate.push_str(" · ");
            write_count(&mut candidate, n, one, many);
        }
        if fits(&candidate) {
            return candidate;
        }
    }
    if let Some(with_pct) = &saved_pct {
        candidate.clear();
        candidate.push_str("justless · ");
        candidate.push_str(with_pct);
        if fits(&candidate) {
            return candidate;
        }
    }
    candidate.clear();
    candidate.push_str("justless · ");
    candidate.push_str(&saved_plain);
    candidate
}

/// One Read, Grep or Glob result a hook replaced, as stored in a session ledger.
#[derive(Deserialize)]
struct Replacement<'a> {
    #[serde(borrow)]
    tool: Cow<'a, str>,
    raw: u64,
    shown: u64,
}

/// The digest that names a Claude Code session's files, the same one its hooks
/// pass to routed commands as `--session`.
fn session_digest(session: &str) -> String {
    crate::capture::source_digest(session.as_bytes())[..24].to_owned()
}

/// Each session's replacements are appended to their own file, named by its
/// digest, so a reader never needs a lock.
fn ledger(state: &Path, digest: &str) -> PathBuf {
    state.join("sessions").join(format!("{digest}.jsonl"))
}

/// The nested projects where a session recorded runs or replacements, one root
/// per line, kept in the project the session started in.
fn roots_file(state: &Path, digest: &str) -> PathBuf {
    state.join("sessions").join(format!("{digest}.roots"))
}

/// Appends `bytes` to a file in a state directory, creating it but never
/// following a link. One short write per call, so concurrent writers do not
/// interleave lines.
pub(crate) fn append(path: &Path, bytes: &[u8]) -> Option<()> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).ok()?.write_all(bytes).ok()
}

/// Records what a Claude Code hook kept out of context, so the status line can
/// count it toward the session that made the call. Errors are ignored: the
/// hook's answer matters more than the tally.
pub fn observe(workspace: &Workspace, mode: &str, input: &Value, output: &Value) {
    let Some(session) = input["session_id"].as_str().filter(|id| !id.is_empty()) else {
        return;
    };
    let digest = session_digest(session);
    match mode {
        "after-read" | "after-search" => {
            let _ = record(workspace, &digest, input, output);
        }
        // A routed command can record its run in a nested project, such as one it
        // enters with `cd`. Mark the session in the project it started in, where
        // that run then lists its project. Any rewrite marks it, whatever form the
        // routed command takes.
        "before-command" if output["hookSpecificOutput"]["updatedInput"]["command"].is_string() => {
            let launch = launch(workspace);
            let _ = mark(launch.as_ref().unwrap_or(workspace), &digest);
        }
        // A replaced Bash result is a hook-routed run, which the status line counts
        // from its record like any other. List the project that holds it where the
        // session started, as a routed command in a nested project does.
        "after-bash" if output["hookSpecificOutput"]["updatedToolOutput"].is_object() => {
            if let Some(launch) = launch(workspace) {
                let _ = mark(&launch, &digest);
            }
            register(&workspace.root, &digest);
        }
        _ => {}
    }
}

/// Logs one Read, Grep or Glob result that a hook replaced.
fn record(workspace: &Workspace, digest: &str, input: &Value, output: &Value) -> Option<()> {
    let tool = input["tool_name"].as_str()?;
    let hook = &output["hookSpecificOutput"];
    let updated = hook.get("updatedToolOutput")?;
    let raw = payload(tool, &input["tool_response"])?;
    let context = hook["additionalContext"].as_str().map_or(0, str::len) as u64;
    let shown = payload(tool, updated)? + context;
    if shown >= raw {
        return None;
    }
    let mut entry = json!({"tool":tool,"raw":raw,"shown":shown,"ms":now_ms()}).to_string();
    entry.push('\n');
    workspace.prepare_state().ok()?;
    let directory = workspace.state.join("sessions");
    private_dir(&directory).ok()?;
    let path = ledger(&workspace.state, digest);
    let new = fs::symlink_metadata(&path).is_err();
    append(&path, entry.as_bytes())?;
    if new {
        prune(&directory, &path);
    }
    // A hook that runs in a nested project logs there. List that project where
    // the session started, so the status line still finds it after a `cd` out.
    if let Some(launch) = launch(workspace) {
        let _ = mark(&launch, digest);
    }
    register(&workspace.root, digest);
    Some(())
}

/// The project the session started in, from the `CLAUDE_PROJECT_DIR` that
/// Claude Code gives its hooks, when it encloses `workspace` without being it.
fn launch(workspace: &Workspace) -> Option<Workspace> {
    let start = std::env::var_os("CLAUDE_PROJECT_DIR").filter(|dir| !dir.is_empty())?;
    Workspace::discover_from(Path::new(&start))
        .ok()
        .filter(|launch| launch.root != workspace.root && workspace.root.starts_with(&launch.root))
}

/// Creates the session's root list in the project it started in, so that runs
/// and hooks in nested projects can add themselves to it.
fn mark(launch: &Workspace, digest: &str) -> Option<()> {
    let path = roots_file(&launch.state, digest);
    if fs::symlink_metadata(&path).is_ok() {
        return Some(());
    }
    launch.prepare_state().ok()?;
    let directory = launch.state.join("sessions");
    private_dir(&directory).ok()?;
    append(&path, b"")?;
    prune(&directory, &path);
    Some(())
}

/// Adds `root` to the root list of this session in each enclosing project that
/// has one. Routed runs call it with their `--session` digest, so a run in a
/// nested repository counts for its session wherever the session moves next.
/// Errors are ignored: the run matters more than the tally.
pub fn register(root: &Path, digest: &str) {
    if digest.len() != 24 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return;
    }
    let Some(text) = root.to_str().filter(|text| !text.contains(['\n', '\r'])) else {
        return;
    };
    let roots_name = format!("{digest}.roots");
    let mut path = PathBuf::new();
    let mut listed = String::new();
    let mut entry_line = String::new();
    for ancestor in root.ancestors().skip(1) {
        path.clear();
        path.push(ancestor);
        path.push(".justless");
        path.push("sessions");
        path.push(&roots_name);
        if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        listed.clear();
        let read = File::open(&path)
            .and_then(|file| file.take(ROOTS_SCAN).read_to_string(&mut listed))
            .is_ok();
        if read && !listed.lines().any(|line| line == text) {
            if entry_line.is_empty() {
                entry_line.reserve(text.len() + 1);
                entry_line.push_str(text);
                entry_line.push('\n');
            }
            let _ = append(&path, entry_line.as_bytes());
        }
    }
}

/// The text a tool result gives the model: a Read's file content, a Grep's
/// matching lines, or the paths a Grep or Glob listed.
fn payload(tool: &str, value: &Value) -> Option<u64> {
    let text = match tool {
        "Read" => value["file"]["content"].as_str(),
        "Grep" if value["mode"] == "content" => value["content"].as_str(),
        "Grep" | "Glob" => {
            let names = value["filenames"].as_array()?;
            let bytes: usize = names.iter().filter_map(Value::as_str).map(str::len).sum();
            return Some((bytes + names.len().saturating_sub(1)) as u64);
        }
        _ => None,
    }?;
    Some(text.len() as u64)
}

fn prune(directory: &Path, keep: &Path) {
    let Some(cutoff) =
        SystemTime::now().checked_sub(std::time::Duration::from_secs(LEDGER_DAYS * 86_400))
    else {
        return;
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path != keep
            && path
                .extension()
                .is_some_and(|ext| ext == "jsonl" || ext == "roots")
            && entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified < cutoff)
        {
            let _ = fs::remove_file(path);
        }
    }
}

/// The user settings file Claude Code reads: `$CLAUDE_CONFIG_DIR/settings.json`,
/// else `~/.claude/settings.json`, where `~` is `%USERPROFILE%` on Windows.
fn user_settings() -> Option<PathBuf> {
    let variable = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    variable("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let home = variable("HOME").or_else(|| variable("USERPROFILE"))?;
            Some(PathBuf::from(home).join(".claude"))
        })
        .map(|dir| dir.join("settings.json"))
}

/// A settings file as JSON, or `None` when it does not exist. A link is followed
/// only when `follow` is set: setup rewrites the local file, which would replace
/// a link with a copy.
fn read_settings(path: &Path, follow: bool) -> Result<Option<Value>> {
    let text = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || (follow && metadata.file_type().is_symlink()) => {
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
        }
        Ok(_) => anyhow::bail!("{} must be a regular file", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Some(json!({})));
    }
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("decode {}", path.display()))?;
    ensure!(
        value.is_object(),
        "{} must contain an object",
        path.display()
    );
    Ok(Some(value))
}

/// `path` with its longest existing ancestor resolved, so two spellings of a
/// file that does not exist yet still compare equal.
fn resolved(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(real) = ancestor.canonicalize() {
            return match path.strip_prefix(ancestor) {
                Ok(rest) => real.join(rest),
                Err(_) => real,
            };
        }
    }
    path.to_path_buf()
}

/// What `init --statusline` would do.
enum Plan {
    /// Files to write, with their new contents: the project's local settings, and
    /// `.gitignore` when it must list them.
    Write(Vec<(PathBuf, String)>),
    /// A Justless status line already applies here, from the named file.
    Present(String),
    /// Another status line applies here: the file that sets it, its `statusLine`
    /// value, and whether that file is the local one setup would write.
    Kept {
        label: String,
        existing: Value,
        local: bool,
    },
}

/// Read-only audit of Claude Code's `statusLine` setting for `workspace`,
/// checked in Claude Code's settings precedence order (local, project, user).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Audit {
    /// A Justless status line applies, from `label`.
    Installed { label: String },
    /// Another status line in `label` takes precedence (optionally shadowing a
    /// lower-precedence Justless status line in `shadowed`).
    Shadowed {
        label: String,
        shadowed: Option<String>,
    },
    /// No local, project or user settings file configures `statusLine`.
    Unconfigured,
    /// A settings file ahead of any `statusLine` could not be read or decoded.
    Unreadable { label: String, error: String },
}

/// Checks Claude Code's local, project and user settings for `statusLine`
/// without writing anything.
pub(crate) fn audit(workspace: &Workspace) -> Audit {
    let directory = workspace.root.join(".claude");
    let project = directory.join("settings.json");
    let local = workspace.root.join(LOCAL);
    let mut sources = vec![
        (local, LOCAL.to_owned()),
        (project.clone(), ".claude/settings.json".to_owned()),
    ];
    if let Some(user) = user_settings()
        && resolved(&project) != resolved(&user)
    {
        let label = user.display().to_string();
        sources.push((user, label));
    }
    let mut winner: Option<(String, bool)> = None;
    let mut shadowed_justless: Option<String> = None;
    for (path, label) in sources {
        let settings = match read_settings(&path, true) {
            Ok(Some(settings)) => settings,
            Ok(None) => continue,
            Err(error) if winner.is_none() => {
                return Audit::Unreadable {
                    label,
                    error: format!("{error:#}"),
                };
            }
            Err(_) => continue,
        };
        let existing = &settings["statusLine"];
        if existing.is_null() {
            continue;
        }
        let is_justless = existing["command"]
            .as_str()
            .is_some_and(|command| command.contains(COMMAND));
        if winner.is_none() {
            if is_justless {
                return Audit::Installed { label };
            }
            winner = Some((label, false));
        } else if is_justless && shadowed_justless.is_none() {
            shadowed_justless = Some(label);
        }
    }
    match winner {
        Some((label, _)) => Audit::Shadowed {
            label,
            shadowed: shadowed_justless,
        },
        None => Audit::Unconfigured,
    }
}

/// Checks every file `init --statusline` reads, so that setup can stop before it
/// changes anything else.
pub fn check(workspace: &Workspace) -> Result<()> {
    plan(workspace).map(|_| ())
}

fn plan(workspace: &Workspace) -> Result<Plan> {
    let directory = workspace.root.join(".claude");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) => ensure!(metadata.is_dir(), ".claude must be a directory"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let project = directory.join("settings.json");
    let local = workspace.root.join(LOCAL);
    let user = user_settings();
    // In the home directory the project file is the user's own settings file,
    // and its local file would apply to every session started there.
    ensure!(
        user.as_deref()
            .is_none_or(|user| resolved(&project) != resolved(user)),
        "{} is your user settings file. Run `justless init --statusline` inside a project folder.",
        project.display()
    );
    let mut sources = vec![
        (local.clone(), LOCAL.to_owned()),
        (project, ".claude/settings.json".to_owned()),
    ];
    if let Some(user) = user {
        let label = user.display().to_string();
        sources.push((user, label));
    }
    // The highest-precedence file that sets a status line decides which one shows.
    // A file that cannot be read may set one, so setup stops rather than hide it.
    for (index, (path, label)) in sources.iter().enumerate() {
        let Some(settings) = read_settings(path, true).with_context(|| {
            format!("cannot tell whether {label} sets a status line, so nothing was changed")
        })?
        else {
            continue;
        };
        let existing = &settings["statusLine"];
        if existing.is_null() {
            continue;
        }
        if existing["command"]
            .as_str()
            .is_some_and(|command| command.contains(COMMAND))
        {
            return Ok(Plan::Present(label.clone()));
        }
        return Ok(Plan::Kept {
            label: label.clone(),
            existing: existing.clone(),
            local: index == 0,
        });
    }
    let created = read_settings(&local, false)?;
    let mut writes = Vec::new();
    if created.is_none()
        && let Some(ignore) = ignore_local(workspace)?
    {
        writes.push(ignore);
    }
    let mut settings = created.unwrap_or_else(|| json!({}));
    settings
        .as_object_mut()
        .context("settings must contain an object")?
        .insert(
            "statusLine".into(),
            json!({"type":"command","command":COMMAND}),
        );
    writes.insert(
        0,
        (
            local,
            format!("{}\n", serde_json::to_string_pretty(&settings)?),
        ),
    );
    Ok(Plan::Write(writes))
}

/// `.gitignore` with the local settings file added, when setup is about to create
/// that file in a Git repository that does not ignore it yet. Claude Code keeps
/// out of Git only the local file it creates itself. Nothing is changed outside
/// the project, such as Git's global excludes.
fn ignore_local(workspace: &Workspace) -> Result<Option<(PathBuf, String)>> {
    // `check-ignore` takes no pathspec magic, so `Workspace::git` cannot run it.
    let Ok(output) = std::process::Command::new("git")
        .args(["check-ignore", "-q", "--", LOCAL])
        .current_dir(&workspace.root)
        .output()
    else {
        return Ok(None);
    };
    // 0 means ignored already, and 128 outside a repository.
    if output.status.code() != Some(1) {
        return Ok(None);
    }
    let path = workspace.root.join(".gitignore");
    let original = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "{} must be a regular file",
                path.display()
            );
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let separator = if original.is_empty() || original.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    Ok(Some((path, format!("{original}{separator}/{LOCAL}\n"))))
}

/// A command that prints the user's status line and then the Justless one, each
/// on its own row, from the same session JSON. A newline rather than `;` ends the
/// user's command, so one that ends in `;`, `&` or a `#` comment still parses.
fn combined(existing: Option<&str>) -> String {
    let existing = existing.unwrap_or("YOUR_COMMAND");
    format!(
        "input=$(cat); printf '%s' \"$input\" | {{\n{existing}\n}}; printf '%s' \"$input\" | {COMMAND}"
    )
}

/// Whether the Claude Code hooks that record savings are set in the local,
/// project or user settings. Unreadable files count as having none.
fn hooks_installed(workspace: &Workspace) -> bool {
    let directory = workspace.root.join(".claude");
    [
        Some(directory.join("settings.local.json")),
        Some(directory.join("settings.json")),
        user_settings(),
    ]
    .into_iter()
    .flatten()
    .any(|path| {
        read_settings(&path, true)
            .ok()
            .flatten()
            .is_some_and(|settings| {
                settings["hooks"]
                    .to_string()
                    .contains("justless hook-claude")
            })
    })
}

/// Adds the Justless status line to the project's local Claude Code settings,
/// unless a status line already applies here. Then it leaves every file as it
/// was and says how to show both.
pub fn install(workspace: &Workspace) -> Result<()> {
    // Check the files before creating the state directory. A write is planned
    // again under the setup lock that `init` takes; anything else changes nothing.
    let mut planned = plan(workspace)?;
    let _guard = if matches!(planned, Plan::Write(..)) {
        workspace.prepare_state()?;
        let guard = lock_state(&workspace.state.join("setup.lock"))?;
        planned = plan(workspace)?;
        Some(guard)
    } else {
        None
    };
    match planned {
        Plan::Write(writes) => {
            for (path, contents) in writes {
                if let Some(parent) = path.parent() {
                    private_dir(parent)?;
                }
                write_text(&path, &contents)?;
                println!("Updated {}", path.display());
            }
            println!(
                "Claude Code's status line now shows this session's Justless savings, for you in this project. It appears once the folder is trusted."
            );
        }
        Plan::Present(label) => {
            println!("{label} already shows Justless savings in the status line.");
        }
        Plan::Kept {
            label,
            existing,
            local: true,
        } => {
            eprintln!(
                "Kept the status line set in {label}. Claude Code shows each line a status line command prints as its own row, so to show both, set its \"command\" to:\n  {}",
                serde_json::to_string(&combined(existing["command"].as_str()))?
            );
        }
        // Changing a shared or user status line would add the Justless row for
        // everyone in the project or in every project, so suggest a copy for the
        // local file, which overrides it here only. Its other fields, such as
        // `padding` or `refreshInterval`, carry over.
        Plan::Kept {
            label,
            existing,
            local: false,
        } => {
            let mut setting = if existing.is_object() {
                existing.clone()
            } else {
                json!({"type":"command"})
            };
            setting["command"] = json!(combined(existing["command"].as_str()));
            eprintln!(
                "Kept the status line set in {label}. Claude Code shows each line a status line command prints as its own row, so to show both in this project, for you only, add this to {LOCAL}:\n  {}",
                json!({ "statusLine": setting })
            );
        }
    }
    if !hooks_installed(workspace) {
        println!(
            "Savings appear once the hooks route commands and reads: run `{} init --hooks --agent claude`.",
            workspace.justless()
        );
    }
    Ok(())
}

/// The project's Claude Code settings as stored, to tell whether setup changed them.
pub fn project_settings(workspace: &Workspace) -> Option<Vec<u8>> {
    fs::read(workspace.root.join(".claude").join("settings.json")).ok()
}

/// Mentions the status line when setup has just changed the project's Claude Code
/// settings, given their contents before, unless the project configures one.
/// Setup that changes nothing stays silent.
pub fn hint(workspace: &Workspace, before: Option<Vec<u8>>) {
    if project_settings(workspace) == before {
        return;
    }
    let directory = workspace.root.join(".claude");
    let configured = ["settings.json", "settings.local.json"].iter().any(|file| {
        read_settings(&directory.join(file), true)
            .ok()
            .flatten()
            .is_some_and(|settings| !settings["statusLine"].is_null())
    });
    if !configured {
        println!(
            "Optional: `{} init --statusline` shows this session's savings in Claude Code's status line.",
            workspace.justless()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_to_unix_milliseconds() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp("2026-11-17T15:42:21.427Z"),
            Some(1_794_930_141_427)
        );
        assert_eq!(
            parse_timestamp("2026-11-17T17:42:21.4+02:00"),
            Some(1_794_930_141_400)
        );
        assert_eq!(
            parse_timestamp("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
        for bad in [
            "",
            "2026-13-01T00:00:00Z",
            "2026-11-17 15:42",
            "2026-11-17T15:42:21",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
    }

    #[test]
    fn lines_shorten_to_the_terminal_width() {
        let tally = Tally {
            runs: 23,
            run_raw: 200_000,
            run_shown: 35_200,
            reads: 1,
            searches: 2,
            ..Tally::default()
        };
        assert_eq!(
            render(&tally, None),
            "justless · net ≈74.7k tokens saved (82%) this session · 23 runs · 1 read · 2 searches"
        );
        assert_eq!(
            render(&tally, Some(72)),
            "justless · net ≈74.7k tokens saved (82%) this session · 23 runs · 1 read"
        );
        assert_eq!(
            render(&tally, Some(71)),
            "justless · net ≈74.7k tokens saved (82%) this session · 23 runs"
        );
        assert_eq!(
            render(&tally, Some(53)),
            "justless · net ≈74.7k tokens saved (82%) this session"
        );
        assert_eq!(
            render(&tally, Some(40)),
            "justless · net ≈74.7k tokens saved (82%)"
        );
        assert_eq!(
            render(&tally, Some(34)),
            "justless · net ≈74.7k tokens saved"
        );
        assert_eq!(
            render(&tally, Some(30)),
            "justless · net ≈74.7k tokens saved"
        );
        assert_eq!(render(&Tally::default(), Some(5)), "justless · ready");
        assert_eq!(short_number(999), "999");
        assert_eq!(short_number(3_000), "3k");
        assert_eq!(short_number(999_949), "999.9k");
        assert_eq!(short_number(1_260_000), "1.3M");
    }

    #[test]
    fn reads_use_code_token_ratio_and_searches_use_logs_ratio() {
        // 95,000 saved bytes of source code: 37,035 tokens at 2.57 B/tok (Code),
        // rather than 43,052 tokens at 2.21 B/tok (Logs, +16%).
        let read_only = Tally {
            reads: 2,
            read_raw: 100_000,
            read_shown: 5_000,
            ..Tally::default()
        };
        assert_eq!(read_only.net_tokens(), Content::Code.tokens_signed(95_000));
        assert_eq!(read_only.net_saved_percent(), Some(95));
        assert_eq!(
            render(&read_only, None),
            "justless · net ≈37k tokens saved (95%) this session · 2 reads"
        );

        // Mixed session splits Read bytes (Code) from run/search bytes (Logs).
        let mixed = Tally {
            runs: 3,
            run_raw: 50_000,
            run_shown: 10_000,
            reads: 1,
            read_raw: 40_000,
            read_shown: 4_000,
            searches: 2,
            search_raw: 10_000,
            search_shown: 2_000,
            ..Tally::default()
        };
        let expected_tokens =
            Content::Code.tokens_signed(36_000) + Content::Logs.tokens_signed(40_000 + 8_000);
        assert_eq!(mixed.net_tokens(), expected_tokens);
        assert_eq!(mixed.net_saved_percent(), Some(84));
        assert_eq!(
            render(&mixed, None),
            format!(
                "justless · net ≈{} tokens saved (84%) this session · 3 runs · 1 read · 2 searches",
                short_number(expected_tokens.unsigned_abs())
            )
        );

        // Zero net savings omits the percentage suffix cleanly.
        let verbatim = Tally {
            runs: 1,
            run_raw: 500,
            run_shown: 500,
            ..Tally::default()
        };
        assert_eq!(verbatim.net_saved_percent(), None);
        assert_eq!(
            render(&verbatim, None),
            "justless · net ≈0 tokens saved this session · 1 run"
        );
    }

    #[test]
    fn replays_and_recalls_come_off_the_session_savings() {
        // A 19,000-byte output shown in 364 bytes, then replayed whole: the
        // session received 364 bytes more than running the command directly.
        let replayed = Tally {
            runs: 1,
            run_raw: 19_000,
            run_shown: 364,
            run_replayed: 19_000,
            ..Tally::default()
        };
        assert_eq!(replayed.net_bytes(), -364);
        assert_eq!(replayed.net_saved_percent(), None);
        assert_eq!(
            render(&replayed, None),
            "justless · initially ≈8.4k tokens saved · net ≈165 extra tokens this session · 1 run"
        );
        // A grouped Grep result recalled in part keeps the rest of its saving.
        let recalled = Tally {
            searches: 1,
            search_raw: 10_000,
            search_shown: 1_000,
            recalled: 4_000,
            ..Tally::default()
        };
        assert_eq!(recalled.net_bytes(), 5_000);
        assert_eq!(recalled.net_saved_percent(), Some(50));
        assert_eq!(
            render(&recalled, None),
            "justless · initially ≈4.1k tokens saved · net ≈2.3k tokens saved (50%) this session · 1 search"
        );
    }

    #[test]
    fn audit_checks_local_and_project_settings_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = Workspace::discover_from(&root).unwrap();
        assert_eq!(audit(&workspace), Audit::Unconfigured);
        assert!(!root.join(".justless").exists());

        let claude = root.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let project = claude.join("settings.json");
        let local = claude.join("settings.local.json");

        fs::write(
            &project,
            r#"{"statusLine":{"type":"command","command":"justless statusline"}}"#,
        )
        .unwrap();
        assert_eq!(
            audit(&workspace),
            Audit::Installed {
                label: ".claude/settings.json".into()
            }
        );

        // Local settings take precedence over project settings.
        fs::write(
            &local,
            r#"{"statusLine":{"type":"command","command":"echo custom"}}"#,
        )
        .unwrap();
        assert_eq!(
            audit(&workspace),
            Audit::Shadowed {
                label: LOCAL.into(),
                shadowed: Some(".claude/settings.json".into()),
            }
        );

        // Combined command in local settings counts as installed.
        fs::write(
            &local,
            r#"{"statusLine":{"type":"command","command":"input=$(cat); printf '%s' \"$input\" | justless statusline"}}"#,
        )
        .unwrap();
        assert_eq!(
            audit(&workspace),
            Audit::Installed {
                label: LOCAL.into()
            }
        );

        // Unreadable/malformed JSON ahead of project settings is flagged.
        fs::write(&local, "{").unwrap();
        assert!(matches!(
            audit(&workspace),
            Audit::Unreadable { label, .. } if label == LOCAL
        ));
        assert!(!root.join(".justless").exists());
    }

    #[test]
    fn single_pass_tally_short_circuits_without_state_and_aggregates_nested_projects() {
        let temp = tempfile::tempdir().unwrap();
        let launch = temp.path().canonicalize().unwrap();
        fs::write(launch.join("Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();

        // 1. Without `.justless/`, `line()` short-circuits cleanly to ready
        //    without opening the transcript or creating any state files.
        let missing_transcript = launch.join("does-not-exist.jsonl");
        let input = json!({
            "session_id": "sess-1",
            "transcript_path": missing_transcript,
            "cwd": launch,
            "workspace": {"current_dir": launch, "project_dir": launch},
        });
        assert_eq!(
            line(None, &input, 2_000_000, None).as_deref(),
            Some("justless · ready")
        );
        assert!(!launch.join(".justless").exists());

        // 2. With launch + nested projects (one with state, one without) plus
        //    corrupt/empty lines in `.roots`, `.jsonl` ledgers, and `record.json`,
        //    single-pass aggregation totals every valid run and tool replacement.
        let nested_active = launch.join("pkg-active");
        let nested_empty = launch.join("pkg-empty");
        fs::create_dir_all(nested_active.join(".git")).unwrap();
        fs::create_dir_all(nested_empty.join(".git")).unwrap();

        let digest = session_digest("sess-1");
        let start = 1_767_323_045_678u64;
        let transcript = launch.join("transcript.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"permission-mode\"}\n\
             not json\n\
             {\"type\":\"user\",\"timestamp\":\"2026-01-02T03:04:05.678Z\"}\n",
        )
        .unwrap();

        // Launch project: one hook run + one corrupt run + one Read replacement.
        let launch_runs = launch.join(".justless/runs");
        fs::create_dir_all(launch_runs.join("run-1")).unwrap();
        fs::write(
            launch_runs.join("run-1/record.json"),
            json!({"completed_ms": start + 500, "raw_bytes": 20_000, "preview_bytes": 1_952, "footer_bytes": 48, "hook": true}).to_string(),
        )
        .unwrap();
        fs::create_dir_all(launch_runs.join("run-corrupt")).unwrap();
        fs::write(launch_runs.join("run-corrupt/record.json"), "{broken").unwrap();
        fs::write(launch_runs.join("stray-file"), "ignore").unwrap();

        let launch_sessions = launch.join(".justless/sessions");
        fs::create_dir_all(&launch_sessions).unwrap();
        fs::write(
            launch_sessions.join(format!("{digest}.jsonl")),
            "\nnot-json\n{\"tool\":\"Read\",\"raw\":50000,\"shown\":5000}\n   \n",
        )
        .unwrap();
        fs::write(
            launch_sessions.join(format!("{digest}.roots")),
            format!(
                "\n{}\n{}\n{}\n/outside/path\n",
                nested_active.display(),
                nested_active.display(),
                nested_empty.display()
            ),
        )
        .unwrap();

        // Active nested project: one hook run + one Grep replacement.
        let nested_runs = nested_active.join(".justless/runs/run-2");
        fs::create_dir_all(&nested_runs).unwrap();
        fs::write(
            nested_runs.join("record.json"),
            json!({"completed_ms": start + 900, "raw_bytes": 10_000, "preview_bytes": 952, "footer_bytes": 48, "hook": true}).to_string(),
        )
        .unwrap();
        let nested_sessions = nested_active.join(".justless/sessions");
        fs::create_dir_all(&nested_sessions).unwrap();
        fs::write(
            nested_sessions.join(format!("{digest}.jsonl")),
            "{\"tool\":\"Grep\",\"raw\":12000,\"shown\":2000}\n",
        )
        .unwrap();

        let active_input = json!({
            "session_id": "sess-1",
            "transcript_path": transcript,
            "cwd": launch,
            "workspace": {"current_dir": launch, "project_dir": launch},
        });
        let expected_tally = Tally {
            runs: 2,
            run_raw: 30_000,
            run_shown: 3_000,
            reads: 1,
            read_raw: 50_000,
            read_shown: 5_000,
            searches: 1,
            search_raw: 12_000,
            search_shown: 2_000,
            ..Tally::default()
        };
        assert_eq!(
            line(None, &active_input, start + 5_000, None).as_deref(),
            Some(render(&expected_tally, None).as_str())
        );
        // Never creates state in nested projects that had none.
        assert!(!nested_empty.join(".justless").exists());
        // Invalid timezone offsets are rejected cleanly.
        assert_eq!(parse_timestamp("2026-11-17T15:42:21+25:00"), None);
        assert_eq!(parse_timestamp("2026-11-17T15:42:21+-1:00"), None);
    }

    #[cfg(unix)]
    #[test]
    fn transcript_start_falls_back_to_created_on_read_error() {
        let temp = tempfile::tempdir().unwrap();
        // Opening a directory succeeds on Unix and `read_until` fails with EISDIR;
        // `transcript_start` must break into the file metadata `created()` fallback.
        let expected = fs::metadata(temp.path())
            .ok()
            .and_then(|meta| meta.created().ok())
            .map(millis);
        assert_eq!(transcript_start(temp.path()), expected);
    }
}
