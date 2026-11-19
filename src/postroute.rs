//! Compact Claude Code Bash output that the command hook left alone.
//!
//! The command hook routes a supported command only when it can rewrite it in
//! place. Pipes such as `cargo test 2>&1 | tail -50`, lists it passes through and
//! Claude Code's own `grep` reach the agent whole. This PostToolUse handler
//! archives such a result as a run and replaces it with the compaction
//! `justless run --hook` gives the same command, followed by the run marker.
//! Secrets in the replacement are redacted as in any run's preview, and the
//! archive keeps the exact output for `log --unredacted`.
//!
//! Contract, from the hooks reference
//! (<https://code.claude.com/docs/en/hooks#posttooluse-decision-control>):
//! PostToolUse "fire[s] after a tool has already executed successfully" and
//! `updatedToolOutput` "Replaces the tool's output with the provided value before
//! it is sent to Claude. The value must match the tool's output shape." The page's
//! example names the Bash shape: "`Bash` returns an object with `stdout`, `stderr`,
//! `interrupted`, and `isImage` fields", and "a value that doesn't match the tool's
//! output schema is ignored and the original output is used". A command that exits
//! with another status fires PostToolUseFailure, which "can't rewrite a result", so
//! a failure this handler sees has been masked by the command line itself, such as
//! by `| tail`. The tools reference
//! (<https://code.claude.com/docs/en/tools-reference#output-limits>) says a result
//! over roughly 30,000 characters reaches Claude as "the path of a file saved to
//! the session directory and truncated past 64 MiB, plus a preview of up to the
//! first 2,000 characters", and that exit 1 is a valid result only for `grep`,
//! `rg`, `egrep`, `fgrep`, `find`, `diff`, `test`, `[`, `git diff` and `git grep`.
//!
//! The reference lists no per-field Bash schema. These fields come from the output
//! schema in the Claude Code 2.1.327 binary
//! (`~/.local/share/claude/versions/2.1.327`, read-only): `stdout`, `stderr` and
//! `interrupted` are required, and the optional fields include `isImage`,
//! `backgroundTaskId`, `timedOutAfterMs`, `returnCodeInterpretation` ("Semantic
//! interpretation for non-error exit codes with special meaning"),
//! `structuredContent`, `persistedOutputPath` ("Path to the persisted full output
//! in tool-results dir (set when output is too large for inline)"),
//! `persistedOutputSize` and `stdoutHoldsOutputEnd`. There is no exit code. The
//! same binary validates a rewrite against that schema and renders it with the
//! tool's own mapper, which shows a persisted path and preview whenever
//! `persistedOutputPath` is set. Claude Code's shell captures stderr into
//! `stdout`; in transcripts `stderr` only ever held its own `Shell cwd was reset`
//! note, so it is left as it is.
use std::borrow::Cow;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::capture::{self, Origin};
use crate::workspace::{Workspace, write_json};

/// The largest output this hook archives, in lines and bytes. Grouping looks up a
/// path for each line, and a review measured up to about 145 µs per line, so a
/// larger result could outrun the hook's 10-second timeout. Claude Code shows such
/// a result as the path of its saved copy and a short preview, so it passes through.
pub(crate) const MAX_LINES: usize = 20_000;
pub(crate) const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Claude Code's preview of a result it saved to a file: up to the first 2,000
/// characters. A report that replaces such a result must be no larger.
pub(crate) const PREVIEW_CHARS: usize = 2_000;
/// Lines of the original that the shown text left out and that report an error.
const ERROR_LINES: usize = 8;
/// Output of a command no hook supports is compacted only past this many context
/// budgets.
const UNKNOWN_FACTOR: usize = 4;
/// Programs whose output is file content an agent may be about to edit or copy.
const READERS: &[&str] = &[
    "cat", "head", "tail", "sed", "awk", "gawk", "less", "more", "bat", "batcat", "nl", "cut",
    "base64", "xxd", "od", "hexdump", "strings", "git", "echo", "printf",
];
const NOTE: &str = "Captured from a Claude Code Bash result after the command ran. The archive holds the output Claude Code recorded, with stdout and stderr interleaved, and Claude Code may have shortened it at its own output limits. The exit status is the whole command line's as Claude Code reported it: 0, or 1 where Claude Code read exit 1 as a valid result, so a pipe can hide its first command's failure.";

