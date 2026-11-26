//! Shell commands Antigravity CLI ran in a workspace, read from its local transcripts.
//!
//! Antigravity CLI maps each conversation to its workspace in
//! `~/.gemini/antigravity-cli/history.jsonl` (`{"conversationId", "workspace"}`),
//! and writes full step records to
//! `~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript_full.jsonl`
//! (falling back to `transcript.jsonl` when the full log is absent). A
//! `PLANNER_RESPONSE` step lists `tool_calls`, and the following `GENERIC` steps
//! answer those calls in order. When a `PreToolUse` hook rewrote `CommandLine`,
//! the result begins with Antigravity's rewrite notice.
//!
//! Only command labels and byte counts leave this module's callers; command lines
//! and output are scanned, never printed.
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::discover::{Group, Route, for_each_jsonl_line};

/// One shell command Antigravity CLI ran.
pub(crate) struct Shell {
    pub file: PathBuf,
    pub line: usize,
    pub session: String,
    pub cwd: String,
    pub command: String,
    pub time_ms: u64,
    /// Exit status from `Command exited with code N.`, if reported.
    pub status: Option<i32>,
    /// Output text after stripping Antigravity's timestamp, truncation and status headers.
    pub output: String,
    pub bytes: u64,
    /// Whether Antigravity prefixed the result with its `CommandLine` rewrite notice.
    pub hook_rewrote: bool,
}

#[derive(Default)]
pub(crate) struct Scan {
    /// `$JUSTLESS_TEST_AGY_HOME`, or `~/.gemini/antigravity-cli`.
    pub home: Option<PathBuf>,
    /// Transcript files belonging to the workspace that were read.
    pub files: u64,
    pub sessions: BTreeSet<String>,
}

const REWRITE_PREFIX: &str =
    "A pre-tool hook changed the arguments of this tool call before it ran.";

pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("JUSTLESS_TEST_AGY_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".gemini/antigravity-cli"))
        })
}

/// Conversation IDs recorded under `root` in `<home>/history.jsonl`.
fn workspace_sessions(home: &Path, root: &Path) -> BTreeSet<String> {
    let mut sessions = BTreeSet::new();
    for_each_jsonl_line(&home.join("history.jsonl"), None, |_, line| {
        if !line.contains("\"conversationId\"") || !line.contains("\"workspace\"") {
            return true;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return true;
        };
        let (Some(id), Some(workspace)) = (
            entry["conversationId"].as_str(),
            entry["workspace"].as_str(),
        ) else {
            return true;
        };
        let raw = Path::new(workspace.strip_prefix("file://").unwrap_or(workspace));
        let resolved = raw.canonicalize().ok();
        if raw.starts_with(root)
            || resolved
                .as_deref()
                .is_some_and(|path| path.starts_with(root))
        {
            sessions.insert(id.to_owned());
        }
        true
    });
    sessions
}

/// Shell calls Antigravity CLI ran in `root` at or after `since_ms`.
pub(crate) fn scan(root: &Path, since_ms: u64, mut each: impl FnMut(Shell)) -> Scan {
    let mut scan = Scan {
        home: home(),
        ..Scan::default()
    };
    let Some(home) = scan.home.clone() else {
        return scan;
    };
    let brain = home.join("brain");
    if !brain.is_dir() {
        return scan;
    }
    let since = UNIX_EPOCH + Duration::from_millis(since_ms);
    let ids = workspace_sessions(&home, root);
    for id in ids {
        let logs = brain.join(&id).join(".system_generated/logs");
        let full = logs.join("transcript_full.jsonl");
        let (path, compact) = if full.is_file() {
            (full, false)
        } else {
            let short = logs.join("transcript.jsonl");
            if short.is_file() {
                (short, true)
            } else {
                continue;
            }
        };
        if fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .is_some_and(|modified| modified < since)
        {
            continue;
        }
        scan.files += 1;
        read(&path, &id, compact, since_ms, &mut scan, &mut each);
    }
    scan
}

fn agy_line_wanted(line: &str, has_pending: bool, has_pending_tasks: bool) -> bool {
    if has_pending {
        if line.contains("\"GENERIC\"")
            || line.contains("\"PLANNER_RESPONSE\"")
            || line.contains("\"USER_INPUT\"")
        {
            return true;
        }
    } else if line.contains("\"PLANNER_RESPONSE\"") && line.contains("\"run_command\"") {
        return true;
    }
    has_pending_tasks
        && line.contains("\"SYSTEM_MESSAGE\"")
        && line.contains("finished with result:")
}

