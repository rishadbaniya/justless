//! Find shell commands that agents ran in this workspace without Justless.
//!
//! Scans local session transcripts read-only across Claude Code, Codex
//! ([`crate::rollouts`]), Antigravity CLI ([`crate::agy_sessions`]), Gemini CLI,
//! OpenCode, Cursor, Cline, and Roo Code when present. Only command labels such
//! as `cargo test` and byte counts are reported — never full command lines,
//! arguments, secrets, or raw output text.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::history::{estimated_tokens, human_bytes, now_ms, utc_date};
use crate::rewrite::Exclusions;
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Route {
    ThroughJustless,
    SupportedNotRouted,
    /// A supported command that the Claude Code hook leaves as written under the
    /// current permission rules and the session's mode.
    LeftByPermissions,
    /// A supported command that the project's `exclude_commands` keeps from the
    /// hooks, or for Claude Code a supported tool in a pass-through form or a
    /// read, which it keeps from the post-run hook.
    Excluded,
    /// A command the Claude Code post-run hook would compact after it ran: a
    /// read, a read list, a supported tool in a pass-through form over the
    /// context budget, or an unsupported command over its floor.
    CompactedAfterRunning,
    /// A supported tool in a form the hooks pass through whole, such as a pipe,
    /// or a read the post-run hook keeps whole because it is small.
    PassedThrough,
    NotSupported,
}

impl Route {
    fn title(self) -> &'static str {
        match self {
            Self::ThroughJustless => "Through Justless",
            Self::SupportedNotRouted => "Supported, not routed",
            Self::LeftByPermissions => "Supported, left to Claude Code's permission rules",
            Self::Excluded => "Supported, excluded by exclude_commands",
            Self::CompactedAfterRunning => "Compacted after running",
            Self::PassedThrough => "Supported, passed through whole",
            Self::NotSupported => "Not supported yet",
        }
    }

    /// How a hook that routes a command when `routes` says so treats a command
    /// that did not go through Justless, under the project's `exclude_commands`:
    /// `SupportedNotRouted` when it would route it now. With `post_run`, the
    /// client's post-run hook compacts the forms the command hook passes through,
    /// unless an entry names them.
    pub(crate) fn unrouted(
        command: &str,
        label: &str,
        exclusions: &Exclusions,
        post_run: bool,
        routes: impl Fn(&str, &Exclusions) -> bool,
    ) -> Self {
        if command_disables_justless(command) {
            return Self::Excluded;
        }
        let none = Exclusions::default();
        if routes(command, exclusions) {
            Self::SupportedNotRouted
        } else if routes(command, &none) {
            Self::Excluded
        } else if !routes(label, &none)
            && !has_routable_segment(command, &routes)
            && !has_supported_tool_prefix(command, &routes)
        {
            Self::NotSupported
        } else if post_run && excluded_stage(exclusions, command) {
            Self::Excluded
        } else {
            Self::PassedThrough
        }
    }
}

fn is_shell_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn disables_justless_assignment(word: &str) -> bool {
    word.strip_prefix("JUSTLESS_DISABLE=")
        .is_some_and(|value| !value.is_empty() && value != "0")
}

/// Whether `command` explicitly disables Justless via a leading
/// `JUSTLESS_DISABLE=<non-empty, non-0>` assignment, `export`, or wrapper
/// (`env`, `time`, `nohup`, `command`, `nice`, `timeout`) in any top-level
/// shell stage. Commands that merely mention `JUSTLESS_DISABLE=1` as a data
/// argument (such as `rg 'JUSTLESS_DISABLE=1' src`) or set `JUSTLESS_DISABLE=0`
/// return `false`.
pub(crate) fn command_disables_justless(command: &str) -> bool {
    if !command.contains("JUSTLESS_DISABLE=") {
        return false;
    }
    split_shell_stages(command).into_iter().any(|stage| {
        let Some(words) = shlex::split(stage) else {
            return false;
        };
        let mut rest: &[String] = &words;
        loop {
            let Some(first) = rest.first() else {
                return false;
            };
            if disables_justless_assignment(first) {
                return true;
            }
            rest = match first.as_str() {
                word if is_shell_assignment(word) => &rest[1..],
                "env" => {
                    let mut i = 1;
                    while let Some(arg) = rest.get(i) {
                        if arg == "--" {
                            i += 1;
                            break;
                        } else if matches!(arg.as_str(), "-u" | "--unset" | "-C" | "--chdir") {
                            i += 2;
                        } else if arg.starts_with('-') {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    rest.get(i..).unwrap_or(&[])
                }
                "export" | "time" | "nohup" => &rest[1..],
                "command" if rest.get(1).is_some_and(|w| !w.starts_with('-')) => &rest[1..],
                "nice" => match rest.get(1).map(String::as_str) {
                    Some("-n") => rest.get(3..).unwrap_or(&[]),
                    Some(w) if w.starts_with('-') => rest.get(2..).unwrap_or(&[]),
                    _ => &rest[1..],
                },
                "timeout" => {
                    let mut i = 1;
                    while let Some(option) = rest.get(i).filter(|w| w.starts_with('-')) {
                        i += if matches!(option.as_str(), "-s" | "--signal" | "-k" | "--kill-after")
                        {
                            2
                        } else {
                            1
                        };
                    }
                    rest.get(i + 1..).unwrap_or(&[])
                }
                _ => return false,
            };
        }
    })
}

/// Split `command` into trimmed top-level stages on `\n`, `;`, `&&`, `||`, `|&`,
/// `|`, and `&` outside single/double quotes and backslash escapes, keeping fd
/// duplications like `2>&1` attached to their stage so single-quoted regexes
/// (`'foo|bar'`) and merged pipes (`cmd 2>&1 | head`) stay intact.
fn split_shell_stages(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut stages = Vec::new();
    let mut start = 0;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(b'\'') => {
                if b == b'\'' {
                    quote = None;
                }
                i += 1;
            }
            Some(b'"') => {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    if b == b'"' {
                        quote = None;
                    }
                    i += 1;
                }
            }
            Some(_) => i += 1,
            None => match b {
                b'\\' if i + 1 < bytes.len() => i += 2,
                b'\'' | b'"' => {
                    quote = Some(b);
                    i += 1;
                }
                // Keep fd duplications (`2>&1`, `1>&2`) inside the current stage.
                b'>' if bytes.get(i + 1) == Some(&b'&')
                    && bytes.get(i + 2).is_some_and(u8::is_ascii_digit) =>
                {
                    i += 3;
                }
                b'\n' | b';' => {
                    let part = command[start..i].trim();
                    if !part.is_empty() {
                        stages.push(part);
                    }
                    i += 1;
                    start = i;
                }
                b'&' | b'|' => {
                    let part = command[start..i].trim();
                    if !part.is_empty() {
                        stages.push(part);
                    }
                    if bytes.get(i + 1) == Some(&b)
                        || (b == b'|' && bytes.get(i + 1) == Some(&b'&'))
                    {
                        i += 2;
                    } else {
                        i += 1;
                    }
                    start = i;
                }
                _ => i += 1,
            },
        }
    }
    let tail = command[start..].trim();
    if !tail.is_empty() {
        stages.push(tail);
    }
    stages
}

/// Whether any top-level segment of `command` would be routed on its own. This
/// keeps chains such as `cargo fmt --all -- --check && python3 x.py`,
/// `ls -la | grep foo`, or `rg 'foo|bar' src | wc -l` in `PassedThrough` when
/// the bare command label alone (`cargo fmt`, `ls`, `rg`) is not routed.
fn has_routable_segment(command: &str, routes: &impl Fn(&str, &Exclusions) -> bool) -> bool {
    let none = Exclusions::default();
    split_shell_stages(command)
        .into_iter()
        .any(|part| routes(part, &none))
}

/// Whether any top-level stage starts with a multi-word supported command prefix
/// (such as `kubectl get events`, `kubectl get svc`, `python3 -m unittest`,
/// `docker compose ps`, `podman image ls`, or `gh pr list`) behind optional
/// assignments and wrappers (`timeout`, `nice`, `stdbuf`, `env`). Keeps watch
/// modes and machine formats (`-w`, `-o yaml`, `--format json`) in
/// `PassedThrough` instead of `NotSupported`, while leaving unsupported
/// resources like `kubectl get secrets` in `NotSupported`.
fn has_supported_tool_prefix(command: &str, routes: &impl Fn(&str, &Exclusions) -> bool) -> bool {
    let none = Exclusions::default();
    let is_subword = |word: &str| {
        word.starts_with(|c: char| c.is_ascii_lowercase())
            && word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
    };
    split_shell_stages(command).into_iter().any(|stage| {
        let text = crate::shell_form::parse(stage)
            .and_then(|form| stage.get(form.command))
            .unwrap_or(stage);
        let Some(words) = shlex::split(text) else {
            return false;
        };
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let unwrapped = crate::wrappers::effective(&refs);
        let effective: Vec<&str> = match &unwrapped {
            Some(inner) => inner.iter().map(String::as_str).collect(),
            None => refs,
        };
        let tool = crate::filters::unwrap_tool(&effective);
        let Some((&program, rest)) = tool.split_first() else {
            return false;
        };
        let mut candidates: Vec<String> = Vec::new();
        if matches!(program, "python" | "python3")
            && let ["-m", module, ..] = rest
            && is_subword(module)
        {
            candidates.push(format!("{program} -m {module}"));
        }
        if let [a, b, ..] = rest
            && is_subword(a)
            && is_subword(b)
        {
            candidates.push(format!("{program} {a} {b}"));
        }
        if let [a, ..] = rest
            && is_subword(a)
        {
            candidates.push(format!("{program} {a}"));
        }
        candidates.into_iter().any(|prefix| routes(&prefix, &none))
    })
}

/// Whether an entry names the line, a command in it, or a command of a pipe
/// after its assignments and wrappers such as `env` or `timeout`, as the post-run
/// hook checks it.
fn excluded_stage(exclusions: &Exclusions, command: &str) -> bool {
    exclusions.any(command).is_some()
        || split_shell_stages(command).into_iter().any(|stage| {
            let text = crate::shell_form::parse(stage)
                .and_then(|form| stage.get(form.command))
                .unwrap_or(stage);
            let words = shlex::split(text).unwrap_or_default();
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            crate::wrappers::effective(&words)
                .is_some_and(|words| exclusions.matching(&words.join(" ")).is_some())
        })
}

/// What the Claude Code post-run hook would make of a result that the command hook
/// leaves as `route`, with the hook's own parsers and limits: compacted, kept
/// whole as a handled command, or left as `route`. `rules` holds the project's exclusions,
/// context budget and `read_outline_lines`, and is `None` while the settings fail to load, when the
/// hook leaves every result. `entry` is the transcript line holding `part`, and
/// `root` the workspace root.
fn after_running(
    route: Route,
    command: &str,
    entry: &Value,
    part: &Value,
    rules: Option<&(Exclusions, (usize, u64))>,
    root: &Path,
) -> Route {
    let Some((exclusions, budget)) = rules else {
        return route;
    };
    // A failed command fires PostToolUseFailure, which cannot rewrite a result.
    if !matches!(
        route,
        Route::LeftByPermissions | Route::PassedThrough | Route::NotSupported
    ) || part["is_error"] == true
    {
        return route;
    }
    // Claude Code records the tool's own result beside the message; it is the
    // hook's input, but names one result only when the line holds one.
    let results = entry["message"]["content"].as_array().map_or(0, |parts| {
        parts.iter().filter(|p| p["type"] == "tool_result").count()
    });
    let response = if results == 1 && entry["toolUseResult"].is_object() {
        &entry["toolUseResult"]
    } else {
        &Value::Null
    };
    if response["interrupted"] == true
        || response["isImage"] == true
        || ["backgroundTaskId", "timedOutAfterMs", "structuredContent"]
            .iter()
            .any(|key| !response[*key].is_null())
    {
        return route;
    }
    let output = match response["stdout"].as_str() {
        Some(stdout) => stdout.to_owned(),
        None => text(&part["content"]),
    };
    let persisted = response["persistedOutputPath"]
        .is_string()
        .then(|| response["persistedOutputSize"].as_u64().unwrap_or(u64::MAX));
    // The hook runs where the session was.
    let cwd = entry["cwd"].as_str().map_or(root, Path::new);
    let judge = |exclusions| {
        let place = (root, cwd);
        crate::postroute::after_run(command, place, exclusions, &output, persisted, *budget)
    };
    match judge(exclusions) {
        Some(crate::postroute::AfterRun::Compacts) => Route::CompactedAfterRunning,
        Some(crate::postroute::AfterRun::KeepsWhole) if route == Route::NotSupported => {
            Route::PassedThrough
        }
        // An entry keeps a command from the post-run hook that would handle it.
        None if route == Route::NotSupported && judge(&Exclusions::default()).is_some() => {
            Route::Excluded
        }
        _ => route,
    }
}

/// The footer `justless run` prints on its own line after a captured command:
/// `[justless run-ID] exit N · …` when a person runs it, and the bracketed
/// `[justless run-ID · exit N]`, `[justless run-ID · 2.0 KB of 9.1 KB shown]` or
/// `[justless run-ID · stderr after stdout]` markers of a hook-routed run, which
/// can also count redacted secrets, as in `[justless run-ID · 1 secret redacted]`.
static FOOTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\[justless run-[A-Za-z0-9]+(?:\] exit -?[0-9]+| · (?:[^\]\n]+ shown|[0-9]+ secrets? redacted|stderr after stdout|exit -?[0-9]+)(?: · (?:[0-9]+ secrets? redacted|stderr after stdout|exit -?[0-9]+))*\]$)",
    )
    .unwrap()
});
/// The first line of `justless read` output (whole or windowed), which the hooks
/// substitute for `cat FILE` and shell range reads (`head`, `tail`, `sed -n`).
static OUTLINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\Arevision [0-9a-f]+ · [0-9]+ lines(?: · window L[0-9]+-[0-9]+)?\n").unwrap()
});

#[derive(Default, Serialize)]
pub(crate) struct Group {
    pub commands: u64,
    pub output_bytes: u64,
    pub labels: BTreeMap<String, (u64, u64)>,
}

struct Call {
    command: String,
    date: String,
    /// The permission mode the transcript last recorded before the call.
    mode: Option<String>,
}

pub(crate) fn transcript_dir(root: &Path) -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))?;
    let slug: String = root
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(base.join("projects").join(slug))
}

fn is_regular_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

pub(crate) fn transcripts(dir: &Path, since: SystemTime, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `symlink_metadata` never follows symlinks, skipping linked files and directories.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() && depth > 0 && path.file_name().is_some_and(|n| n != "memory") {
            transcripts(&path, since, depth - 1, out);
        } else if meta.is_file()
            && meta.len() <= MAX_TRANSCRIPT_BYTES
            && path.extension().is_some_and(|e| e == "jsonl")
            && meta.modified().is_ok_and(|m| m >= since)
        {
            out.push(path);
        }
    }
}

/// Upper bound on any single transcript file or line read by `discover` probes.
pub(crate) const MAX_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;
const READER_CAPACITY: usize = 64 * 1024;
const MAX_RETAINED_LINE_BUF: usize = 256 * 1024;
/// Claude Code `api-request-blob` lines replay earlier API payloads without a cwd or timestamp.
const CLAUDE_BLOB_PREFIX: &str = "{\"type\":\"api-request-blob\"";

/// Whether `id` contains only plain ASCII characters that JSON serializers emit
/// verbatim, making raw-line substring prefiltering lossless for pending call IDs.
pub(crate) fn is_plain_json_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'))
}

/// Whether `line` can possibly reference any pending call ID in `ids`.
pub(crate) fn ids_may_match<'a>(
    mut ids: impl Iterator<Item = &'a str>,
    len: usize,
    line: &str,
) -> bool {
    if len == 0 {
        return false;
    }
    if len > 8 {
        return true;
    }
    ids.any(|id| !is_plain_json_id(id) || line.contains(id))
}

/// Read one `\n`-terminated line up to `max_line_bytes` without allocating past
/// the cap. Returns `Ok(Some(true))` when the line fit within `max_line_bytes`,
/// `Ok(Some(false))` when the line was oversized and discarded up to `\n`/EOF,
/// and `Ok(None)` at EOF.
fn read_bounded_line(
    reader: &mut BufReader<File>,
    buf: &mut Vec<u8>,
    max_line_bytes: usize,
) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut read_any = false;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(read_any.then_some(!oversized));
        }
        read_any = true;
        let (consume, done) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (available.len(), false),
        };
        if !oversized {
            if buf.len().saturating_add(consume) <= max_line_bytes {
                buf.extend_from_slice(&available[..consume]);
            } else {
                oversized = true;
                buf.clear();
            }
        }
        reader.consume(consume);
        if done {
            return Ok(Some(!oversized));
        }
    }
}

/// Stream a JSONL file line-by-line into a reusable byte buffer (zero per-line
/// heap allocation on steady-state lines), safely skipping symlinks, non-UTF8,
/// or oversized files and lines. Stops early when `f` returns `false`.
pub(crate) fn for_each_jsonl_line(
    path: &Path,
    max_file_bytes: Option<u64>,
    mut f: impl FnMut(usize, &str) -> bool,
) {
    // Reject symlinks, special files and oversized files before opening.
    let Ok(sym_meta) = fs::symlink_metadata(path) else {
        return;
    };
    if !sym_meta.is_file() || max_file_bytes.is_some_and(|limit| sym_meta.len() > limit) {
        return;
    }
    let Ok(file) = File::open(path) else {
        return;
    };
    if file
        .metadata()
        .is_ok_and(|m| !m.is_file() || max_file_bytes.is_some_and(|limit| m.len() > limit))
    {
        return;
    }
    let max_line_bytes = max_file_bytes
        .unwrap_or(MAX_TRANSCRIPT_BYTES)
        .min(MAX_TRANSCRIPT_BYTES) as usize;
    let mut reader = BufReader::with_capacity(READER_CAPACITY, file);
    let mut buf = Vec::with_capacity(4096);
    let mut index = 0usize;
    loop {
        match read_bounded_line(&mut reader, &mut buf, max_line_bytes) {
            Ok(None) | Err(_) => break,
            Ok(Some(fit)) => {
                if fit && let Ok(raw) = std::str::from_utf8(&buf) {
                    let line = raw.trim_end_matches(['\r', '\n']);
                    if !f(index, line) {
                        break;
                    }
                }
                if buf.capacity() > MAX_RETAINED_LINE_BUF {
                    buf = Vec::with_capacity(4096);
                }
                index += 1;
            }
        }
    }
}