/// A run marker that `justless run` already printed, alone on its line.
static RUN_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\[justless run-[A-Za-z0-9]+(?:\]| · )").expect("run marker pattern")
});
/// Lines that report an error wherever they appear in merged output, at the start
/// of the line, as compilers, Git, Go, Node and Python print them.
static ERROR_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:error(?:\[[A-Za-z0-9]+\])?:|fatal(?: error)?:|FATAL[: ]|Error:|ERROR:|panic:|npm ERR!|Traceback \(most recent call last\):)",
    )
    .expect("error line pattern")
});
/// Lines of a failing test: a Rust panic and its assertion, a Go `--- FAIL:` and a
/// pytest `FAILED` line. A test report that names no failing test, such as one
/// whose `FAILED` line a `| tail` cut off, is followed by these instead.
static FAILURE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:thread '[^']*' (?:\(\d+\) )?panicked at |assertion|  left: | right: |--- FAIL: |FAILED |failures:\s*$)",
    )
    .expect("test failure pattern")
});
/// Programs whose own messages, such as `find: x: Permission denied`, can sit among
/// the records of a search or listing in merged output.
const TOOLS: &[&str] = &[
    "grep", "ugrep", "egrep", "fgrep", "rg", "find", "bfs", "fd", "ls", "tree", "git", "ag", "ack",
    "sh", "bash", "zsh", "xargs", "env", "timeout",
];
const TOOL_ERRORS: &[&str] = &[
    "error",
    "no such file",
    "permission denied",
    "not permitted",
    "cannot ",
    "not found",
    "invalid",
    "is a directory",
    "not a directory",
];