fn read(
    file: &Path,
    session: &str,
    compact: bool,
    since_ms: u64,
    scan: &mut Scan,
    each: &mut impl FnMut(Shell),
) {
    read_inner(file, session, compact, since_ms, scan, true, None, each);
}

#[cfg(test)]
pub(crate) fn read_for_test(
    file: &Path,
    session: &str,
    compact: bool,
    since_ms: u64,
    use_prefilter: bool,
) -> (Scan, Vec<Shell>, usize) {
    let mut scan = Scan::default();
    let mut shells = Vec::new();
    let mut parsed = 0usize;
    read_inner(
        file,
        session,
        compact,
        since_ms,
        &mut scan,
        use_prefilter,
        Some(&mut parsed),
        &mut |shell| shells.push(shell),
    );
    (scan, shells, parsed)
}

#[allow(clippy::too_many_arguments)]
fn read_inner(
    file: &Path,
    session: &str,
    compact: bool,
    since_ms: u64,
    scan: &mut Scan,
    use_prefilter: bool,
    mut parsed_lines: Option<&mut usize>,
    each: &mut impl FnMut(Shell),
) {
    // Each `tool_calls` slot matches the next `GENERIC` result in order:
    // `Some((command, cwd, time_ms))` for `run_command`, `None` for any other tool.
    // Trailing `None`s are trimmed so turns with no `run_command` never force parsing
    // of subsequent `GENERIC` file-read outputs.
    let mut pending: VecDeque<Option<(String, String, u64)>> = VecDeque::new();
    // Background `run_command` calls return a task id first and deliver their
    // real exit code and output in a later `SYSTEM_MESSAGE`.
    let mut pending_tasks: HashMap<String, (String, String, u64, bool)> = HashMap::new();
    for_each_jsonl_line(file, None, |index, line| {
        if use_prefilter && !agy_line_wanted(line, !pending.is_empty(), !pending_tasks.is_empty()) {
            return true;
        }
        if let Some(count) = parsed_lines.as_deref_mut() {
            *count += 1;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return true;
        };
        match entry["type"].as_str() {
            Some("USER_INPUT") => pending.clear(),
            Some("PLANNER_RESPONSE") => {
                pending.clear();
                let Some(calls) = entry["tool_calls"].as_array() else {
                    return true;
                };
                let time_ms = entry["created_at"]
                    .as_str()
                    .and_then(crate::learn::events::parse_timestamp)
                    .unwrap_or(0);
                for call in calls {
                    if call["name"] == "run_command"
                        && let Some(raw) = call["args"]["CommandLine"].as_str()
                    {
                        let command = decode_command(raw, compact);
                        let cwd = call["args"]["Cwd"]
                            .as_str()
                            .map(|raw| decode_command(raw, compact))
                            .unwrap_or_default();
                        pending.push_back(Some((command, cwd, time_ms)));
                    } else {
                        pending.push_back(None);
                    }
                }
                while pending.back().is_some_and(Option::is_none) {
                    pending.pop_back();
                }
            }
            Some("GENERIC") => {
                let Some(slot) = pending.pop_front() else {
                    return true;
                };
                let Some((command, cwd, call_time)) = slot else {
                    return true;
                };
                let raw = entry["content"].as_str().unwrap_or("");
                if let Some((started_rewrote, task_id)) = background_task_id(raw) {
                    pending_tasks.insert(
                        task_id.to_owned(),
                        (command, cwd, call_time, started_rewrote),
                    );
                    return true;
                }
                let result_time = entry["created_at"]
                    .as_str()
                    .and_then(crate::learn::events::parse_timestamp)
                    .unwrap_or(call_time);
                let time_ms = if result_time > 0 {
                    result_time
                } else {
                    call_time
                };
                if time_ms > 0 && time_ms < since_ms {
                    return true;
                }
                let (hook_rewrote, status, body) = parse_result(raw);
                scan.sessions.insert(session.to_owned());
                each(Shell {
                    file: file.to_path_buf(),
                    line: index + 1,
                    session: session.to_owned(),
                    cwd,
                    command,
                    time_ms,
                    status,
                    output: body.to_owned(),
                    bytes: body.len() as u64,
                    hook_rewrote,
                });
            }
            Some("SYSTEM_MESSAGE") => {
                let raw = entry["content"].as_str().unwrap_or("");
                let Some((task_id, task_body)) = parse_task_completion(raw) else {
                    return true;
                };
                let Some((command, cwd, call_time, started_rewrote)) =
                    pending_tasks.remove(task_id)
                else {
                    return true;
                };
                let result_time = entry["created_at"]
                    .as_str()
                    .and_then(crate::learn::events::parse_timestamp)
                    .unwrap_or(call_time);
                let time_ms = if result_time > 0 {
                    result_time
                } else {
                    call_time
                };
                if time_ms > 0 && time_ms < since_ms {
                    return true;
                }
                let (finished_rewrote, status, body) = parse_result(task_body);
                scan.sessions.insert(session.to_owned());
                each(Shell {
                    file: file.to_path_buf(),
                    line: index + 1,
                    session: session.to_owned(),
                    cwd,
                    command,
                    time_ms,
                    status,
                    output: body.to_owned(),
                    bytes: body.len() as u64,
                    hook_rewrote: started_rewrote || finished_rewrote,
                });
            }
            _ => {}
        }
        true
    });
}

