//! Hook kill switches and `justless rewrite`.
//!
//! - `JUSTLESS_DISABLE=1` makes every `hook-…` handler answer with its client's
//!   no-op, so a session runs as if no hooks were installed, without removing them.
//! - `exclude_commands` in `.justless.toml` lists commands every command hook and
//!   the post-run hook leave alone.
//! - `justless rewrite "COMMAND"` asks the real command hook what it would do with
//!   a command, and why, without running it or recording anything.

use std::io::Read;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use clap::{Arg, ArgMatches, Command};
use regex::Regex;
use serde_json::{Value, json};

use crate::agents::{self, Agent};
use crate::claude_permissions::{self, Answer, Sources, Verdict};
use crate::workspace::Workspace;

/// The environment variable that turns every hook into a no-op.
pub const DISABLE: &str = "JUSTLESS_DISABLE";

/// Whether `JUSTLESS_DISABLE` is set to anything but an empty value or `0`.
pub fn disabled() -> bool {
    std::env::var_os(DISABLE).is_some_and(|value| !value.is_empty() && value != "0")
}

/// What a client's hook prints when it leaves a call alone, or `None` to print
/// nothing: Claude Code reads `PreCompact` output as summary instructions.
pub fn noop(agent: Agent, mode: &str) -> Option<&'static str> {
    match (agent, mode) {
        (Agent::Claude, "before-compact") => None,
        (Agent::Agy, "before-command") => Some(r#"{"decision":"allow"}"#),
        _ => Some("{}"),
    }
}

/// Answer a hook call with the no-op, after reading the client's input so it
/// never writes to a closed pipe.
pub fn pass(agent: Agent, mode: &str) {
    let _ = std::io::copy(
        &mut std::io::stdin().take(8 * 1024 * 1024),
        &mut std::io::sink(),
    );
    if let Some(answer) = noop(agent, mode) {
        println!("{answer}");
    }
}

/// Set while `justless rewrite` asks a hook, so the hook records nothing.
static PREVIEW: AtomicBool = AtomicBool::new(false);

/// Whether the running hook is only a preview for `justless rewrite`.
pub fn previewing() -> bool {
    PREVIEW.load(Ordering::Relaxed)
}

/// Reject an `exclude_commands` entry that is empty or an invalid expression.
pub fn check_exclusion(entry: &str) -> Result<()> {
    ensure!(
        !entry.trim().is_empty(),
        "exclude_commands entries must not be empty"
    );
    if entry.starts_with('^') {
        Regex::new(entry)
            .map_err(|error| anyhow::anyhow!("invalid exclude_commands regex {entry}: {error}"))?;
    }
    Ok(())
}

/// The project's `exclude_commands`. Settings that cannot be read exclude
/// nothing, so the hooks behave as they did before the setting existed.
pub struct Exclusions(Vec<String>);

impl Exclusions {
    pub fn of(workspace: &Workspace) -> Self {
        Self(
            workspace
                .settings()
                .map(|settings| settings.exclude_commands)
                .unwrap_or_default(),
        )
    }

    /// The entry that matches one command: its text, or the words after any
    /// `NAME=value` assignments, start with the entry's words, or match the
    /// entry's expression.
    pub fn matching(&self, segment: &str) -> Option<&str> {
        if self.0.is_empty() {
            return None;
        }
        let text = segment.trim();
        let words = crate::shell_form::parse(text).and_then(|form| text.get(form.command));
        self.0
            .iter()
            .find(|entry| {
                [Some(text), words]
                    .into_iter()
                    .flatten()
                    .any(|candidate| matches(entry, candidate))
            })
            .map(String::as_str)
    }

    /// The entry that matches a line that is a single command, or that cannot be
    /// split into commands, such as a pipe.
    pub fn line(&self, command: &str) -> Option<&str> {
        let single = agents::split_compound(command).is_none_or(|segments| segments.len() < 2);
        single.then(|| self.matching(command)).flatten()
    }