/// Claude Code PostToolUse handler for the Bash tool. Returns `None` to leave the
/// result unchanged: a command already routed through Justless, a background,
/// interrupted or image result, output the run path would show verbatim, and any
/// command line whose output is not one supported command's lines.
pub fn claude_bash(ws: &Workspace, input: &Value) -> Option<Value> {
    if input["hook_event_name"] != "PostToolUse" || input["tool_name"] != "Bash" {
        return None;
    }
    let call = &input["tool_input"];
    let command = call["command"].as_str()?;
    if call["run_in_background"] == true || excluded(ws, command) {
        return None;
    }
    let response = input["tool_response"].as_object()?;
    let stdout = response.get("stdout")?.as_str()?;
    response.get("stderr")?.as_str()?;
    if response.get("interrupted")?.as_bool()?
        || response
            .get("isImage")
            .is_some_and(|image| image != &Value::Bool(false))
        || [
            "backgroundTaskId",
            "timedOutAfterMs",
            "structuredContent",
            "rawOutputPath",
        ]
        .iter()
        .any(|key| response.get(*key).is_some_and(|value| !value.is_null()))
    {
        return None;
    }
    // Output with a run marker was already routed, unless it is the text of a
    // `git show` blob, which the read below matches byte for byte.
    let routed = RUN_MARKER.is_match(stdout);
    // A batch the recheck cannot verify, such as a diff shaped by repository
    // settings, still gets the report for its one kind of command below.
    if !cfg!(windows)
        && !routed
        && let Some(compacted) =
            crate::batch::Batch::parse(command).and_then(|batch| batch.compact(ws, input))
    {
        return Some(compacted);
    }
    if !cfg!(windows)
        && let Some(words) = crate::agents::literal_command(command)
        && let Some((file, range)) = crate::shellread::command(&words)
    {
        // A `git show` blob that is not outlined gets the ordinary report below.
        let object = range.object();
        if routed && !object {
            return None;
        }
        let compacted = crate::shellread::claude(ws, input, file, range);
        if compacted.is_some() || !object {
            return compacted;
        }
    }
    if routed {
        return None;
    }
    let settings = ws.settings().ok()?;
    let persisted = response.get("persistedOutputPath").and_then(Value::as_str);
    if persisted.is_none() && stdout.len() <= settings.context_budget {
        return None;
    }
    // A command no hook supports gets only the floor and JSON tables, never a
    // clipped preview, and only when its output is large.
    let (plan, unknown) = match plan(command) {
        Some(plan) => (plan, false),
        None if persisted.is_none() && stdout.len() > UNKNOWN_FACTOR * settings.context_budget => {
            (unknown_plan(command)?, true)
        }
        None => return None,
    };
    let cwd = input["cwd"]
        .as_str()
        .map_or_else(|| ws.cwd.clone(), PathBuf::from)
        .canonicalize()
        .ok()?;
    if !cwd.starts_with(&ws.root) {
        return None;
    }
    let directory = entered(&cwd, &ws.root, &plan.cd)?;
    let run_ws = Workspace {
        root: ws.root.clone(),
        cwd: directory,
        state: ws.state.clone(),
        // The agent's shell runs a printed command from where this line started.
        origin: cwd.clone(),
    };
    let args: Vec<OsString> = plan.words.iter().map(OsString::from).collect();
    // The preview budget `justless run` would give these words, which adapts to
    // how often agents reopened the family's clipped logs. The run path shows
    // output up to this size, or the context budget, verbatim.
    let run_budget = if unknown {
        stdout.len()
    } else {
        crate::adaptive::preview_budget(&run_ws, &args).ok()?
    };
    let threshold = if unknown {
        UNKNOWN_FACTOR * settings.context_budget
    } else {
        settings.context_budget.max(run_budget)
    };
    let output = match persisted {
        Some(path) => read_persisted(path, response.get("persistedOutputSize")?, stdout)?,
        None => stdout.as_bytes().to_vec(),
    };
    if output.len() <= threshold
        || output.len() > MAX_BYTES
        || output.iter().filter(|&&byte| byte == b'\n').count() > MAX_LINES
    {
        return None;
    }
    // For a result Claude Code saved to a file, the agent would have seen the first
    // 2,000 characters. The report gets three quarters of that, so it fits with its
    // error lines and marker.
    let preview = persisted.map(|_| {
        stdout
            .chars()
            .take(PREVIEW_CHARS)
            .map(char::len_utf8)
            .sum::<usize>()
    });
    let budget = preview.map_or(run_budget, |preview| run_budget.min(preview * 3 / 4));
    // Claude Code reports exit 1 as a valid result only with an interpretation,
    // such as "No matches found".
    let status = i32::from(
        response
            .get("returnCodeInterpretation")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty()),
    );
    let session = input["session_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map_or_else(
            || "terminal".to_owned(),
            |id| capture::source_digest(id.as_bytes())[..24].to_owned(),
        );
    let mut note = NOTE.to_owned();
    if let Some(preview) = preview {
        note.push_str(&format!(
            " Claude Code saved this {}-byte output to a file and would have shown its first {PREVIEW_CHARS} characters, so raw_bytes counts those {preview} bytes.",
            output.len()
        ));
    }
    let origin = Origin {
        command: command.to_owned(),
        label: plan.label.clone(),
        note,
        outline: None,
    };
    // The same redactor the capture uses for the previews and footer. A
    // configuration it cannot read stops the capture too.
    let redactor = crate::redact::for_run(ws).ok()?;
    let mut shown =
        capture::record(&run_ws, &args, &output, status, budget, &session, &origin).ok()?;
    let archive = ws.state.join("runs").join(&shown.run.id);
    let mut text: String = shown
        .previews
        .iter()
        .map(|(_, bytes)| String::from_utf8_lossy(bytes))
        .collect();
    // A lint report covers both streams by design. Other reports were written for
    // stdout alone, so error lines from the interleaved stderr that they left out
    // follow them, and so do the failure lines a test report could not attribute.
    let summary = &shown.run.summary;
    let errors = if shown.footer.is_empty() || summary.kind == "lint" {
        String::new()
    } else {
        let failures =
            summary.kind.is_empty() || summary.kind == "tests" && summary.items.is_empty();
        // The previews are redacted, so the error lines come from a redacted copy,
        // whose line numbers match the archive. A line clipped to fit then never
        // keeps part of a secret.
        let redacted = redactor.map_or(Cow::Borrowed(output.as_slice()), |r| r.bytes(&output));
        // Past `max_run_bytes` the archive holds no lines, so `log` could not replay
        // an error line listed there by its number.
        let archived = shown.run.stream_bytes.first().map_or(usize::MAX, |&bytes| {
            usize::try_from(bytes).unwrap_or(usize::MAX)
        });
        let whole_lines = if archived >= output.len() {
            usize::MAX
        } else {
            output[..archived]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count()
        };
        unshown_errors(
            &redacted,
            &text,
            &plan.program,
            failures,
            budget / 4,
            whole_lines,
        )
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&errors);
    text.push_str(shown.footer.trim_start_matches('\n'));
    // Previews go through the redactor once more before they are printed, so the
    // whole replacement does too.
    if let Some(redactor) = redactor
        && let Cow::Owned(redacted) = redactor.text(&text)
    {
        text = redacted;
    }
    let small = match preview {
        Some(preview) => text.len() > preview,
        None => output.len().saturating_sub(text.len()) < (output.len() / 4).max(512),
    };
    // A search with context lines is shown as blocks or not at all: any other
    // preview would hide lines the agent asked for.
    let words: Vec<&str> = plan.words.iter().map(String::as_str).collect();
    let blocks = crate::search::prints_context(crate::filters::unwrap_tool(&words));
    if shown.footer.is_empty() || small || blocks && summary.kind != "search" {
        // The agent keeps the original, so the archive adds nothing.
        let _ = fs::remove_dir_all(&archive);
        return None;
    }
    if !errors.is_empty() || preview.is_some() {
        shown.run.footer_bytes += errors.len();
        // Savings count against what Claude Code would have shown.
        if let Some(preview) = preview {
            shown.run.raw_bytes = preview as u64;
        }
        let _ = write_json(&archive.join("record.json"), &shown.run);
    }
    let mut updated = input["tool_response"].clone();
    let fields = updated.as_object_mut()?;
    fields.insert("stdout".into(), json!(text));
    // The rewrite replaces the whole result, so the stderr it carries is redacted
    // like stdout.
    if let Some(redactor) = redactor
        && let Some(Value::String(stderr)) = fields.get_mut("stderr")
        && let Cow::Owned(redacted) = redactor.text(stderr)
    {
        *stderr = redacted;
    }
    // The compaction covers the whole output, so it is shown inline.
    for key in [
        "persistedOutputPath",
        "persistedOutputSize",
        "stdoutHoldsOutputEnd",
    ] {
        fields.remove(key);
    }
    Some(json!({"hookSpecificOutput":{
        "hookEventName":"PostToolUse",
        "updatedToolOutput":updated
    }}))
}