/// Compact `transcript.jsonl` stores each tool argument as a JSON-encoded string
/// (`"\"git diff\""`), while `transcript_full.jsonl` stores the plain text.
fn decode_command(raw: &str, compact: bool) -> String {
    if compact
        && raw.starts_with('"')
        && raw.ends_with('"')
        && let Ok(decoded) = serde_json::from_str::<String>(raw)
    {
        return decoded;
    }
    raw.to_owned()
}

/// If `raw` is Antigravity CLI's background-task launch notice, return
/// `(hook_rewrote, task_id)`.
fn background_task_id(raw: &str) -> Option<(bool, &str)> {
    let mut rest = raw;
    let mut hook_rewrote = false;
    loop {
        let trimmed = rest.trim_start_matches(['\n', '\r']);
        if trimmed.len() != rest.len() {
            rest = trimmed;
            continue;
        }
        if let Some(after) = rest
            .strip_prefix("Created At:")
            .or_else(|| rest.strip_prefix("Completed At:"))
        {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
            continue;
        }
        if let Some(after) = rest.strip_prefix(REWRITE_PREFIX) {
            let (line, tail) = after.split_once('\n').unwrap_or((after, ""));
            if line.contains("CommandLine") {
                hook_rewrote = true;
            }
            rest = tail;
            continue;
        }
        break;
    }
    let after = rest.strip_prefix("Tool is running as a background task with task id:")?;
    let id = after
        .split_once('\n')
        .map_or(after, |(line, _)| line)
        .trim();
    (!id.is_empty()).then_some((hook_rewrote, id))
}

/// Extract `(task_id, body)` from an Antigravity CLI `SYSTEM_MESSAGE` reporting
/// a finished background `run_command` task.
fn parse_task_completion(raw: &str) -> Option<(&str, &str)> {
    let after_marker = raw.split_once("Task id \"")?.1;
    let (task_id, rest) = after_marker.split_once("\" finished with result:")?;
    let mut body = rest
        .trim_start_matches(['\r', '\n'])
        .trim_end_matches(['\r', '\n']);
    if let Some(before_end) = body.strip_suffix("</SYSTEM_MESSAGE>") {
        body = before_end.trim_end_matches(['\r', '\n']);
    }
    if let Some((before_log, last_line)) = body.rsplit_once('\n')
        && last_line.trim_start().starts_with("Log: file://")
    {
        body = before_log.trim_end_matches(['\r', '\n']);
    } else if body.trim_start().starts_with("Log: file://") {
        body = "";
    }
    Some((task_id, body))
}

/// Strips Antigravity CLI's `Created At` / `Completed At` / truncation /
/// `Command exited with code` headers from a `run_command` result, returning
/// `(hook_rewrote, exit_status, body)`.
fn parse_result(raw: &str) -> (bool, Option<i32>, &str) {
    let mut rest = raw;
    let mut hook_rewrote = false;
    loop {
        let trimmed = rest.trim_start_matches(['\n', '\r']);
        if trimmed.len() != rest.len() {
            rest = trimmed;
            continue;
        }
        if let Some(after) = rest
            .strip_prefix("Created At:")
            .or_else(|| rest.strip_prefix("Completed At:"))
            .or_else(|| rest.strip_prefix("The tool's output was truncated"))
            .or_else(|| rest.strip_prefix("Here is a preview"))
        {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
            continue;
        }
        if let Some(after) = rest.strip_prefix(REWRITE_PREFIX) {
            let (line, tail) = after.split_once('\n').unwrap_or((after, ""));
            if line.contains("CommandLine") {
                hook_rewrote = true;
            }
            rest = tail;
            continue;
        }
        break;
    }
    if let Some(after) = rest.strip_prefix("Command exited with code ") {
        let (head, tail) = after.split_once('\n').unwrap_or((after, ""));
        let status = head.trim().trim_end_matches('.').parse::<i32>().ok();
        let tail = tail.trim_start_matches(['\n', '\r']);
        if let Some(body) = tail
            .strip_prefix("Command output:\n")
            .or_else(|| tail.strip_prefix("Command output:\r\n"))
        {
            return (hook_rewrote, status, body);
        }
        if tail.trim() == "No output from the command." {
            return (hook_rewrote, status, "");
        }
        return (hook_rewrote, status, tail);
    }
    (hook_rewrote, None, rest)
}