    /// The entry that matches the line or any command in it.
    pub fn any(&self, command: &str) -> Option<&str> {
        self.matching(command).or_else(|| {
            agents::split_compound(command)?
                .into_iter()
                .find_map(|range| self.matching(&command[range]))
        })
    }

    /// The entry that matches the command of `line` whose words are at `words`.
    pub fn within(&self, line: &str, words: &Range<usize>) -> Option<&str> {
        let segment = agents::split_compound(line)
            .and_then(|segments| {
                segments
                    .into_iter()
                    .find(|range| range.start <= words.start && words.end <= range.end)
            })
            .unwrap_or_else(|| words.clone());
        self.matching(&line[segment])
            .or_else(|| self.matching(&line[words.clone()]))
    }
}

fn matches(entry: &str, text: &str) -> bool {
    if entry.starts_with('^') {
        return Regex::new(entry).is_ok_and(|pattern| pattern.is_match(text));
    }
    let entry = entry.trim_start();
    text.strip_prefix(entry).is_some_and(|rest| {
        rest.is_empty()
            || rest.starts_with(char::is_whitespace)
            || entry.ends_with(char::is_whitespace)
    })
}

// ---------------------------------------------------------------------------
// `justless rewrite`

const AGENTS: [&str; 6] = ["claude", "codex", "agy", "cursor", "gemini", "copilot"];
const MODES: [&str; 6] = [
    "default",
    "acceptEdits",
    "plan",
    "auto",
    "dontAsk",
    "bypassPermissions",
];

pub fn command() -> Command<'static> {
    Command::new("rewrite")
        .about(
            "Show what an agent's command hook would do with a shell command, without running it",
        )
        .arg(Arg::new("command").required(true).value_name("COMMAND"))
        .arg(
            Arg::new("agent")
                .long("agent")
                .takes_value(true)
                .default_value("claude")
                .possible_values(AGENTS)
                .help("The client whose hook to ask"),
        )
        .arg(
            Arg::new("mode")
                .long("mode")
                .takes_value(true)
                .default_value("default")
                .possible_values(MODES)
                .help("Claude Code's permission mode"),
        )
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<()> {
    let command = matches.value_of("command").unwrap();
    let name = matches.value_of("agent").unwrap();
    let agent = Agent::parse(name)?;
    let mode = matches.value_of("mode").unwrap();
    let report = explain(workspace, agent, mode, command);
    print!("{report}");
    Ok(())
}

/// The hook input each client sends for a shell command run in `cwd`.
fn input(workspace: &Workspace, agent: Agent, mode: &str, command: &str) -> Value {
    let cwd = workspace.cwd.display().to_string();
    match agent {
        Agent::Claude => json!({
            "hook_event_name": "PreToolUse", "tool_name": "Bash", "cwd": cwd,
            "permission_mode": mode, "tool_input": {"command": command},
        }),
        Agent::Codex => json!({
            "hook_event_name": "PreToolUse", "tool_name": "Bash", "cwd": cwd,
            "tool_input": {"command": command},
        }),
        Agent::Agy => json!({
            "workspacePaths": [workspace.root.display().to_string()],
            "toolCall": {"name": "run_command", "args": {"CommandLine": command, "Cwd": cwd}},
        }),
        Agent::Cursor => json!({
            "tool_name": "Shell", "cwd": cwd,
            "tool_input": {"command": command, "working_directory": cwd},
        }),
        Agent::Gemini => json!({
            "hook_event_name": "BeforeTool", "tool_name": "run_shell_command", "cwd": cwd,
            "tool_input": {"command": command},
        }),
        _ => json!({"toolName": "bash", "cwd": cwd, "toolArgs": {"command": command}}),
    }
}