/// How `history` and `resume` show a run's exit status. A run this hook recorded
/// from a pipe or a list holds the whole line's status, which can hide a failure of
/// the command it names, so the status says whose it is.
pub fn exit_status(run: &crate::capture::Run) -> String {
    if run.note.is_empty() {
        run.status.to_string()
    } else if run.label.contains(" | ") {
        format!("{} (pipe)", run.status)
    } else if !run.pieces.is_empty() || run.label.ends_with("; …") {
        format!("{} (last command)", run.status)
    } else {
        run.status.to_string()
    }
}

/// The full output Claude Code saved for a large result. The inline `stdout` is
/// only its start, so a file that is missing, larger than this hook archives or not
/// this result's leaves the result unchanged. Claude Code cuts its copy at 64 MiB,
/// far above that size.
pub(crate) fn read_persisted(path: &str, size: &Value, stdout: &str) -> Option<Vec<u8>> {
    let path = Path::new(path);
    if !path.is_absolute()
        || path
            .parent()
            .and_then(Path::file_name)
            .is_none_or(|name| name != "tool-results")
    {
        return None;
    }
    let size = size.as_u64()?;
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || size > MAX_BYTES as u64 || metadata.len() < size {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let start = stdout.trim_start();
    let start = &start[..start.floor_char_boundary(start.len().min(256))];
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    (!start.is_empty() && head.trim_start().starts_with(start)).then_some(bytes)
}

/// Lines of `output` that report an error, or with `failures` a failing test, and
/// do not appear in `shown`, with their original stdout line numbers, within
/// `budget` bytes. Only the first `archived` lines, those the archive holds whole,
/// are listed.
fn unshown_errors(
    output: &[u8],
    shown: &str,
    program: &str,
    failures: bool,
    budget: usize,
    archived: usize,
) -> String {
    let text = String::from_utf8_lossy(output);
    let mut lines = String::new();
    let (mut listed, mut more) = (0, 0);
    for (index, line) in text.lines().enumerate().take(archived) {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || !(reports_error(line, program) || failures && FAILURE_LINE.is_match(line))
            || shown.contains(trimmed)
        {
            continue;
        }
        let entry = format!("stdout:{}: {}\n", index + 1, crate::search::clip(trimmed));
        if listed < ERROR_LINES && lines.len() + entry.len() <= budget {
            lines.push_str(&entry);
            listed += 1;
        } else {
            more += 1;
        }
    }
    if listed == 0 {
        return String::new();
    }
    if more > 0 {
        lines.push_str(&format!("+{more} more\n"));
    }
    format!("Error lines not shown above:\n{lines}")
}

fn reports_error(line: &str, program: &str) -> bool {
    if ERROR_LINE.is_match(line) {
        return true;
    }
    let Some((tool, message)) = line.split_once(": ") else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    (tool == program || TOOLS.contains(&tool))
        && TOOL_ERRORS.iter().any(|needle| message.contains(needle))
}

/// The directory a command's relative paths start from. Claude Code's main session
/// keeps a `cd` for later commands, so its `cwd` can already be the directory the
/// command entered, while a subagent's is not.
fn entered(cwd: &Path, root: &Path, cd: &Path) -> Option<PathBuf> {
    if cd.as_os_str().is_empty() {
        return Some(cwd.to_path_buf());
    }
    match cwd.join(cd).canonicalize() {
        Ok(directory) if directory.is_dir() => directory.starts_with(root).then_some(directory),
        _ if cd.is_relative()
            && cd
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))) =>
        {
            Some(cwd.to_path_buf())
        }
        _ => None,
    }
}