/// Antigravity CLI shell commands by route and label, as `discover` and `stats`
/// report them.
#[derive(Default)]
pub(crate) struct Coverage {
    pub scan: Scan,
    pub groups: BTreeMap<Route, Group>,
}

impl Coverage {
    pub fn commands(&self) -> u64 {
        self.groups.values().map(|g| g.commands).sum()
    }

    pub fn bytes(&self) -> u64 {
        self.groups.values().map(|g| g.output_bytes).sum()
    }

    /// The `agy` object of `discover --json`.
    pub fn json(&self) -> Value {
        json!({
            "transcripts": self.scan.home.as_ref().map(|home| home.join("brain")),
            "sessions": self.scan.sessions.len(),
            "commands": self.commands(),
            "output_bytes": self.bytes(),
            "groups": crate::discover::groups_json(&self.groups),
        })
    }
}

/// Classify each Antigravity CLI `run_command` call by whether it went through
/// Justless, using the Antigravity CLI hook's two-tier rules.
pub(crate) fn coverage(workspace: &crate::workspace::Workspace, since_ms: u64) -> Coverage {
    let mut groups: BTreeMap<Route, Group> = BTreeMap::new();
    let exclusions = crate::rewrite::Exclusions::of(workspace).unwrap_or_default();
    let scan = scan(&workspace.root, since_ms, |shell| {
        let _ = &shell.session;
        let words = crate::agents::command_label(&shell.command);
        let label = crate::discover::sanitize_discover_label(crate::history::label(
            &words.split(' ').collect::<Vec<_>>(),
        ));
        let content = Value::String(shell.output);
        let routed = crate::discover::routed(&shell.command, &label, &content)
            || shell.hook_rewrote && crate::agents::agy_would_route(&shell.command, &exclusions);
        let route = if routed {
            Route::ThroughJustless
        } else {
            Route::unrouted(
                &shell.command,
                &label,
                &exclusions,
                false,
                crate::agents::agy_would_route,
            )
        };
        let group = groups.entry(route).or_default();
        group.commands += 1;
        group.output_bytes += shell.bytes;
        let entry = group.labels.entry(label).or_default();
        entry.0 += 1;
        entry.1 += shell.bytes;
    });
    Coverage { scan, groups }
}

/// Aggregate Antigravity CLI coverage across several workspaces, counting each
/// conversation session at most once.
pub(crate) fn stats_all(roots: &[PathBuf], since_ms: u64) -> Option<Stats> {
    let mut seen_sessions = BTreeSet::new();
    let mut commands = 0u64;
    let mut bytes = 0u64;
    let mut routed = 0u64;
    let mut routed_bytes = 0u64;
    for root in roots {
        let Ok(workspace) = crate::workspace::Workspace::discover(Some(root)) else {
            continue;
        };
        let exclusions = crate::rewrite::Exclusions::of(&workspace).unwrap_or_default();
        let mut local_sessions = BTreeSet::new();
        scan(&workspace.root, since_ms, |shell| {
            if seen_sessions.contains(&shell.session) {
                return;
            }
            local_sessions.insert(shell.session.clone());
            let words = crate::agents::command_label(&shell.command);
            let label = crate::discover::sanitize_discover_label(crate::history::label(
                &words.split(' ').collect::<Vec<_>>(),
            ));
            let content = Value::String(shell.output);
            let is_routed = crate::discover::routed(&shell.command, &label, &content)
                || shell.hook_rewrote
                    && crate::agents::agy_would_route(&shell.command, &exclusions);
            commands += 1;
            bytes += shell.bytes;
            if is_routed {
                routed += 1;
                routed_bytes += shell.bytes;
            }
        });
        seen_sessions.extend(local_sessions);
    }
    if commands == 0 {
        return None;
    }
    let human = crate::history::human_bytes;
    Some(Stats {
        json: json!({
            "sessions": seen_sessions.len(),
            "commands": commands,
            "output_bytes": bytes,
            "through_justless_commands": routed,
            "through_justless_bytes": routed_bytes,
        }),
        line: format!(
            "Antigravity CLI shell commands: {commands} returning {}, {routed} of them ({}) through Justless across {} session(s).",
            human(bytes),
            human(routed_bytes),
            seen_sessions.len()
        ),
    })
}

