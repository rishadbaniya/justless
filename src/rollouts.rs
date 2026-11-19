//! Shell commands Codex ran in a workspace, read from its session rollouts.
//!
//! Codex writes each thread as a JSONL rollout, `rollout-<time>-<thread>.jsonl`,
//! under `$CODEX_HOME/sessions/YYYY/MM/DD/` (`~/.codex/sessions` by default), and
//! moves archived threads to `$CODEX_HOME/archived_sessions/`. Each line is
//! `{"timestamp", "type", "payload"}`. Codex does not document the format. These
//! records were observed in rollouts written from June to November 2026:
//!
//! - `session_meta`: `session_id`, the root thread that hooks also send as
//!   `session_id` (`id` when it is missing), and `cwd`, where the thread started.
//! - `turn_context`: the turn's `cwd`.
//! - `response_item` `function_call` named `exec_command` (`arguments` holds `cmd`
//!   and `workdir`), `shell_command` (`command` and `workdir`) or `shell` (`command`
//!   as words), answered by a `function_call_output` with the same `call_id`. That
//!   output is what the model read. `exec_command` text starts with a header that
//!   holds `Process exited with code N` and ends with `Output:`, and a command that
//!   keeps running reports `Process running with session ID N` instead; the model
//!   reads the rest of its output later, through `write_stdin`.
//! - `event_msg` `item_completed` whose `item` is a `CommandExecution`, written
//!   since late August 2026: `command` (`[shell, "-lc", text]`), `cwd` (a `file://`
//!   URI), `exit_code` and `aggregated_output`. Its `id` is `exec-…` for a call from
//!   a code-mode script and the `call_id` for a direct call, which is read once: with
//!   its result, or from this item when the command kept running.
//! - `response_item` `custom_tool_call` named `exec`, a code-mode script, answered
//!   by a `custom_tool_call_output` that the model read. Before the
//!   `CommandExecution` items, the commands a script ran were not recorded.
//!
//! Only labels and byte counts leave this module's callers; output is scanned, never
//! printed.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::discover::{Group, Route};

/// One shell command Codex ran.
pub(crate) struct Shell {
    /// The root thread, which Codex hooks send as `session_id`.
    pub session: String,
    pub time_ms: u64,
    /// The directory the command ran in, as Codex recorded it.
    pub cwd: String,
    pub command: String,
    /// What came back without Codex's header: the output the model read for a direct
    /// call, or the command's whole output for a call from a code-mode script.
    pub output: String,
    /// A direct call's result as the model read it, header included, or the output a
    /// code-mode script received.
    pub bytes: u64,
    /// `None` when Codex recorded no exit status, as for a call a hook refused.
    pub status: Option<i32>,
    /// Called from a code-mode script, which received the output instead of the model.
    pub nested: bool,
    /// The rollout and the line that finished the call.
    pub file: PathBuf,
    pub line: usize,
}

#[derive(Default)]
pub(crate) struct Scan {
    /// `$CODEX_HOME`, or `~/.codex`.
    pub home: Option<PathBuf>,
    /// Rollouts of threads that started in the workspace.
    pub files: u64,
    pub sessions: BTreeSet<String>,
    /// Code-mode script results, and the bytes the model read from them.
    pub scripts: u64,
    pub script_bytes: u64,
    /// Calls read again from a later rollout, such as a forked thread's copy.
    pub repeated: u64,
}

const SHELL_TOOLS: [&str; 3] = ["exec_command", "shell_command", "shell"];

pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
}

/// Shell calls that ran in `root` or below at or after `since_ms`, from rollouts of
/// threads that started there, each rollout's calls in order.
pub(crate) fn scan(root: &Path, since_ms: u64, mut each: impl FnMut(Shell)) -> Scan {
    let mut scan = Scan {
        home: home(),
        ..Scan::default()
    };
    let Some(home) = scan.home.clone() else {
        return scan;
    };
    let since = UNIX_EPOCH + Duration::from_millis(since_ms);
    let mut files = Vec::new();
    crate::discover::transcripts(&home.join("sessions"), since, 4, &mut files);
    crate::discover::transcripts(&home.join("archived_sessions"), since, 0, &mut files);
    files.retain(|file| {
        file.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
    });
    // A rollout's name starts with the time its thread started.
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    let mut seen = HashSet::new();
    for file in files {
        read(&file, root, since_ms, &mut seen, &mut scan, &mut each);
    }
    scan
}

struct Call {
    command: String,
    cwd: String,
    time_ms: u64,
    status: Option<i32>,
}

