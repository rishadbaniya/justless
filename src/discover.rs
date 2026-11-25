//! Find shell commands that agents ran in this workspace without Justless.
//!
//! Claude Code keeps a JSONL transcript per session under
//! `~/.claude/projects/<workspace path with non-alphanumerics as dashes>/`. Each
//! Bash `tool_use` carries the command and each `tool_result` carries the output the
//! model read. Only command labels such as `cargo test` and byte counts are reported.
//! Codex rollouts for the workspace are read too; see [`crate::rollouts`].
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

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

pub(crate) fn transcripts(dir: &Path, since: SystemTime, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() && depth > 0 && path.file_name().is_some_and(|n| n != "memory") {
            transcripts(&path, since, depth - 1, out);
        } else if meta.is_file()
            && path.extension().is_some_and(|e| e == "jsonl")
            && meta.modified().is_ok_and(|m| m >= since)
        {
            out.push(path);
        }
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

/// Command label for `discover`: delegates to `agents::command_label` +
/// `history::label`, and preserves allowlisted 3-word labels (`python3 -m
/// unittest`, `python -m unittest`, `kubectl get events|svc|pods|…`) without
/// ever exposing arguments, paths, namespaces or patterns.
fn discover_label(command: &str) -> String {
    let words = crate::agents::command_label(command);
    let base = crate::history::label(&words.split(' ').collect::<Vec<_>>());
    if !matches!(base.as_str(), "python" | "python3" | "kubectl get") {
        return base;
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
            return refined;
        }
    }
    base
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

pub fn discover(workspace: &Workspace, days: u64, as_json: bool, plan: bool) -> Result<()> {
    let window = Duration::from_secs(days.saturating_mul(86_400));
    let since = SystemTime::now()
        .checked_sub(window)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let first_date = utc_date(now_ms().saturating_sub(days.saturating_mul(86_400_000)));
    let Some(dir) = transcript_dir(&workspace.root) else {
        anyhow::bail!("cannot locate the Claude Code configuration directory");
    };
    let since_ms = now_ms().saturating_sub(days.saturating_mul(86_400_000));
    let codex = crate::rollouts::coverage(workspace, since_ms);
    let agy = crate::agy_sessions::coverage(workspace, since_ms);
    // Past commands are judged by the rules in force now, or by none while the
    // settings fail to load.
    let exclusions = Exclusions::of(workspace).unwrap_or_default();
    // The post-run hook leaves every result while either fails to load.
    let post_run = Exclusions::of(workspace).ok().zip(
        workspace
            .settings()
            .ok()
            .map(|settings| (settings.context_budget, settings.read_outline_lines)),
    );
    // Sessions keep subagent transcripts under <session>/subagents/, and workflow
    // subagents one level deeper under subagents/workflows/<run>/.
    let mut files = Vec::new();
    transcripts(&dir, since, 6, &mut files);
    // Each session's own transcript first, so its subagents can take its mode.
    files.sort_by_cached_key(|file| (file.parent() != Some(dir.as_path()), file.clone()));

    let root = workspace.root.to_string_lossy().into_owned();
    let mut groups: BTreeMap<Route, Group> = BTreeMap::new();
    let mut unrouted: BTreeMap<String, PlanEntry> = BTreeMap::new();
    let mut sessions = BTreeSet::new();
    // Claude Code's permission rules, read once a transcript records a mode.
    let mut rules: Option<crate::claude_permissions::Rules> = None;
    // The modes each session's own transcript recorded, for its subagents'
    // transcripts, which record none.
    let mut session_modes: HashMap<String, BTreeSet<String>> = HashMap::new();
    for file in &files {
        // <session>.jsonl and everything under <session>/ belong to one session.
        let session = file
            .strip_prefix(&dir)
            .ok()
            .and_then(|relative| relative.components().next())
            .map(|first| {
                let name = first.as_os_str().to_string_lossy();
                name.strip_suffix(".jsonl").unwrap_or(&name).to_owned()
            })
            .unwrap_or_default();
        let own = file.parent() == Some(dir.as_path());
        // A subagent takes its session's mode when the session recorded only one.
        let mut mode = session_modes
            .get(&session)
            .filter(|modes| modes.len() == 1)
            .and_then(|modes| modes.first().cloned());
        let mut calls: HashMap<String, Call> = HashMap::new();
        let Ok(handle) = File::open(file) else {
            continue;
        };
        for line in BufReader::new(handle).lines() {
            let Ok(line) = line else {
                break;
            };
            // Skip cheaply before parsing: only tool calls, results and the
            // permission mode matter.
            if !line.contains("\"tool_use\"")
                && !line.contains("\"tool_result\"")
                && !line.contains("\"permissionMode\"")
            {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(recorded) = entry["permissionMode"].as_str() {
                mode = Some(recorded.to_owned());
                if own {
                    session_modes
                        .entry(session.clone())
                        .or_default()
                        .insert(recorded.to_owned());
                }
            }
            if entry["cwd"]
                .as_str()
                .is_some_and(|cwd| cwd != root && !cwd.starts_with(&format!("{root}/")))
            {
                continue;
            }
            let date = entry["timestamp"]
                .as_str()
                .unwrap_or("")
                .get(..10)
                .unwrap_or("");
            let Some(parts) = entry["message"]["content"].as_array() else {
                continue;
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
                        let Some(call) =
                            part["tool_use_id"].as_str().and_then(|id| calls.remove(id))
                        else {
                            continue;
                        };
                        if call.date < first_date {
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
                                &exclusions,
                                true,
                                crate::agents::would_route,
                            );
                            // Without a recorded mode the hook rewrites as it
                            // always has.
                            let left = route == Route::SupportedNotRouted
                                && call.mode.as_deref().is_some_and(|mode| {
                                    let rules = rules.get_or_insert_with(|| {
                                        let root = &workspace.root;
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
                                post_run.as_ref(),
                                &workspace.root,
                            )
                        };
                        let bytes = text_bytes(content);
                        // Excluded commands stay as written by choice.
                        if plan && !matches!(route, Route::ThroughJustless | Route::Excluded) {
                            let entry = unrouted.entry(label.clone()).or_default();
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
                            // The command hook would route it, or the post-run hook
                            // would compact it.
                            if matches!(
                                route,
                                Route::SupportedNotRouted | Route::CompactedAfterRunning
                            ) {
                                entry.now_commands += 1;
                                entry.now_bytes += bytes;
                            }
                        }
                        let group = groups.entry(route).or_default();
                        group.commands += 1;
                        group.output_bytes += bytes;
                        let label = group.labels.entry(label).or_default();
                        label.0 += 1;
                        label.1 += bytes;
                        sessions.insert(session.clone());
                    }
                    _ => {}
                }
            }
        }
    }

    let total: u64 = groups.values().map(|g| g.commands).sum();
    let output: u64 = groups.values().map(|g| g.output_bytes).sum();
    if as_json {
        let report = serde_json::json!({
            "days": days,
            "transcripts": dir,
            "sessions": sessions.len(),
            "commands": total,
            "output_bytes": output,
            "token_estimate": format!(
                "UTF-8 bytes / {:.2}, the bytes per token measured for command output",
                crate::tokens::Content::Logs.bytes_per_token()
            ),
            "plan": plan.then(|| {
                let mut entries: Vec<_> = unrouted.iter().collect();
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
            "groups": groups_json(&groups),
            "what_if": what_if_json(&groups),
            "top_opportunities": opportunities_json(&groups, &codex.groups, &agy.groups),
            "codex": codex.json(),
            "agy": agy.json(),
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if total == 0 {
        println!(
            "No Claude Code shell commands found for this workspace in the last {days} day(s) under {}.",
            dir.display()
        );
        print_codex(workspace, &codex, days, plan);
        print_agy(workspace, &agy, days, plan);
        if plan {
            print_top_opportunities(&groups, &codex.groups, &agy.groups);
        }
        return Ok(());
    }
    println!(
        "Claude Code ran {total} shell command(s) here in the last {days} day(s), across {} session(s), returning {} of output.",
        sessions.len(),
        human_bytes(output)
    );
    print_groups(&groups);
    print_what_if(&groups);
    if plan {
        let mut entries: Vec<_> = unrouted.iter().collect();
        entries.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(a.0.cmp(b.0)));
        println!("\nUnrouted output by command family, largest first (labels only, no commands):");
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
            println!(
                "  {label} ×{} ({}){now}{left}",
                entry.commands,
                human_bytes(entry.bytes)
            );
        }
    }
    if groups.contains_key(&Route::SupportedNotRouted)
        || groups.contains_key(&Route::CompactedAfterRunning)
    {
        println!(
            "\nSupported commands reached the agent unfiltered. Install or refresh hooks with `{} init --hooks --agent claude`, then start a new session.",
            workspace.justless()
        );
    }
    if groups.contains_key(&Route::LeftByPermissions) {
        let unreadable = rules
            .as_ref()
            .is_some_and(|rules| !rules.broken().is_empty());
        println!(
            "\nThe command hook leaves a supported command for Claude Code to judge when an ask or deny rule might match it, and in default, acceptEdits and dontAsk modes when no allow rule covers it. Allow the commands you run, such as Bash(cargo test *), rather than Bash(justless …), to route them.{}",
            if unreadable {
                " A Claude Code settings file cannot be read, so the hook leaves every command; `justless doctor` names it."
            } else {
                ""
            }
        );
    }
    print_codex(workspace, &codex, days, plan);
    print_agy(workspace, &agy, days, plan);
    if plan {
        print_top_opportunities(&groups, &codex.groups, &agy.groups);
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

fn print_what_if(groups: &BTreeMap<Route, Group>) {
    let [routed, compacted, written] = what_if(groups);
    let commands = routed.0 + compacted.0 + written.0;
    if commands == 0 {
        return;
    }
    let part = |(commands, bytes): (u64, u64)| format!("{commands} ({})", human_bytes(bytes));
    println!(
        "\nWith the current build, of the {commands} command(s) that did not go through Justless ({} output): the command hook would route {}, the post-run hook would compact {}, and {} would reach the agent as written. These are output sizes, not savings.",
        human_bytes(routed.1 + compacted.1 + written.1),
        part(routed),
        part(compacted),
        part(written),
    );
}

fn print_groups(groups: &BTreeMap<Route, Group>) {
    for (route, group) in groups {
        println!(
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
        println!("  {}", shown.join(" · "));
        if labels.len() > 8 {
            println!("  +{} more command type(s)", labels.len() - 8);
        }
    }
}

/// The Codex part of the report, when Codex threads started in this workspace.
fn print_codex(workspace: &Workspace, codex: &crate::rollouts::Coverage, days: u64, plan: bool) {
    let scan = &codex.scan;
    if scan.files == 0 {
        return;
    }
    let total = codex.commands();
    println!(
        "\nCodex ran {total} shell command(s) here in the last {days} day(s), across {} session(s), returning {} of output.",
        scan.sessions.len(),
        human_bytes(codex.bytes())
    );
    if codex.nested > 0 {
        println!(
            "{} of them ran inside code-mode scripts, which received {} of that output.",
            codex.nested,
            human_bytes(codex.nested_bytes)
        );
    }
    if scan.scripts > 0 {
        println!(
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
    print_groups(&codex.groups);
    print_client_what_if("Codex", &codex.groups);
    if plan {
        print_unrouted_plan(
            &codex.groups,
            "Unrouted Codex output by command family, largest first (labels only, no commands):",
        );
    }
    if codex.groups.contains_key(&Route::SupportedNotRouted) {
        println!(
            "\nSupported commands reached Codex unfiltered. Install or refresh hooks with `{} init --hooks --agent codex`, trust them with /hooks in Codex, then start a new session.",
            workspace.justless()
        );
    }
}

/// The Antigravity CLI part of the report, when Antigravity CLI sessions ran in this workspace.
fn print_agy(workspace: &Workspace, agy: &crate::agy_sessions::Coverage, days: u64, plan: bool) {
    let total = agy.commands();
    if total == 0 {
        return;
    }
    println!(
        "\nAntigravity CLI ran {total} shell command(s) here in the last {days} day(s), across {} session(s), returning {} of output.",
        agy.scan.sessions.len(),
        human_bytes(agy.bytes())
    );
    print_groups(&agy.groups);
    print_client_what_if("Antigravity CLI", &agy.groups);
    if plan {
        print_unrouted_plan(
            &agy.groups,
            "Unrouted Antigravity CLI output by command family, largest first (labels only, no commands):",
        );
    }
    if agy.groups.contains_key(&Route::SupportedNotRouted) {
        println!(
            "\nSupported commands reached Antigravity CLI unfiltered. Install or refresh hooks with `{} init --hooks --agent agy` (or `{} init --global --agent agy`), then start a new session.",
            workspace.justless(),
            workspace.justless()
        );
    }
}

/// What-if split for clients without a post-run Bash hook (Codex, Antigravity CLI).
fn print_client_what_if(client: &str, groups: &BTreeMap<Route, Group>) {
    let [routed, compacted, written] = what_if(groups);
    let commands = routed.0 + compacted.0 + written.0;
    if commands == 0 {
        return;
    }
    let part = |(commands, bytes): (u64, u64)| format!("{commands} ({})", human_bytes(bytes));
    println!(
        "\nWith the current build, of the {commands} {client} command(s) that did not go through Justless ({} output): hooks would route {}, and {} would reach {client} as written. These are output sizes, not savings.",
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
    claude: &BTreeMap<Route, Group>,
    codex: &BTreeMap<Route, Group>,
    agy: &BTreeMap<Route, Group>,
    predicate: impl Fn(Route) -> bool,
) -> Vec<(String, Opportunity)> {
    let mut map: BTreeMap<String, Opportunity> = BTreeMap::new();
    for (client, groups) in [("claude", claude), ("codex", codex), ("agy", agy)] {
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

fn opportunities_json(
    claude: &BTreeMap<Route, Group>,
    codex: &BTreeMap<Route, Group>,
    agy: &BTreeMap<Route, Group>,
) -> Vec<Value> {
    collect_opportunities(claude, codex, agy, |route| {
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

fn print_top_opportunities(
    claude: &BTreeMap<Route, Group>,
    codex: &BTreeMap<Route, Group>,
    agy: &BTreeMap<Route, Group>,
) {
    let compactable = collect_opportunities(claude, codex, agy, |route| {
        matches!(
            route,
            Route::SupportedNotRouted | Route::CompactedAfterRunning
        )
    });
    if !compactable.is_empty() {
        println!("\nTop opportunities (would compact now, labels only):");
        for (label, opp) in compactable.into_iter().take(5) {
            let clients: Vec<&str> = opp.clients.into_iter().collect();
            println!(
                "  {label} ×{} ({}, ≈{} tokens) [{}]",
                opp.commands,
                human_bytes(opp.output_bytes),
                estimated_tokens(opp.output_bytes),
                clients.join(", ")
            );
        }
    }
    let unsupported: Vec<_> =
        collect_opportunities(claude, codex, agy, |route| route == Route::NotSupported)
            .into_iter()
            .filter(|(_, opp)| opp.output_bytes > 0)
            .collect();
    if !unsupported.is_empty() {
        println!("\nLargest unsupported command families (labels only):");
        for (label, opp) in unsupported.into_iter().take(3) {
            let clients: Vec<&str> = opp.clients.into_iter().collect();
            println!(
                "  {label} ×{} ({}, ≈{} tokens) [{}]",
                opp.commands,
                human_bytes(opp.output_bytes),
                estimated_tokens(opp.output_bytes),
                clients.join(", ")
            );
        }
    }
}

fn print_unrouted_plan(groups: &BTreeMap<Route, Group>, heading: &str) {
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
    println!("\n{heading}");
    for (label, (commands, bytes, now_bytes)) in entries.into_iter().take(12) {
        let now = if now_bytes > 0 {
            format!(
                " · {} would compact now with a hook or `run --`",
                human_bytes(now_bytes)
            )
        } else {
            String::new()
        };
        println!("  {label} ×{commands} ({}){now}", human_bytes(bytes));
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
}