/// The `agy` part of `stats`: how much Antigravity CLI shell output went through
/// Justless, or `None` when Antigravity CLI ran no shell command here in the window.
pub(crate) struct Stats {
    pub json: Value,
    pub line: String,
}

pub(crate) fn stats(workspace: &crate::workspace::Workspace, since_ms: u64) -> Option<Stats> {
    let agy = coverage(workspace, since_ms);
    let (commands, bytes) = (agy.commands(), agy.bytes());
    if commands == 0 {
        return None;
    }
    let routed = agy.groups.get(&Route::ThroughJustless);
    let (routed, routed_bytes) = routed.map_or((0, 0), |g| (g.commands, g.output_bytes));
    let human = crate::history::human_bytes;
    Some(Stats {
        json: json!({
            "commands": commands,
            "output_bytes": bytes,
            "through_justless_commands": routed,
            "through_justless_bytes": routed_bytes,
        }),
        line: format!(
            "\nAntigravity CLI shell commands here: {commands} returning {}, {routed} of them ({}) through Justless. `{} discover` breaks them down.",
            human(bytes),
            human(routed_bytes),
            workspace.justless()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::{background_task_id, decode_command, parse_result, parse_task_completion};

    #[test]
    fn wrapper_detects_hook_rewrite_and_strips_headers() {
        let rewritten = concat!(
            "Created At: 2026-11-25T00:05:32+05:45\n",
            "Completed At: 2026-11-25T00:05:36+05:45\n",
            "A pre-tool hook changed the arguments of this tool call before it ran. Changed: CommandLine.\n\n",
            "Created At: 2026-11-25T00:05:32+05:45\n",
            "Completed At: 2026-11-25T00:05:36+05:45\n",
            "Command exited with code 0.\n\n",
            "Command output:\n",
            "ok\n",
        );
        assert_eq!(parse_result(rewritten), (true, Some(0), "ok\n"));

        let raw = concat!(
            "Created At: 2026-11-25T00:05:32+05:45\n",
            "Completed At: 2026-11-25T00:05:36+05:45\n",
            "Command exited with code 1.\n\n",
            "Command output:\n",
            "boom\n",
        );
        assert_eq!(parse_result(raw), (false, Some(1), "boom\n"));

        let truncated = concat!(
            "Created At: 2026-11-25T00:05:32+05:45\n",
            "Completed At: 2026-11-25T00:05:36+05:45\n",
            "The tool's output was truncated due to being too long. You can find the full output in a file at file:///tmp/out.txt.\n\n",
            "Here is a preview with the middle of the output truncated:\n",
            "A pre-tool hook changed the arguments of this tool call before it ran. Changed: CommandLine.\n\n",
            "Command exited with code 101.\n\n",
            "Command output:\n",
            "error[E0308]: mismatched types\n",
        );
        assert_eq!(
            parse_result(truncated),
            (true, Some(101), "error[E0308]: mismatched types\n")
        );

        let empty = concat!(
            "Created At: 2026-11-25T00:05:32+05:45\n",
            "Completed At: 2026-11-25T00:05:36+05:45\n",
            "Command exited with code 0.\n\n",
            "No output from the command.\n",
        );
        assert_eq!(parse_result(empty), (false, Some(0), ""));

        let bg = concat!(
            "Created At: 2026-11-25T00:05:32+05:45\n",
            "Tool is running as a background task with task id: conv-1/task-9\n",
            "Task logs are available at: file:///tmp/task-9.log\n",
        );
        assert_eq!(background_task_id(bg), Some((false, "conv-1/task-9")));

        let done = concat!(
            "<SYSTEM_MESSAGE>\n",
            "[Message] timestamp=2026-11-24T18:52:51Z sender=conv-1/task-9 content=Task id \"conv-1/task-9\" finished with result:\n",
            "A pre-tool hook changed the arguments of this tool call before it ran. Changed: CommandLine.\n\n",
            "Command exited with code 101.\n\n",
            "Command output:\n",
            "clippy failed\n",
            "Log: file:///tmp/task-9.log\n",
            "</SYSTEM_MESSAGE>\n",
        );
        let (task_id, body) = parse_task_completion(done).unwrap();
        assert_eq!(task_id, "conv-1/task-9");
        assert_eq!(parse_result(body), (true, Some(101), "clippy failed"));

        assert_eq!(decode_command("\"git diff\"", true), "git diff");
        assert_eq!(decode_command("\"git diff\"", false), "\"git diff\"");
    }
}