fn read(
    file: &Path,
    root: &Path,
    since_ms: u64,
    seen: &mut HashSet<String>,
    scan: &mut Scan,
    each: &mut impl FnMut(Shell),
) {
    let Ok(handle) = File::open(file) else {
        return;
    };
    let within = |cwd: &str| Path::new(cwd).starts_with(root);
    let mut session: Option<String> = None;
    let mut turn = String::new();
    let mut calls: HashMap<String, Call> = HashMap::new();
    // Direct calls answered, and those whose command was still running then.
    let mut answered: HashSet<String> = HashSet::new();
    let mut running: HashMap<String, Call> = HashMap::new();
    let mut scripts: HashSet<String> = HashSet::new();
    for (index, line) in BufReader::new(handle).lines().enumerate() {
        let Ok(line) = line else {
            break;
        };
        // Parse only lines that can matter; script results are most of a rollout.
        let wanted = if session.is_none() {
            line.contains("\"session_meta\"")
        } else {
            line.contains("\"turn_context\"")
                || line.contains("\"function_call\"")
                || line.contains("\"CommandExecution\"")
                || line.contains("\"custom_tool_call\"")
                || !calls.is_empty() && line.contains("\"function_call_output\"")
                || !scripts.is_empty() && line.contains("\"custom_tool_call_output\"")
        };
        if !wanted {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = &entry["payload"];
        let time_ms = entry["timestamp"]
            .as_str()
            .and_then(crate::learn::events::parse_timestamp)
            .unwrap_or(0);
        let id = || payload["call_id"].as_str().map(str::to_owned);
        match (entry["type"].as_str(), payload["type"].as_str()) {
            (Some("session_meta"), _) if session.is_none() => {
                let cwd = native(&payload["cwd"]);
                if !within(cwd) {
                    return;
                }
                let thread = payload["session_id"].as_str().or(payload["id"].as_str());
                session = Some(thread.unwrap_or_default().to_owned());
                cwd.clone_into(&mut turn);
                scan.files += 1;
            }
            _ if session.is_none() => {}
            (Some("turn_context"), _) => {
                if payload["cwd"].is_string() {
                    native(&payload["cwd"]).clone_into(&mut turn);
                }
            }
            (Some("response_item"), Some("function_call")) => {
                let name = payload["name"].as_str().unwrap_or_default();
                let (Some(id), true) = (id(), SHELL_TOOLS.contains(&name)) else {
                    continue;
                };
                let arguments: Value = payload["arguments"]
                    .as_str()
                    .and_then(|text| serde_json::from_str(text).ok())
                    .unwrap_or(Value::Null);
                let command = match &arguments[if name == "exec_command" {
                    "cmd"
                } else {
                    "command"
                }] {
                    Value::String(text) => Some(text.clone()),
                    Value::Array(words) => words_text(words),
                    _ => None,
                };
                let Some(command) = command else {
                    continue;
                };
                let cwd = match arguments["workdir"].as_str() {
                    Some(dir) => Path::new(&turn).join(dir).to_string_lossy().into_owned(),
                    None => turn.clone(),
                };
                calls.insert(
                    id,
                    Call {
                        command,
                        cwd,
                        time_ms,
                        status: None,
                    },
                );
            }
            (Some("response_item"), Some("custom_tool_call")) => {
                if payload["name"] == "exec"
                    && time_ms >= since_ms
                    && let Some(id) = id()
                {
                    scripts.insert(id);
                }
            }
            (Some("response_item"), Some("custom_tool_call_output")) => {
                if let Some(id) = id()
                    && scripts.remove(&id)
                {
                    if seen.insert(id) {
                        scan.scripts += 1;
                        scan.script_bytes += text(&payload["output"]).len() as u64;
                    } else {
                        scan.repeated += 1;
                    }
                }
            }
            (Some("event_msg"), Some("item_completed")) => {
                let item = &payload["item"];
                let Some(id) = item["id"].as_str() else {
                    continue;
                };
                // The user's own `!` commands are not the agent's.
                if item["type"] != "CommandExecution" || item["source"] == "user_shell" {
                    continue;
                }
                let status = item["exit_code"]
                    .as_i64()
                    .and_then(|c| i32::try_from(c).ok());
                // A direct call is read with the result the model received.
                if let Some(call) = calls.get_mut(id) {
                    call.status = status;
                    if item["cwd"].is_string() {
                        native(&item["cwd"]).clone_into(&mut call.cwd);
                    }
                    continue;
                }
                let direct = running.remove(id);
                if direct.is_none() && answered.contains(id) {
                    continue;
                }
                let cwd = match native(&item["cwd"]) {
                    "" => turn.clone(),
                    cwd => cwd.to_owned(),
                };
                let nested = direct.is_none();
                let (command, time_ms) = match direct {
                    Some(call) => (Some(call.command), call.time_ms),
                    None => (
                        item["command"].as_array().and_then(|w| words_text(w)),
                        time_ms,
                    ),
                };
                let (Some(command), true) = (command, time_ms >= since_ms && within(&cwd)) else {
                    continue;
                };
                if !seen.insert(id.to_owned()) {
                    scan.repeated += 1;
                    continue;
                }
                let output = item["aggregated_output"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let thread = session.clone().unwrap_or_default();
                scan.sessions.insert(thread.clone());
                each(Shell {
                    session: thread,
                    time_ms,
                    cwd,
                    command,
                    bytes: output.len() as u64,
                    output,
                    status,
                    nested,
                    file: file.to_path_buf(),
                    line: index + 1,
                });
            }
            (Some("response_item"), Some("function_call_output")) => {
                let Some(call) = id().and_then(|id| calls.remove(&id).map(|call| (id, call)))
                else {
                    continue;
                };
                let (id, call) = call;
                answered.insert(id.clone());
                if call.time_ms < since_ms || !within(&call.cwd) {
                    continue;
                }
                let whole = text(&payload["output"]);
                let Some((status, output)) = result(&whole) else {
                    running.insert(id, call);
                    continue;
                };
                if !seen.insert(id) {
                    scan.repeated += 1;
                    continue;
                }
                let thread = session.clone().unwrap_or_default();
                scan.sessions.insert(thread.clone());
                each(Shell {
                    session: thread,
                    time_ms: call.time_ms,
                    cwd: call.cwd,
                    command: call.command,
                    output,
                    bytes: whole.len() as u64,
                    status: call.status.or(status),
                    nested: false,
                    file: file.to_path_buf(),
                    line: index + 1,
                });
            }
            _ => {}
        }
    }
}

/// A directory Codex recorded, as a path or, in newer items, a `file://` URI. Like
/// the hooks, this takes the URI's path as written.
fn native(value: &Value) -> &str {
    let path = value.as_str().unwrap_or_default();
    path.strip_prefix("file://").unwrap_or(path)
}

/// The command a shell ran: the script of `bash -lc SCRIPT`, or else the words.
fn words_text(words: &[Value]) -> Option<String> {
    let words: Vec<&str> = words.iter().map(Value::as_str).collect::<Option<_>>()?;
    match words.as_slice() {
        [_, flag, script] if flag.starts_with('-') && flag.ends_with('c') => {
            Some((*script).to_owned())
        }
        _ => Some(shlex::try_join(words.iter().copied()).unwrap_or_else(|_| words.join(" "))),
    }
}

/// A tool output as text: a string, or the text of each `input_text` part.
fn text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The exit status and output of a direct call's result, or `None` while the command
/// keeps running. An `exec_command` header ends with `Output:`; an older `shell`
/// result is JSON with `output` and `metadata.exit_code`. A result with neither,
/// such as a refusal, has no status.
fn result(text: &str) -> Option<(Option<i32>, String)> {
    let start = if text.starts_with("Output:") {
        Some(0)
    } else {
        text.find("\nOutput:").map(|at| at + 1)
    };
    if let Some(at) = start.filter(|&at| at < 1024) {
        let mut status = None;
        for line in text[..at].lines() {
            if line.starts_with("Process running with session ID") {
                return None;
            }
            let code = line
                .strip_prefix("Process exited with code ")
                .or_else(|| line.strip_prefix("Exit code: "));
            status = status.or_else(|| code.and_then(|code| code.trim().parse().ok()));
        }
        let body = &text[at + "Output:".len()..];
        return Some((status, body.strip_prefix('\n').unwrap_or(body).to_owned()));
    }
    if text.starts_with('{')
        && let Ok(value) = serde_json::from_str::<Value>(text)
        && let Some(output) = value["output"].as_str()
    {
        let status = value["metadata"]["exit_code"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok());
        return Some((status, output.to_owned()));
    }
    Some((None, text.to_owned()))
}

/// Codex shell commands by route and label, as `discover` and `stats` report them.
#[derive(Default)]
pub(crate) struct Coverage {
    pub scan: Scan,
    pub groups: BTreeMap<Route, Group>,
    pub nested: u64,
    pub nested_bytes: u64,
}

impl Coverage {
    pub fn commands(&self) -> u64 {
        self.groups.values().map(|g| g.commands).sum()
    }

    pub fn bytes(&self) -> u64 {
        self.groups.values().map(|g| g.output_bytes).sum()
    }

    /// The `codex` object of `discover --json`.
    pub fn json(&self) -> Value {
        json!({
            "rollouts": self.scan.home.as_ref().map(|home| home.join("sessions")),
            "sessions": self.scan.sessions.len(),
            "commands": self.commands(),
            "output_bytes": self.bytes(),
            "code_mode_commands": self.nested,
            "code_mode_output_bytes": self.nested_bytes,
            "code_mode_results": self.scan.scripts,
            "code_mode_result_bytes": self.scan.script_bytes,
            "repeated_calls": self.scan.repeated,
            "groups": crate::discover::groups_json(&self.groups),
        })
    }
}

/// Classify each Codex shell command by whether it went through Justless, with the
/// Codex hook's rules: a list is routed only when every command in it is supported.
pub(crate) fn coverage(root: &Path, since_ms: u64) -> Coverage {
    let mut groups: BTreeMap<Route, Group> = BTreeMap::new();
    let (mut nested, mut nested_bytes) = (0, 0);
    let scan = scan(root, since_ms, |shell| {
        let words = crate::agents::command_label(&shell.command);
        let label = crate::history::label(&words.split(' ').collect::<Vec<_>>());
        let content = Value::String(shell.output);
        let route = if crate::discover::routed(&shell.command, &label, &content) {
            Route::ThroughJustless
        } else if crate::agents::codex_would_route(&shell.command) {
            Route::SupportedNotRouted
        } else if crate::agents::codex_would_route(&label) {
            Route::PassedThrough
        } else {
            Route::NotSupported
        };
        if shell.nested {
            nested += 1;
            nested_bytes += shell.bytes;
        }
        let group = groups.entry(route).or_default();
        group.commands += 1;
        group.output_bytes += shell.bytes;
        let entry = group.labels.entry(label).or_default();
        entry.0 += 1;
        entry.1 += shell.bytes;
    });
    Coverage {
        scan,
        groups,
        nested,
        nested_bytes,
    }
}

/// The `codex` part of `stats`: how much Codex shell output went through Justless,
/// or `None` when Codex ran no shell command here in the window.
pub(crate) struct Stats {
    pub json: Value,
    pub line: String,
}

pub(crate) fn stats(workspace: &crate::workspace::Workspace, since_ms: u64) -> Option<Stats> {
    let codex = coverage(&workspace.root, since_ms);
    let (commands, bytes) = (codex.commands(), codex.bytes());
    if commands == 0 {
        return None;
    }
    let routed = codex.groups.get(&Route::ThroughJustless);
    let (routed, routed_bytes) = routed.map_or((0, 0), |g| (g.commands, g.output_bytes));
    let human = crate::history::human_bytes;
    Some(Stats {
        json: json!({
            "commands": commands,
            "output_bytes": bytes,
            "through_justless_commands": routed,
            "through_justless_bytes": routed_bytes,
            "code_mode_commands": codex.nested,
            "code_mode_output_bytes": codex.nested_bytes,
        }),
        line: format!(
            "\nCodex shell commands here: {commands} returning {}, {routed} of them ({}) through Justless. `{} discover` breaks them down.",
            human(bytes),
            human(routed_bytes),
            workspace.justless()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::result;

    #[test]
    fn results_keep_their_exit_status_and_drop_the_header() {
        let exec = "Chunk ID: a1\nWall time: 0.1 seconds\nProcess exited with code 101\nOriginal token count: 9\nOutput:\nerror[E0425]\n";
        assert_eq!(result(exec), Some((Some(101), "error[E0425]\n".into())));
        let running = "Chunk ID: a1\nWall time: 10 seconds\nProcess running with session ID 7\nOutput:\nCompiling\n";
        assert_eq!(result(running), None);
        let shell = r#"{"output":"ok\n","metadata":{"exit_code":0,"duration_seconds":0.1}}"#;
        assert_eq!(result(shell), Some((Some(0), "ok\n".into())));
        let refused = "Command blocked by PreToolUse hook: no. Command: rm -rf x";
        assert_eq!(result(refused), Some((None, refused.into())));
        assert_eq!(result("Output:"), Some((None, String::new())));
    }
}
