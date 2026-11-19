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
use crate::workspace::Workspace;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Route {
    ThroughJustless,
    SupportedNotRouted,
    /// A supported tool in a form the hooks pass through whole, such as a pipe.
    PassedThrough,
    NotSupported,
}

impl Route {
    fn title(self) -> &'static str {
        match self {
            Self::ThroughJustless => "Through Justless",
            Self::SupportedNotRouted => "Supported, not routed",
            Self::PassedThrough => "Supported tool, passed through",
            Self::NotSupported => "Not supported yet",
        }
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
/// The first line of `justless read` output, which the hooks substitute for `cat FILE`.
static OUTLINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\Arevision [0-9a-f]+ · [0-9]+ lines\n").unwrap());

#[derive(Default, Serialize)]
pub(crate) struct Group {
    pub commands: u64,
    pub output_bytes: u64,
    pub labels: BTreeMap<String, (u64, u64)>,
}

struct Call {
    command: String,
    date: String,
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
/// labelled command is a `justless` binary at any path, or it is a bare
/// `cat FILE` whose result is the outline the hooks substitute.
pub(crate) fn routed(command: &str, label: &str, content: &Value) -> bool {
    if text_matches(content, &FOOTER) {
        return true;
    }
    let invokes_justless = label == "justless" || label.starts_with("justless ");
    let words: Vec<_> = command.split_whitespace().collect();
    let bare_cat = matches!(words.as_slice(), ["cat", file]
        if !file.starts_with('-') && !file.contains(|c: char| "|;&<>$`*?(){}[]!#~\\\"'".contains(c)));
    let first = match content {
        Value::Array(parts) => parts.first().and_then(|p| p["text"].as_str()),
        other => other.as_str(),
    };
    invokes_justless || bare_cat && first.is_some_and(|text| OUTLINE.is_match(text))
}

#[derive(Default)]
struct PlanEntry {
    commands: u64,
    bytes: u64,
    now_commands: u64,
    now_bytes: u64,
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
    let codex = crate::rollouts::coverage(&workspace.root, since_ms);
    // Sessions keep subagent transcripts under <session>/subagents/, and workflow
    // subagents one level deeper under subagents/workflows/<run>/.
    let mut files = Vec::new();
    transcripts(&dir, since, 6, &mut files);
    files.sort();

    let root = workspace.root.to_string_lossy().into_owned();
    let mut groups: BTreeMap<Route, Group> = BTreeMap::new();
    let mut unrouted: BTreeMap<String, PlanEntry> = BTreeMap::new();
    let mut sessions = BTreeSet::new();
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
        let mut calls: HashMap<String, Call> = HashMap::new();
        let Ok(handle) = File::open(file) else {
            continue;
        };
        for line in BufReader::new(handle).lines() {
            let Ok(line) = line else {
                break;
            };
            // Skip cheaply before parsing: only tool calls and results matter.
            if !line.contains("\"tool_use\"") && !line.contains("\"tool_result\"") {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
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
                        let words = crate::agents::command_label(&call.command);
                        let label = crate::history::label(&words.split(' ').collect::<Vec<_>>());
                        let route = if routed(&call.command, &label, content) {
                            Route::ThroughJustless
                        } else if crate::agents::would_route(&call.command) {
                            Route::SupportedNotRouted
                        } else if crate::agents::would_route(&label) {
                            Route::PassedThrough
                        } else {
                            Route::NotSupported
                        };
                        let bytes = text_bytes(content);
                        if plan && route != Route::ThroughJustless {
                            let entry = unrouted.entry(label.clone()).or_default();
                            entry.commands += 1;
                            entry.bytes += bytes;
                            // The command hook would route it, or the post-command
                            // hook would outline a range read or a read list.
                            if route == Route::SupportedNotRouted
                                || crate::batch::Batch::parse(&call.command).is_some()
                                || crate::agents::literal_command(&call.command).is_some_and(
                                    |words| crate::shellread::command(&words).is_some(),
                                )
                            {
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
            "token_estimate": "UTF-8 bytes / 4",
            "plan": plan.then(|| {
                let mut entries: Vec<_> = unrouted.iter().collect();
                entries.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(a.0.cmp(b.0)));
                entries.into_iter().map(|(label, e)| serde_json::json!({
                    "label": label,
                    "commands": e.commands,
                    "output_bytes": e.bytes,
                    "compactable_now_commands": e.now_commands,
                    "compactable_now_bytes": e.now_bytes,
                })).collect::<Vec<_>>()
            }),
            "groups": groups_json(&groups),
            "codex": codex.json(),
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if total == 0 {
        println!(
            "No Claude Code shell commands found for this workspace in the last {days} day(s) under {}.",
            dir.display()
        );
        print_codex(workspace, &codex, days);
        return Ok(());
    }
    println!(
        "Claude Code ran {total} shell command(s) here in the last {days} day(s), across {} session(s), returning {} of output.",
        sessions.len(),
        human_bytes(output)
    );
    print_groups(&groups);
    if plan {
        let mut entries: Vec<_> = unrouted.iter().collect();
        entries.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(a.0.cmp(b.0)));
        println!("\nUnrouted output by command family, largest first (labels only, no commands):");
        for (label, entry) in entries.iter().take(12) {
            let now = if entry.now_bytes > 0 {
                format!(
                    " · {} would compact now with a hook or `run --`",
                    human_bytes(entry.now_bytes)
                )
            } else {
                String::new()
            };
            println!(
                "  {label} ×{} ({}){now}",
                entry.commands,
                human_bytes(entry.bytes)
            );
        }
    }
    if groups.contains_key(&Route::SupportedNotRouted) {
        println!(
            "\nSupported commands reached the agent unfiltered. Install or refresh hooks with `{} init --hooks --agent claude`, then start a new session.",
            workspace.justless()
        );
    }
    print_codex(workspace, &codex, days);
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
fn print_codex(workspace: &Workspace, codex: &crate::rollouts::Coverage, days: u64) {
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
    if codex.groups.contains_key(&Route::SupportedNotRouted) {
        println!(
            "\nSupported commands reached Codex unfiltered. Install or refresh hooks with `{} init --hooks --agent codex`, trust them with /hooks in Codex, then start a new session.",
            workspace.justless()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::FOOTER;

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
}