/// Read a UTF-8 regular file up to `max_bytes`, failing silently on symlinks,
/// missing, oversized, or non-UTF8 files.
fn read_bounded_utf8(path: &Path, max_bytes: u64) -> Option<String> {
    let sym_meta = fs::symlink_metadata(path).ok()?;
    if !sym_meta.is_file() || sym_meta.len() > max_bytes {
        return None;
    }
    let file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > max_bytes {
        return None;
    }
    let mut bytes = Vec::with_capacity(meta.len().min(1024 * 1024) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > max_bytes {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn collect_files_with_ext(
    dir: &Path,
    since: SystemTime,
    depth: usize,
    ext: &str,
    max_bytes: u64,
    out: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() && depth > 0 {
            collect_files_with_ext(&path, since, depth - 1, ext, max_bytes, out);
        } else if meta.is_file()
            && meta.len() <= max_bytes
            && path.extension().is_some_and(|e| e == ext)
            && meta.modified().is_ok_and(|m| m >= since)
        {
            out.push(path);
        }
    }
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn env_non_empty(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn path_in_workspace(candidate: &str, root: &Path) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return false;
    }
    let raw = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    let path = Path::new(raw);
    if path.starts_with(root) {
        return true;
    }
    path.canonicalize()
        .ok()
        .is_some_and(|canon| canon.starts_with(root))
}

fn normalize_slug_exact(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

fn normalize_slug_collapsed(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

fn slug_matches_workspace(dir_name: &str, root: &Path) -> bool {
    let root_str = root.to_string_lossy();
    if dir_name.is_empty() || root_str.is_empty() {
        return false;
    }
    let exact_dir = normalize_slug_exact(dir_name);
    let exact_root = normalize_slug_exact(&root_str);
    if !exact_dir.is_empty() && exact_dir == exact_root {
        return true;
    }
    let col_dir = normalize_slug_collapsed(dir_name);
    let col_root = normalize_slug_collapsed(&root_str);
    !col_dir.is_empty() && col_dir == col_root
}

fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn parse_iso_with_offset(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 25 {
        return None;
    }
    let split = bytes.len() - 6;
    let sign = bytes[split];
    if !matches!(sign, b'+' | b'-') || bytes[split + 3] != b':' {
        return None;
    }
    let hh: u64 = text.get(split + 1..split + 3)?.parse().ok()?;
    let mm: u64 = text.get(split + 4..split + 6)?.parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    let utc_like = format!("{}Z", &text[..split]);
    let base_ms = crate::learn::events::parse_timestamp(&utc_like)?;
    let offset_ms = (hh * 3600 + mm * 60) * 1000;
    if sign == b'+' {
        base_ms.checked_sub(offset_ms)
    } else {
        base_ms.checked_add(offset_ms)
    }
}

fn parse_any_timestamp_ms(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => {
            let raw = n
                .as_u64()
                .or_else(|| n.as_f64().filter(|f| *f > 0.0).map(|f| f as u64))?;
            Some(if raw < 100_000_000_000 {
                raw.saturating_mul(1000)
            } else {
                raw
            })
        }
        Value::String(s) => {
            let trimmed = s.trim();
            crate::learn::events::parse_timestamp(trimmed)
                .or_else(|| parse_iso_with_offset(trimmed))
                .or_else(|| {
                    trimmed.parse::<u64>().ok().map(|n| {
                        if n < 100_000_000_000 {
                            n.saturating_mul(1000)
                        } else {
                            n
                        }
                    })
                })
        }
        _ => None,
    }
}

fn text_bytes(content: &Value) -> u64 {
    match content {
        Value::String(text) => text.len() as u64,
        Value::Array(parts) => parts
            .iter()
            .map(|p| p["text"].as_str().map_or(0, |t| t.len() as u64))
            .sum(),
        _ => 0,
    }
}

fn text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    }
}

fn text_matches(content: &Value, pattern: &Regex) -> bool {
    match content {
        Value::String(text) => pattern.is_match(text),
        Value::Array(parts) => parts
            .iter()
            .any(|p| p["text"].as_str().is_some_and(|t| pattern.is_match(t))),
        _ => false,
    }
}

/// Whether a command ran through Justless: its output has a run footer, its
/// labelled command is a `justless` binary at any path, or it is a shell file
/// read (`cat FILE`, `head`, `tail`, `sed -n`) whose result is the outline the
/// hooks substitute.
pub(crate) fn routed(command: &str, label: &str, content: &Value) -> bool {
    if text_matches(content, &FOOTER) {
        return true;
    }
    let invokes_justless = label == "justless"
        || label.starts_with("justless ")
        || split_shell_stages(command).into_iter().any(|stage| {
            let text = crate::shell_form::parse(stage)
                .and_then(|form| stage.get(form.command))
                .unwrap_or(stage);
            text.split_whitespace()
                .next()
                .is_some_and(|prog| Path::new(prog).file_name().is_some_and(|n| n == "justless"))
        });
    let words: Vec<_> = command.split_whitespace().collect();
    let bare_cat = matches!(words.as_slice(), ["cat", file]
        if !file.starts_with('-') && !file.contains(|c: char| "|;&<>$`*?(){}[]!#~\\\"'".contains(c)));
    let shell_read = bare_cat || (!cfg!(windows) && crate::shellread::parse(command).is_some());
    let first = match content {
        Value::Array(parts) => parts.first().and_then(|p| p["text"].as_str()),
        other => other.as_str(),
    };
    invokes_justless || shell_read && first.is_some_and(|text| OUTLINE.is_match(text))
}

/// Collapses any label that looks like a pasted token or oversized identifier
/// to `"other"` so `discover` never emits secrets even when an agent invokes a
/// malformed command starting with a credential token.
fn sanitize_discover_label(label: String) -> String {
    const SECRET_PREFIXES: &[&str] = &[
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "sk-",
        "sk_live_",
        "sk_test_",
        "pk_live_",
        "AKIA",
        "ASIA",
        "xoxb-",
        "xoxp-",
        "AIza",
        "glpat-",
        "npm_",
        "pypi-",
    ];
    if label.is_empty()
        || label.len() > 48
        || label.chars().any(char::is_control)
        || label.split(' ').any(|word| {
            SECRET_PREFIXES
                .iter()
                .any(|prefix| word.starts_with(prefix))
        })
    {
        return "other".into();
    }
    label
}

/// Command label for `discover`: delegates to `agents::command_label` +
/// `history::label`, and preserves allowlisted 3-word labels (`python3 -m
/// unittest`, `python -m unittest`, `kubectl get events|svc|pods|…`) without
/// ever exposing arguments, paths, namespaces or patterns.
fn discover_label(command: &str) -> String {
    let words = crate::agents::command_label(command);
    let base = crate::history::label(&words.split(' ').collect::<Vec<_>>());
    if !matches!(base.as_str(), "python" | "python3" | "kubectl get") {
        return sanitize_discover_label(base);
    }
    for stage in split_shell_stages(command) {
        let text = crate::shell_form::parse(stage)
            .and_then(|form| stage.get(form.command))
            .unwrap_or(stage);
        let Some(stage_words) = shlex::split(text) else {
            continue;
        };
        let refs: Vec<&str> = stage_words.iter().map(String::as_str).collect();
        let refined = crate::history::label(&refs);
        if refined != base && refined.starts_with(&base) {
            return sanitize_discover_label(refined);
        }
    }
    sanitize_discover_label(base)
}

#[derive(Default)]
struct PlanEntry {
    commands: u64,
    bytes: u64,
    now_commands: u64,
    now_bytes: u64,
    left_commands: u64,
    left_bytes: u64,
    after_commands: u64,
    after_bytes: u64,
}

/// Coverage for an agent discovered from local session transcripts.
/// Only aggregate command counts, byte totals, session counts, and command-family
/// labels are retained — never full command lines or raw output text.
pub(crate) struct AgentCoverage {
    pub key: &'static str,
    pub name: &'static str,
    pub install_hint: Option<&'static str>,
    pub hook_based: bool,
    pub root: Option<PathBuf>,
    pub sessions: BTreeSet<String>,
    pub groups: BTreeMap<Route, Group>,
}

impl AgentCoverage {
    fn new(
        key: &'static str,
        name: &'static str,
        install_hint: Option<&'static str>,
        hook_based: bool,
        root: Option<PathBuf>,
    ) -> Self {
        Self {
            key,
            name,
            install_hint,
            hook_based,
            root,
            sessions: BTreeSet::new(),
            groups: BTreeMap::new(),
        }
    }

    pub fn commands(&self) -> u64 {
        self.groups.values().map(|g| g.commands).sum()
    }

    pub fn bytes(&self) -> u64 {
        self.groups.values().map(|g| g.output_bytes).sum()
    }

    pub fn json(&self) -> Value {
        serde_json::json!({
            "transcripts": self.root,
            "sessions": self.sessions.len(),
            "commands": self.commands(),
            "output_bytes": self.bytes(),
            "groups": groups_json(&self.groups),
        })
    }

    fn record(
        &mut self,
        session: &str,
        command: &str,
        content: &Value,
        bytes: u64,
        exclusions: &Exclusions,
        routes: impl Fn(&str, &Exclusions) -> bool,
    ) {
        let trimmed = command.trim();
        if trimmed.is_empty() || command_disables_justless(trimmed) {
            return;
        }
        let label = discover_label(trimmed);
        let route = if routed(trimmed, &label, content) {
            Route::ThroughJustless
        } else {
            Route::unrouted(trimmed, &label, exclusions, false, routes)
        };
        let group = self.groups.entry(route).or_default();
        group.commands += 1;
        group.output_bytes += bytes;
        let entry = group.labels.entry(label).or_default();
        entry.0 += 1;
        entry.1 += bytes;
        self.sessions.insert(session.to_owned());
    }
}

fn is_shell_tool_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "bash"
            | "shell"
            | "exec"
            | "run_shell_command"
            | "run_command"
            | "execute_command"
            | "run_terminal_cmd"
            | "run_terminal_command"
            | "terminal"
    )
}

/// Fast substring prefilter matching every tool name accepted by
/// [`is_shell_tool_name`] (plus Cline/Roo XML `<execute_command>` tags) without
/// allocating a lowercased copy of a transcript.
fn text_may_mention_shell_tool(raw: &str) -> bool {
    raw.contains("run_shell_command")
        || raw.contains("execute_command")
        || raw.contains("run_command")
        || raw.contains("run_terminal_cmd")
        || raw.contains("run_terminal_command")
        || raw.contains("\"bash\"")
        || raw.contains("\"Bash\"")
        || raw.contains("\"BASH\"")
        || raw.contains("\"shell\"")
        || raw.contains("\"Shell\"")
        || raw.contains("\"SHELL\"")
        || raw.contains("\"exec\"")
        || raw.contains("\"Exec\"")
        || raw.contains("\"EXEC\"")
        || raw.contains("\"terminal\"")
        || raw.contains("\"Terminal\"")
        || raw.contains("\"TERMINAL\"")
}