/// The supported command whose lines make up a shell command line's output.
/// Whether `exclude_commands` names the line, a command in it, or a command in a
/// pipe, after its assignments and wrappers such as `env` or `timeout`.
fn excluded(ws: &Workspace, command: &str) -> bool {
    let exclusions = crate::rewrite::Exclusions::of(ws);
    !exclusions.is_empty()
        && (exclusions.any(command).is_some()
            || parse(command).is_some_and(|segments| {
                segments.iter().flatten().any(|stage| {
                    let words = strip(&stage.words).unwrap_or(&stage.words);
                    exclusions.matching(&words.join(" ")).is_some()
                })
            }))
}

struct Plan {
    /// Its words after assignments and wrappers such as `env` or `timeout`.
    words: Vec<String>,
    /// The program's file name, which prefixes its own error messages.
    program: String,
    /// How lists name the run: the command label, and the filters after a pipe.
    label: String,
    /// The directories entered with `cd` before it, joined.
    cd: PathBuf,
}

/// Find the one kind of supported command a command line runs. Its output may pass
/// through filters that keep whole lines, such as `head`, `tail` or `grep`. Other
/// commands in a list may only change directory, change a setting, or print a short
/// note such as `echo` before it. Several supported commands must select the same
/// compaction, such as two searches. Anything else, including a command already
/// routed through Justless, returns `None`.
fn plan(command: &str) -> Option<Plan> {
    let segments = parse(command)?;
    let mut cd = PathBuf::new();
    let mut found: Option<(Plan, (Option<crate::filters::Profile>, String))> = None;
    let (mut after, mut moved) = (false, false);
    for segment in &segments {
        if segment.iter().any(|stage| {
            strip(&stage.words)
                .and_then(|words| words.first())
                .is_some_and(|program| basename(program) == "justless")
        }) {
            return None;
        }
        if let [stage] = segment.as_slice() {
            match quiet(strip(&stage.words)?) {
                Some(Quiet::Enter(directory)) => {
                    if found.is_some() {
                        moved = true;
                    } else {
                        cd.push(home(&directory)?);
                    }
                    after |= found.is_some();
                    continue;
                }
                Some(Quiet::Elsewhere) => return None,
                Some(Quiet::Note) => {
                    after |= found.is_some();
                    continue;
                }
                // A line printed after the command, such as `echo "exit=$?"` or
                // `&& echo OK || echo FAIL`, often reports how it ended, and a
                // report of the command's own lines would leave it out.
                Some(Quiet::Print) if found.is_some() => return None,
                Some(Quiet::Print) => continue,
                None => {}
            }
        }
        // Relative paths after a second `cd` start from another directory.
        if moved {
            return None;
        }
        let (first, filters) = segment.split_first()?;
        if !keeps_stdout(first) || !filters.iter().all(keeps_lines) {
            return None;
        }
        let words = strip(&first.words)?;
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        if !crate::agents::supported_words(&refs) {
            return None;
        }
        let args: Vec<OsString> = words.iter().map(OsString::from).collect();
        let kind = (
            crate::filters::profile(&refs),
            crate::summary::family(&args),
        );
        // A search whose pattern matches every line, such as `grep -rn "" file`, is
        // how an agent reads code, and a report grouped by file would show only a few
        // lines. A search with context lines qualifies only for its block report,
        // which `claude_bash` checks after the run. A listing filtered by `grep`
        // names the paths the agent asked for, which a listing report can collapse
        // into one directory count.
        let tool = crate::filters::unwrap_tool(&refs);
        if crate::search::matches_every_line(tool)
            || kind.0 == Some(crate::filters::Profile::Listing)
                && filters.iter().any(|filter| {
                    strip(&filter.words)
                        .and_then(|words| words.first())
                        .is_some_and(|program| {
                            matches!(basename(program), "grep" | "egrep" | "fgrep" | "rg")
                        })
                })
        {
            return None;
        }
        match &found {
            Some((_, first_kind)) if *first_kind != kind => return None,
            Some(_) => after = true,
            None => {
                let mut label = crate::history::label(&refs);
                for filter in filters {
                    if let Some(program) = strip(&filter.words).and_then(|w| w.first()) {
                        label.push_str(" | ");
                        label.push_str(basename(program));
                    }
                }
                let plan = Plan {
                    words: words.to_vec(),
                    program: basename(tool.first().copied().unwrap_or("")).to_owned(),
                    label,
                    cd: cd.clone(),
                };
                found = Some((plan, kind));
            }
        }
    }
    let (mut plan, _) = found?;
    if after {
        plan.label.push_str("; …");
    }
    Some(plan)
}

