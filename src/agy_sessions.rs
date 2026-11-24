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
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::discover::{Group, Route};

/// One shell command Antigravity CLI ran.
pub(crate) struct Shell {
    pub session: String,
    pub command: String,
    /// Output bytes after stripping Antigravity's timestamp/status wrapper.
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
    let Ok(handle) = File::open(home.join("history.jsonl")) else {
        return sessions;
    };
    for line in BufReader::new(handle).lines() {
        let Ok(line) = line else {
            break;
        };
        if !line.contains("\"conversationId\"") || !line.contains("\"workspace\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (Some(id), Some(workspace)) = (
            entry["conversationId"].as_str(),
            entry["workspace"].as_str(),
        ) else {
            continue;
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
    }
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

fn read(
    file: &Path,
    session: &str,
    compact: bool,
    since_ms: u64,
    scan: &mut Scan,
    each: &mut impl FnMut(Shell),
) {
    let Ok(handle) = File::open(file) else {
        return;
    };
    // Each `tool_calls` slot matches the next `GENERIC` result in order:
    // `Some((command, time_ms))` for `run_command`, `None` for any other tool.
    let mut pending: VecDeque<Option<(String, u64)>> = VecDeque::new();
    for line in BufReader::new(handle).lines() {
        let Ok(line) = line else {
            break;
        };
        let wanted = line.contains("\"PLANNER_RESPONSE\"")
            || line.contains("\"USER_INPUT\"")
            || !pending.is_empty() && line.contains("\"GENERIC\"");
        if !wanted {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match entry["type"].as_str() {
            Some("USER_INPUT") => pending.clear(),
            Some("PLANNER_RESPONSE") => {
                pending.clear();
                let Some(calls) = entry["tool_calls"].as_array() else {
                    continue;
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
                        pending.push_back(Some((command, time_ms)));
                    } else {
                        pending.push_back(None);
                    }
                }
            }
            Some("GENERIC") => {
                let Some(slot) = pending.pop_front() else {
                    continue;
                };
                let Some((command, call_time)) = slot else {
                    continue;
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
                    continue;
                }
                let raw = entry["content"].as_str().unwrap_or("");
                let (hook_rewrote, body) = strip_wrapper(raw);
                scan.sessions.insert(session.to_owned());
                each(Shell {
                    session: session.to_owned(),
                    command,
                    output: body.to_owned(),
                    bytes: body.len() as u64,
                    hook_rewrote,
                });
            }
            _ => {}
        }
    }
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

/// Strips Antigravity CLI's `Created At` / `Completed At` / `Command exited with
/// code` headers from a `run_command` result, and reports whether a pre-tool hook
/// rewrote `CommandLine`.
fn strip_wrapper(raw: &str) -> (bool, &str) {
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
    if let Some(after) = rest.strip_prefix("Command exited with code ")
        && let Some((_, tail)) = after.split_once('\n')
    {
        let tail = tail.trim_start_matches(['\n', '\r']);
        if let Some(body) = tail
            .strip_prefix("Command output:\n")
            .or_else(|| tail.strip_prefix("Command output:\r\n"))
        {
            return (hook_rewrote, body);
        }
        return (hook_rewrote, tail);
    }
    (hook_rewrote, rest)
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
/// Justless, using the Antigravity CLI hook's rules (the same pre-command rules
/// as Codex).
pub(crate) fn coverage(workspace: &crate::workspace::Workspace, since_ms: u64) -> Coverage {
    let mut groups: BTreeMap<Route, Group> = BTreeMap::new();
    let exclusions = crate::rewrite::Exclusions::of(workspace).unwrap_or_default();
    let scan = scan(&workspace.root, since_ms, |shell| {
        let _ = &shell.session;
        let words = crate::agents::command_label(&shell.command);
        let label = crate::history::label(&words.split(' ').collect::<Vec<_>>());
        let content = Value::String(shell.output);
        let routed = crate::discover::routed(&shell.command, &label, &content)
            || shell.hook_rewrote && crate::agents::codex_would_route(&shell.command, &exclusions);
        let route = if routed {
            Route::ThroughJustless
        } else {
            Route::unrouted(
                &shell.command,
                &label,
                &exclusions,
                false,
                crate::agents::codex_would_route,
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
    use super::{decode_command, strip_wrapper};

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
        assert_eq!(strip_wrapper(rewritten), (true, "ok\n"));

        let raw = concat!(
            "Created At: 2026-11-25T00:05:32+05:45\n",
            "Completed At: 2026-11-25T00:05:36+05:45\n",
            "Command exited with code 1.\n\n",
            "Command output:\n",
            "boom\n",
        );
        assert_eq!(strip_wrapper(raw), (false, "boom\n"));

        assert_eq!(decode_command("\"git diff\"", true), "git diff");
        assert_eq!(decode_command("\"git diff\"", false), "\"git diff\"");
    }
}