/// The command a hook's answer runs instead, if it rewrites one.
fn rewritten(agent: Agent, answer: &Value) -> Option<String> {
    let command = match agent {
        Agent::Claude | Agent::Codex => &answer["hookSpecificOutput"]["updatedInput"]["command"],
        Agent::Agy => &answer["overwrite"]["CommandLine"],
        Agent::Cursor => &answer["updated_input"]["command"],
        Agent::Gemini => &answer["hookSpecificOutput"]["tool_input"]["command"],
        _ => &answer["modifiedArgs"]["command"],
    };
    command.as_str().map(str::to_owned)
}

fn label(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude Code",
        Agent::Codex => "Codex",
        Agent::Agy => "Antigravity CLI",
        Agent::Cursor => "Cursor",
        Agent::Gemini => "Gemini CLI",
        Agent::Copilot => "GitHub Copilot CLI",
        Agent::OpenCode => "OpenCode",
    }
}

/// Ask the hook without letting it record a pass-through.
fn ask(workspace: &Workspace, agent: Agent, input: &Value) -> Option<Value> {
    PREVIEW.store(true, Ordering::Relaxed);
    let answer = agents::preview_command(workspace, agent, input);
    PREVIEW.store(false, Ordering::Relaxed);
    answer
}

/// What the hook does with `command`, and why, as printed lines.
pub fn explain(workspace: &Workspace, agent: Agent, mode: &str, command: &str) -> String {
    let client = label(agent);
    let mut lines = Vec::new();
    let claude = agent == Agent::Claude;
    let heading = if claude {
        format!("{client} hook, {mode} mode")
    } else {
        format!("{client} hook")
    };
    let exclusions = Exclusions::of(workspace);
    let answer = if disabled() || cfg!(windows) {
        None
    } else {
        ask(workspace, agent, &input(workspace, agent, mode, command))
    };
    let new = answer.as_ref().and_then(|answer| rewritten(agent, answer));
    // Whether Claude Code's permission rules decided the answer.
    let mut judged = new.is_some();
    match &new {
        Some(new) if new != command => {
            lines.push(format!("{heading}: rewritten"));
            lines.push(format!("  {new}"));
            let runs = new.matches(" run --hook").count();
            lines.push(if new.contains(" read --") {
                "Why: a file read becomes `justless read`, which prints an outline or the exact lines with a revision to expand.".into()
            } else if runs == 0 {
                "Why: the hook names the session, so a symbol read again prints only what changed.".into()
            } else {
                format!(
                    "Why: {} through `justless run`, which archives the full output and prints a compact preview.",
                    if runs == 1 {
                        "it runs a supported command".to_owned()
                    } else {
                        format!("it runs {runs} supported commands")
                    }
                )
            });
        }
        Some(_) => {
            lines.push(format!("{heading}: unchanged"));
            lines.push("Why: the hook answers without changing the command.".into());
        }
        None => {
            lines.push(format!("{heading}: unchanged"));
            let (why, rules) = left_alone(workspace, agent, mode, command, &exclusions);
            lines.push(format!("Why: {why}"));
            judged = rules;
        }
    }
    // A list's own exclusions; a whole excluded line is the reason above.
    if exclusions.line(command).is_none()
        && let Some(segments) = agents::split_compound(command)
    {
        for range in segments {
            if let Some(entry) = exclusions.matching(&command[range.clone()]) {
                lines.push(format!(
                    "Excluded: `{}` matches `{entry}` in exclude_commands, so it stays as written.",
                    &command[range]
                ));
            }
        }
    }
    if claude && judged {
        lines.push(permission(workspace, mode, command, answer.as_ref()));
    } else if new.is_some() {
        match agent {
            Agent::Codex | Agent::Cursor | Agent::Agy => lines.push(format!(
                "Permission: the hook allows the rewrite, and {client} runs it without asking."
            )),
            Agent::Gemini => lines.push(
                "Permission: Gemini CLI checks the rewritten command with its own policy.".into(),
            ),
            _ => lines
                .push("Permission: the hook makes no decision; Copilot applies its own.".into()),
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Why a hook leaves `command` as written, and whether Claude Code's permission
/// rules are the reason.
fn left_alone(
    workspace: &Workspace,
    agent: Agent,
    mode: &str,
    command: &str,
    exclusions: &Exclusions,
) -> (String, bool) {
    let claude = agent == Agent::Claude;
    if disabled() {
        return (
            format!("{DISABLE} is set, so every hook leaves commands alone."),
            false,
        );
    }
    if cfg!(windows) {
        return (
            "command hooks pass every command through on Windows.".into(),
            false,
        );
    }
    if claude && mode == "plan" {
        return (
            "Claude Code runs no commands in plan mode, so the hook leaves them alone.".into(),
            false,
        );
    }
    if let Some(entry) = exclusions.line(command) {
        return (
            format!("it matches `{entry}` in exclude_commands in .justless.toml."),
            false,
        );
    }
    if claude && crate::batch::Batch::parse_list(command).is_some() {
        return (
            "a list of reads runs as written, and the after-bash hook compacts its output.".into(),
            false,
        );
    }
    if let Some(words) = agents::literal_command(command)
        && crate::shellread::command(&words).is_some()
    {
        let why = if claude {
            "a range read runs as written, and the after-bash hook outlines its output."
        } else {
            "the range read cannot be served from the file as it is now, such as one counted from the end."
        };
        return (why.into(), false);
    }
    if claude {
        // The rules leave a command the hook would otherwise route.
        let mut free = input(workspace, agent, mode, command);
        free["permission_mode"] = Value::Null;
        if ask(workspace, agent, &free).is_some() {
            return (
                "Claude Code's permission rules leave it to be judged as written (see below)."
                    .into(),
                true,
            );
        }
    }
    let segments = agents::split_compound(command).unwrap_or_default();
    if !matches!(agent, Agent::Claude | Agent::Gemini) && segments.len() > 1 {
        let routable = segments.iter().any(|range| {
            ask(
                workspace,
                agent,
                &input(workspace, agent, mode, &command[range.clone()]),
            )
            .is_some()
        });
        if routable {
            return (
                format!(
                    "{} runs a rewritten list without asking again, so its hook rewrites a list only when every command in it is supported and none is excluded.",
                    label(agent)
                ),
                false,
            );
        }
    }
    let after = if claude {
        " The after-bash hook may still compact its output after it runs."
    } else {
        ""
    };
    (
        format!(
            "no command in it is one the hook routes: a supported build, test, lint, diff or search command without pipes, redirections, expansions or a `cd` out of the workspace.{after}"
        ),
        false,
    )
}

/// Claude Code's permission verdict for `command` and what the hook does with it.
fn permission(workspace: &Workspace, mode: &str, command: &str, answer: Option<&Value>) -> String {
    let segments: Vec<&str> = agents::split_compound(command)
        .map(|ranges| ranges.into_iter().map(|range| &command[range]).collect())
        .unwrap_or_else(|| vec![command]);
    let verdict = claude_permissions::load(&Sources::for_hook(&workspace.root)).verdict(&segments);
    let rules = match verdict {
        Verdict::Deny => "a deny rule may match it",
        Verdict::Ask => "an ask rule may match it",
        Verdict::Allow => "allow rules cover every command in it",
        Verdict::Unknown => "no rule allows it",
    };
    let approved =
        answer.is_some_and(|answer| answer["hookSpecificOutput"]["permissionDecision"] == "allow");
    let outcome = match claude_permissions::answer(mode, verdict) {
        _ if answer.is_none() => "so Claude Code judges the command as written",
        Answer::Approve if approved => "so the hook allows the rewrite without a prompt",
        Answer::Leave => "so the hook leaves it for Claude Code to judge as written",
        _ => "so Claude Code judges the rewritten command itself",
    };
    format!("Permission: {rules} in {mode} mode, {outcome}.")
}