/// One command that no hook supports, alone on its line with its output reaching
/// the agent: no pipe, list or redirection of stdout, and not a program that prints
/// file content.
fn unknown_plan(command: &str) -> Option<Plan> {
    let segments = parse(command)?;
    let [segment] = segments.as_slice() else {
        return None;
    };
    let [stage] = segment.as_slice() else {
        return None;
    };
    let words = strip(&stage.words)?;
    let program = basename(words.first()?);
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    if !keeps_stdout(stage)
        || program == "justless"
        || READERS.contains(&program)
        || quiet(words).is_some()
        || crate::agents::supported_words(&refs)
    {
        return None;
    }
    Some(Plan {
        words: words.to_vec(),
        program: program.to_owned(),
        label: crate::history::label(&refs),
        cd: PathBuf::new(),
    })
}

fn basename(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
}

/// A command in a list whose output needs no compaction.
enum Quiet {
    /// A plain `cd DIR`.
    Enter(String),
    /// A command that prints nothing, such as `set -e` or `true`.
    Note,
    /// A short note, such as `echo` or `pwd`.
    Print,
    /// A change of directory this line does not name, such as a bare `cd` or `cd -`.
    Elsewhere,
}

fn quiet(words: &[String]) -> Option<Quiet> {
    let Some((program, rest)) = words.split_first() else {
        return Some(Quiet::Note);
    };
    match program.as_str() {
        "cd" => Some(match rest {
            [directory]
                if !directory.is_empty()
                    && !directory.starts_with('-')
                    && !directory.contains('$') =>
            {
                Quiet::Enter(directory.clone())
            }
            _ => Quiet::Elsewhere,
        }),
        "echo" | "printf" | "pwd" => Some(Quiet::Print),
        "true" | ":" | "set" | "export" | "unset" | "sleep" => Some(Quiet::Note),
        _ => None,
    }
}