fn extract_command_from_args(args: &Value) -> Option<String> {
    match args {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.starts_with('{')
                && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
            {
                return extract_command_from_args(&parsed);
            }
            None
        }
        Value::Object(obj) => {
            for key in ["command", "cmd", "commandLine", "CommandLine"] {
                match obj.get(key) {
                    Some(Value::String(text)) if !text.trim().is_empty() => {
                        return Some(text.clone());
                    }
                    Some(Value::Array(words)) => {
                        let strs: Option<Vec<&str>> = words.iter().map(Value::as_str).collect();
                        if let Some(strs) = strs
                            && !strs.is_empty()
                        {
                            return Some(
                                shlex::try_join(strs.iter().copied())
                                    .unwrap_or_else(|_| strs.join(" ")),
                            );
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_output_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                if let Some(s) = extract_output_text(item) {
                    parts.push(s);
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(obj) => {
            if let Some(fr) = obj.get("functionResponse")
                && let Some(resp) = fr.get("response")
            {
                return extract_output_text(resp);
            }
            for key in ["output", "result", "text", "content"] {
                if let Some(val) = obj.get(key)
                    && let Some(s) = extract_output_text(val)
                {
                    return Some(s);
                }
            }
            let stdout = obj.get("stdout").and_then(Value::as_str);
            let stderr = obj.get("stderr").and_then(Value::as_str);
            match (stdout, stderr) {
                (Some(out), Some(err)) if !err.is_empty() => Some(format!("{out}{err}")),
                (Some(out), _) => Some(out.to_owned()),
                (None, Some(err)) => Some(err.to_owned()),
                (None, None) => None,
            }
        }
        _ => None,
    }
}

/// Locate Gemini CLI's home (`$GEMINI_CLI_HOME`, checking `.gemini/tmp` vs `tmp`,
/// or `~/.gemini`).
fn gemini_home() -> Option<PathBuf> {
    if let Some(home) = env_non_empty("GEMINI_CLI_HOME") {
        if home.join(".gemini/tmp").is_dir() {
            return Some(home.join(".gemini"));
        }
        return Some(home);
    }
    user_home().map(|home| home.join(".gemini"))
}

fn gemini_coverage(
    workspace: &Workspace,
    exclusions: &Exclusions,
    since: SystemTime,
    since_ms: u64,
) -> AgentCoverage {
    let Some(base) = gemini_home() else {
        return AgentCoverage::new(
            "gemini",
            "Gemini CLI",
            Some("init --hooks --agent gemini"),
            true,
            None,
        );
    };
    scan_gemini_in(&base, workspace, exclusions, since, since_ms)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirMatch {
    Matched,
    Rejected,
    InspectFile,
}

fn scan_gemini_in(
    base: &Path,
    workspace: &Workspace,
    exclusions: &Exclusions,
    since: SystemTime,
    since_ms: u64,
) -> AgentCoverage {
    let tmp_dir = base.join("tmp");
    let mut cov = AgentCoverage::new(
        "gemini",
        "Gemini CLI",
        Some("init --hooks --agent gemini"),
        true,
        Some(tmp_dir.clone()),
    );
    let Ok(project_entries) = fs::read_dir(&tmp_dir) else {
        return cov;
    };
    let root_str = workspace.root.to_string_lossy();
    let root_hash = sha256_hex(&root_str);

    // Optional `projects.json` maps workspace root paths to short project names.
    let mut project_map: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(raw) = read_bounded_utf8(&base.join("projects.json"), 4 * 1024 * 1024)
        && let Ok(val) = serde_json::from_str::<Value>(&raw)
        && let Some(obj) = val["projects"].as_object()
    {
        for (path, name) in obj {
            if let Some(name) = name.as_str() {
                project_map
                    .entry(name.to_owned())
                    .or_default()
                    .push(path.clone());
            }
        }
    }

    for entry in project_entries.flatten() {
        let proj_dir = entry.path();
        if !is_regular_dir(&proj_dir) {
            continue;
        }
        let chats_dir = proj_dir.join("chats");
        if !is_regular_dir(&chats_dir) {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let dir_match =
            if let Some(proj_root) = read_bounded_utf8(&proj_dir.join(".project_root"), 4096) {
                if path_in_workspace(&proj_root, &workspace.root) {
                    DirMatch::Matched
                } else {
                    DirMatch::Rejected
                }
            } else if dir_name.eq_ignore_ascii_case(&root_hash) {
                DirMatch::Matched
            } else if dir_name.len() == 64 && dir_name.bytes().all(|b| b.is_ascii_hexdigit()) {
                DirMatch::Rejected
            } else if let Some(paths) = project_map.get(&dir_name) {
                if paths.iter().any(|p| path_in_workspace(p, &workspace.root)) {
                    DirMatch::Matched
                } else {
                    DirMatch::Rejected
                }
            } else if slug_matches_workspace(&dir_name, &workspace.root) {
                DirMatch::Matched
            } else {
                DirMatch::InspectFile
            };
        if dir_match == DirMatch::Rejected {
            continue;
        }

        let Ok(chat_entries) = fs::read_dir(&chats_dir) else {
            continue;
        };
        for chat in chat_entries.flatten() {
            let path = chat.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let name = chat.file_name();
            let name_str = name.to_string_lossy();
            if !meta.is_file()
                || meta.len() > MAX_TRANSCRIPT_BYTES
                || !name_str.starts_with("session-")
                || !name_str.ends_with(".json")
                || !meta.modified().is_ok_and(|m| m >= since)
            {
                continue;
            }
            let Some(raw) = read_bounded_utf8(&path, MAX_TRANSCRIPT_BYTES) else {
                continue;
            };
            // Fast prefilter before parsing JSON.
            if !text_may_mention_shell_tool(&raw) {
                continue;
            }
            let Ok(session_val) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let session_matched = dir_match == DirMatch::Matched
                || session_val["projectHash"]
                    .as_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case(&root_hash))
                || ["cwd", "workingDirectory", "workspace", "projectRoot"]
                    .iter()
                    .any(|key| {
                        session_val[*key]
                            .as_str()
                            .is_some_and(|p| path_in_workspace(p, &workspace.root))
                    });
            let session_id = session_val["sessionId"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
            let session_ts = parse_any_timestamp_ms(&session_val["lastUpdated"])
                .or_else(|| parse_any_timestamp_ms(&session_val["startTime"]));
            let Some(messages) = session_val["messages"].as_array() else {
                continue;
            };

            let mut pending_fc: VecDeque<(Option<String>, String, Option<u64>, bool)> =
                VecDeque::new();
            for msg in messages {
                let msg_ts = parse_any_timestamp_ms(&msg["timestamp"]).or(session_ts);
                // Format A: Gemini CLI `toolCalls` array on assistant messages.
                if let Some(tool_calls) = msg["toolCalls"].as_array() {
                    for tc in tool_calls {
                        let tool_name = tc["name"].as_str().unwrap_or_default();
                        if !is_shell_tool_name(tool_name) {
                            continue;
                        }
                        let Some(command) = extract_command_from_args(&tc["args"]) else {
                            continue;
                        };
                        let call_dir = tc["args"]["directory"]
                            .as_str()
                            .or_else(|| tc["args"]["dir_path"].as_str())
                            .or_else(|| tc["args"]["cwd"].as_str());
                        let in_ws = match call_dir {
                            Some(dir) if Path::new(dir).is_absolute() => {
                                path_in_workspace(dir, &workspace.root)
                            }
                            _ => session_matched,
                        };
                        if !in_ws {
                            continue;
                        }
                        let ts = parse_any_timestamp_ms(&tc["timestamp"]).or(msg_ts);
                        if ts.is_some_and(|t| t < since_ms) {
                            continue;
                        }
                        let output = extract_output_text(&tc["result"])
                            .or_else(|| tc["resultDisplay"].as_str().map(str::to_owned));
                        let Some(output) = output else {
                            continue;
                        };
                        let bytes = output.len() as u64;
                        let content = Value::String(output);
                        cov.record(
                            &session_id,
                            &command,
                            &content,
                            bytes,
                            exclusions,
                            crate::agents::agy_would_route,
                        );
                    }
                }
                // Format B: GenAI `parts` with `functionCall` / `functionResponse`.
                let parts = msg["parts"]
                    .as_array()
                    .or_else(|| msg["content"].as_array());
                if let Some(parts) = parts {
                    for part in parts {
                        if let Some(fc) = part.get("functionCall")
                            && let Some(name) = fc["name"].as_str()
                            && is_shell_tool_name(name)
                            && let Some(command) = extract_command_from_args(&fc["args"])
                        {
                            let call_dir = fc["args"]["directory"]
                                .as_str()
                                .or_else(|| fc["args"]["dir_path"].as_str())
                                .or_else(|| fc["args"]["cwd"].as_str());
                            let in_ws = match call_dir {
                                Some(dir) if Path::new(dir).is_absolute() => {
                                    path_in_workspace(dir, &workspace.root)
                                }
                                _ => session_matched,
                            };
                            let id = fc["id"].as_str().map(str::to_owned);
                            pending_fc.push_back((id, command, msg_ts, in_ws));
                        } else if let Some(fr) = part.get("functionResponse")
                            && !pending_fc.is_empty()
                        {
                            let resp_id = fr["id"].as_str();
                            let idx = resp_id
                                .and_then(|rid| {
                                    pending_fc
                                        .iter()
                                        .position(|(id, _, _, _)| id.as_deref() == Some(rid))
                                })
                                .or_else(|| {
                                    fr["name"]
                                        .as_str()
                                        .filter(|n| is_shell_tool_name(n))
                                        .map(|_| 0)
                                });
                            if let Some(idx) = idx
                                && let Some((_, command, call_ts, in_ws)) = pending_fc.remove(idx)
                            {
                                let ts = msg_ts.or(call_ts);
                                if !in_ws || ts.is_some_and(|t| t < since_ms) {
                                    continue;
                                }
                                let output =
                                    extract_output_text(&fr["response"]).unwrap_or_default();
                                let bytes = output.len() as u64;
                                let content = Value::String(output);
                                cov.record(
                                    &session_id,
                                    &command,
                                    &content,
                                    bytes,
                                    exclusions,
                                    crate::agents::agy_would_route,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    cov
}

/// Locate OpenCode storage (`$OPENCODE_DATA_DIR`, `$XDG_DATA_HOME/opencode/storage`,
/// or `~/.local/share/opencode/storage`).
fn opencode_storage_dir() -> Option<PathBuf> {
    if let Some(dir) = env_non_empty("OPENCODE_DATA_DIR") {
        if dir.join("storage").is_dir() {
            return Some(dir.join("storage"));
        }
        return Some(dir);
    }
    if let Some(xdg) = env_non_empty("XDG_DATA_HOME") {
        return Some(xdg.join("opencode/storage"));
    }
    user_home().map(|home| home.join(".local/share/opencode/storage"))
}

fn opencode_coverage(
    workspace: &Workspace,
    exclusions: &Exclusions,
    since: SystemTime,
    since_ms: u64,
) -> AgentCoverage {
    let Some(storage) = opencode_storage_dir() else {
        return AgentCoverage::new(
            "opencode",
            "OpenCode",
            Some("init --hooks --agent opencode"),
            true,
            None,
        );
    };
    scan_opencode_in(&storage, workspace, exclusions, since, since_ms)
}

fn scan_opencode_in(
    storage: &Path,
    workspace: &Workspace,
    exclusions: &Exclusions,
    since: SystemTime,
    since_ms: u64,
) -> AgentCoverage {
    let mut cov = AgentCoverage::new(
        "opencode",
        "OpenCode",
        Some("init --hooks --agent opencode"),
        true,
        Some(storage.to_path_buf()),
    );
    if !storage.is_dir() {
        return cov;
    }

    // 1. Identify matching OpenCode project IDs from `storage/project/*.json`.
    let mut matching_projects = HashSet::new();
    let project_dir = storage.join("project");
    if let Ok(entries) = fs::read_dir(&project_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if let Some(raw) = read_bounded_utf8(&path, 1024 * 1024)
                && let Ok(val) = serde_json::from_str::<Value>(&raw)
            {
                let in_ws = ["worktree", "directory", "path", "cwd"].iter().any(|k| {
                    val[*k]
                        .as_str()
                        .is_some_and(|p| path_in_workspace(p, &workspace.root))
                });
                if in_ws
                    && let Some(id) = val["id"]
                        .as_str()
                        .or_else(|| path.file_stem().and_then(|s| s.to_str()))
                {
                    matching_projects.insert(id.to_owned());
                }
            }
        }
    }

    // 2. Identify workspace sessions from `storage/session/`.
    let mut matching_sessions = HashSet::new();
    let mut other_sessions = HashSet::new();
    let mut session_files = Vec::new();
    collect_files_with_ext(
        &storage.join("session"),
        SystemTime::UNIX_EPOCH,
        2,
        "json",
        2 * 1024 * 1024,
        &mut session_files,
    );
    for file in &session_files {
        if let Some(raw) = read_bounded_utf8(file, 2 * 1024 * 1024)
            && let Ok(val) = serde_json::from_str::<Value>(&raw)
        {
            let sid = val["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()));
            let Some(sid) = sid else {
                continue;
            };
            let dir_match = ["directory", "cwd", "worktree", "path"].iter().any(|k| {
                val[*k]
                    .as_str()
                    .is_some_and(|p| path_in_workspace(p, &workspace.root))
            });
            let proj_match = val["projectID"]
                .as_str()
                .is_some_and(|pid| matching_projects.contains(pid));
            if dir_match || proj_match {
                matching_sessions.insert(sid);
            } else {
                other_sessions.insert(sid);
            }
        }
    }

    // 3. Collect candidate part JSON files, scoped to matching sessions via `message/<sid>/`
    // when available so parts from unrelated projects are never opened.
    let part_root = storage.join("part");
    if !part_root.is_dir() {
        return cov;
    }
    let message_root = storage.join("message");
    let mut part_files = Vec::new();
    let mut scoped_by_message = false;
    if message_root.is_dir() && !matching_sessions.is_empty() {
        for sid in &matching_sessions {
            let msg_dir = message_root.join(sid);
            let Ok(entries) = fs::read_dir(&msg_dir) else {
                continue;
            };
            scoped_by_message = true;
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if !meta.is_file()
                    || path.extension().is_none_or(|e| e != "json")
                    || !meta.modified().is_ok_and(|m| m >= since)
                {
                    continue;
                }
                if let Some(mid) = path.file_stem() {
                    collect_files_with_ext(
                        &part_root.join(mid),
                        since,
                        1,
                        "json",
                        MAX_TRANSCRIPT_BYTES,
                        &mut part_files,
                    );
                }
            }
        }
    }
    if !scoped_by_message && (!matching_sessions.is_empty() || !message_root.is_dir()) {
        collect_files_with_ext(
            &part_root,
            since,
            2,
            "json",
            MAX_TRANSCRIPT_BYTES,
            &mut part_files,
        );
    }
    part_files.sort();

    for file in part_files {
        let Some(raw) = read_bounded_utf8(&file, MAX_TRANSCRIPT_BYTES) else {
            continue;
        };
        // Fast prefilter before parsing part JSON.
        if !(raw.contains("\"tool\"") || raw.contains("\"tool-invocation\""))
            || !text_may_mention_shell_tool(&raw)
        {
            continue;
        }
        let Ok(part) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let kind = part["type"].as_str().unwrap_or_default();
        if !matches!(kind, "tool" | "tool-invocation") {
            continue;
        }
        let tool_name = part["tool"]
            .as_str()
            .or_else(|| part["toolName"].as_str())
            .unwrap_or_default();
        if !is_shell_tool_name(tool_name) {
            continue;
        }
        let sid = part["sessionID"]
            .as_str()
            .or_else(|| part["sessionId"].as_str())
            .unwrap_or_default();
        if !sid.is_empty() && other_sessions.contains(sid) {
            continue;
        }
        let in_ws = (!sid.is_empty() && matching_sessions.contains(sid))
            || scoped_by_message
            || ["directory", "cwd"].iter().any(|k| {
                part[*k]
                    .as_str()
                    .is_some_and(|p| path_in_workspace(p, &workspace.root))
            })
            || ["workdir", "cwd", "directory"].iter().any(|k| {
                part["state"]["input"][*k]
                    .as_str()
                    .is_some_and(|p| path_in_workspace(p, &workspace.root))
            });
        if !in_ws {
            continue;
        }
        let state = &part["state"];
        let input = if state["input"].is_object() {
            &state["input"]
        } else {
            &part["input"]
        };
        let Some(command) = extract_command_from_args(input) else {
            continue;
        };
        let ts = parse_any_timestamp_ms(&state["time"]["end"])
            .or_else(|| parse_any_timestamp_ms(&state["time"]["start"]))
            .or_else(|| parse_any_timestamp_ms(&part["time"]["created"]));
        if ts.is_some_and(|t| t < since_ms) {
            continue;
        }
        let output = extract_output_text(&state["output"])
            .or_else(|| extract_output_text(&state["metadata"]["output"]))
            .or_else(|| extract_output_text(&part["output"]));
        let Some(output) = output else {
            continue;
        };
        if matches!(state["status"].as_str(), Some("pending" | "running")) && output.is_empty() {
            continue;
        }
        let session_label = if !sid.is_empty() {
            sid.to_owned()
        } else {
            file.parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let bytes = output.len() as u64;
        let content = Value::String(output);
        cov.record(
            &session_label,
            &command,
            &content,
            bytes,
            exclusions,
            crate::agents::agy_would_route,
        );
    }
    cov
}

/// Locate Cursor's home (`$CURSOR_HOME`, or `~/.cursor`).
fn cursor_home() -> Option<PathBuf> {
    env_non_empty("CURSOR_HOME").or_else(|| user_home().map(|home| home.join(".cursor")))
}

fn cursor_coverage(
    workspace: &Workspace,
    exclusions: &Exclusions,
    since: SystemTime,
    since_ms: u64,
) -> AgentCoverage {
    let Some(base) = cursor_home() else {
        return AgentCoverage::new(
            "cursor",
            "Cursor",
            Some("init --hooks --agent cursor"),
            true,
            None,
        );
    };
    scan_cursor_in(&base, workspace, exclusions, since, since_ms)
}

fn cursor_line_wanted(line: &str, has_pending: bool) -> bool {
    let may_start = (line.contains("\"tool_use\"")
        || line.contains("\"tool_calls\"")
        || line.contains("\"tool_call\"")
        || line.contains("\"toolFormerData\"")
        || line.contains("\"function_call\""))
        && (text_may_mention_shell_tool(line)
            || line.contains("terminal")
            || line.contains("Terminal")
            || line.contains("command")
            || line.contains("Command"));
    let may_finish = has_pending
        && (line.contains("\"tool_result\"")
            || line.contains("\"tool_call_id\"")
            || line.contains("\"tool_output\"")
            || line.contains("\"role\":\"tool\"")
            || line.contains("\"role\": \"tool\"")
            || line.contains("\"output\""));
    may_start || may_finish
}

fn scan_cursor_in(
    base: &Path,
    workspace: &Workspace,
    exclusions: &Exclusions,
    since: SystemTime,
    since_ms: u64,
) -> AgentCoverage {
    let projects_dir = base.join("projects");
    let mut cov = AgentCoverage::new(
        "cursor",
        "Cursor",
        Some("init --hooks --agent cursor"),
        true,
        Some(projects_dir.clone()),
    );
    let mut files = Vec::new();
    let mut require_explicit_cwd = false;
    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let proj_dir = entry.path();
            if !is_regular_dir(&proj_dir) {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            if !slug_matches_workspace(&dir_name, &workspace.root) {
                continue;
            }
            let transcripts_dir = proj_dir.join("agent-transcripts");
            if is_regular_dir(&transcripts_dir) {
                collect_files_with_ext(
                    &transcripts_dir,
                    since,
                    3,
                    "jsonl",
                    MAX_TRANSCRIPT_BYTES,
                    &mut files,
                );
            }
        }
    } else if base.join("agent-transcripts").is_dir() {
        require_explicit_cwd = !base
            .file_name()
            .is_some_and(|n| slug_matches_workspace(&n.to_string_lossy(), &workspace.root));
        collect_files_with_ext(
            &base.join("agent-transcripts"),
            since,
            3,
            "jsonl",
            MAX_TRANSCRIPT_BYTES,
            &mut files,
        );
    }
    files.sort();

    for file in files {
        let session = file
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut pending: HashMap<String, (String, Option<u64>, bool)> = HashMap::new();
        let mut anonymous: VecDeque<(String, Option<u64>, bool)> = VecDeque::new();
        for_each_jsonl_line(&file, Some(MAX_TRANSCRIPT_BYTES), |_, line| {
            let has_pending = !pending.is_empty() || !anonymous.is_empty();
            if !cursor_line_wanted(line, has_pending) {
                return true;
            }
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                return true;
            };
            let line_cwd = entry["cwd"]
                .as_str()
                .or_else(|| entry["workingDirectory"].as_str())
                .or_else(|| entry["working_directory"].as_str());
            let line_cwd_ok = match line_cwd {
                Some(cwd) if Path::new(cwd).is_absolute() => {
                    path_in_workspace(cwd, &workspace.root)
                }
                _ => !require_explicit_cwd,
            };
            let line_ts = parse_any_timestamp_ms(&entry["timestamp"])
                .or_else(|| parse_any_timestamp_ms(&entry["createdAt"]));

            let check_call_cwd = |args: &Value| -> bool {
                let arg_cwd = args["working_directory"]
                    .as_str()
                    .or_else(|| args["workingDirectory"].as_str())
                    .or_else(|| args["cwd"].as_str());
                match arg_cwd {
                    Some(cwd) if Path::new(cwd).is_absolute() => {
                        path_in_workspace(cwd, &workspace.root)
                    }
                    _ => line_cwd_ok,
                }
            };

            // Shape 1: Anthropic-style content blocks under `message.content` or `content`.
            if let Some(parts) = entry["message"]["content"]
                .as_array()
                .or_else(|| entry["content"].as_array())
            {
                for part in parts {
                    match part["type"].as_str() {
                        Some("tool_use")
                            if part["name"].as_str().is_some_and(is_shell_tool_name) =>
                        {
                            if let Some(command) = extract_command_from_args(&part["input"]) {
                                let in_ws = check_call_cwd(&part["input"]);
                                if let Some(id) = part["id"].as_str() {
                                    pending.insert(id.to_owned(), (command, line_ts, in_ws));
                                } else {
                                    anonymous.push_back((command, line_ts, in_ws));
                                }
                            }
                        }
                        Some("tool_result") => {
                            let matched = match part["tool_use_id"].as_str() {
                                Some(id) => pending.remove(id),
                                None => anonymous.pop_front(),
                            };
                            if let Some((command, call_ts, in_ws)) = matched {
                                let ts = line_ts.or(call_ts);
                                if !in_ws || ts.is_some_and(|t| t < since_ms) {
                                    continue;
                                }
                                let content_owned;
                                let (record_content, bytes) = match &part["content"] {
                                    Value::String(s) => (&part["content"], s.len() as u64),
                                    Value::Array(_) => {
                                        (&part["content"], text_bytes(&part["content"]))
                                    }
                                    other => {
                                        let text = extract_output_text(other).unwrap_or_default();
                                        let len = text.len() as u64;
                                        content_owned = Value::String(text);
                                        (&content_owned, len)
                                    }
                                };
                                cov.record(
                                    &session,
                                    &command,
                                    record_content,
                                    bytes,
                                    exclusions,
                                    crate::agents::codex_would_route,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Shape 2: OpenAI-style `tool_calls` + `role: "tool"`.
            if let Some(tool_calls) = entry["tool_calls"]
                .as_array()
                .or_else(|| entry["message"]["tool_calls"].as_array())
            {
                for tc in tool_calls {
                    let name = tc["function"]["name"]
                        .as_str()
                        .or_else(|| tc["name"].as_str())
                        .unwrap_or_default();
                    if !is_shell_tool_name(name) {
                        continue;
                    }
                    let args_val = if !tc["function"]["arguments"].is_null() {
                        &tc["function"]["arguments"]
                    } else if !tc["arguments"].is_null() {
                        &tc["arguments"]
                    } else {
                        &tc["args"]
                    };
                    if let Some(command) = extract_command_from_args(args_val) {
                        let parsed_args = match args_val {
                            Value::String(s) => {
                                serde_json::from_str::<Value>(s).unwrap_or(Value::Null)
                            }
                            other => other.clone(),
                        };
                        let in_ws = check_call_cwd(&parsed_args);
                        if let Some(id) = tc["id"].as_str() {
                            pending.insert(id.to_owned(), (command, line_ts, in_ws));
                        } else {
                            anonymous.push_back((command, line_ts, in_ws));
                        }
                    }
                }
            }
            let role = entry["role"]
                .as_str()
                .or_else(|| entry["message"]["role"].as_str());
            if role == Some("tool") {
                let id = entry["tool_call_id"]
                    .as_str()
                    .or_else(|| entry["message"]["tool_call_id"].as_str());
                let matched = match id {
                    Some(id) => pending.remove(id),
                    None => anonymous.pop_front(),
                };
                if let Some((command, call_ts, in_ws)) = matched {
                    let ts = line_ts.or(call_ts);
                    if in_ws && !ts.is_some_and(|t| t < since_ms) {
                        let content = if !entry["content"].is_null() {
                            &entry["content"]
                        } else {
                            &entry["message"]["content"]
                        };
                        let output = extract_output_text(content).unwrap_or_default();
                        let bytes = output.len() as u64;
                        let content_val = Value::String(output);
                        cov.record(
                            &session,
                            &command,
                            &content_val,
                            bytes,
                            exclusions,
                            crate::agents::codex_would_route,
                        );
                    }
                }
            }

            // Shape 3: Flat Cursor `tool_call` / `tool_use` / `tool_result` event lines.
            match entry["type"].as_str() {
                Some("tool_call" | "tool_use") => {
                    let name = entry["tool"]
                        .as_str()
                        .or_else(|| entry["name"].as_str())
                        .unwrap_or_default();
                    if is_shell_tool_name(name) {
                        let args_val = if !entry["args"].is_null() {
                            &entry["args"]
                        } else if !entry["input"].is_null() {
                            &entry["input"]
                        } else {
                            &entry["arguments"]
                        };
                        if let Some(command) = extract_command_from_args(args_val) {
                            let in_ws = check_call_cwd(args_val);
                            let immediate = extract_output_text(&entry["output"])
                                .or_else(|| extract_output_text(&entry["result"]));
                            if let Some(output) = immediate {
                                if in_ws && !line_ts.is_some_and(|t| t < since_ms) {
                                    let bytes = output.len() as u64;
                                    let content_val = Value::String(output);
                                    cov.record(
                                        &session,
                                        &command,
                                        &content_val,
                                        bytes,
                                        exclusions,
                                        crate::agents::codex_would_route,
                                    );
                                }
                            } else if let Some(id) =
                                entry["id"].as_str().or_else(|| entry["call_id"].as_str())
                            {
                                pending.insert(id.to_owned(), (command, line_ts, in_ws));
                            } else {
                                anonymous.push_back((command, line_ts, in_ws));
                            }
                        }
                    }
                }
                Some("tool_result" | "tool_output") => {
                    let id = entry["tool_use_id"]
                        .as_str()
                        .or_else(|| entry["call_id"].as_str())
                        .or_else(|| entry["id"].as_str());
                    let matched = match id {
                        Some(id) => pending.remove(id),
                        None => anonymous.pop_front(),
                    };
                    if let Some((command, call_ts, in_ws)) = matched {
                        let ts = line_ts.or(call_ts);
                        if in_ws && !ts.is_some_and(|t| t < since_ms) {
                            let output = extract_output_text(&entry["output"])
                                .or_else(|| extract_output_text(&entry["result"]))
                                .or_else(|| extract_output_text(&entry["content"]))
                                .unwrap_or_default();
                            let bytes = output.len() as u64;
                            let content_val = Value::String(output);
                            cov.record(
                                &session,
                                &command,
                                &content_val,
                                bytes,
                                exclusions,
                                crate::agents::codex_would_route,
                            );
                        }
                    }
                }
                _ => {}
            }

            // Shape 4: Embedded `toolFormerData` record.
            if let Some(tfd) = entry.get("toolFormerData").filter(|v| v.is_object())
                && let Some(name) = tfd["name"].as_str()
                && is_shell_tool_name(name)
            {
                let args_val = if !tfd["rawArgs"].is_null() {
                    &tfd["rawArgs"]
                } else {
                    &tfd["params"]
                };
                if let Some(command) = extract_command_from_args(args_val)
                    && line_cwd_ok
                    && !line_ts.is_some_and(|t| t < since_ms)
                    && let Some(output) = extract_output_text(&tfd["result"])
                {
                    let bytes = output.len() as u64;
                    let content_val = Value::String(output);
                    cov.record(
                        &session,
                        &command,
                        &content_val,
                        bytes,
                        exclusions,
                        crate::agents::codex_would_route,
                    );
                }
            }
            true
        });
    }
    cov
}

fn vscode_extension_task_dirs(env_override: &str, extension_id: &str) -> Vec<PathBuf> {
    if let Some(dir) = env_non_empty(env_override) {
        return vec![dir];
    }
    let mut roots = Vec::new();
    if let Some(xdg) = env_non_empty("XDG_CONFIG_HOME") {
        roots.push(xdg);
    }
    if let Some(home) = user_home() {
        roots.push(home.join(".config"));
        roots.push(home.join("Library/Application Support"));
    }
    if let Some(appdata) = env_non_empty("APPDATA") {
        roots.push(appdata);
    }
    let mut dirs = Vec::new();
    for base in roots {
        for editor in ["Code", "Code - Insiders", "Cursor"] {
            let candidate = base
                .join(editor)
                .join("User/globalStorage")
                .join(extension_id)
                .join("tasks");
            if candidate.is_dir() && !dirs.contains(&candidate) {
                dirs.push(candidate);
            }
        }
    }
    dirs
}

fn cline_coverage(
    workspace: &Workspace,
    exclusions: &Exclusions,
    since: SystemTime,
    since_ms: u64,
) -> AgentCoverage {
    let dirs = vscode_extension_task_dirs("CLINE_TASKS_DIR", "saoudrizwan.claude-dev");
    scan_cline_like_in(
        &dirs,
        "cline",
        "Cline",
        Some("init --agent cline"),
        workspace,
        exclusions,
        since,
        since_ms,
    )
}

fn roo_coverage(
    workspace: &Workspace,
    exclusions: &Exclusions,
    since: SystemTime,
    since_ms: u64,
) -> AgentCoverage {
    let dirs = vscode_extension_task_dirs("ROO_TASKS_DIR", "rooveterinaryinc.roo-cline");
    scan_cline_like_in(
        &dirs,
        "roo",
        "Roo Code",
        Some("init --agent roo"),
        workspace,
        exclusions,
        since,
        since_ms,
    )
}

fn extract_cline_cwd_from_text(text: &str) -> Option<&str> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# Current Working Directory (")
            && let Some(path) = rest.strip_suffix(')')
        {
            return Some(path.trim());
        }
        if let Some(rest) = trimmed.strip_prefix("Current Working Directory:") {
            return Some(rest.trim());
        }
    }
    None
}

fn extract_xml_commands(text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut rest = text;
    while let Some((_, after_open)) = rest.split_once("<execute_command>") {
        let Some((block, after_close)) = after_open.split_once("</execute_command>") else {
            break;
        };
        if let Some((_, after_cmd)) = block.split_once("<command>")
            && let Some((cmd, _)) = after_cmd.split_once("</command>")
        {
            let trimmed = cmd.trim();
            if !trimmed.is_empty() {
                commands.push(trimmed.to_owned());
            }
        }
        rest = after_close;
    }
    commands
}

#[allow(clippy::too_many_arguments)]
fn scan_cline_like_in(
    tasks_dirs: &[PathBuf],
    key: &'static str,
    name: &'static str,
    install_hint: Option<&'static str>,
    workspace: &Workspace,
    exclusions: &Exclusions,
    since: SystemTime,
    since_ms: u64,
) -> AgentCoverage {
    let mut cov = AgentCoverage::new(key, name, install_hint, false, tasks_dirs.first().cloned());
    for tasks_dir in tasks_dirs {
        let Ok(task_entries) = fs::read_dir(tasks_dir) else {
            continue;
        };
        for task_entry in task_entries.flatten() {
            let task_dir = task_entry.path();
            if !is_regular_dir(&task_dir) {
                continue;
            }
            let hist_path = task_dir.join("api_conversation_history.json");
            let Ok(meta) = fs::symlink_metadata(&hist_path) else {
                continue;
            };
            if !meta.is_file()
                || meta.len() > MAX_TRANSCRIPT_BYTES
                || !meta.modified().is_ok_and(|m| m >= since)
            {
                continue;
            }
            let task_id = task_entry.file_name().to_string_lossy().into_owned();
            // Optional `task_metadata.json` lets us reject unrelated workspaces early.
            let mut ws_match = DirMatch::InspectFile;
            if let Some(meta_raw) =
                read_bounded_utf8(&task_dir.join("task_metadata.json"), 1024 * 1024)
                && let Ok(meta_val) = serde_json::from_str::<Value>(&meta_raw)
            {
                let explicit: Vec<&str> = ["cwd", "workspace", "root", "projectRoot"]
                    .iter()
                    .filter_map(|k| meta_val[*k].as_str())
                    .collect();
                if !explicit.is_empty() {
                    ws_match = if explicit
                        .iter()
                        .any(|p| path_in_workspace(p, &workspace.root))
                    {
                        DirMatch::Matched
                    } else {
                        DirMatch::Rejected
                    };
                }
            }
            if ws_match == DirMatch::Rejected {
                continue;
            }
            let Some(raw) = read_bounded_utf8(&hist_path, MAX_TRANSCRIPT_BYTES) else {
                continue;
            };
            // Fast prefilter before JSON parsing.
            if !text_may_mention_shell_tool(&raw) {
                continue;
            }
            let Ok(Value::Array(messages)) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if ws_match == DirMatch::InspectFile {
                let mut found_in_ws = false;
                for msg in &messages {
                    if ["cwd", "workspace"].iter().any(|k| {
                        msg[*k]
                            .as_str()
                            .is_some_and(|p| path_in_workspace(p, &workspace.root))
                    }) {
                        found_in_ws = true;
                        break;
                    }
                    let check_text = |s: &str| {
                        extract_cline_cwd_from_text(s)
                            .is_some_and(|p| path_in_workspace(p, &workspace.root))
                    };
                    match &msg["content"] {
                        Value::String(s) if check_text(s) => {
                            found_in_ws = true;
                            break;
                        }
                        Value::Array(parts)
                            if parts
                                .iter()
                                .any(|p| p["text"].as_str().is_some_and(check_text)) =>
                        {
                            found_in_ws = true;
                            break;
                        }
                        _ => {}
                    }
                }
                if !found_in_ws {
                    continue;
                }
            }

            let mut pending_structured: HashMap<String, String> = HashMap::new();
            let mut pending_xml: VecDeque<String> = VecDeque::new();
            for msg in &messages {
                if let Some(ts) = parse_any_timestamp_ms(&msg["ts"])
                    .or_else(|| parse_any_timestamp_ms(&msg["timestamp"]))
                    && ts < since_ms
                {
                    continue;
                }
                let role = msg["role"].as_str().unwrap_or_default();
                match &msg["content"] {
                    Value::String(s) => {
                        if role == "assistant" {
                            for cmd in extract_xml_commands(s) {
                                pending_xml.push_back(cmd);
                            }
                        } else if role == "user"
                            && !pending_xml.is_empty()
                            && (s.contains("[execute_command") || s.contains("Command executed"))
                            && let Some(cmd) = pending_xml.pop_front()
                        {
                            let bytes = s.len() as u64;
                            let content = Value::String(s.clone());
                            cov.record(
                                &task_id,
                                &cmd,
                                &content,
                                bytes,
                                exclusions,
                                crate::agents::agy_would_route,
                            );
                        }
                    }
                    Value::Array(parts) => {
                        for part in parts {
                            match part["type"].as_str() {
                                Some("tool_use")
                                    if part["name"].as_str().is_some_and(is_shell_tool_name) =>
                                {
                                    if let (Some(id), Some(cmd)) = (
                                        part["id"].as_str(),
                                        extract_command_from_args(&part["input"]),
                                    ) {
                                        pending_structured.insert(id.to_owned(), cmd);
                                    }
                                }
                                Some("tool_result") => {
                                    if let Some(cmd) = part["tool_use_id"]
                                        .as_str()
                                        .and_then(|id| pending_structured.remove(id))
                                    {
                                        let content = &part["content"];
                                        let bytes = text_bytes(content);
                                        cov.record(
                                            &task_id,
                                            &cmd,
                                            content,
                                            bytes,
                                            exclusions,
                                            crate::agents::agy_would_route,
                                        );
                                    }
                                }
                                Some("text") => {
                                    if let Some(s) = part["text"].as_str() {
                                        if role == "assistant" {
                                            for cmd in extract_xml_commands(s) {
                                                pending_xml.push_back(cmd);
                                            }
                                        } else if role == "user"
                                            && !pending_xml.is_empty()
                                            && (s.contains("[execute_command")
                                                || s.contains("Command executed"))
                                            && let Some(cmd) = pending_xml.pop_front()
                                        {
                                            let bytes = s.len() as u64;
                                            let content = Value::String(s.to_owned());
                                            cov.record(
                                                &task_id,
                                                &cmd,
                                                &content,
                                                bytes,
                                                exclusions,
                                                crate::agents::agy_would_route,
                                            );
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    cov
}

fn claude_line_wanted(line: &str, calls: &HashMap<String, Call>) -> bool {
    if line.starts_with(CLAUDE_BLOB_PREFIX) {
        return false;
    }
    line.contains("\"permissionMode\"")
        || (line.contains("\"tool_use\"") && line.contains("\"Bash\""))
        || (!calls.is_empty()
            && line.contains("\"tool_result\"")
            && ids_may_match(calls.keys().map(String::as_str), calls.len(), line))
}

struct ClaudeScanContext<'a> {
    workspace: &'a Workspace,
    root: &'a str,
    first_date: &'a str,
    plan: bool,
    exclusions: &'a Exclusions,
    post_run: Option<&'a (Exclusions, (usize, u64))>,
    rules: Option<crate::claude_permissions::Rules>,
    session_modes: HashMap<String, BTreeSet<String>>,
    groups: BTreeMap<Route, Group>,
    unrouted: BTreeMap<String, PlanEntry>,
    sessions: BTreeSet<String>,
}

fn scan_claude_file(
    ctx: &mut ClaudeScanContext<'_>,
    file: &Path,
    session: &str,
    own: bool,
    use_prefilter: bool,
    mut parsed_lines: Option<&mut usize>,
) {
    // A subagent takes its session's mode when the session recorded only one.
    let mut mode = ctx
        .session_modes
        .get(session)
        .filter(|modes| modes.len() == 1)
        .and_then(|modes| modes.first().cloned());
    let mut calls: HashMap<String, Call> = HashMap::new();
    for_each_jsonl_line(file, Some(MAX_TRANSCRIPT_BYTES), |_, line| {
        if use_prefilter && !claude_line_wanted(line, &calls) {
            return true;
        }
        if let Some(count) = parsed_lines.as_deref_mut() {
            *count += 1;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return true;
        };
        if let Some(recorded) = entry["permissionMode"].as_str() {
            mode = Some(recorded.to_owned());
            if own {
                ctx.session_modes
                    .entry(session.to_owned())
                    .or_default()
                    .insert(recorded.to_owned());
            }
        }
        if entry["cwd"]
            .as_str()
            .is_some_and(|cwd| cwd != ctx.root && !cwd.starts_with(&format!("{}/", ctx.root)))
        {
            return true;
        }
        let date = entry["timestamp"]
            .as_str()
            .unwrap_or("")
            .get(..10)
            .unwrap_or("");
        let Some(parts) = entry["message"]["content"].as_array() else {
            return true;
        };
        for part in parts {
            match part["type"].as_str() {
                Some("tool_use") if part["name"] == "Bash" => {
                    if let (Some(id), Some(command)) =
                        (part["id"].as_str(), part["input"]["command"].as_str())
                    {
                        calls.insert(
                            id.to_owned(),
                            Call {
                                command: command.to_owned(),
                                date: date.to_owned(),
                                mode: mode.clone(),
                            },
                        );
                    }
                }
                Some("tool_result") => {
                    let Some(call) = part["tool_use_id"].as_str().and_then(|id| calls.remove(id))
                    else {
                        continue;
                    };
                    if call.date.as_str() < ctx.first_date
                        || command_disables_justless(&call.command)
                    {
                        continue;
                    }
                    let content = &part["content"];
                    // Keep only the program and a known subcommand.
                    let label = discover_label(&call.command);
                    let route = if routed(&call.command, &label, content) {
                        Route::ThroughJustless
                    } else {
                        let route = Route::unrouted(
                            &call.command,
                            &label,
                            ctx.exclusions,
                            true,
                            crate::agents::would_route,
                        );
                        // Without a recorded mode the hook rewrites as it always has.
                        let left = route == Route::SupportedNotRouted
                            && call.mode.as_deref().is_some_and(|mode| {
                                let rules = ctx.rules.get_or_insert_with(|| {
                                    let root = &ctx.workspace.root;
                                    crate::claude_permissions::load(
                                        &crate::claude_permissions::Sources::locate(root, root),
                                    )
                                });
                                crate::agents::left_by_permissions(&call.command, mode, rules)
                            });
                        let route = if left {
                            Route::LeftByPermissions
                        } else {
                            route
                        };
                        after_running(
                            route,
                            &call.command,
                            &entry,
                            part,
                            ctx.post_run,
                            &ctx.workspace.root,
                        )
                    };
                    let bytes = text_bytes(content);
                    // Excluded commands stay as written by choice.
                    if ctx.plan && !matches!(route, Route::ThroughJustless | Route::Excluded) {
                        let entry = ctx.unrouted.entry(label.clone()).or_default();
                        entry.commands += 1;
                        entry.bytes += bytes;
                        if route == Route::LeftByPermissions {
                            entry.left_commands += 1;
                            entry.left_bytes += bytes;
                        }
                        if route == Route::CompactedAfterRunning {
                            entry.after_commands += 1;
                            entry.after_bytes += bytes;
                        }
                        // The command hook would route it, or the post-run hook would compact it.
                        if matches!(
                            route,
                            Route::SupportedNotRouted | Route::CompactedAfterRunning
                        ) {
                            entry.now_commands += 1;
                            entry.now_bytes += bytes;
                        }
                    }
                    let group = ctx.groups.entry(route).or_default();
                    group.commands += 1;
                    group.output_bytes += bytes;
                    let label = group.labels.entry(label).or_default();
                    label.0 += 1;
                    label.1 += bytes;
                    ctx.sessions.insert(session.to_owned());
                }
                _ => {}
            }
        }
        true
    });
}

struct DiscoverReport {
    claude_dir: Option<PathBuf>,
    claude_sessions: BTreeSet<String>,
    claude_groups: BTreeMap<Route, Group>,
    claude_unrouted: BTreeMap<String, PlanEntry>,
    claude_unreadable_permissions: bool,
    codex: crate::rollouts::Coverage,
    agy: crate::agy_sessions::Coverage,
    agents: Vec<AgentCoverage>,
}

impl DiscoverReport {
    fn all_clients(&self) -> Vec<(&'static str, &BTreeMap<Route, Group>)> {
        let mut clients: Vec<(&'static str, &BTreeMap<Route, Group>)> = vec![
            ("claude", &self.claude_groups),
            ("codex", &self.codex.groups),
            ("agy", &self.agy.groups),
        ];
        for agent in &self.agents {
            clients.push((agent.key, &agent.groups));
        }
        clients
    }

    fn to_json(&self, days: u64, plan: bool) -> Value {
        let total: u64 = self.claude_groups.values().map(|g| g.commands).sum();
        let output: u64 = self.claude_groups.values().map(|g| g.output_bytes).sum();
        let clients = self.all_clients();
        let mut report = serde_json::json!({
            "days": days,
            "transcripts": self.claude_dir,
            "sessions": self.claude_sessions.len(),
            "commands": total,
            "output_bytes": output,
            "token_estimate": format!(
                "UTF-8 bytes / {:.2}, the bytes per token measured for command output",
                crate::tokens::Content::Logs.bytes_per_token()
            ),
            "plan": plan.then(|| {
                let mut entries: Vec<_> = self.claude_unrouted.iter().collect();
                entries.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(a.0.cmp(b.0)));
                entries.into_iter().map(|(label, e)| serde_json::json!({
                    "label": label,
                    "commands": e.commands,
                    "output_bytes": e.bytes,
                    "compactable_now_commands": e.now_commands,
                    "compactable_now_bytes": e.now_bytes,
                    "compacted_after_running_commands": e.after_commands,
                    "compacted_after_running_bytes": e.after_bytes,
                    "left_by_permissions_commands": e.left_commands,
                    "left_by_permissions_bytes": e.left_bytes,
                })).collect::<Vec<_>>()
            }),
            "groups": groups_json(&self.claude_groups),
            "what_if": what_if_json(&self.claude_groups),
            "top_opportunities": opportunities_json_for(&clients),
            "codex": self.codex.json(),
            "agy": self.agy.json(),
        });
        if let Some(obj) = report.as_object_mut() {
            for agent in &self.agents {
                obj.insert(agent.key.to_owned(), agent.json());
            }
        }
        report
    }

    fn to_text(&self, workspace: &Workspace, days: u64, plan: bool) -> String {
        let mut out = String::new();
        let total: u64 = self.claude_groups.values().map(|g| g.commands).sum();
        let output: u64 = self.claude_groups.values().map(|g| g.output_bytes).sum();
        let clients = self.all_clients();

        if total == 0 {
            if let Some(dir) = &self.claude_dir {
                let _ = writeln!(
                    out,
                    "No Claude Code shell commands found for this workspace in the last {days} day(s) under {}.",
                    dir.display()
                );
            } else {
                let _ = writeln!(
                    out,
                    "No Claude Code shell commands found for this workspace in the last {days} day(s)."
                );
            }
        } else {
            let _ = writeln!(
                out,
                "Claude Code ran {total} shell command(s) here in the last {days} day(s), across {} session(s), returning {} of output.",
                self.claude_sessions.len(),
                human_bytes(output)
            );
            write_groups(&mut out, &self.claude_groups);
            write_what_if(&mut out, &self.claude_groups);
            if plan {
                let mut entries: Vec<_> = self.claude_unrouted.iter().collect();
                entries.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(a.0.cmp(b.0)));
                let _ = writeln!(
                    out,
                    "\nUnrouted output by command family, largest first (labels only, no commands):"
                );
                for (label, entry) in entries.iter().take(12) {
                    let after = if entry.after_bytes > 0 {
                        format!(", {} of it after running", human_bytes(entry.after_bytes))
                    } else {
                        String::new()
                    };
                    let left = if entry.left_bytes > 0 {
                        format!(
                            " · {} left to permission rules",
                            human_bytes(entry.left_bytes)
                        )
                    } else {
                        String::new()
                    };
                    let now = if entry.now_bytes > 0 {
                        format!(
                            " · {} would compact now with a hook or `run --`{after}",
                            human_bytes(entry.now_bytes)
                        )
                    } else {
                        String::new()
                    };
                    let _ = writeln!(
                        out,
                        "  {label} ×{} ({}){now}{left}",
                        entry.commands,
                        human_bytes(entry.bytes)
                    );
                }
            }
            if self.claude_groups.contains_key(&Route::SupportedNotRouted)
                || self
                    .claude_groups
                    .contains_key(&Route::CompactedAfterRunning)
            {
                let _ = writeln!(
                    out,
                    "\nSupported commands reached the agent unfiltered. Install or refresh hooks with `{} init --hooks --agent claude`, then start a new session.",
                    workspace.justless()
                );
            }
            if self.claude_groups.contains_key(&Route::LeftByPermissions) {
                let _ = writeln!(
                    out,
                    "\nThe command hook leaves a supported command for Claude Code to judge when an ask or deny rule might match it, and in default, acceptEdits and dontAsk modes when no allow rule covers it. Allow the commands you run, such as Bash(cargo test *), rather than Bash(justless …), to route them.{}",
                    if self.claude_unreadable_permissions {
                        " A Claude Code settings file cannot be read, so the hook leaves every command; `justless doctor` names it."
                    } else {
                        ""
                    }
                );
            }
        }
        write_codex(&mut out, workspace, &self.codex, days, plan);
        write_agy(&mut out, workspace, &self.agy, days, plan);
        for agent in &self.agents {
            write_agent(&mut out, workspace, agent, days, plan);
        }
        if plan {
            write_top_opportunities(&mut out, &clients);
        }
        out
    }
}

pub fn discover(workspace: &Workspace, days: u64, as_json: bool, plan: bool) -> Result<()> {
    let window = Duration::from_secs(days.saturating_mul(86_400));
    let since = SystemTime::now()
        .checked_sub(window)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let since_ms = now_ms().saturating_sub(days.saturating_mul(86_400_000));
    let first_date = utc_date(since_ms);
    let claude_dir = transcript_dir(&workspace.root);
    let exclusions = Exclusions::of(workspace).unwrap_or_default();
    let codex = crate::rollouts::coverage(workspace, since_ms);
    let agy = crate::agy_sessions::coverage(workspace, since_ms);
    let agents = vec![
        gemini_coverage(workspace, &exclusions, since, since_ms),
        opencode_coverage(workspace, &exclusions, since, since_ms),
        cursor_coverage(workspace, &exclusions, since, since_ms),
        cline_coverage(workspace, &exclusions, since, since_ms),
        roo_coverage(workspace, &exclusions, since, since_ms),
    ];
    let any_agent_home = claude_dir.is_some()
        || codex.scan.home.is_some()
        || agy.scan.home.is_some()
        || agents.iter().any(|a| a.root.is_some());
    if !any_agent_home {
        anyhow::bail!("cannot locate the Claude Code configuration directory");
    }

    // The post-run hook leaves every result while either fails to load.
    let post_run = Exclusions::of(workspace).ok().zip(
        workspace
            .settings()
            .ok()
            .map(|settings| (settings.context_budget, settings.read_outline_lines)),
    );
    let root = workspace.root.to_string_lossy().into_owned();
    let mut ctx = ClaudeScanContext {
        workspace,
        root: &root,
        first_date: &first_date,
        plan,
        exclusions: &exclusions,
        post_run: post_run.as_ref(),
        rules: None,
        session_modes: HashMap::new(),
        groups: BTreeMap::new(),
        unrouted: BTreeMap::new(),
        sessions: BTreeSet::new(),
    };
    if let Some(dir) = &claude_dir {
        // Sessions keep subagent transcripts under <session>/subagents/, and workflow
        // subagents one level deeper under subagents/workflows/<run>/.
        let mut files = Vec::new();
        transcripts(dir, since, 6, &mut files);
        // Each session's own transcript first, so its subagents can take its mode.
        files.sort_by_cached_key(|file| (file.parent() != Some(dir.as_path()), file.clone()));
        for file in &files {
            // <session>.jsonl and everything under <session>/ belong to one session.
            let session = file
                .strip_prefix(dir)
                .ok()
                .and_then(|relative| relative.components().next())
                .map(|first| {
                    let name = first.as_os_str().to_string_lossy();
                    name.strip_suffix(".jsonl").unwrap_or(&name).to_owned()
                })
                .unwrap_or_default();
            let own = file.parent() == Some(dir.as_path());
            scan_claude_file(&mut ctx, file, &session, own, true, None);
        }
    }

    let report = DiscoverReport {
        claude_dir,
        claude_sessions: ctx.sessions,
        claude_groups: ctx.groups,
        claude_unrouted: ctx.unrouted,
        claude_unreadable_permissions: ctx.rules.as_ref().is_some_and(|r| !r.broken().is_empty()),
        codex,
        agy,
        agents,
    };
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report.to_json(days, plan))?
        );
    } else {
        print!("{}", report.to_text(workspace, days, plan));
    }
    Ok(())
}

pub(crate) fn groups_json(groups: &BTreeMap<Route, Group>) -> Vec<Value> {
    groups
        .iter()
        .map(|(route, g)| {
            serde_json::json!({
                "route": route,
                "commands": g.commands,
                "output_bytes": g.output_bytes,
                "estimated_tokens": estimated_tokens(g.output_bytes),
                "labels": g.labels.iter().map(|(label, (n, b))| serde_json::json!({
                    "label": label, "commands": n, "output_bytes": b
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// The commands that did not go through Justless, as the current build would
/// treat them: routed before running, compacted after, or left as written. Each
/// part is a command count and its output bytes.
fn what_if(groups: &BTreeMap<Route, Group>) -> [(u64, u64); 3] {
    let mut parts = [(0, 0); 3];
    for (route, group) in groups {
        let part = match route {
            Route::ThroughJustless => continue,
            Route::SupportedNotRouted => 0,
            Route::CompactedAfterRunning => 1,
            _ => 2,
        };
        parts[part].0 += group.commands;
        parts[part].1 += group.output_bytes;
    }
    parts
}

fn what_if_json(groups: &BTreeMap<Route, Group>) -> Value {
    let [routed, compacted, written] = what_if(groups);
    let part = |(commands, bytes): (u64, u64)| serde_json::json!({"commands": commands, "output_bytes": bytes});
    serde_json::json!({
        "commands": routed.0 + compacted.0 + written.0,
        "output_bytes": routed.1 + compacted.1 + written.1,
        "routed_before_running": part(routed),
        "compacted_after_running": part(compacted),
        "as_written": part(written),
        "basis": "output bytes of the commands that did not go through Justless, as the current build's hooks would treat them; not savings",
    })
}

fn write_what_if(out: &mut String, groups: &BTreeMap<Route, Group>) {
    let [routed, compacted, written] = what_if(groups);
    let commands = routed.0 + compacted.0 + written.0;
    if commands == 0 {
        return;
    }
    let part = |(commands, bytes): (u64, u64)| format!("{commands} ({})", human_bytes(bytes));
    let _ = writeln!(
        out,
        "\nWith the current build, of the {commands} command(s) that did not go through Justless ({} output): the command hook would route {}, the post-run hook would compact {}, and {} would reach the agent as written. These are output sizes, not savings.",
        human_bytes(routed.1 + compacted.1 + written.1),
        part(routed),
        part(compacted),
        part(written),
    );
}

fn write_groups(out: &mut String, groups: &BTreeMap<Route, Group>) {
    for (route, group) in groups {
        let _ = writeln!(
            out,
            "\n{}: {} command(s) · {} output · ≈{} tokens",
            route.title(),
            group.commands,
            human_bytes(group.output_bytes),
            estimated_tokens(group.output_bytes)
        );
        let mut labels: Vec<_> = group.labels.iter().collect();
        labels.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(b.0)));
        let shown: Vec<_> = labels
            .iter()
            .take(8)
            .map(|(label, (n, bytes))| format!("{label} ×{n} ({})", human_bytes(*bytes)))
            .collect();
        let _ = writeln!(out, "  {}", shown.join(" · "));
        if labels.len() > 8 {
            let _ = writeln!(out, "  +{} more command type(s)", labels.len() - 8);
        }
    }
}

/// The Codex part of the report, when Codex threads started in this workspace.
fn write_codex(
    out: &mut String,
    workspace: &Workspace,
    codex: &crate::rollouts::Coverage,
    days: u64,
    plan: bool,
) {
    let scan = &codex.scan;
    if scan.files == 0 {
        return;
    }
    let total = codex.commands();
    let _ = writeln!(
        out,
        "\nCodex ran {total} shell command(s) here in the last {days} day(s), across {} session(s), returning {} of output.",
        scan.sessions.len(),
        human_bytes(codex.bytes())
    );
    if codex.nested > 0 {
        let _ = writeln!(
            out,
            "{} of them ran inside code-mode scripts, which received {} of that output.",
            codex.nested,
            human_bytes(codex.nested_bytes)
        );
    }
    if scan.scripts > 0 {
        let _ = writeln!(
            out,
            "Code-mode scripts returned {} to the model in {} result(s){}.",
            human_bytes(scan.script_bytes),
            scan.scripts,
            if codex.nested == 0 {
                "; this Codex did not record the commands they ran"
            } else {
                ""
            }
        );
    }
    write_groups(out, &codex.groups);
    write_client_what_if(out, "Codex", true, &codex.groups);
    if plan {
        write_unrouted_plan(
            out,
            &codex.groups,
            "Unrouted Codex output by command family, largest first (labels only, no commands):",
        );
    }
    if codex.groups.contains_key(&Route::SupportedNotRouted) {
        let _ = writeln!(
            out,
            "\nSupported commands reached Codex unfiltered. Install or refresh hooks with `{} init --hooks --agent codex`, trust them with /hooks in Codex, then start a new session.",
            workspace.justless()
        );
    }
}

/// The Antigravity CLI part of the report, when Antigravity CLI sessions ran in this workspace.
fn write_agy(
    out: &mut String,
    workspace: &Workspace,
    agy: &crate::agy_sessions::Coverage,
    days: u64,
    plan: bool,
) {
    let total = agy.commands();
    if total == 0 {
        return;
    }
    let _ = writeln!(
        out,
        "\nAntigravity CLI ran {total} shell command(s) here in the last {days} day(s), across {} session(s), returning {} of output.",
        agy.scan.sessions.len(),
        human_bytes(agy.bytes())
    );
    write_groups(out, &agy.groups);
    write_client_what_if(out, "Antigravity CLI", true, &agy.groups);
    if plan {
        write_unrouted_plan(
            out,
            &agy.groups,
            "Unrouted Antigravity CLI output by command family, largest first (labels only, no commands):",
        );
    }
    if agy.groups.contains_key(&Route::SupportedNotRouted) {
        let _ = writeln!(
            out,
            "\nSupported commands reached Antigravity CLI unfiltered. Install or refresh hooks with `{} init --hooks --agent agy` (or `{} init --global --agent agy`), then start a new session.",
            workspace.justless(),
            workspace.justless()
        );
    }
}

/// Report section for Gemini CLI, OpenCode, Cursor, Cline, or Roo Code when present.
fn write_agent(
    out: &mut String,
    workspace: &Workspace,
    agent: &AgentCoverage,
    days: u64,
    plan: bool,
) {
    let total = agent.commands();
    if total == 0 {
        return;
    }
    let _ = writeln!(
        out,
        "\n{} ran {total} shell command(s) here in the last {days} day(s), across {} session(s), returning {} of output.",
        agent.name,
        agent.sessions.len(),
        human_bytes(agent.bytes())
    );
    write_groups(out, &agent.groups);
    write_client_what_if(out, agent.name, agent.hook_based, &agent.groups);
    if plan {
        let heading = format!(
            "Unrouted {} output by command family, largest first (labels only, no commands):",
            agent.name
        );
        write_unrouted_plan(out, &agent.groups, &heading);
    }
    if agent.groups.contains_key(&Route::SupportedNotRouted)
        && let Some(hint) = agent.install_hint
    {
        if agent.hook_based {
            let _ = writeln!(
                out,
                "\nSupported commands reached {} unfiltered. Install or refresh hooks with `{} {hint}`, then start a new session.",
                agent.name,
                workspace.justless()
            );
        } else {
            let _ = writeln!(
                out,
                "\nSupported commands reached {} unfiltered. Install project instructions with `{} {hint}` so it runs commands through `justless run --`.",
                agent.name,
                workspace.justless()
            );
        }
    }
}

/// What-if split for clients without a post-run Bash hook (Codex, Antigravity CLI,
/// Gemini CLI, OpenCode, Cursor, Cline, Roo Code).
fn write_client_what_if(
    out: &mut String,
    client: &str,
    hook_based: bool,
    groups: &BTreeMap<Route, Group>,
) {
    let [routed, compacted, written] = what_if(groups);
    let commands = routed.0 + compacted.0 + written.0;
    if commands == 0 {
        return;
    }
    let part = |(commands, bytes): (u64, u64)| format!("{commands} ({})", human_bytes(bytes));
    let route_verb = if hook_based {
        "hooks would route"
    } else {
        "`justless run --` would compact"
    };
    let _ = writeln!(
        out,
        "\nWith the current build, of the {commands} {client} command(s) that did not go through Justless ({} output): {route_verb} {}, and {} would reach {client} as written. These are output sizes, not savings.",
        human_bytes(routed.1 + compacted.1 + written.1),
        part(routed),
        part((written.0 + compacted.0, written.1 + compacted.1)),
    );
}

#[derive(Default)]
struct Opportunity {
    commands: u64,
    output_bytes: u64,
    clients: BTreeSet<&'static str>,
}

fn collect_opportunities(
    clients: &[(&'static str, &BTreeMap<Route, Group>)],
    predicate: impl Fn(Route) -> bool,
) -> Vec<(String, Opportunity)> {
    let mut map: BTreeMap<String, Opportunity> = BTreeMap::new();
    for &(client, groups) in clients {
        for (&route, group) in groups {
            if !predicate(route) {
                continue;
            }
            for (label, &(commands, bytes)) in &group.labels {
                let entry = map.entry(label.clone()).or_default();
                entry.commands += commands;
                entry.output_bytes += bytes;
                entry.clients.insert(client);
            }
        }
    }
    let mut ranked: Vec<_> = map.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.output_bytes
            .cmp(&a.1.output_bytes)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
}

fn opportunities_json_for(clients: &[(&'static str, &BTreeMap<Route, Group>)]) -> Vec<Value> {
    collect_opportunities(clients, |route| {
        matches!(
            route,
            Route::SupportedNotRouted | Route::CompactedAfterRunning
        )
    })
    .into_iter()
    .take(10)
    .map(|(label, opp)| {
        let clients: Vec<&str> = opp.clients.into_iter().collect();
        serde_json::json!({
            "label": label,
            "commands": opp.commands,
            "output_bytes": opp.output_bytes,
            "estimated_tokens": estimated_tokens(opp.output_bytes),
            "clients": clients,
        })
    })
    .collect()
}

#[cfg(test)]
fn opportunities_json(
    claude: &BTreeMap<Route, Group>,
    codex: &BTreeMap<Route, Group>,
    agy: &BTreeMap<Route, Group>,
) -> Vec<Value> {
    opportunities_json_for(&[("claude", claude), ("codex", codex), ("agy", agy)])
}

fn write_top_opportunities(out: &mut String, clients: &[(&'static str, &BTreeMap<Route, Group>)]) {
    let compactable = collect_opportunities(clients, |route| {
        matches!(
            route,
            Route::SupportedNotRouted | Route::CompactedAfterRunning
        )
    });
    if !compactable.is_empty() {
        let _ = writeln!(out, "\nTop opportunities (would compact now, labels only):");
        for (label, opp) in compactable.into_iter().take(5) {
            let clients: Vec<&str> = opp.clients.into_iter().collect();
            let _ = writeln!(
                out,
                "  {label} ×{} ({}, ≈{} tokens) [{}]",
                opp.commands,
                human_bytes(opp.output_bytes),
                estimated_tokens(opp.output_bytes),
                clients.join(", ")
            );
        }
    }
    let unsupported: Vec<_> = collect_opportunities(clients, |route| route == Route::NotSupported)
        .into_iter()
        .filter(|(_, opp)| opp.output_bytes > 0)
        .collect();
    if !unsupported.is_empty() {
        let _ = writeln!(out, "\nLargest unsupported command families (labels only):");
        for (label, opp) in unsupported.into_iter().take(3) {
            let clients: Vec<&str> = opp.clients.into_iter().collect();
            let _ = writeln!(
                out,
                "  {label} ×{} ({}, ≈{} tokens) [{}]",
                opp.commands,
                human_bytes(opp.output_bytes),
                estimated_tokens(opp.output_bytes),
                clients.join(", ")
            );
        }
    }
}

fn write_unrouted_plan(out: &mut String, groups: &BTreeMap<Route, Group>, heading: &str) {
    let mut ranked: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new();
    for (route, group) in groups {
        if matches!(route, Route::ThroughJustless | Route::Excluded) {
            continue;
        }
        for (label, &(n, bytes)) in &group.labels {
            let entry = ranked.entry(label.as_str()).or_default();
            entry.0 += n;
            entry.1 += bytes;
            if *route == Route::SupportedNotRouted {
                entry.2 += bytes;
            }
        }
    }
    if ranked.is_empty() {
        return;
    }
    let mut entries: Vec<_> = ranked.into_iter().collect();
    entries.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(b.0)));
    let _ = writeln!(out, "\n{heading}");
    for (label, (commands, bytes, now_bytes)) in entries.into_iter().take(12) {
        let now = if now_bytes > 0 {
            format!(
                " · {} would compact now with a hook or `run --`",
                human_bytes(now_bytes)
            )
        } else {
            String::new()
        };
        let _ = writeln!(out, "  {label} ×{commands} ({}){now}", human_bytes(bytes));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::{
        FOOTER, Group, Route, discover_label, opportunities_json, routed, split_shell_stages,
    };

    #[test]
    fn footers_with_secret_counts_and_split_streams_count_as_routed() {
        for line in [
            "[justless run-Ab1] exit 0 · 10 of 20 output bytes · justless log run-Ab1",
            "[justless run-Ab1 · exit 2]",
            "[justless run-Ab1 · 2.0 KB of 9.1 KB shown]",
            "[justless run-Ab1 · 2.0 KB of 9.1 KB shown · exit 101]",
            "[justless run-Ab1 · 1 secret redacted]",
            "[justless run-Ab1 · 2 secrets redacted · exit 1]",
            "[justless run-Ab1 · 2.0 KB of 9.1 KB shown · 1 secret redacted · exit 2]",
            "[justless run-Ab1 · stderr after stdout]",
            "[justless run-Ab1 · 1 secret redacted · stderr after stdout · exit 2]",
        ] {
            assert!(FOOTER.is_match(&format!("output\n{line}")), "{line}");
        }
        for line in [
            "[justless run-Ab1 · stderr after stdout here]",
            "[justless run-Ab1 · secrets redacted]",
            "see [justless run-Ab1 · exit 2]",
        ] {
            assert!(!FOOTER.is_match(&format!("output\n{line}")), "{line}");
        }
    }

    #[test]
    fn split_shell_stages_preserves_quotes_escapes_and_fd_merges() {
        assert_eq!(
            split_shell_stages("rg 'foo|bar' src | wc -l"),
            vec!["rg 'foo|bar' src", "wc -l"]
        );
        assert_eq!(
            split_shell_stages("rg 'a;b' src && echo ok"),
            vec!["rg 'a;b' src", "echo ok"]
        );
        assert_eq!(
            split_shell_stages("cargo test 2>&1 | head -n 20"),
            vec!["cargo test 2>&1", "head -n 20"]
        );
        assert_eq!(
            split_shell_stages("RUST_LOG=1 timeout 60 cargo build 2>&1 | tail -n 30"),
            vec!["RUST_LOG=1 timeout 60 cargo build 2>&1", "tail -n 30"]
        );
        assert_eq!(
            split_shell_stages(r#"rg "a|b\"|c" src | sort"#),
            vec![r#"rg "a|b\"|c" src"#, "sort"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unrouted_chains_with_routable_segments_count_as_passed_through() {
        let none = crate::rewrite::Exclusions::default();
        assert_eq!(
            Route::unrouted(
                "cargo fmt --all -- --check && python3 scripts/check.py",
                "cargo fmt",
                &none,
                false,
                crate::agents::codex_would_route,
            ),
            Route::PassedThrough
        );
        assert_eq!(
            Route::unrouted(
                "cargo fmt && python3 scripts/check.py",
                "cargo fmt",
                &none,
                false,
                crate::agents::codex_would_route,
            ),
            Route::NotSupported
        );
        assert_eq!(
            Route::unrouted(
                "ls -la | grep foo",
                "ls",
                &none,
                false,
                crate::agents::codex_would_route,
            ),
            Route::PassedThrough
        );
    }

    #[cfg(unix)]
    #[test]
    fn wave2_routing_rules_classify_accurately_across_claude_codex_and_agy() {
        let none = crate::rewrite::Exclusions::default();
        let claude = |cmd: &str| {
            let label = discover_label(cmd);
            Route::unrouted(cmd, &label, &none, true, crate::agents::would_route)
        };
        let codex = |cmd: &str| {
            let label = discover_label(cmd);
            Route::unrouted(cmd, &label, &none, false, crate::agents::codex_would_route)
        };
        let agy = |cmd: &str| {
            let label = discover_label(cmd);
            Route::unrouted(cmd, &label, &none, false, crate::agents::agy_would_route)
        };

        // Single-quoted regexes: direct invocations route across all 3 clients;
        // `rg 'foo|bar' src` is left by Claude's command hook to `after-bash` as a
        // read list (PassedThrough before `after_running`) while Codex & Agy route it;
        // piped to an unsupported consumer stays PassedThrough (never NotSupported).
        for cmd in [
            r"rg '\bfoo\b' src",
            "rg 'foo$' src",
            r"git grep '\bTODO\b'",
            "go test ./... -run '^TestFoo$'",
        ] {
            assert_eq!(claude(cmd), Route::SupportedNotRouted, "claude {cmd}");
            assert_eq!(codex(cmd), Route::SupportedNotRouted, "codex {cmd}");
            assert_eq!(agy(cmd), Route::SupportedNotRouted, "agy {cmd}");
        }
        assert_eq!(claude("rg 'foo|bar' src"), Route::PassedThrough);
        assert_eq!(codex("rg 'foo|bar' src"), Route::SupportedNotRouted);
        assert_eq!(agy("rg 'foo|bar' src"), Route::SupportedNotRouted);
        assert_eq!(claude("rg 'foo|bar' src | wc -l"), Route::PassedThrough);
        assert_eq!(codex("rg 'foo|bar' src | wc -l"), Route::PassedThrough);
        assert_eq!(agy("rg 'foo|bar' src | wc -l"), Route::PassedThrough);
        assert_eq!(
            codex("rg 'foo|bar' src | head -n 20"),
            Route::SupportedNotRouted
        );
        assert_eq!(
            agy("rg 'foo|bar' src | head -n 20"),
            Route::SupportedNotRouted
        );

        // `2>&1 | head` and scoped `| tail`: Codex & Agy route build/test/lint;
        // search behind `| tail` stays PassedThrough.
        for cmd in [
            "cargo test 2>&1 | head -n 40",
            "cargo test 2>&1 | tail -n 40",
            "timeout 60 cargo test 2>&1 | tail -n 20",
        ] {
            assert_eq!(claude(cmd), Route::PassedThrough, "claude {cmd}");
            assert_eq!(codex(cmd), Route::SupportedNotRouted, "codex {cmd}");
            assert_eq!(agy(cmd), Route::SupportedNotRouted, "agy {cmd}");
        }
        assert_eq!(codex("rg foo src | tail -n 20"), Route::PassedThrough);
        assert_eq!(agy("rg foo src | tail -n 20"), Route::PassedThrough);

        // Safe Wave-2 wrappers (`timeout`, `nice`, `stdbuf`, `env`).
        for cmd in [
            "timeout 60 cargo test",
            "nice -n 5 cargo test",
            "stdbuf -oL cargo test",
            "env RUST_BACKTRACE=1 cargo test",
        ] {
            assert_eq!(claude(cmd), Route::SupportedNotRouted, "claude {cmd}");
            assert_eq!(codex(cmd), Route::SupportedNotRouted, "codex {cmd}");
            assert_eq!(agy(cmd), Route::SupportedNotRouted, "agy {cmd}");
        }
        assert_eq!(claude("env FOO=bar cargo test"), Route::SupportedNotRouted);
        assert_eq!(agy("env FOO=bar cargo test"), Route::SupportedNotRouted);
        assert_eq!(codex("env FOO=bar cargo test"), Route::PassedThrough);

        // Wave-2 tools (`kubectl get events|svc`, `podman`, `bazel`, `zig`, `unittest`).
        for cmd in [
            "kubectl get events",
            "kubectl get svc",
            "podman images",
            "podman build .",
            "bazel test //...",
            "zig build",
            "python3 -m unittest discover",
        ] {
            assert_eq!(claude(cmd), Route::SupportedNotRouted, "claude {cmd}");
            assert_eq!(agy(cmd), Route::SupportedNotRouted, "agy {cmd}");
        }

        // Watch/machine-format/redirect pass-throughs of multi-word supported tools
        // count as PassedThrough rather than NotSupported; unsupported secrets stay NotSupported.
        for cmd in [
            "kubectl get events -w",
            "kubectl get svc -o yaml",
            "timeout 30 kubectl get events -w",
            "python3 -m unittest discover > out.log",
            "podman build --push .",
            "podman image ls > out.txt",
        ] {
            assert_eq!(claude(cmd), Route::PassedThrough, "claude {cmd}");
            assert_eq!(agy(cmd), Route::PassedThrough, "agy {cmd}");
        }
        assert_eq!(claude("kubectl get secrets"), Route::NotSupported);
        assert_eq!(codex("kubectl get secrets"), Route::NotSupported);
        assert_eq!(agy("kubectl get secrets"), Route::NotSupported);

        // Exclusion still matches wrapped single-quoted piped commands.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".justless.toml"),
            "exclude_commands = [\"rg\"]\n",
        )
        .unwrap();
        let ws = crate::workspace::Workspace::discover(Some(dir.path())).unwrap();
        let excl = crate::rewrite::Exclusions::of(&ws).unwrap();
        assert_eq!(
            Route::unrouted(
                "timeout 30 rg 'foo|bar' src | wc -l",
                "rg",
                &excl,
                true,
                crate::agents::would_route,
            ),
            Route::Excluded
        );
    }

    #[cfg(unix)]
    #[test]
    fn routed_detects_windowed_shell_read_outlines() {
        let windowed = Value::String(
            "revision a1b2c3d4e5f60718 · 500 lines · window L10-500\n#1 L10-40 function main\n"
                .into(),
        );
        assert!(routed("sed -n '10,$p' src/big.rs", "sed", &windowed));
        assert!(routed("head -n200 src/big.rs", "head", &windowed));
        let plain = Value::String("fn main() {}\n".into());
        assert!(!routed("sed -n '10,$p' src/big.rs", "sed", &plain));
    }

    #[test]
    fn discover_label_refines_allowlisted_commands_without_leaking_arguments() {
        assert_eq!(
            discover_label("python3 -m unittest discover -s secret_dir"),
            "python3 -m unittest"
        );
        assert_eq!(
            discover_label("timeout 30 python3 -m unittest tests/test_secret.py"),
            "python3 -m unittest"
        );
        assert_eq!(
            discover_label("kubectl get events -n prod-secret"),
            "kubectl get events"
        );
        assert_eq!(
            discover_label("kubectl get svc my-secret-service"),
            "kubectl get svc"
        );
        assert_eq!(
            discover_label("kubectl get secrets my-secret"),
            "kubectl get"
        );
        assert_eq!(discover_label("python3 secret_script.py"), "python3");
    }

    #[test]
    fn opportunities_json_ranks_compactable_families_across_clients_with_labels_only() {
        let mut claude = BTreeMap::new();
        claude.insert(
            Route::SupportedNotRouted,
            Group {
                commands: 2,
                output_bytes: 8_000,
                labels: BTreeMap::from([("cargo test".into(), (2, 8_000))]),
            },
        );
        claude.insert(
            Route::CompactedAfterRunning,
            Group {
                commands: 1,
                output_bytes: 12_000,
                labels: BTreeMap::from([("sed".into(), (1, 12_000))]),
            },
        );
        claude.insert(
            Route::NotSupported,
            Group {
                commands: 5,
                output_bytes: 50_000,
                labels: BTreeMap::from([("python3".into(), (5, 50_000))]),
            },
        );

        let mut codex = BTreeMap::new();
        codex.insert(
            Route::SupportedNotRouted,
            Group {
                commands: 3,
                output_bytes: 10_000,
                labels: BTreeMap::from([("cargo test".into(), (3, 10_000))]),
            },
        );

        let mut agy = BTreeMap::new();
        agy.insert(
            Route::SupportedNotRouted,
            Group {
                commands: 1,
                output_bytes: 5_000,
                labels: BTreeMap::from([("bazel test".into(), (1, 5_000))]),
            },
        );

        let opps = opportunities_json(&claude, &codex, &agy);
        assert_eq!(opps.len(), 3, "{opps:?}");
        assert_eq!(opps[0]["label"], "cargo test");
        assert_eq!(opps[0]["commands"], 5);
        assert_eq!(opps[0]["output_bytes"], 18_000);
        assert_eq!(opps[0]["clients"], serde_json::json!(["claude", "codex"]));
        assert!(opps[0]["estimated_tokens"].as_u64().unwrap() > 0);
        assert_eq!(opps[1]["label"], "sed");
        assert_eq!(opps[2]["label"], "bazel test");
    }

    #[cfg(unix)]
    #[test]
    fn mixed_var_chains_classify_as_supported_not_routed_for_claude_and_agy() {
        let none = crate::rewrite::Exclusions::default();
        let claude = |cmd: &str| {
            let label = discover_label(cmd);
            Route::unrouted(cmd, &label, &none, true, crate::agents::would_route)
        };
        let codex = |cmd: &str| {
            let label = discover_label(cmd);
            Route::unrouted(cmd, &label, &none, false, crate::agents::codex_would_route)
        };
        let agy = |cmd: &str| {
            let label = discover_label(cmd);
            Route::unrouted(cmd, &label, &none, false, crate::agents::agy_would_route)
        };

        for cmd in [
            "cargo fmt --all && cargo fmt --all -- --check && cargo clippy --locked --all-targets -- -D warnings && tmp=/var/tmp/justless-test-tmp && mkdir -p \"$tmp\" && trap 'rm -rf \"$tmp\"' EXIT && TMPDIR=\"$tmp\" cargo test --locked",
            r#"cargo build && ./target/debug/app "$ARG""#,
            r#"git status && echo "branch=$BRANCH""#,
            "cargo test && git diff > /tmp/out.patch",
        ] {
            assert_eq!(claude(cmd), Route::SupportedNotRouted, "claude: {cmd}");
            assert_eq!(agy(cmd), Route::SupportedNotRouted, "agy: {cmd}");
            assert_eq!(codex(cmd), Route::PassedThrough, "codex: {cmd}");
        }

        // Unsafe chains remain PassedThrough everywhere.
        for rejected in [
            r#"cd "$dir" && cargo test"#,
            r#"PATH="$x" cargo test"#,
            r#"export PATH="$x" && cargo test"#,
            "FOO=$(id) cargo test",
            r#"cargo test && eval "$x""#,
        ] {
            assert_eq!(claude(rejected), Route::PassedThrough, "claude: {rejected}");
            assert_eq!(agy(rejected), Route::PassedThrough, "agy: {rejected}");
            assert_eq!(codex(rejected), Route::PassedThrough, "codex: {rejected}");
        }

        // A rewritten mixed chain whose first segment is not `justless` still
        // counts as routed even without a footer.
        let rewritten = r#"tmp=/var/tmp/x && mkdir -p "$tmp" && TMPDIR="$tmp" justless run --hook -- cargo test --locked"#;
        let label = discover_label(rewritten);
        assert!(routed(rewritten, &label, &Value::String("ok\n".into())));
    }

    #[test]
    fn gemini_and_opencode_sessions_discover_labels_and_bytes() {
        use std::time::{Duration, SystemTime};

        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("repo");
        let other = temp.path().join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let ws = crate::workspace::Workspace::discover(Some(&root)).unwrap();
        let exclusions = crate::rewrite::Exclusions::default();
        let since = SystemTime::now() - Duration::from_secs(3600);
        let since_ms = crate::history::now_ms().saturating_sub(3_600_000);

        // 1. Synthetic Gemini CLI sessions: hashed project + unrelated project.
        let gemini_home = temp.path().join("gemini");
        let proj_hash = super::sha256_hex(&ws.root.to_string_lossy());
        let other_hash = super::sha256_hex(&other.to_string_lossy());
        let chats = gemini_home.join("tmp").join(&proj_hash).join("chats");
        let other_chats = gemini_home.join("tmp").join(&other_hash).join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::create_dir_all(&other_chats).unwrap();
        let cargo_out = "test alpha ... ok\n".repeat(120);
        let routed_out = "ok\n[justless run-Gm1 · exit 0]";
        std::fs::write(
            chats.join("session-2099-01-01.json"),
            serde_json::json!({
                "sessionId": "gem-s1",
                "projectHash": proj_hash,
                "startTime": "2099-01-01T00:00:00.000Z",
                "lastUpdated": "2099-01-01T00:05:00.000Z",
                "messages": [
                    {
                        "id": "m1",
                        "timestamp": "2099-01-01T00:01:00.000Z",
                        "type": "gemini",
                        "toolCalls": [
                            {
                                "id": "tc1",
                                "name": "run_shell_command",
                                "args": {"command": "cargo test --locked"},
                                "result": [{"functionResponse": {"response": {"output": cargo_out}}}],
                                "status": "success"
                            },
                            {
                                "id": "tc2",
                                "name": "run_shell_command",
                                "args": {"command": "git diff HEAD~1"},
                                "result": [{"functionResponse": {"response": {"output": routed_out}}}],
                                "status": "success"
                            },
                            {
                                "id": "tc3",
                                "name": "read_file",
                                "args": {"path": "src/main.rs"},
                                "result": "ignored non-shell"
                            }
                        ]
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        // Unrelated Gemini project must be skipped.
        std::fs::write(
            other_chats.join("session-other.json"),
            serde_json::json!({
                "sessionId": "gem-other",
                "projectHash": other_hash,
                "messages": [{
                    "timestamp": "2099-01-01T00:01:00.000Z",
                    "toolCalls": [{
                        "name": "run_shell_command",
                        "args": {"command": "pytest"},
                        "result": [{"functionResponse": {"response": {"output": "x".repeat(5000)}}}]
                    }]
                }]
            })
            .to_string(),
        )
        .unwrap();

        let gemini = super::scan_gemini_in(&gemini_home, &ws, &exclusions, since, since_ms);
        assert_eq!(gemini.sessions.len(), 1);
        assert_eq!(gemini.commands(), 2);
        let unrouted_gem = gemini.groups.get(&Route::SupportedNotRouted).unwrap();
        assert_eq!(unrouted_gem.commands, 1);
        assert_eq!(unrouted_gem.output_bytes, cargo_out.len() as u64);
        assert_eq!(
            unrouted_gem.labels.get("cargo test"),
            Some(&(1, cargo_out.len() as u64))
        );
        let routed_gem = gemini.groups.get(&Route::ThroughJustless).unwrap();
        assert_eq!(routed_gem.commands, 1);
        assert_eq!(routed_gem.output_bytes, routed_out.len() as u64);

        // 2. Synthetic OpenCode storage: session + message + part JSONs (+ unrelated project).
        let oc_storage = temp.path().join("opencode/storage");
        std::fs::create_dir_all(oc_storage.join("session/proj-1")).unwrap();
        std::fs::create_dir_all(oc_storage.join("session/proj-other")).unwrap();
        std::fs::create_dir_all(oc_storage.join("message/ses-1")).unwrap();
        std::fs::create_dir_all(oc_storage.join("part/msg-1")).unwrap();
        std::fs::create_dir_all(oc_storage.join("part/msg-other")).unwrap();

        std::fs::write(
            oc_storage.join("session/proj-1/ses-1.json"),
            serde_json::json!({"id": "ses-1", "projectID": "proj-1", "directory": ws.root})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            oc_storage.join("session/proj-other/ses-other.json"),
            serde_json::json!({"id": "ses-other", "projectID": "proj-other", "directory": other})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            oc_storage.join("message/ses-1/msg-1.json"),
            serde_json::json!({"id": "msg-1", "sessionID": "ses-1", "role": "assistant"})
                .to_string(),
        )
        .unwrap();
        let pytest_out = "3 passed in 0.42s\n".repeat(80);
        std::fs::write(
            oc_storage.join("part/msg-1/prt-1.json"),
            serde_json::json!({
                "id": "prt-1",
                "sessionID": "ses-1",
                "messageID": "msg-1",
                "type": "tool",
                "tool": "bash",
                "state": {
                    "status": "completed",
                    "input": {"command": "pytest tests/unit"},
                    "output": pytest_out,
                    "time": {"start": 4_000_000_000_000u64, "end": 4_000_000_001_000u64}
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            oc_storage.join("part/msg-other/prt-other.json"),
            serde_json::json!({
                "id": "prt-other",
                "sessionID": "ses-other",
                "type": "tool",
                "tool": "bash",
                "state": {
                    "status": "completed",
                    "input": {"command": "cargo build"},
                    "output": "z".repeat(4000),
                    "time": {"end": 4_000_000_001_000u64}
                }
            })
            .to_string(),
        )
        .unwrap();

        let opencode = super::scan_opencode_in(&oc_storage, &ws, &exclusions, since, since_ms);
        assert_eq!(opencode.sessions.len(), 1);
        assert_eq!(opencode.commands(), 1);
        let unrouted_oc = opencode.groups.get(&Route::SupportedNotRouted).unwrap();
        assert_eq!(unrouted_oc.output_bytes, pytest_out.len() as u64);
        assert_eq!(
            unrouted_oc.labels.get("pytest"),
            Some(&(1, pytest_out.len() as u64))
        );
    }

    #[test]
    fn cursor_jsonl_transcripts_discover_labels_and_bytes() {
        use std::time::{Duration, SystemTime};

        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("my_cursor_repo");
        std::fs::create_dir_all(&root).unwrap();
        let ws = crate::workspace::Workspace::discover(Some(&root)).unwrap();
        let exclusions = crate::rewrite::Exclusions::default();
        let since = SystemTime::now() - Duration::from_secs(3600);
        let since_ms = crate::history::now_ms().saturating_sub(3_600_000);

        let cursor_home = temp.path().join("cursor");
        let slug = super::normalize_slug_collapsed(&ws.root.to_string_lossy());
        let transcripts = cursor_home
            .join("projects")
            .join(&slug)
            .join("agent-transcripts");
        let nested = transcripts.join("nested-session");
        let unrelated = cursor_home
            .join("projects")
            .join("some-other-repo")
            .join("agent-transcripts");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();

        let diff_out = "+fn cursor_saved() {}\n".repeat(150);
        let clippy_out = "warning: unused variable\n".repeat(60);
        let routed_out = "ok\n[justless run-Cur1 · exit 0]";

        // Flat transcript using Anthropic-style `Shell` tool_use/tool_result + noise lines.
        let s1 = [
            serde_json::json!({"role":"user","message":{"content":[{"type":"text","text":"check diff"}]}}).to_string(),
            serde_json::json!({"role":"assistant","timestamp":"2099-01-01T00:00:00Z","message":{"content":[
                {"type":"tool_use","id":"c1","name":"Read","input":{"path":"src/lib.rs"}},
                {"type":"tool_use","id":"c2","name":"Shell","input":{"command":"git diff src/main.rs"}}
            ]}}).to_string(),
            serde_json::json!({"role":"user","timestamp":"2099-01-01T00:00:01Z","message":{"content":[
                {"type":"tool_result","tool_use_id":"c1","content":"read noise".repeat(200)},
                {"type":"tool_result","tool_use_id":"c2","content":diff_out}
            ]}}).to_string(),
            serde_json::json!({"role":"assistant","timestamp":"2099-01-01T00:00:02Z","message":{"content":[
                {"type":"tool_use","id":"c3","name":"Shell","input":{"command":"cargo test"}}
            ]}}).to_string(),
            serde_json::json!({"role":"user","timestamp":"2099-01-01T00:00:03Z","message":{"content":[
                {"type":"tool_result","tool_use_id":"c3","content":routed_out}
            ]}}).to_string(),
        ]
        .join("\n");
        std::fs::write(transcripts.join("s1.jsonl"), s1).unwrap();

        // Nested `<id>/<id>.jsonl` transcript using OpenAI-style `run_terminal_cmd` tool_calls.
        let s2 = [
            serde_json::json!({
                "role": "assistant",
                "timestamp": "2099-01-01T00:01:00Z",
                "tool_calls": [{
                    "id": "call_clippy",
                    "type": "function",
                    "function": {
                        "name": "run_terminal_cmd",
                        "arguments": "{\"command\":\"cargo clippy --all-targets\"}"
                    }
                }]
            })
            .to_string(),
            serde_json::json!({
                "role": "tool",
                "timestamp": "2099-01-01T00:01:02Z",
                "tool_call_id": "call_clippy",
                "content": clippy_out
            })
            .to_string(),
        ]
        .join("\n");
        std::fs::write(nested.join("nested-session.jsonl"), s2).unwrap();

        // Unrelated Cursor project must not be counted.
        std::fs::write(
            unrelated.join("other.jsonl"),
            serde_json::json!({
                "type": "tool_use",
                "name": "Shell",
                "input": {"command": "npm test"},
                "output": "noise".repeat(500)
            })
            .to_string(),
        )
        .unwrap();

        let cursor = super::scan_cursor_in(&cursor_home, &ws, &exclusions, since, since_ms);
        assert_eq!(cursor.sessions.len(), 2);
        assert_eq!(cursor.commands(), 3);
        let unrouted = cursor.groups.get(&Route::SupportedNotRouted).unwrap();
        assert_eq!(unrouted.commands, 2);
        assert_eq!(
            unrouted.output_bytes,
            (diff_out.len() + clippy_out.len()) as u64
        );
        assert_eq!(
            unrouted.labels.get("git diff"),
            Some(&(1, diff_out.len() as u64))
        );
        assert_eq!(
            unrouted.labels.get("cargo clippy"),
            Some(&(1, clippy_out.len() as u64))
        );
        let routed_group = cursor.groups.get(&Route::ThroughJustless).unwrap();
        assert_eq!(routed_group.commands, 1);
        assert_eq!(routed_group.output_bytes, routed_out.len() as u64);

        let opps = super::opportunities_json_for(&[("cursor", &cursor.groups)]);
        assert_eq!(opps.len(), 2);
        assert_eq!(opps[0]["label"], "git diff");
        assert_eq!(opps[0]["clients"], serde_json::json!(["cursor"]));
    }

    #[test]
    fn discover_output_never_leaks_arguments_secrets_or_raw_output() {
        use std::collections::BTreeSet;
        use std::time::{Duration, SystemTime};

        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("privacy_repo");
        std::fs::create_dir_all(&root).unwrap();
        let ws = crate::workspace::Workspace::discover(Some(&root)).unwrap();
        let exclusions = crate::rewrite::Exclusions::default();
        let since = SystemTime::now() - Duration::from_secs(3600);
        let since_ms = crate::history::now_ms().saturating_sub(3_600_000);

        let secret_arg = "SECRET_ARG_TOKEN_ghp_99887766";
        let secret_path = "secrets/payroll_2099.sql";
        let secret_stdout = "AWS_SECRET_ACCESS_KEY=SUPER_SENSITIVE_RAW_STDOUT_LINE\n";

        // Cursor transcript with secret arguments and secret raw output.
        let cursor_home = temp.path().join("cursor");
        let slug = super::normalize_slug_collapsed(&ws.root.to_string_lossy());
        let cursor_dir = cursor_home
            .join("projects")
            .join(&slug)
            .join("agent-transcripts");
        std::fs::create_dir_all(&cursor_dir).unwrap();
        std::fs::write(
            cursor_dir.join("sess.jsonl"),
            [
                serde_json::json!({"role":"assistant","timestamp":"2099-01-01T00:00:00Z","message":{"content":[
                    {"type":"tool_use","id":"s1","name":"Shell","input":{"command":format!("rg {secret_arg} {secret_path}")}}
                ]}}).to_string(),
                serde_json::json!({"role":"user","timestamp":"2099-01-01T00:00:01Z","message":{"content":[
                    {"type":"tool_result","tool_use_id":"s1","content":secret_stdout.repeat(20)}
                ]}}).to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        // Cline task with XML `<execute_command>` containing secret args and stdout.
        let cline_tasks = temp.path().join("cline_tasks");
        let task_dir = cline_tasks.join("1999999999000");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(
            task_dir.join("api_conversation_history.json"),
            serde_json::json!([
                {
                    "role": "user",
                    "content": [{"type": "text", "text": format!("<environment_details>\n# Current Working Directory ({})\n</environment_details>", ws.root.display())}]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": format!("<execute_command>\n<command>cargo test -- {secret_arg}</command>\n</execute_command>")}]
                },
                {
                    "role": "user",
                    "content": [{"type": "text", "text": format!("[execute_command for 'cargo test -- {secret_arg}'] Result:\nCommand executed.\nOutput:\n{}", secret_stdout.repeat(10))}]
                }
            ])
            .to_string(),
        )
        .unwrap();

        let cursor = super::scan_cursor_in(&cursor_home, &ws, &exclusions, since, since_ms);
        let cline = super::scan_cline_like_in(
            std::slice::from_ref(&cline_tasks),
            "cline",
            "Cline",
            Some("init --agent cline"),
            &ws,
            &exclusions,
            since,
            since_ms,
        );
        assert_eq!(cursor.commands(), 1);
        assert_eq!(cline.commands(), 1);

        let report = super::DiscoverReport {
            claude_dir: Some(temp.path().join("claude_empty")),
            claude_sessions: BTreeSet::new(),
            claude_groups: BTreeMap::new(),
            claude_unrouted: BTreeMap::new(),
            claude_unreadable_permissions: false,
            codex: crate::rollouts::Coverage::default(),
            agy: crate::agy_sessions::Coverage::default(),
            agents: vec![cursor, cline],
        };
        let json_out = serde_json::to_string_pretty(&report.to_json(30, true)).unwrap();
        let text_out = report.to_text(&ws, 30, true);

        // Expected labels and client sections appear.
        assert!(text_out.contains("Cursor ran 1 shell command(s)"));
        assert!(text_out.contains("Cline ran 1 shell command(s)"));
        assert!(text_out.contains("cargo test"));
        assert!(text_out.contains("rg"));
        assert!(json_out.contains("\"cursor\""));
        assert!(json_out.contains("\"cline\""));

        // Neither JSON nor plain-text output ever leaks arguments, paths, or stdout.
        for forbidden in [
            secret_arg,
            "ghp_99887766",
            secret_path,
            "payroll_2099",
            "AWS_SECRET_ACCESS_KEY",
            "SUPER_SENSITIVE_RAW_STDOUT_LINE",
        ] {
            assert!(
                !json_out.contains(forbidden),
                "leaked `{forbidden}` in JSON:\n{json_out}"
            );
            assert!(
                !text_out.contains(forbidden),
                "leaked `{forbidden}` in text:\n{text_out}"
            );
        }
    }

    #[test]
    fn streaming_prefilter_preserves_exact_results_for_claude_codex_and_agy() {
        use std::collections::{BTreeSet, HashMap};

        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("prefilter_repo");
        std::fs::create_dir_all(&root).unwrap();
        let ws = crate::workspace::Workspace::discover(Some(&root)).unwrap();
        let root_str = ws.root.to_string_lossy().into_owned();
        let exclusions = crate::rewrite::Exclusions::default();
        let since_ms = 1_700_000_000_000u64;

        // 1. Claude Code transcript with noise, non-Bash Read/Edit tools, orphaned Bash call,
        // api-request-blob, and permissionMode transitions.
        let claude_file = temp.path().join("claude_session.jsonl");
        let mut claude_lines = vec![
            serde_json::json!({"type":"permission-mode","permissionMode":"bypassPermissions"}).to_string(),
            serde_json::json!({"type":"api-request-blob","payload":"{\"tool_use\":\"Bash\",\"tool_result\":\"noise\"}".repeat(20)}).to_string(),
            serde_json::json!({"type":"assistant","cwd":&root_str,"timestamp":"2099-01-01T00:00:00Z","message":{"content":[
                {"type":"tool_use","id":"orphan_bash","name":"Bash","input":{"command":"cargo build"}}
            ]}}).to_string(),
        ];
        for i in 0..25 {
            claude_lines.push(serde_json::json!({"type":"assistant","cwd":&root_str,"timestamp":"2099-01-01T00:00:01Z","message":{"content":[
                {"type":"tool_use","id":format!("read_{i}"),"name":"Read","input":{"file_path":"src/lib.rs"}}
            ]}}).to_string());
            claude_lines.push(serde_json::json!({"type":"user","cwd":&root_str,"timestamp":"2099-01-01T00:00:02Z","message":{"content":[
                {"type":"tool_result","tool_use_id":format!("read_{i}"),"content":"file body ".repeat(100)}
            ]}}).to_string());
        }
        claude_lines.push(serde_json::json!({"type":"assistant","cwd":&root_str,"timestamp":"2099-01-01T00:00:03Z","message":{"content":[
            {"type":"tool_use","id":"bash_1","name":"Bash","input":{"command":"cargo test"}}
        ]}}).to_string());
        claude_lines.push(serde_json::json!({"type":"user","cwd":&root_str,"timestamp":"2099-01-01T00:00:04Z","message":{"content":[
            {"type":"tool_result","tool_use_id":"bash_1","content":"running 4 tests\n".repeat(40)}
        ]}}).to_string());
        std::fs::write(&claude_file, claude_lines.join("\n")).unwrap();

        let run_claude = |prefilter: bool| {
            let mut ctx = super::ClaudeScanContext {
                workspace: &ws,
                root: &root_str,
                first_date: "2020-01-01",
                plan: true,
                exclusions: &exclusions,
                post_run: None,
                rules: None,
                session_modes: HashMap::new(),
                groups: BTreeMap::new(),
                unrouted: BTreeMap::new(),
                sessions: BTreeSet::new(),
            };
            let mut parsed = 0usize;
            super::scan_claude_file(
                &mut ctx,
                &claude_file,
                "s1",
                true,
                prefilter,
                Some(&mut parsed),
            );
            (ctx.groups, ctx.sessions, parsed)
        };
        let (cl_fast, cl_sess_fast, cl_parsed_fast) = run_claude(true);
        let (cl_full, cl_sess_full, cl_parsed_full) = run_claude(false);
        assert_eq!(
            super::groups_json(&cl_fast),
            super::groups_json(&cl_full),
            "Claude prefilter changed grouped results"
        );
        assert_eq!(cl_sess_fast, cl_sess_full);
        assert!(
            cl_parsed_fast * 4 < cl_parsed_full,
            "expected Claude prefilter to skip noise: fast={cl_parsed_fast}, full={cl_parsed_full}"
        );

        // 2. Codex rollout with apply_patch noise, direct exec_command, and code-mode script.
        let codex_file = temp.path().join("rollout-2099-s1.jsonl");
        let mut codex_lines = vec![
            serde_json::json!({"timestamp":"2099-01-01T00:00:00Z","type":"session_meta","payload":{"session_id":"codex-1","cwd":&root_str}}).to_string(),
        ];
        for i in 0..25 {
            codex_lines.push(serde_json::json!({"timestamp":"2099-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"apply_patch","call_id":format!("p_{i}"),"arguments":"{}"}}).to_string());
            codex_lines.push(serde_json::json!({"timestamp":"2099-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call_output","call_id":format!("p_{i}"),"output":"patched ".repeat(80)}}).to_string());
        }
        codex_lines.push(serde_json::json!({"timestamp":"2099-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"sh_1","arguments":"{\"cmd\":\"git status\"}"}}).to_string());
        codex_lines.push(serde_json::json!({"timestamp":"2099-01-01T00:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"sh_1","output":"Process exited with code 0\nOutput:\nM src/lib.rs\n"}}).to_string());
        codex_lines.push(serde_json::json!({"timestamp":"2099-01-01T00:00:04Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"scr_1"}}).to_string());
        codex_lines.push(serde_json::json!({"timestamp":"2099-01-01T00:00:05Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"scr_1","output":"script done"}}).to_string());
        std::fs::write(&codex_file, codex_lines.join("\n")).unwrap();

        let (cx_scan_fast, cx_fast, cx_parsed_fast) =
            crate::rollouts::read_for_test(&codex_file, &ws.root, since_ms, true);
        let (cx_scan_full, cx_full, cx_parsed_full) =
            crate::rollouts::read_for_test(&codex_file, &ws.root, since_ms, false);
        assert_eq!(cx_fast.len(), cx_full.len());
        assert_eq!(cx_fast.len(), 1);
        assert_eq!(cx_fast[0].command, cx_full[0].command);
        assert_eq!(cx_fast[0].bytes, cx_full[0].bytes);
        assert_eq!(cx_scan_fast.scripts, cx_scan_full.scripts);
        assert_eq!(cx_scan_fast.script_bytes, cx_scan_full.script_bytes);
        assert!(
            cx_parsed_fast * 4 < cx_parsed_full,
            "expected Codex prefilter to skip noise: fast={cx_parsed_fast}, full={cx_parsed_full}"
        );

        // 3. Antigravity transcript with many non-shell read_file turns + mixed shell + bg task.
        let agy_file = temp.path().join("transcript_full.jsonl");
        let mut agy_lines = Vec::new();
        for i in 0..25 {
            agy_lines.push(
                serde_json::json!({"type":"USER_INPUT","content":format!("turn {i}")}).to_string(),
            );
            agy_lines.push(serde_json::json!({"type":"PLANNER_RESPONSE","created_at":"2099-01-01T00:00:00Z","tool_calls":[
                {"name":"read_file","args":{"Path":"src/main.rs"}}
            ]}).to_string());
            agy_lines.push(serde_json::json!({"type":"GENERIC","created_at":"2099-01-01T00:00:01Z","content":"big file ".repeat(100)}).to_string());
        }
        agy_lines.push(serde_json::json!({"type":"PLANNER_RESPONSE","created_at":"2099-01-01T00:01:00Z","tool_calls":[
            {"name":"read_file","args":{"Path":"Cargo.toml"}},
            {"name":"run_command","args":{"CommandLine":"cargo check"}},
            {"name":"read_file","args":{"Path":"README.md"}}
        ]}).to_string());
        agy_lines.push(serde_json::json!({"type":"GENERIC","created_at":"2099-01-01T00:01:01Z","content":"cargo toml text"}).to_string());
        agy_lines.push(serde_json::json!({"type":"GENERIC","created_at":"2099-01-01T00:01:02Z","content":"Command exited with code 0.\n\nCommand output:\nFinished dev\n"}).to_string());
        agy_lines.push(serde_json::json!({"type":"GENERIC","created_at":"2099-01-01T00:01:03Z","content":"trailing readme skipped"}).to_string());
        std::fs::write(&agy_file, agy_lines.join("\n")).unwrap();

        let (ag_scan_fast, ag_fast, ag_parsed_fast) =
            crate::agy_sessions::read_for_test(&agy_file, "agy-1", false, since_ms, true);
        let (ag_scan_full, ag_full, ag_parsed_full) =
            crate::agy_sessions::read_for_test(&agy_file, "agy-1", false, since_ms, false);
        assert_eq!(ag_fast.len(), ag_full.len());
        assert_eq!(ag_fast.len(), 1);
        assert_eq!(ag_fast[0].command, ag_full[0].command);
        assert_eq!(ag_fast[0].bytes, ag_full[0].bytes);
        assert_eq!(ag_scan_fast.sessions, ag_scan_full.sessions);
        assert!(
            ag_parsed_fast * 4 < ag_parsed_full,
            "expected Agy prefilter to skip noise: fast={ag_parsed_fast}, full={ag_parsed_full}"
        );
    }

    #[test]
    fn cursor_unmatched_tool_use_id_never_steals_anonymous_shell_call() {
        use std::time::{Duration, SystemTime};

        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("cursor_anon_repo");
        std::fs::create_dir_all(&root).unwrap();
        let ws = crate::workspace::Workspace::discover(Some(&root)).unwrap();
        let exclusions = crate::rewrite::Exclusions::default();
        let since = SystemTime::now() - Duration::from_secs(3600);
        let since_ms = crate::history::now_ms().saturating_sub(3_600_000);

        let cursor_home = temp.path().join("cursor");
        let slug = super::normalize_slug_collapsed(&ws.root.to_string_lossy());
        let cursor_dir = cursor_home
            .join("projects")
            .join(&slug)
            .join("agent-transcripts");
        std::fs::create_dir_all(&cursor_dir).unwrap();

        let non_shell_payload = "FILE_CONTENTS_FROM_READ_TOOL\n".repeat(40);
        let shell_payload = "real shell output\n".repeat(10);
        std::fs::write(
            cursor_dir.join("anon.jsonl"),
            [
                // Anonymous shell tool_use (no `id`) queued in `anonymous`.
                serde_json::json!({"role":"assistant","timestamp":"2099-01-01T00:00:00Z","message":{"content":[
                    {"type":"tool_use","name":"Shell","input":{"command":"git status"}}
                ]}}).to_string(),
                // Non-shell tool_result with an explicit unmatched `tool_use_id` MUST NOT pop `anonymous`.
                serde_json::json!({"role":"user","timestamp":"2099-01-01T00:00:01Z","message":{"content":[
                    {"type":"tool_result","tool_use_id":"read_file_42","content":non_shell_payload}
                ]}}).to_string(),
                // Anonymous tool_result (no `tool_use_id`) matches the queued anonymous shell call.
                serde_json::json!({"role":"user","timestamp":"2099-01-01T00:00:02Z","message":{"content":[
                    {"type":"tool_result","content":shell_payload}
                ]}}).to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        let cursor = super::scan_cursor_in(&cursor_home, &ws, &exclusions, since, since_ms);
        assert_eq!(cursor.commands(), 1);
        let unrouted = cursor.groups.get(&Route::SupportedNotRouted).unwrap();
        assert_eq!(
            unrouted.labels.get("git status"),
            Some(&(1, shell_payload.len() as u64))
        );
    }

    #[test]
    fn justless_disable_commands_symlinks_and_oversized_files_skip_cleanly_with_zero_leaks() {
        use std::collections::{BTreeSet, HashMap};
        use std::time::{Duration, SystemTime};

        let none = crate::rewrite::Exclusions::default();

        // 1. Direct `Route::unrouted` classifies `JUSTLESS_DISABLE=<non-0>` as Excluded
        //    (never SupportedNotRouted), while `JUSTLESS_DISABLE=0` and data arguments still route.
        for disabled_cmd in [
            "JUSTLESS_DISABLE=1 cargo test",
            "FOO=1 JUSTLESS_DISABLE=true git status",
            "env JUSTLESS_DISABLE=1 cargo clippy",
            "env -i JUSTLESS_DISABLE=1 rg foo src",
            "export JUSTLESS_DISABLE=1 && cargo test",
            "timeout 30 JUSTLESS_DISABLE=1 cargo build",
        ] {
            assert!(
                super::command_disables_justless(disabled_cmd),
                "{disabled_cmd}"
            );
            assert_eq!(
                Route::unrouted(
                    disabled_cmd,
                    "cargo test",
                    &none,
                    true,
                    crate::agents::would_route,
                ),
                Route::Excluded,
                "{disabled_cmd}"
            );
        }
        for active_cmd in [
            "JUSTLESS_DISABLE=0 cargo test",
            "JUSTLESS_DISABLE= cargo test",
            "rg 'JUSTLESS_DISABLE=1' src",
            "cargo test",
        ] {
            assert!(
                !super::command_disables_justless(active_cmd),
                "{active_cmd}"
            );
        }

        // 2. Claude & Cursor scanners skip `JUSTLESS_DISABLE=1` commands cleanly while keeping
        //    `JUSTLESS_DISABLE=0` and never leaking secrets (even when pasted as a command token).
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("disable_repo");
        std::fs::create_dir_all(&root).unwrap();
        let ws = crate::workspace::Workspace::discover(Some(&root)).unwrap();
        let root_str = ws.root.to_string_lossy().into_owned();
        let since = SystemTime::now() - Duration::from_secs(3600);
        let since_ms = crate::history::now_ms().saturating_sub(3_600_000);

        let secret_token = concat!("ghp", "_99887766554433221100aabbccddeeff00");
        let secret_stdout = "TOKEN=SUPER_SECRET_DISABLE_STDOUT_VALUE\n";

        let claude_file = temp.path().join("claude_disable.jsonl");
        let claude_lines = [
            serde_json::json!({"type":"assistant","cwd":&root_str,"timestamp":"2099-01-01T00:00:00Z","message":{"content":[
                {"type":"tool_use","id":"d1","name":"Bash","input":{"command":format!("JUSTLESS_DISABLE=1 cargo test -- {secret_token}")}}
            ]}}).to_string(),
            serde_json::json!({"type":"user","cwd":&root_str,"timestamp":"2099-01-01T00:00:01Z","message":{"content":[
                {"type":"tool_result","tool_use_id":"d1","content":secret_stdout}
            ]}}).to_string(),
            serde_json::json!({"type":"assistant","cwd":&root_str,"timestamp":"2099-01-01T00:00:02Z","message":{"content":[
                {"type":"tool_use","id":"d2","name":"Bash","input":{"command":"env JUSTLESS_DISABLE=true git status"}}
            ]}}).to_string(),
            serde_json::json!({"type":"user","cwd":&root_str,"timestamp":"2099-01-01T00:00:03Z","message":{"content":[
                {"type":"tool_result","tool_use_id":"d2","content":"M src/lib.rs\n"}
            ]}}).to_string(),
            serde_json::json!({"type":"assistant","cwd":&root_str,"timestamp":"2099-01-01T00:00:04Z","message":{"content":[
                {"type":"tool_use","id":"a1","name":"Bash","input":{"command":format!("JUSTLESS_DISABLE=0 git status -- {secret_token}")}}
            ]}}).to_string(),
            serde_json::json!({"type":"user","cwd":&root_str,"timestamp":"2099-01-01T00:00:05Z","message":{"content":[
                {"type":"tool_result","tool_use_id":"a1","content":secret_stdout}
            ]}}).to_string(),
            serde_json::json!({"type":"assistant","cwd":&root_str,"timestamp":"2099-01-01T00:00:06Z","message":{"content":[
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":secret_token}}
            ]}}).to_string(),
            serde_json::json!({"type":"user","cwd":&root_str,"timestamp":"2099-01-01T00:00:07Z","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":secret_stdout}
            ]}}).to_string(),
        ];
        std::fs::write(&claude_file, claude_lines.join("\n")).unwrap();

        let mut ctx = super::ClaudeScanContext {
            workspace: &ws,
            root: &root_str,
            first_date: "2020-01-01",
            plan: true,
            exclusions: &none,
            post_run: None,
            rules: None,
            session_modes: HashMap::new(),
            groups: BTreeMap::new(),
            unrouted: BTreeMap::new(),
            sessions: BTreeSet::new(),
        };
        super::scan_claude_file(&mut ctx, &claude_file, "s1", true, true, None);
        let total_claude: u64 = ctx.groups.values().map(|g| g.commands).sum();
        assert_eq!(
            total_claude, 2,
            "only JUSTLESS_DISABLE=0 and token cmd counted"
        );
        assert!(
            !ctx.groups
                .values()
                .any(|g| g.labels.contains_key("cargo test")),
            "JUSTLESS_DISABLE=1 cargo test must be skipped"
        );
        assert_eq!(discover_label(secret_token), "other");

        // 3. Cursor skips `JUSTLESS_DISABLE=1`, invalid UTF-8 lines, oversized files, and symlinks.
        let cursor_home = temp.path().join("cursor");
        let slug = super::normalize_slug_collapsed(&ws.root.to_string_lossy());
        let cursor_dir = cursor_home
            .join("projects")
            .join(&slug)
            .join("agent-transcripts");
        std::fs::create_dir_all(&cursor_dir).unwrap();
        let mut cursor_bytes = Vec::new();
        cursor_bytes.extend_from_slice(
            serde_json::json!({"role":"assistant","timestamp":"2099-01-01T00:00:00Z","message":{"content":[
                {"type":"tool_use","id":"c1","name":"Shell","input":{"command":"JUSTLESS_DISABLE=1 cargo test"}}
            ]}}).to_string().as_bytes(),
        );
        cursor_bytes.push(b'\n');
        // Invalid UTF-8 line must be skipped without aborting the file scan.
        cursor_bytes.extend_from_slice(&[0xff, 0xfe, b'{', b'}', b'\n']);
        cursor_bytes.extend_from_slice(
            serde_json::json!({"role":"user","timestamp":"2099-01-01T00:00:01Z","message":{"content":[
                {"type":"tool_result","tool_use_id":"c1","content":secret_stdout}
            ]}}).to_string().as_bytes(),
        );
        cursor_bytes.push(b'\n');
        cursor_bytes.extend_from_slice(
            serde_json::json!({"role":"assistant","timestamp":"2099-01-01T00:00:02Z","message":{"content":[
                {"type":"tool_use","id":"c2","name":"Shell","input":{"command":format!("cargo test -- {secret_token}")}}
            ]}}).to_string().as_bytes(),
        );
        cursor_bytes.push(b'\n');
        cursor_bytes.extend_from_slice(
            serde_json::json!({"role":"user","timestamp":"2099-01-01T00:00:03Z","message":{"content":[
                {"type":"tool_result","tool_use_id":"c2","content":secret_stdout}
            ]}}).to_string().as_bytes(),
        );
        cursor_bytes.push(b'\n');
        std::fs::write(cursor_dir.join("real.jsonl"), &cursor_bytes).unwrap();

        // Symlinked transcript pointing outside must be ignored cleanly.
        #[cfg(unix)]
        {
            let outside = temp.path().join("outside.jsonl");
            std::fs::write(&outside, &cursor_bytes).unwrap();
            std::os::unix::fs::symlink(&outside, cursor_dir.join("linked.jsonl")).unwrap();
            assert!(
                super::read_bounded_utf8(&cursor_dir.join("linked.jsonl"), 1024 * 1024).is_none()
            );
            let mut seen = 0usize;
            super::for_each_jsonl_line(
                &cursor_dir.join("linked.jsonl"),
                Some(1024 * 1024),
                |_, _| {
                    seen += 1;
                    true
                },
            );
            assert_eq!(seen, 0, "symlinked jsonl must never be read");
        }

        // Oversized file limit enforced cleanly by `for_each_jsonl_line` and `read_bounded_utf8`.
        let small = temp.path().join("capped.jsonl");
        std::fs::write(&small, "1234567890\n").unwrap();
        assert!(super::read_bounded_utf8(&small, 5).is_none());
        let mut seen = 0usize;
        super::for_each_jsonl_line(&small, Some(5), |_, _| {
            seen += 1;
            true
        });
        assert_eq!(seen, 0, "oversized file must be skipped");

        let cursor = super::scan_cursor_in(&cursor_home, &ws, &none, since, since_ms);
        assert_eq!(
            cursor.commands(),
            1,
            "only non-disabled command in real.jsonl"
        );

        let report = super::DiscoverReport {
            claude_dir: Some(temp.path().join("claude_empty")),
            claude_sessions: ctx.sessions,
            claude_groups: ctx.groups,
            claude_unrouted: ctx.unrouted,
            claude_unreadable_permissions: false,
            codex: crate::rollouts::Coverage::default(),
            agy: crate::agy_sessions::Coverage::default(),
            agents: vec![cursor],
        };
        let json_out = serde_json::to_string_pretty(&report.to_json(30, true)).unwrap();
        let text_out = report.to_text(&ws, 30, true);
        for forbidden in [secret_token, "SUPER_SECRET_DISABLE_STDOUT_VALUE"] {
            assert!(
                !json_out.contains(forbidden),
                "leaked `{forbidden}` in JSON:\n{json_out}"
            );
            assert!(
                !text_out.contains(forbidden),
                "leaked `{forbidden}` in text:\n{text_out}"
            );
        }
    }
}