/// A `cd` operand with a leading `~` or `~/` taken from `HOME`, as the shell
/// expands it. Another user's home, such as `~alice`, is not resolved.
fn home(directory: &str) -> Option<PathBuf> {
    let Some(rest) = directory.strip_prefix('~') else {
        return Some(PathBuf::from(directory));
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(home.join(rest.trim_start_matches('/')))
}

/// Skip leading assignments and wrappers that run the next word unchanged.
fn strip(words: &[String]) -> Option<&[String]> {
    let mut rest = words;
    loop {
        let Some(first) = rest.first() else {
            return Some(rest);
        };
        rest = match first.as_str() {
            word if assignment(word) => &rest[1..],
            "env" => {
                let after = &rest[1..];
                if after.first().is_some_and(|w| w.starts_with('-')) {
                    return None;
                }
                after
            }
            "time" | "nohup" => &rest[1..],
            "command" if rest.get(1).is_some_and(|w| !w.starts_with('-')) => &rest[1..],
            "nice" => match rest.get(1).map(String::as_str) {
                Some("-n") => rest.get(3..)?,
                Some(w) if w.starts_with('-') => &rest[2..],
                _ => &rest[1..],
            },
            "timeout" => {
                let mut i = 1;
                while let Some(option) = rest.get(i).filter(|w| w.starts_with('-')) {
                    i += if matches!(option.as_str(), "-s" | "--signal" | "-k" | "--kill-after") {
                        2
                    } else {
                        1
                    };
                }
                // The duration.
                rest.get(i)
                    .filter(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
                &rest[i + 1..]
            }
            _ => return Some(rest),
        };
    }
}

fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Redirections that leave the command's output in Claude Code's capture: input,
/// any stderr redirection, and stdout sent to stderr.
fn keeps_stdout(stage: &Stage) -> bool {
    stage
        .redirects
        .iter()
        .all(|redirect| match (redirect.fd, redirect.op.as_str()) {
            (_, "<" | "<&") => true,
            (Some(2), ">" | ">>" | ">&" | ">|") => true,
            (None | Some(1), ">&") => redirect.target == "2",
            _ => false,
        })
}

/// A filter that passes whole lines of its input through unchanged, so the output
/// keeps the shape of the command before it.
fn keeps_lines(stage: &Stage) -> bool {
    if !keeps_stdout(stage) {
        return false;
    }
    let Some((program, args)) = strip(&stage.words).and_then(|words| words.split_first()) else {
        return false;
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match basename(program) {
        "head" => count_options(&args, false),
        "tail" => count_options(&args, true),
        "grep" | "egrep" | "fgrep" | "rg" => line_filter(&args),
        "sort" => args.iter().all(|arg| {
            arg.starts_with('-') && !arg.starts_with("-o") && !arg.starts_with("--output")
        }),
        "uniq" => args.iter().all(|arg| {
            matches!(
                *arg,
                "-d" | "-u" | "-i" | "--repeated" | "--unique" | "--ignore-case"
            )
        }),
        "cat" | "less" | "more" => args.iter().all(|arg| *arg == "-"),
        "tee" => args
            .iter()
            .all(|arg| !arg.starts_with('-') || matches!(*arg, "-a" | "--append")),
        _ => false,
    }
}

/// `head` and `tail` options that pick lines or bytes from standard input.
fn count_options(args: &[&str], tail: bool) -> bool {
    let number = |text: &str| {
        let text = if tail {
            text.strip_prefix('+').unwrap_or(text)
        } else {
            text
        };
        !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let ok = match *arg {
            "-n" | "-c" | "--lines" | "--bytes" => args.next().is_some_and(|v| number(v)),
            "-q" | "--quiet" | "--silent" => true,
            _ => {
                if let Some(value) = arg
                    .strip_prefix("--lines=")
                    .or_else(|| arg.strip_prefix("--bytes="))
                    .or_else(|| arg.strip_prefix("-n"))
                    .or_else(|| arg.strip_prefix("-c"))
                {
                    number(value)
                } else if let Some(value) = arg.strip_prefix('-') {
                    number(value) && !value.starts_with('+')
                } else {
                    tail && arg.starts_with('+') && number(arg)
                }
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

/// A `grep` or `rg` that selects lines of standard input and prints them as they
/// are: no counts, file names, line numbers, byte offsets or partial matches.
fn line_filter(args: &[&str]) -> bool {
    const REJECTED: &[&str] = &[
        "--count",
        "--files-with-matches",
        "--files-without-match",
        "--only-matching",
        "--line-number",
        "--byte-offset",
        "--with-filename",
        "--quiet",
        "--silent",
        "--null",
        "--null-data",
        "--recursive",
        "--dereference-recursive",
        "--files",
        "--json",
        "--vimgrep",
        "--column",
        "--replace",
        "--passthru",
    ];
    const VALUED: &[&str] = &[
        "--regexp",
        "--file",
        "--max-count",
        "--context",
        "--after-context",
        "--before-context",
        "--label",
    ];
    let mut patterns = 0;
    let mut positional = 0;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if *arg == "--" {
            positional += args.len();
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let name = format!("--{}", long.split('=').next().unwrap_or(long));
            if REJECTED.contains(&name.as_str()) {
                return false;
            }
            if VALUED.contains(&name.as_str()) && !long.contains('=') && args.next().is_none() {
                return false;
            }
            patterns += usize::from(matches!(name.as_str(), "--regexp" | "--file"));
        } else if let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) {
            for (i, flag) in cluster.char_indices() {
                if "clLonbHqZzrRp".contains(flag) {
                    return false;
                }
                if "efmABC".contains(flag) {
                    patterns += usize::from(matches!(flag, 'e' | 'f'));
                    if i + 1 == cluster.len() && args.next().is_none() {
                        return false;
                    }
                    break;
                }
            }
        } else {
            positional += 1;
        }
    }
    // With `-e` or `-f` every operand is a file, and otherwise the first is the
    // pattern. A file operand means the filter reads it instead of the pipe.
    positional == usize::from(patterns == 0)
}

/// One command of a pipeline: its words and redirections.
#[derive(Default)]
struct Stage {
    words: Vec<String>,
    redirects: Vec<Redirect>,
}

struct Redirect {
    fd: Option<u32>,
    op: String,
    target: String,
}

enum Token {
    Word(String),
    /// `;`, `&&`, `||`, `|` or `|&`.
    Operator(&'static str),
    Redirect(Option<u32>, String),
}

/// Split a shell command line into its list of pipelines. Command and process
/// substitution, subshells, groups, background jobs, here-documents, comments and
/// control keywords return `None`, since their output cannot be attributed.
fn parse(command: &str) -> Option<Vec<Vec<Stage>>> {
    let mut list = vec![Vec::new()];
    let mut stage = Stage::default();
    let mut pending: Option<(Option<u32>, String)> = None;
    let mut joined = None;
    for token in tokens(command)? {
        match token {
            Token::Word(word) => {
                if let Some((fd, op)) = pending.take() {
                    stage.redirects.push(Redirect {
                        fd,
                        op,
                        target: word,
                    });
                } else {
                    stage.words.push(word);
                }
                joined = None;
            }
            Token::Redirect(fd, op) => {
                if pending.is_some() {
                    return None;
                }
                pending = Some((fd, op));
            }
            Token::Operator(operator) => {
                if pending.is_some() {
                    return None;
                }
                let empty = stage.words.is_empty() && stage.redirects.is_empty();
                let pipeline = list.last_mut().unwrap();
                match operator {
                    "|" | "|&" => {
                        if empty {
                            return None;
                        }
                        pipeline.push(std::mem::take(&mut stage));
                        joined = Some(operator);
                    }
                    _ => {
                        if empty {
                            // A line break or `;` may end a list, but no operator
                            // may follow another.
                            if operator != ";" || joined.is_some() || !pipeline.is_empty() {
                                return None;
                            }
                            continue;
                        }
                        pipeline.push(std::mem::take(&mut stage));
                        list.push(Vec::new());
                        joined = (operator != ";").then_some(operator);
                    }
                }
            }
        }
    }
    if pending.is_some() || joined.is_some() {
        return None;
    }
    if !stage.words.is_empty() || !stage.redirects.is_empty() {
        list.last_mut().unwrap().push(stage);
    }
    list.retain(|pipeline| !pipeline.is_empty());
    let keyword = |word: &str| {
        matches!(
            word,
            "if" | "then"
                | "else"
                | "elif"
                | "fi"
                | "for"
                | "while"
                | "until"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "select"
                | "function"
                | "[["
                | "!"
                | "{"
                | "}"
                | "exec"
                | "eval"
                | "source"
                | "."
                | "pushd"
                | "popd"
        )
    };
    let valid = !list.is_empty()
        && list
            .iter()
            .flatten()
            .all(|stage| stage.words.first().is_some_and(|word| !keyword(word)));
    valid.then_some(list)
}

fn tokens(command: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = command.chars().peekable();
    let flush = |tokens: &mut Vec<Token>, word: &mut String, started: &mut bool| {
        if *started {
            tokens.push(Token::Word(std::mem::take(word)));
            *started = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                flush(&mut tokens, &mut word, &mut started);
                quoted = false;
            }
            '\n' => {
                flush(&mut tokens, &mut word, &mut started);
                quoted = false;
                tokens.push(Token::Operator(";"));
            }
            '\\' => match chars.next()? {
                '\n' => {}
                next => {
                    word.push(next);
                    started = true;
                    quoted = true;
                }
            },
            '\'' => {
                loop {
                    match chars.next()? {
                        '\'' => break,
                        next => word.push(next),
                    }
                }
                started = true;
                quoted = true;
            }
            '"' => {
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            next @ ('$' | '`' | '"' | '\\') => word.push(next),
                            '\n' => {}
                            next => {
                                word.push('\\');
                                word.push(next);
                            }
                        },
                        '`' => return None,
                        '$' if chars.peek() == Some(&'(') => return None,
                        next => word.push(next),
                    }
                }
                started = true;
                quoted = true;
            }
            '`' | '(' | ')' => return None,
            '$' => {
                if chars.peek() == Some(&'(') {
                    return None;
                }
                word.push('$');
                if chars.peek() == Some(&'{') {
                    loop {
                        let next = chars.next()?;
                        word.push(next);
                        if next == '}' {
                            break;
                        }
                    }
                }
                started = true;
            }
            '#' if !started => return None,
            ';' => {
                if chars.peek() == Some(&';') {
                    return None;
                }
                flush(&mut tokens, &mut word, &mut started);
                quoted = false;
                tokens.push(Token::Operator(";"));
            }
            '&' => {
                flush(&mut tokens, &mut word, &mut started);
                quoted = false;
                match chars.peek() {
                    Some('&') => {
                        chars.next();
                        tokens.push(Token::Operator("&&"));
                    }
                    Some('>') => {
                        chars.next();
                        let op = if chars.peek() == Some(&'>') {
                            chars.next();
                            "&>>"
                        } else {
                            "&>"
                        };
                        tokens.push(Token::Redirect(None, op.into()));
                    }
                    // A background job keeps running after the result.
                    _ => return None,
                }
            }
            '|' => {
                flush(&mut tokens, &mut word, &mut started);
                quoted = false;
                match chars.peek() {
                    Some('|') => {
                        chars.next();
                        tokens.push(Token::Operator("||"));
                    }
                    Some('&') => {
                        chars.next();
                        tokens.push(Token::Operator("|&"));
                    }
                    _ => tokens.push(Token::Operator("|")),
                }
            }
            '<' | '>' => {
                let fd = (started && !quoted && word.chars().all(|d| d.is_ascii_digit()))
                    .then(|| word.parse().ok())
                    .flatten();
                if fd.is_some() {
                    word.clear();
                    started = false;
                } else {
                    flush(&mut tokens, &mut word, &mut started);
                }
                quoted = false;
                let mut op = c.to_string();
                match (c, chars.peek()) {
                    (_, Some('(')) => return None,
                    ('<', Some('<')) => return None,
                    ('>', Some(next @ ('>' | '&' | '|'))) | ('<', Some(next @ '&')) => {
                        op.push(*next);
                        chars.next();
                    }
                    _ => {}
                }
                tokens.push(Token::Redirect(fd, op));
            }
            c if c.is_control() => return None,
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    flush(&mut tokens, &mut word, &mut started);
    Some(tokens)
}
