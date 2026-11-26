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
/// Programs whose output is file content an agent may be about to edit or copy.
const READERS: &[&str] = &[
    "cat", "head", "tail", "sed", "awk", "gawk", "less", "more", "bat", "batcat", "nl", "cut",
    "base64", "xxd", "od", "hexdump", "strings", "git", "echo", "printf",
];
/// Readers that, right after a pipe, only pass on lines of the output before them.
const LINE_FILTERS: &[&str] = &[
    "head", "tail", "cut", "sed", "awk", "gawk", "nl", "less", "more",
];
/// Words that start a command and print nothing, or only the words given to them.
const SILENT: &[&str] = &[
    "echo", "printf", "cd", "pwd", "true", "false", ":", "set", "export", "unset", "local",
    "declare", "typeset", "readonly", "test", "[", "sleep", "wait", "shift", "break", "continue",
    "return", "exit", "trap", "umask", "mkdir", "touch",
];
/// Shell words that open or close a compound command, and those a command follows.
const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "select", "function", "time", "!", "{", "}", "[[",
];
/// Search and listing programs, whose lines the rules for supported commands
/// and read lists know in the forms they take.
const SEARCHERS: &[&str] = &[
    "grep", "egrep", "fgrep", "ugrep", "rg", "ag", "ack", "find", "fd", "bfs", "ls", "tree", "wc",
];
/// Shells that run the text of a word after `-c`.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];
const BEFORE_A_COMMAND: &[&str] = &[
    "if", "then", "else", "elif", "while", "until", "do", "time", "!", "{",
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
/// Lines of a failing test: a Rust panic and its assertion, a Go `--- FAIL:`, a
/// pytest `FAILED` line, or a Dart/Flutter test failure. A test report that names
/// no failing test, such as one whose `FAILED` line a `| tail` cut off, is
/// followed by these instead.
static FAILURE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:thread '[^']*' (?:\(\d+\) )?panicked at |assertion|  left: | right: |--- FAIL: |FAILED |\s*(?:FAIL|TIMEOUT) \[|failures:\s*$|\s*\d{2,}:\d{2}\s+\+\d+(?:\s+~\d+)?\s+-\d+:.*\[E\]|Some tests failed\.?$)",
    )
    .expect("test failure pattern")
});
/// Leading structured-log indicators used to recognize noisy multi-line log
/// streams from unrouted commands (e.g. app servers, workers, scripts) without
/// matching code search output, diffs or plain progress messages.
static STRUCTURED_LOG_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:",
        r"\[pod/[^\]]+\]\s+",
        r"|[IWEF]\d{4}\s+\d{2}:\d{2}:\d{2}(?:\.\d+)?\s+",
        r"|\[?\d{4}[-/]\d{2}[-/]\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\]?(?:\s|$)",
        r"|\[?\d{2}:\d{2}:\d{2}(?:[.,]\d+)?\]?\s+",
        r"|(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}\s+\S+",
        r"|\[?(?:TRACE|DEBUG|INFO|WARN(?:ING)?|ERROR|FATAL|CRITICAL|NOTICE)\]?(?::|\s)\s*\S",
        r"|(?:ts|time|timestamp|level|lvl|severity)=\S+",
        r")",
    ))
    .expect("structured log line pattern")
});
/// Compose-style service prefix (`api-1  | ...`) followed by a structured log
/// payload (timestamp, level, JSON object, or HTTP request/status).
static COMPOSE_LOG_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z][A-Za-z0-9._-]{0,31}\s+\|\s+(.+)$").expect("compose log line pattern")
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

/// Whether a Bash result's `stderr` holds nothing the command printed: it is
/// empty, or only the note Claude Code adds on a line of its own after a command
/// left the session's working directories, `Shell cwd was reset to DIR`. Claude
/// Code's main session adds it to every `cd` to a folder outside them.
pub(crate) fn no_stderr(stderr: &str) -> bool {
    let note = stderr.trim_matches('\n');
    note.is_empty()
        || note
            .strip_prefix("Shell cwd was reset to ")
            .is_some_and(|dir| !dir.is_empty() && !dir.contains('\n'))
}

/// Extract the command's exit status from a `tool_response` object. Preserves
/// explicit non-zero exit codes (`exitCode`, `exit_code`, `returnCode`,
/// `exitStatus`, integer `status`) and falls back to `returnCodeInterpretation`
/// (parsing an explicit non-zero code when phrased like `Exit code N`, else 1).
pub(crate) fn response_exit_status(response: &serde_json::Map<String, Value>) -> i32 {
    for key in [
        "exitCode",
        "exit_code",
        "returnCode",
        "exitStatus",
        "status",
    ] {
        if let Some(code) = response
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
        {
            return code;
        }
    }
    if let Some(text) = response
        .get("returnCodeInterpretation")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return parse_interpretation_code(text).unwrap_or(1);
    }
    0
}

fn parse_interpretation_code(text: &str) -> Option<i32> {
    let bytes = text.as_bytes();
    for needle in [&b"exit code"[..], b"exit status", b"return code", b"status"] {
        if let Some(pos) = find_ascii_icase(bytes, needle) {
            let rest = text[pos + needle.len()..]
                .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == ':' || c == '=');
            let digits_end = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digits_end > 0
                && let Ok(code) = rest[..digits_end].parse::<i32>()
                && code != 0
            {
                return Some(code);
            }
        }
    }
    None
}

#[inline]
fn find_ascii_icase(hay: &[u8], needle_lower: &[u8]) -> Option<usize> {
    let (&first, rest) = needle_lower.split_first()?;
    if hay.len() < needle_lower.len() {
        return None;
    }
    let first_upper = first.to_ascii_uppercase();
    let last = hay.len() - needle_lower.len();
    let mut i = 0;
    while i <= last {
        let off = hay[i..=last]
            .iter()
            .position(|&b| b == first || b == first_upper)?;
        let pos = i + off;
        if hay[pos + 1..pos + needle_lower.len()].eq_ignore_ascii_case(rest) {
            return Some(pos);
        }
        i = pos + 1;
    }
    None
}

#[inline]
fn has_run_marker(text: &str) -> bool {
    text.contains("[justless run-") && RUN_MARKER.is_match(text)
}

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
    // While `.justless.toml` fails to load, the output stays as the command left it.
    let exclusions = crate::rewrite::Exclusions::of(ws).ok()?;
    if call["run_in_background"] == true || excluded(&exclusions, command) {
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
    let routed = has_run_marker(stdout);
    // The read parsers take the line with the plain variables it sets itself
    // filled in and its globs expanded, as the shell ran it. The reports below
    // read it as written.
    let session = input["cwd"].as_str().map_or(ws.cwd.as_path(), Path::new);
    let expanded = (!cfg!(windows))
        .then(|| crate::batch::as_run(command, Some(session)))
        .flatten();
    let reads = expanded.as_deref().unwrap_or(command);
    let read = (!cfg!(windows))
        .then(|| crate::shellread::parse(reads))
        .flatten();
    // A read that ends in `| cat` printed the read's bytes, so it is handled as
    // the read alone.
    let reads = read.as_ref().map_or(reads, |read| read.unpiped(reads));
    let command = if expanded.is_some() { command } else { reads };
    if let Some(read) = read {
        // A `git show` blob that is not outlined gets the ordinary report below.
        let object = read.object();
        if routed && !object {
            return None;
        }
        // A wide read the range outline cannot show, such as a shell script,
        // keeps the read list's excerpt, and so does a read of a file outside
        // the workspace, which the single-file read never takes.
        let outside = !read.inside(&ws.root, session);
        let compacted = crate::shellread::claude(ws, input, &read, || {
            read_list(reads)
                .or_else(|| crate::batch::Batch::parse_read(reads).filter(|_| outside))
                .filter(|_| !object)
                .and_then(|batch| batch.compact(ws, input))
        });
        if compacted.is_some() || !object {
            return compacted;
        }
    }
    // A batch the recheck cannot verify, such as a diff shaped by repository
    // settings, still gets the report for its one kind of command below.
    if !cfg!(windows)
        && !routed
        && let Some(compacted) = read_list(reads).and_then(|batch| batch.compact(ws, input))
    {
        return Some(compacted);
    }
    if routed {
        return None;
    }
    let settings = ws.settings().ok()?;
    let persisted = response.get("persistedOutputPath").and_then(Value::as_str);
    if persisted.is_none() && stdout.len() <= settings.context_budget {
        return None;
    }
    // A line no rule above reads, such as a script, a loop or a tool no hook
    // knows, gets the clipped preview of a run, unless it prints file content.
    // For a result Claude Code saved to a file, an unknown command is replaced
    // only when a structured view (JSON tables, logview, inventory, or stack
    // trace collapsing) compacts it within Claude Code's preview budget.
    let (plan, unknown_persisted) = match plan(command) {
        Some(plan) => (plan, false),
        None if persisted.is_none() => (unknown_plan(command)?, false),
        None => (unknown_plan(command)?, true),
    };
    // A window of the lines, such as `| sed -n 'A,Bp'`, is compacted only from the
    // text Claude Code shows inline.
    if plan.strict() && persisted.is_some() {
        return None;
    }
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
    let run_budget = crate::adaptive::preview_budget(&run_ws, &args).ok()?;
    let threshold = settings.context_budget.max(run_budget);
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
    let column_width = plan.columns();
    let has_filters = !plan.filters.is_empty() || !plan.later.is_empty();
    let (clipped_columns, start_cut, end_cut, ends_early) =
        if column_width.is_none() && !has_filters {
            (false, false, false, false)
        } else {
            let shown_text = String::from_utf8_lossy(&output);
            let clipped_columns = column_width
                .is_some_and(|width| shown_text.lines().any(|line| line.len() >= width));
            // A line `cut -c1-N` may have clipped reads as a search match still, since a
            // match keeps its path and line number at the start, but not as a test, patch,
            // lint or listing line.
            if clipped_columns && plan.profile != Some(crate::filters::Profile::Search) {
                return None;
            }
            let (start_cut, end_cut, ends_early) = plan.cut(shown_text.lines().count());
            (clipped_columns, start_cut, end_cut, ends_early)
        };
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
    // Preserve explicit non-zero exit codes and `returnCodeInterpretation` so
    // failing runs keep their exit status and failure signals through compaction.
    let status = response_exit_status(response);
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
    let record = |budget: usize| {
        capture::record_cut(
            &run_ws,
            &args,
            &output,
            status,
            budget,
            &session,
            (&origin, &[], ends_early),
        )
        .ok()
    };
    let mut shown = record(budget)?;
    let mut archive = ws.state.join("runs").join(&shown.run.id);
    let mut text: String = shown
        .previews
        .iter()
        .map(|(_, bytes)| String::from_utf8_lossy(bytes))
        .collect();
    let archive_complete = shown.run.produced_bytes.is_empty();
    let is_json_view = text.starts_with("[JSON ");
    let was_clipped = text.contains("[output clipped. Full log retained.]");
    let cmd_refs: Vec<&str> = plan.source_words().iter().map(String::as_str).collect();
    let is_log_cmd = is_log_command(&cmd_refs);
    let mut replaced = false;
    let mut used_logview =
        is_log_cmd && shown.report && shown.run.summary.kind.is_empty() && !is_json_view;
    let will_try_replace = archive_complete && !shown.report && !is_json_view && !clipped_columns;
    let will_try_floor = plan.strict() && !shown.report;
    let will_scan_errors =
        !(shown.footer.is_empty() || shown.run.summary.kind == "lint" || used_logview);
    // Redact the full output once lazily only when post-run formatters, floor
    // compaction, or unshown error scans actually inspect it.
    let redacted_bytes = if will_try_replace || will_try_floor || will_scan_errors {
        redactor.map_or(Cow::Borrowed(output.as_slice()), |r| r.bytes(&output))
    } else {
        Cow::Borrowed(output.as_slice())
    };
    let redacted_text = if will_try_replace || will_try_floor {
        String::from_utf8_lossy(&redacted_bytes)
    } else {
        Cow::Borrowed("")
    };
    if will_try_replace {
        let mut replacement: Option<(String, bool, Option<crate::summary::Summary>)> = None;
        // 1. Extended DevOps inventories (`kubectl get events|svc|ing|pvc|cj`,
        //    `helm list|ls`, `docker|podman images`) when capture did not match
        //    them directly (e.g. flags before subcommands, aliases, or pipes).
        if let Some(kind) = extra_inventory_kind(&cmd_refs)
            && let Some(view) = format_extra_inventory(kind, &redacted_text, budget)
            && view.len() <= budget
            && view.len() < output.len()
            && (was_clipped || view.len() + 64 < text.len())
        {
            replacement = Some((view, false, None));
        }
        // 2. High-frequency CLI raw-dump compactors (`kubectl`/`oc describe`,
        //    `gh`/`glab` views, security audits, `outdated`/`pip list|freeze`,
        //    and `terraform`/`tofu state list`).
        if replacement.is_none()
            && let Some(view) = crate::cli_views::format(&cmd_refs, &redacted_text, budget)
            && view.len() <= budget
            && view.len() < output.len()
            && (was_clipped || view.len() + 64 < text.len())
        {
            replacement = Some((view, false, None));
        }
        // 2a. Cloud & Kubernetes diff/manifest/listing compactors (`kubectl`/`oc diff`,
        //     `helm template`/`get manifest`, `aws s3 ls`, `gcloud`/`az`/`aws` display lists).
        if replacement.is_none()
            && let Some(view) = crate::cloud_views::format(&cmd_refs, &redacted_text, budget)
            && view.len() <= budget
            && view.len() < output.len()
            && (was_clipped || view.len() + 64 < text.len())
        {
            replacement = Some((view, false, None));
        }
        // 2b. BuildKit / Docker / Compose / Podman build logs.
        if replacement.is_none()
            && crate::buildkit::is_build_command(&cmd_refs)
            && let Some(view) = crate::buildkit::format_for_command(
                &cmd_refs,
                &redacted_text,
                "",
                Some(shown.run.status),
                budget,
            )
            && view.len() <= budget
            && view.len() < output.len()
            && (was_clipped || view.len() + 64 < text.len())
        {
            replacement = Some((view, false, None));
        }
        // 2c. Dependency trees and graphs (`cargo tree`, `npm ls`, `pnpm list`,
        //     `yarn list`, `pipdeptree`, `go mod graph`, `mvn`, `gradle`,
        //     `dart pub deps`, `flutter pub deps`).
        if replacement.is_none()
            && let Some(view) = crate::deptree::format_words(&cmd_refs, &redacted_text, budget)
            && view.len() <= budget
            && view.len() < output.len()
            && (was_clipped || view.len() + 64 < text.len())
        {
            replacement = Some((view, false, None));
        }
        // 2c2. Dart & Flutter analyzer grouped lint reports (handles leading
        //      flags such as `flutter --no-color analyze` or `loose()` fallback).
        if replacement.is_none()
            && is_dart_analyze(&cmd_refs)
            && let Some((view, summary)) = format_dart_analyze(
                &run_ws.root,
                &cmd_refs,
                &redacted_text,
                shown.run.status,
                budget,
            )
            && view.len() <= budget
            && view.len() < output.len()
            && (was_clipped || view.len() + 64 < text.len())
        {
            replacement = Some((view, false, summary));
        }
        // 2d. Structured polyglot test runner reports (`pytest`, `unittest`,
        //     `go test`, `dotnet`, `mvn`, `gradle`, `swift`, `ctest`, `mix`,
        //     `deno`, `dart test`, `flutter test`).
        if replacement.is_none()
            && let Some(runner) = polyglot_test_runner(&cmd_refs)
            && let Some(poly) = crate::polyglot_tests::format(
                runner,
                &redacted_text,
                "",
                Some(shown.run.status),
                budget,
            )
            && poly.text.len() <= budget
            && poly.text.len() < output.len()
            && (was_clipped || poly.text.len() + 64 < text.len())
        {
            let mut summary = shown.run.summary.clone();
            summary.kind = "tests".to_owned();
            summary.items = poly
                .items
                .iter()
                .map(crate::polyglot_tests::FailureItem::to_summary_item)
                .collect();
            replacement = Some((poly.text, false, Some(summary)));
        }
        // 2e. Universal lossless / bounded tables (`psql`, `mysql`, `duckdb`,
        //     `sqlite3`, `aws --output table`, Markdown tables).
        if replacement.is_none()
            && !crate::floor::prints_file(&cmd_refs)
            && redacted_text.len() <= crate::tableview::LIMIT
            && let Some(view) = crate::tableview::format(&redacted_text, budget)
            && view.len() <= budget
            && view.len() < output.len()
            && (was_clipped || view.len() + 64 < text.len())
        {
            replacement = Some((view, false, None));
        }
        // 3. Structured or container/JSON/systemd log streams via `logview`.
        if replacement.is_none()
            && (is_log_cmd
                || (!plan.strict()
                    && plan.profile.is_none()
                    && looks_like_structured_logs(&redacted_text)))
            && let Some(folded) = crate::logview::format(&redacted_text, "", budget)
            && folded.len() <= budget
            && folded.len() < output.len()
            && (was_clipped || folded.len() + 64 < text.len())
            && (is_log_cmd || folded.len() * 4 <= output.len() * 3)
        {
            replacement = Some((folded, true, None));
        }
        // 3. Traceback-heavy outputs: collapse external runtime frames while
        //    keeping 100% of workspace frames & errors verbatim with `[stdout:A-B]`
        //    links, avoiding blind head/tail clipping whenever the result fits.
        if replacement.is_none()
            && let Some(collapsed) =
                crate::stacktrace::collapse(&redacted_text, Some(&run_ws.root), Some("stdout"))
        {
            let candidate = if collapsed.len() <= budget {
                Some(collapsed)
            } else {
                crate::floor::compact_stream(&redacted_text, &run_ws.root, "stdout")
                    .filter(|floored| floored.len() <= budget)
            };
            if let Some(candidate) = candidate
                && candidate.len() < output.len()
                && (was_clipped || candidate.len() < text.len())
            {
                replacement = Some((candidate, false, None));
            }
        }
        if let Some((mut body, logview, new_summary)) = replacement {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            if let Some(redactor) = redactor
                && let Cow::Owned(redacted) = redactor.text(&body)
            {
                body = redacted;
            }
            if let Some(summary) = new_summary {
                shown.run.summary = summary;
            }
            text = body;
            shown.run.preview_bytes = text.len();
            shown.footer = rebuild_footer(&shown.run, text.len());
            shown.run.footer_bytes = shown.footer.len();
            shown.report = true;
            replaced = true;
            used_logview = logview;
        }
    }
    if unknown_persisted && !shown.report && !is_json_view {
        let _ = fs::remove_dir_all(&archive);
        return None;
    }
    // A window with no report gets the floor, when that alone saves enough, and is
    // otherwise left as it is.
    if plan.strict() && !shown.report {
        let _ = fs::remove_dir_all(&archive);
        let floor = crate::floor::compact(&redacted_text)?;
        if output.len().saturating_sub(floor.len()) < (output.len() / 4).max(512) {
            return None;
        }
        // Within this budget the stream is shown whole, so it gets the floor.
        shown = record(output.len().saturating_mul(2))?;
        archive = ws.state.join("runs").join(&shown.run.id);
        text = shown
            .previews
            .iter()
            .map(|(_, bytes)| String::from_utf8_lossy(bytes))
            .collect();
    }
    // A lint report or logview report covers both streams by design. Other reports
    // were written for stdout alone, so error lines from the interleaved stderr that
    // they left out follow them, and so do the failure lines a test report could not
    // attribute.
    let summary = &shown.run.summary;
    let errors = if shown.footer.is_empty() || summary.kind == "lint" || used_logview {
        String::new()
    } else {
        let failures = (status != 0 && summary.items.is_empty())
            || summary.kind.is_empty()
            || summary.kind == "tests" && summary.items.is_empty();
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
            &redacted_bytes,
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
    // A report of what a `head`, `tail` or `sed -n` window kept says so, since the
    // command may have printed more.
    let note = if start_cut || end_cut {
        let chain = crate::summary::short(&plan.chains(), 160);
        format!("Covers only the lines `{chain}` passed on, not the command's whole output.\n")
    } else {
        String::new()
    };
    text.insert_str(0, &note);
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
    if replaced || !errors.is_empty() || !note.is_empty() || preview.is_some() {
        shown.run.footer_bytes += errors.len() + note.len();
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
            || !(reports_error(line, program) || failures && matches_failure_line(line))
            || shown.contains(trimmed)
        {
            continue;
        }
        if listed >= ERROR_LINES {
            more += 1;
            continue;
        }
        let entry = format!("stdout:{}: {}\n", index + 1, crate::search::clip(trimmed));
        if lines.len() + entry.len() <= budget {
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

#[inline]
fn matches_failure_line(line: &str) -> bool {
    let Some(&first) = line.as_bytes().first() else {
        return false;
    };
    (first.is_ascii_whitespace()
        || first.is_ascii_digit()
        || matches!(first, b't' | b'a' | b'-' | b'F' | b'T' | b'f' | b'S'))
        && FAILURE_LINE.is_match(line)
}

fn reports_error(line: &str, program: &str) -> bool {
    if line
        .as_bytes()
        .first()
        .is_some_and(|&b| matches!(b, b'e' | b'f' | b'F' | b'E' | b'p' | b'n' | b'T'))
        && ERROR_LINE.is_match(line)
    {
        return true;
    }
    let Some((tool, message)) = line.split_once(": ") else {
        return false;
    };
    if tool != program && !TOOLS.contains(&tool) {
        return false;
    }
    let bytes = message.as_bytes();
    TOOL_ERRORS
        .iter()
        .any(|needle| find_ascii_icase(bytes, needle.as_bytes()).is_some())
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
fn excluded(exclusions: &crate::rewrite::Exclusions, command: &str) -> bool {
    !exclusions.is_empty()
        && (exclusions.any(command).is_some()
            || parse(command).is_some_and(|segments| {
                segments.iter().flatten().any(|stage| {
                    let words = strip(&stage.words).unwrap_or(&stage.words);
                    exclusions.matching(&words.join(" ")).is_some()
                })
            }))
}

/// What [`claude_bash`] does with a successful Bash result, judged from the command
/// line and the size of the output alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AfterRun {
    /// It replaces the result with a compaction.
    Compacts,
    /// It handles this kind of command but keeps this result whole, as it does a
    /// small read or a supported command's output within the context budget.
    KeepsWhole,
}

/// How [`claude_bash`] would treat `command` run from `cwd` in the workspace at
/// `root`, whose result was `output`, or `persisted` bytes that Claude Code saved
/// to a file, for `discover`. `None` when it leaves such a command alone at any
/// size. It applies the hook's own parsers, exclusions and size limits, with
/// paths judged from the command's text, since the files may have changed. It
/// takes `context_budget` as the threshold, so a family whose adaptive preview
/// budget is larger keeps more output whole, and it cannot tell a compaction that
/// the hook drops after the run for saving too little.
pub(crate) fn after_run(
    command: &str,
    (root, cwd): (&Path, &Path),
    exclusions: &crate::rewrite::Exclusions,
    output: &str,
    persisted: Option<u64>,
    (context_budget, outline_lines): (usize, u64),
) -> Option<AfterRun> {
    if !cwd.starts_with(root) || excluded(exclusions, command) || has_run_marker(output) {
        return None;
    }
    let size = persisted.unwrap_or(output.len() as u64);
    let lines = output.matches('\n').count();
    let fits = |threshold: usize| persisted.is_none() && size <= threshold as u64;
    let verdict = |compacts: bool| {
        Some(if compacts {
            AfterRun::Compacts
        } else {
            AfterRun::KeepsWhole
        })
    };
    let mut handled = false;
    // Shown whole and at most `read_outline_lines` lines, unless minified onto
    // one or two huge lines.
    let minified = persisted.is_none() && lines <= 2 && output.len() > crate::batch::FLOOR;
    let few = persisted.is_none() && !minified && lines as u64 <= outline_lines;
    if !cfg!(windows) {
        // As the hook reads it, with the variables the line sets filled in and
        // one file standing for each glob.
        let expanded = crate::batch::as_run(command, None);
        let reads = expanded.as_deref().unwrap_or(command);
        if let Some(read) = crate::shellread::parse(reads) {
            // Outside the workspace a `git show` blob falls back to the ordinary
            // report, and a file is shown as a read list or left alone.
            if !read.inside(root, cwd) && !read.object() {
                let reads = read.unpiped(reads);
                return read_list(reads)
                    .or_else(|| crate::batch::Batch::parse_read(reads))
                    .filter(|batch| batch.reachable(root, cwd))
                    .and_then(|_| verdict(!fits(crate::batch::FLOOR) && !few));
            }
            let small = crate::shellread::small(
                output.lines().count(),
                output.len(),
                persisted.is_none().then_some(outline_lines),
            ) && persisted.is_none();
            // A `git show` blob too small to outline gets the ordinary report.
            if !read.object() || !small {
                return verdict(!small);
            }
            handled = true;
        }
        if let Some(batch) = read_list(reads).filter(|batch| batch.reachable(root, cwd)) {
            // File reads of few lines in all stay as printed.
            let kept = few && batch.files_only();
            if !fits(crate::batch::FLOOR) && !kept {
                return verdict(true);
            }
            handled = true;
        }
    }
    let large = size > MAX_BYTES as u64 || persisted.is_none() && lines > MAX_LINES;
    if let Some(plan) = plan(command) {
        // As `entered` judges the directory, from the text alone.
        let entered = crate::find::lexical(&cwd.join(&plan.cd)).starts_with(root)
            || plan.cd.is_relative()
                && plan
                    .cd
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)));
        return if entered {
            verdict(!large && !fits(context_budget))
        } else {
            handled.then_some(AfterRun::KeepsWhole)
        };
    }
    if unknown_plan(command).is_some() && !large && !fits(context_budget) {
        return verdict(true);
    }
    handled.then_some(AfterRun::KeepsWhole)
}

struct Plan {
    /// Its words after assignments and wrappers such as `env` or `timeout`.
    words: Vec<String>,
    /// The original first-command words when [`loose`] collapsed `words` to a
    /// single program name, used for post-run inventory and log detection
    /// without altering the recorded run family.
    command_words: Vec<String>,
    /// The program's file name, which prefixes its own error messages.
    program: String,
    /// How lists name the run: the command label, and the filters after a pipe.
    label: String,
    /// The directories entered with `cd` before it, joined.
    cd: PathBuf,
    /// The filters its output passed through, with their words.
    filters: Vec<(Filter, String)>,
    /// The filters of each later command of the same kind in a list.
    later: Vec<Vec<(Filter, String)>>,
    /// The display profile that compacts its output.
    profile: Option<crate::filters::Profile>,
}

impl Plan {
    /// The full words of the underlying command, preserving subcommands and
    /// flags even when [`loose`] records the run under its program name alone.
    fn source_words(&self) -> &[String] {
        if self.command_words.is_empty() {
            &self.words
        } else {
            &self.command_words
        }
    }

    /// Whether a filter windowed or clipped the lines, as `sed -n 'A,Bp'` or
    /// `cut -c1-N` does. Such output is replaced only by a report or the floor, never
    /// by a clipped preview of the lines the agent picked.
    fn strict(&self) -> bool {
        self.filters
            .iter()
            .any(|(filter, _)| matches!(filter, Filter::Range(..) | Filter::Columns(_)))
    }

    /// The width of the narrowest `cut -c1-N`.
    fn columns(&self) -> Option<usize> {
        self.filters
            .iter()
            .filter_map(|(filter, _)| match filter {
                Filter::Columns(width) => Some(*width),
                _ => None,
            })
            .min()
    }

    /// Whether the filters may have dropped lines from the start and from the end
    /// of the output, given the `lines` shown, and whether the last command's
    /// output may end early. In a list, whose lines several commands printed, any
    /// window may have cut.
    fn cut(&self, lines: usize) -> (bool, bool, bool) {
        let Some(last) = self.later.last() else {
            let (start, end) = window(&self.filters, Some(lines));
            return (start, end, end);
        };
        let (start, end) = self
            .later
            .iter()
            .map(|chain| window(chain, None))
            .fold(window(&self.filters, None), |(a, b), (c, d)| {
                (a || c, b || d)
            });
        (start, end, window(last, None).1)
    }

    /// The filters of every command, as `head -n 40 | cat; head -5`.
    fn chains(&self) -> String {
        std::iter::once(&self.filters)
            .chain(&self.later)
            .filter(|chain| !chain.is_empty())
            .map(|chain| {
                let words: Vec<&str> = chain.iter().map(|(_, words)| words.as_str()).collect();
                words.join(" | ")
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Whether `chain` may have dropped lines from the start and from the end of its
/// input, given the `lines` it passed on when known. A window filter that passed on
/// fewer lines than its limit dropped none, when only filters that keep every line
/// follow it. Claude Code drops trailing blank lines, so a window within a few lines
/// of its limit counts as full.
fn window(chain: &[(Filter, String)], lines: Option<usize>) -> (bool, bool) {
    let (mut start, mut end) = (false, false);
    for (index, (filter, _)) in chain.iter().enumerate() {
        let counted = chain[index + 1..]
            .iter()
            .all(|(filter, _)| matches!(filter, Filter::Pass | Filter::Columns(_)));
        let reached = |limit: usize| !counted || lines.is_none_or(|lines| lines + TRIMMED >= limit);
        match *filter {
            Filter::Pass | Filter::Lines | Filter::Columns(_) => {}
            Filter::Head(limit) => end |= limit.is_none_or(reached),
            Filter::Tail(limit) => start |= limit.is_none_or(reached),
            Filter::Range(first, last_line) => {
                start |= first > 1;
                end |= last_line.is_some_and(|last_line| reached(last_line + 1 - first));
            }
        }
    }
    (start, end)
}

/// Trailing blank lines Claude Code may have dropped from a window's last lines.
const TRIMMED: usize = 3;

/// What a filter after the supported command does to its output.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Filter {
    /// Passes every line on, maybe in another order: `cat`, `sort` or `tee`.
    Pass,
    /// Selects whole lines, such as `grep` or `uniq`.
    Lines,
    /// `head`: the first lines, or with `None` the first bytes.
    Head(Option<usize>),
    /// `tail`: the last lines, or with `None` the last bytes or the lines from a
    /// numbered one.
    Tail(Option<usize>),
    /// `sed -n 'A,Bp'`: lines A through B, or through the last.
    Range(usize, Option<usize>),
    /// `cut -c1-N`: the first N characters of each line.
    Columns(usize),
}

/// The read list this hook compacts: one of reads alone, or one that mixes in
/// other pieces when no single-command report below takes the line, so
/// `echo NOTE; git diff` keeps the diff report.
pub(crate) fn read_list(command: &str) -> Option<crate::batch::Batch<'_>> {
    crate::batch::Batch::parse(command)
        .filter(|batch| batch.reads_only() || plan(command).is_none())
}

/// Find the one kind of supported command a command line runs. Its output may pass
/// through filters that keep whole lines, such as `head`, `tail` or `grep`, and in a
/// line that runs nothing else, a `sed -n 'A,Bp'` window or a `cut -c1-N`. Other
/// commands in a list may only change directory, change a setting, or print a short
/// note such as `echo` before it. Several supported commands must select the same
/// compaction, such as two searches. Anything else, including a command already
/// routed through Justless, returns `None`.
fn plan(command: &str) -> Option<Plan> {
    let segments = parse(command)?;
    let mut cd = PathBuf::new();
    type KindKey = (
        Option<crate::filters::Profile>,
        String,
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
        bool,
    );
    let mut found: Option<(Plan, KindKey)> = None;
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
        if !keeps_stdout(first) {
            return None;
        }
        let kept = filters
            .iter()
            .map(|stage| {
                let words = strip(&stage.words)?;
                let text = shlex::try_join(words.iter().map(String::as_str))
                    .unwrap_or_else(|_| words.join(" "));
                Some((filter(stage)?, text))
            })
            .collect::<Option<Vec<_>>>()?;
        let words = strip(&first.words)?;
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let extra_inv = extra_inventory_kind(&refs).map(ExtraInventory::tag);
        let cli_view = crate::cli_views::kind_tag(&refs);
        let cloud_view = crate::cloud_views::kind_tag(&refs);
        let dart_flutter = dart_flutter_kind(&refs);
        let log_cmd = is_log_command(&refs);
        if !crate::agents::supported_words(&refs)
            && extra_inv.is_none()
            && cli_view.is_none()
            && cloud_view.is_none()
            && dart_flutter.is_none()
            && !log_cmd
        {
            return None;
        }
        let args: Vec<OsString> = words.iter().map(OsString::from).collect();
        let kind: KindKey = (
            crate::filters::profile(&refs),
            crate::summary::family(&args),
            extra_inv,
            cli_view,
            cloud_view,
            dart_flutter,
            log_cmd,
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
        match &mut found {
            Some((plan, first_kind)) if *first_kind != kind || plan.strict() => return None,
            Some(_)
                if kept
                    .iter()
                    .any(|(f, _)| matches!(f, Filter::Range(..) | Filter::Columns(_))) =>
            {
                return None;
            }
            Some((plan, _)) => {
                after = true;
                plan.later.push(kept);
            }
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
                    command_words: Vec::new(),
                    program: basename(tool.first().copied().unwrap_or("")).to_owned(),
                    label,
                    cd: cd.clone(),
                    filters: kept,
                    later: Vec::new(),
                    profile: kind.0,
                };
                found = Some((plan, kind));
            }
        }
    }
    let (mut plan, _) = found?;
    if after {
        // A window of one command's lines is reported alone.
        if plan.strict() {
            return None;
        }
        plan.label.push_str("; …");
    }
    Some(plan)
}

/// A line no rule reads whose output is not file content, as [`runs`] judges it:
/// one command that no hook supports, alone on its line, or any other line, such
/// as a script fed by a here-document, a loop, a list or a pipe. Its output gets
/// the clipped preview of a run.
fn unknown_plan(command: &str) -> Option<Plan> {
    let (commands, alone) = runs(command)?;
    lone(command).or_else(|| loose(&commands, alone))
}

/// One command that no hook supports, alone on its line with its output reaching
/// the agent: no pipe, list or redirection of stdout, and not a program that prints
/// file content.
fn lone(command: &str) -> Option<Plan> {
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
        command_words: Vec::new(),
        program: program.to_owned(),
        label: crate::history::label(&refs),
        cd: PathBuf::new(),
        filters: Vec::new(),
        later: Vec::new(),
        profile: None,
    })
}

/// A plan for a line [`runs`] reads, when no command that starts a pipeline on it
/// is one a hook supports or a search or listing: such a command's lines are left
/// to the rules above,
/// which pass them through when a report or a preview would hide lines the agent
/// asked to see. The run is recorded under its first command's program alone, so
/// no report made for one command's lines is applied to a line that mixes several.
fn loose(commands: &Commands, alone: bool) -> Option<Plan> {
    let mut first = None;
    for (words, piped) in commands {
        let words = strip(words).unwrap_or(words);
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        // A command routed for its preview alone has no report that would hide
        // lines, so a line that mixes it with others keeps the clipped preview.
        let known = refs
            .first()
            .is_some_and(|word| SEARCHERS.contains(&basename(word)))
            || crate::agents::supported_words(&refs) && !crate::known::listed(&refs);
        if !piped && known {
            return None;
        }
        first = first.or(Some(words));
    }
    let words = first?;
    let program = basename(words.first()?).to_owned();
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    let mut label = crate::history::label(&refs);
    if !alone {
        label.push_str("; …");
    }
    // A program some report is made for, such as a linter printing a machine
    // format, is recorded as `sh`, which selects none.
    let reported = crate::filters::Selection::for_run(&[OsString::from(&program)], &[])
        .map_or(true, |selection| selection.profile.is_some());
    let recorded = if reported { "sh" } else { program.as_str() };
    Some(Plan {
        words: vec![recorded.to_owned()],
        command_words: words.to_vec(),
        program,
        label,
        cd: PathBuf::new(),
        filters: Vec::new(),
        later: Vec::new(),
        profile: None,
    })
}

/// The words of each command on a line, and whether it follows a pipe.
type Commands = Vec<(Vec<String>, bool)>;

/// The commands on a line that print a result of their own, each with whether it
/// follows a pipe, and whether the line is one command alone, read from the text
/// so that loops, here-documents, substitutions and comments are no obstacle.
/// `None` when the line may print file content: a reader such as `cat`, `sed`,
/// `git`, `diff` or the `read` of a loop appears as a word anywhere outside single
/// quotes, other than a line filter right after a pipe, as in
/// `python3 report.py | tail -40`. Text in single quotes and the body of a
/// here-document are a program's own input and are not read, except that a shell
/// given its commands with `-c`, and `eval`, run text this cannot see, so such a
/// line is `None`. `None` too when the line runs `justless` or nothing but notes
/// such as `echo`.
fn runs(command: &str) -> Option<(Commands, bool)> {
    enum Piece {
        Word(String),
        Pipe,
        /// Any other operator, or the start of a substitution: a command follows.
        Break,
        /// A redirection, whose next word is a file or descriptor.
        Target,
    }
    if command.len() > 64 * 1024 {
        return None;
    }
    // Drop the body of each here-document (zero-alloc borrow when no `<<`).
    let text: Cow<'_, str> = if command.contains("<<") {
        let mut stripped = String::with_capacity(command.len());
        let mut body: Option<(String, bool)> = None;
        for line in command.split_inclusive('\n') {
            if let Some((end, tabs)) = &body {
                let mut last = line.trim_end_matches('\n');
                if *tabs {
                    last = last.trim_start_matches('\t');
                }
                if last == end {
                    body = None;
                }
                continue;
            }
            stripped.push_str(line);
            let marker = line.find("<<").filter(|&at| !line[at..].starts_with("<<<"));
            if let Some(at) = marker {
                let rest = &line[at + 2..];
                let tabs = rest.starts_with('-');
                let end: String = rest
                    .trim_start_matches('-')
                    .trim_start()
                    .trim_start_matches(['\'', '"', '\\'])
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !end.is_empty() {
                    body = Some((end, tabs));
                }
            }
        }
        Cow::Owned(stripped)
    } else {
        Cow::Borrowed(command)
    };
    let mut pieces = Vec::new();
    let mut word = String::new();
    let mut double = false;
    let mut chars = text.chars().peekable();
    fn flush(pieces: &mut Vec<Piece>, word: &mut String) {
        if !word.is_empty() {
            pieces.push(Piece::Word(std::mem::take(word)));
        }
    }
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => {
                // Opaque text, which still makes a word.
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                }
                word.push('\0');
            }
            '"' => double = !double,
            '\\' => {
                if let Some(next) = chars.next().filter(|next| *next != '\n') {
                    word.push(next);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                flush(&mut pieces, &mut word);
                pieces.push(Piece::Break);
            }
            '`' => {
                flush(&mut pieces, &mut word);
                pieces.push(Piece::Break);
            }
            '\n' if !double => {
                flush(&mut pieces, &mut word);
                pieces.push(Piece::Break);
            }
            c if c.is_whitespace() => flush(&mut pieces, &mut word),
            c if double => word.push(c),
            '#' if word.is_empty() => {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            ';' | '&' | '(' | ')' => {
                flush(&mut pieces, &mut word);
                pieces.push(Piece::Break);
            }
            '|' => {
                flush(&mut pieces, &mut word);
                if chars.peek() == Some(&'|') {
                    chars.next();
                    pieces.push(Piece::Break);
                } else {
                    if chars.peek() == Some(&'&') {
                        chars.next();
                    }
                    pieces.push(Piece::Pipe);
                }
            }
            '<' | '>' => {
                // A descriptor before the operator is part of it.
                if word.bytes().all(|b| b.is_ascii_digit()) {
                    word.clear();
                }
                flush(&mut pieces, &mut word);
                while chars
                    .peek()
                    .is_some_and(|next| matches!(next, '<' | '>' | '&' | '|' | '-'))
                {
                    chars.next();
                }
                pieces.push(Piece::Target);
            }
            c => word.push(c),
        }
    }
    flush(&mut pieces, &mut word);

    let mut commands: Commands = Vec::new();
    let mut compound = false;
    // Whether the next word starts a command, follows a pipe, names a
    // redirection's target, or belongs to the last command found.
    let (mut starts, mut piped, mut target, mut collecting) = (true, false, false, false);
    // The word before this one.
    let mut shell = String::new();
    for piece in pieces {
        let word = match piece {
            Piece::Word(word) => word,
            Piece::Target => {
                target = true;
                continue;
            }
            Piece::Pipe | Piece::Break => {
                piped = matches!(piece, Piece::Pipe);
                (starts, target, collecting, compound) = (true, false, false, true);
                continue;
            }
        };
        if std::mem::take(&mut target) {
            continue;
        }
        let name = basename(&word);
        let reader = READERS.contains(&name) && !matches!(name, "echo" | "printf")
            || matches!(name, "read" | "diff" | "justless" | "eval");
        if reader && !(starts && piped && LINE_FILTERS.contains(&name)) {
            return None;
        }
        // A shell told its commands in a word, as in `bash -c '…'`.
        let told = word.starts_with('-') && !word.starts_with("--") && word.ends_with('c');
        if told && SHELLS.contains(&basename(&shell)) {
            return None;
        }
        shell.clone_from(&word);
        if !starts {
            if collecting && let Some((words, _)) = commands.last_mut() {
                words.push(word);
            }
            continue;
        }
        if assignment(&word) {
            continue;
        }
        starts = false;
        if KEYWORDS.contains(&name) {
            starts = BEFORE_A_COMMAND.contains(&name);
            compound = true;
        } else if !SILENT.contains(&name) {
            commands.push((vec![word], piped));
            collecting = true;
        }
    }
    let alone = commands.len() == 1 && !compound;
    (!commands.is_empty()).then_some((commands, alone))
}

fn basename(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExtraInventory<'a> {
    Events,
    HelmList,
    Images,
    Resource(&'a str),
}

impl ExtraInventory<'_> {
    fn tag(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::HelmList => "helm_list",
            Self::Images => "images",
            Self::Resource(resource) => match resource.split('.').next().unwrap_or("") {
                "svc" | "service" | "services" => "services",
                "ing" | "ingress" | "ingresses" => "ingresses",
                "pvc" | "pvcs" | "persistentvolumeclaim" | "persistentvolumeclaims" => "pvcs",
                "cj" | "cronjob" | "cronjobs" => "cronjobs",
                _ => "resource",
            },
        }
    }
}

/// Skip leading global CLI options (`-f val`, `--flag=val`, boolean flags)
/// before a subcommand.
fn skip_global_flags<'a>(
    mut args: &'a [&'a str],
    valued: &[&str],
    boolean: &[&str],
) -> &'a [&'a str] {
    while let Some((&arg, rest)) = args.split_first() {
        if valued.contains(&arg) && rest.first().is_some_and(|v| !v.starts_with('-')) {
            args = &rest[1..];
        } else if let Some((flag, _)) = arg.split_once('=')
            && valued.contains(&flag)
        {
            args = rest;
        } else if boolean.contains(&arg) {
            args = rest;
        } else {
            break;
        }
    }
    args
}

/// Whether `tail` carries any machine-output, watch, quiet, or help option that
/// disqualifies table formatting.
fn has_blocked_flag(tail: &[&str], blocked: &[&str], reject_attached_o: bool) -> bool {
    tail.iter().any(|&arg| {
        let flag = arg.split_once('=').map_or(arg, |(f, _)| f);
        blocked.contains(&flag)
            || (reject_attached_o
                && arg.starts_with("-o")
                && arg.len() > 2
                && !arg.starts_with("--"))
    })
}

/// Recognize DevOps inventory commands formatted by
/// `crate::inventory::{events, helm_list, images, resources}`:
/// `kubectl`/`oc get events|svc|ing|pvc|cj`, `helm list|ls`, and
/// `docker`/`podman images|image ls|image list`.
fn extra_inventory_kind<'a>(words: &'a [&'a str]) -> Option<ExtraInventory<'a>> {
    let tool = crate::filters::unwrap_tool(words);
    let (&program, args) = tool.split_first()?;
    let program = basename(program);
    match program {
        "kubectl" | "oc" => {
            let args = skip_global_flags(
                args,
                &[
                    "-n",
                    "--namespace",
                    "--context",
                    "--kubeconfig",
                    "--cluster",
                    "--user",
                    "--as",
                    "--as-group",
                    "--request-timeout",
                    "-v",
                    "--v",
                ],
                &["--insecure-skip-tls-verify"],
            );
            let ["get", resource, tail @ ..] = args else {
                return None;
            };
            if has_blocked_flag(
                tail,
                &[
                    "-o",
                    "--output",
                    "-w",
                    "--watch",
                    "--watch-only",
                    "--no-headers",
                    "--raw",
                    "-h",
                    "--help",
                ],
                true,
            ) {
                return None;
            }
            let base = resource.split('.').next().unwrap_or("");
            match base {
                "ev" | "event" | "events" => Some(ExtraInventory::Events),
                "svc"
                | "service"
                | "services"
                | "ing"
                | "ingress"
                | "ingresses"
                | "pvc"
                | "pvcs"
                | "persistentvolumeclaim"
                | "persistentvolumeclaims"
                | "cj"
                | "cronjob"
                | "cronjobs" => Some(ExtraInventory::Resource(resource)),
                _ => None,
            }
        }
        "helm" => {
            let args = skip_global_flags(
                args,
                &[
                    "-n",
                    "--namespace",
                    "--kube-context",
                    "--kubeconfig",
                    "--kube-apiserver",
                    "--registry-config",
                    "--repository-config",
                    "--repository-cache",
                ],
                &["--debug"],
            );
            let ["list" | "ls", tail @ ..] = args else {
                return None;
            };
            if has_blocked_flag(
                tail,
                &["-o", "--output", "-q", "--short", "-h", "--help"],
                true,
            ) {
                return None;
            }
            Some(ExtraInventory::HelmList)
        }
        "docker" | "podman" => {
            let args = skip_global_flags(
                args,
                &["-H", "--host", "--context", "-c", "--config"],
                &["-D", "--debug"],
            );
            let tail = match args {
                ["images", tail @ ..] | ["image", "ls" | "list", tail @ ..] => tail,
                _ => return None,
            };
            if has_blocked_flag(tail, &["--format", "-q", "--quiet", "-h", "--help"], false) {
                return None;
            }
            Some(ExtraInventory::Images)
        }
        _ => None,
    }
}

fn format_extra_inventory(
    kind: ExtraInventory<'_>,
    text: &str,
    max_bytes: usize,
) -> Option<String> {
    match kind {
        ExtraInventory::Events => crate::inventory::events(text, max_bytes),
        ExtraInventory::HelmList => crate::inventory::helm_list(text, max_bytes),
        ExtraInventory::Images => crate::inventory::images(text, max_bytes),
        ExtraInventory::Resource(resource) => {
            crate::inventory::resources(resource, text, max_bytes)
        }
    }
}

/// Skip safe leading `dart` / `flutter` display flags before a subcommand.
fn dart_flutter_subcommand<'a>(words: &'a [&'a str]) -> Option<(&'a str, &'a [&'a str])> {
    let tool = crate::filters::unwrap_tool(words);
    let (&program, mut args) = tool.split_first()?;
    let base = basename(program);
    if !matches!(base, "dart" | "flutter") {
        return None;
    }
    while let Some((&arg, rest)) = args.split_first() {
        if matches!(
            arg,
            "--no-color"
                | "--color"
                | "--suppress-analytics"
                | "--disable-analytics"
                | "--no-version-check"
                | "-v"
                | "--verbose"
        ) || arg.starts_with("--directory=")
        {
            args = rest;
        } else if matches!(arg, "-C" | "--directory")
            && rest.first().is_some_and(|v| !v.starts_with('-'))
        {
            args = &rest[1..];
        } else {
            break;
        }
    }
    Some((base, args))
}

/// Whether `words` runs display-oriented `dart analyze` or `flutter analyze`
/// (matching `capture::known_lint_tool`, plus safe leading display flags).
fn is_dart_analyze(words: &[&str]) -> bool {
    let owned = crate::wrappers::effective(words);
    let refs: Vec<&str> = match &owned {
        Some(w) => w.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let Some((_, ["analyze", tail @ ..])) = dart_flutter_subcommand(&refs) else {
        return false;
    };
    let mut iter = tail.iter().copied();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "-h" | "--help" | "-V" | "--version" | "--json" | "--xml" | "--sarif"
            | "--checkstyle" | "--watch" | "-w" | "--write" | "--machine" => return false,
            "-f" | "--format" | "-format" | "--error-format" | "--formatter" | "--reporter"
            | "--output-format" => {
                let val = inline.or_else(|| iter.next()).unwrap_or("");
                if matches!(
                    val.to_ascii_lowercase().as_str(),
                    "json"
                        | "jsonl"
                        | "ndjson"
                        | "sarif"
                        | "xml"
                        | "checkstyle"
                        | "junit"
                        | "github"
                        | "gitlab"
                        | "rdjson"
                        | "yaml"
                        | "machine"
                ) {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

/// Polyglot test runner recognition for `postroute`, also unwrapping safe
/// leading `dart` / `flutter` display flags before `test`.
fn polyglot_test_runner(words: &[&str]) -> Option<crate::polyglot_tests::Runner> {
    if let Some(runner) = crate::polyglot_tests::runner_for_words(words) {
        return Some(runner);
    }
    let owned = crate::wrappers::effective(words);
    let refs: Vec<&str> = match &owned {
        Some(w) => w.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let (base, args) = dart_flutter_subcommand(&refs)?;
    if args.first() != Some(&"test") {
        return None;
    }
    let mut normalized = Vec::with_capacity(args.len() + 1);
    normalized.push(base);
    normalized.extend_from_slice(args);
    crate::polyglot_tests::runner_for_words(&normalized)
}

/// Recognize display-oriented `dart`/`flutter` `analyze`, `test`, and `pub deps`
/// commands so `plan` routes them cleanly with full words, `cd`, and pipes.
fn dart_flutter_kind(words: &[&str]) -> Option<&'static str> {
    if is_dart_analyze(words) {
        return Some("dart-analyze");
    }
    if polyglot_test_runner(words) == Some(crate::polyglot_tests::Runner::DartTest) {
        return Some("dart-test");
    }
    let owned = crate::wrappers::effective(words);
    let refs: Vec<&str> = match &owned {
        Some(w) => w.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    if dart_flutter_subcommand(&refs).is_some() && crate::deptree::is_supported(&refs) {
        return Some("pub-deps");
    }
    None
}

fn format_dart_analyze(
    root: &Path,
    words: &[&str],
    text: &str,
    status: i32,
    budget: usize,
) -> Option<(String, Option<crate::summary::Summary>)> {
    let owned = crate::wrappers::effective(words);
    let refs: Vec<&str> = match &owned {
        Some(w) => w.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let (base, _) = dart_flutter_subcommand(&refs)?;
    let label = format!("{base} analyze");
    let mut lint = crate::lint::Lint::for_tool(root, crate::lint::Tool::DartAnalyze);
    for (index, raw) in text.lines().enumerate() {
        let cleaned = if raw.contains('\x1b') {
            crate::capture::ANSI.replace_all(raw, "")
        } else {
            Cow::Borrowed(raw)
        };
        lint.observe(&cleaned, index + 1, crate::capture::Stream::Stdout);
    }
    if status == 0
        && let Some(clean) = lint.clean(&label)
    {
        return Some((clean, None));
    }
    lint.finish(&label, budget, None)
        .map(|(report, summary)| (report, Some(summary)))
}

/// Whether `words` runs a container, Kubernetes, systemd or cloud log stream
/// command eligible for `crate::logview::format` post-run compaction.
fn is_log_command(words: &[&str]) -> bool {
    let tool = crate::filters::unwrap_tool(words);
    let Some((&program, args)) = tool.split_first() else {
        return false;
    };
    if args
        .iter()
        .any(|&a| matches!(a, "-h" | "--help" | "--version"))
    {
        return false;
    }
    let compose_logs = |rest: &[&str]| {
        let rest = skip_global_flags(
            rest,
            &[
                "-f",
                "--file",
                "-p",
                "--project-name",
                "--env-file",
                "--project-directory",
                "--profile",
                "--ansi",
            ],
            &["--compatibility"],
        );
        matches!(rest, ["logs", ..])
    };
    match basename(program) {
        "docker" | "podman" => {
            let args = skip_global_flags(
                args,
                &["-H", "--host", "--context", "-c", "--config"],
                &["-D", "--debug"],
            );
            match args {
                ["logs", ..] | ["container", "logs", ..] => true,
                ["compose", rest @ ..] => compose_logs(rest),
                _ => false,
            }
        }
        "docker-compose" | "podman-compose" => compose_logs(args),
        "kubectl" | "oc" => {
            let args = skip_global_flags(
                args,
                &[
                    "-n",
                    "--namespace",
                    "--context",
                    "--kubeconfig",
                    "--cluster",
                    "--user",
                    "--request-timeout",
                    "-v",
                    "--v",
                ],
                &["--insecure-skip-tls-verify"],
            );
            matches!(args, ["logs", ..])
        }
        "journalctl" => {
            let mut iter = args.iter();
            while let Some(&arg) = iter.next() {
                let format = match arg {
                    "-o" | "--output" => iter.next().copied(),
                    _ => arg
                        .strip_prefix("--output=")
                        .or_else(|| arg.strip_prefix("-o").filter(|v| !v.is_empty())),
                };
                if format.is_some_and(|v| v.starts_with("json") || v == "export") {
                    return false;
                }
            }
            true
        }
        "stern" | "kubetail" => true,
        "pm2" | "heroku" | "fly" | "flyctl" | "railway" => matches!(args, ["logs", ..]),
        "aws" => {
            let args = skip_global_flags(args, &["--profile", "--region", "--endpoint-url"], &[]);
            matches!(args, ["logs", "tail", ..])
                && !args.windows(2).any(|w| {
                    matches!(w[0], "--format" | "--output") && w[1].eq_ignore_ascii_case("json")
                })
                && !args.iter().any(|a| {
                    a.eq_ignore_ascii_case("--format=json")
                        || a.eq_ignore_ascii_case("--output=json")
                })
        }
        _ => false,
    }
}

#[inline]
fn is_structured_log_line(trimmed: &str) -> bool {
    let Some(&first) = trimmed.as_bytes().first() else {
        return false;
    };
    matches!(
        first,
        b'[' | b'0'
            ..=b'9'
                | b'A'
                | b'C'
                | b'D'
                | b'E'
                | b'F'
                | b'I'
                | b'J'
                | b'M'
                | b'N'
                | b'O'
                | b'S'
                | b'T'
                | b'W'
                | b'l'
                | b's'
                | b't'
    ) && STRUCTURED_LOG_LINE.is_match(trimmed)
}

/// Whether `text` is predominantly structured multi-line log output (timestamps,
/// syslog/klog headers, container/compose prefixes, log-level prefixes, or
/// structured JSON log records), suitable for `crate::logview::format`.
fn looks_like_structured_logs(text: &str) -> bool {
    let mut non_empty = 0usize;
    let mut structured = 0usize;
    let is_json_log = |trimmed: &str| {
        trimmed.starts_with('{')
            && trimmed.ends_with('}')
            && [
                "\"level\"",
                "\"severity\"",
                "\"lvl\"",
                "\"msg\"",
                "\"message\"",
                "\"timestamp\"",
                "\"ts\"",
                "\"time\"",
            ]
            .iter()
            .any(|key| trimmed.contains(key))
    };
    for raw in text.lines() {
        let cleaned = if raw.contains('\x1b') {
            crate::capture::ANSI.replace_all(raw, "")
        } else {
            Cow::Borrowed(raw)
        };
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("diff --git ") || trimmed.starts_with("@@ -") {
            return false;
        }
        non_empty += 1;
        let matched = is_json_log(trimmed)
            || is_structured_log_line(trimmed)
            || (trimmed.contains('|')
                && COMPOSE_LOG_LINE.captures(trimmed).is_some_and(|caps| {
                    let body = caps.get(1).map_or("", |m| m.as_str()).trim();
                    is_json_log(body)
                        || is_structured_log_line(body)
                        || body.contains("HTTP/")
                        || ["GET ", "POST ", "PUT ", "DELETE ", "PATCH "]
                            .iter()
                            .any(|m| body.contains(m))
                }));
        if matched {
            structured += 1;
        }
        if non_empty >= 200 {
            break;
        }
    }
    non_empty >= 8 && structured * 2 >= non_empty
}

/// Rebuild the agent run marker footer after replacing the preview body on a
/// complete (untruncated) archive, keeping `preview_bytes + footer_bytes` exact
/// for `record.json` and `stats`.
fn rebuild_footer(run: &crate::capture::Run, preview_bytes: usize) -> String {
    let secrets = run.redaction.as_ref().map_or(0, |found| found.count);
    let redacted = match secrets {
        0 => String::new(),
        1 => " · 1 secret redacted".to_owned(),
        n => format!(" · {n} secrets redacted"),
    };
    let exit = if run.status == 0 {
        String::new()
    } else {
        format!(" · exit {}", run.status)
    };
    format!(
        "[justless {} · {} of {} shown{redacted}{exit}]\n",
        run.id,
        crate::history::human_bytes(preview_bytes as u64),
        crate::history::human_bytes(run.raw_bytes),
    )
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

/// A filter that passes lines of its input through unchanged, so the output keeps
/// the shape of the command before it: whole lines, or with `cut -c1-N` the start of
/// each line.
fn filter(stage: &Stage) -> Option<Filter> {
    if !keeps_stdout(stage) {
        return None;
    }
    let (program, args) = strip(&stage.words)?.split_first()?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let lines = match basename(program) {
        "head" => return count_options(&args, false).map(Filter::Head),
        "tail" => return count_options(&args, true).map(Filter::Tail),
        "sed" => return line_range(&args),
        "cut" => return columns(&args).map(Filter::Columns),
        "grep" | "egrep" | "fgrep" | "rg" => line_filter(&args),
        "sort" => {
            let unique = args
                .iter()
                .any(|arg| *arg == "--unique" || !arg.starts_with("--") && arg.contains('u'));
            return args
                .iter()
                .all(|arg| {
                    arg.starts_with('-') && !arg.starts_with("-o") && !arg.starts_with("--output")
                })
                .then_some(if unique { Filter::Lines } else { Filter::Pass });
        }
        "uniq" => args.iter().all(|arg| {
            matches!(
                *arg,
                "-d" | "-u" | "-i" | "--repeated" | "--unique" | "--ignore-case"
            )
        }),
        "cat" | "less" | "more" => {
            return args.iter().all(|arg| *arg == "-").then_some(Filter::Pass);
        }
        "tee" => {
            return args
                .iter()
                .all(|arg| !arg.starts_with('-') || matches!(*arg, "-a" | "--append"))
                .then_some(Filter::Pass);
        }
        _ => false,
    };
    lines.then_some(Filter::Lines)
}

/// `head` and `tail` options that pick lines or bytes from standard input: the
/// number of lines they keep, or `None` for bytes or, with `tail`, the lines from a
/// numbered one.
fn count_options(args: &[&str], tail: bool) -> Option<Option<usize>> {
    let number = |text: &str| -> Option<Option<usize>> {
        let (from, digits) = match text.strip_prefix('+') {
            Some(digits) if tail => (true, digits),
            _ => (false, text),
        };
        (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
            .then(|| digits.parse().ok().filter(|_| !from))
    };
    // Both print ten lines unless told otherwise.
    let mut kept = Some(10);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (bytes, value) = match *arg {
            "-n" | "--lines" => (false, number(args.next()?)?),
            "-c" | "--bytes" => (true, number(args.next()?)?),
            "-q" | "--quiet" | "--silent" => continue,
            _ => {
                if let Some(value) = arg
                    .strip_prefix("--lines=")
                    .or_else(|| arg.strip_prefix("-n"))
                {
                    (false, number(value)?)
                } else if let Some(value) = arg
                    .strip_prefix("--bytes=")
                    .or_else(|| arg.strip_prefix("-c"))
                {
                    (true, number(value)?)
                } else if let Some(value) = arg.strip_prefix('-').filter(|v| !v.starts_with('+')) {
                    (false, number(value)?)
                } else if tail && arg.starts_with('+') {
                    (false, number(arg)?)
                } else {
                    return None;
                }
            }
        };
        kept = value.filter(|_| !bytes);
    }
    Some(kept)
}

/// A `sed -n` that prints one range of numbered lines of standard input, as
/// `sed -n 'A,Bp'`, `sed -n 'Ap'` or `sed -n 'A,$p'` do.
fn line_range(args: &[&str]) -> Option<Filter> {
    let (mut quiet, mut script) = (false, None);
    let mut args = args.iter();
    while let Some(&arg) = args.next() {
        match arg {
            "-n" | "--quiet" | "--silent" => quiet = true,
            "-e" | "--expression" if script.is_none() => script = Some(*args.next()?),
            "-ne" if script.is_none() => (quiet, script) = (true, Some(*args.next()?)),
            _ if !arg.starts_with('-') && script.is_none() => script = Some(arg),
            _ => return None,
        }
    }
    let script = script?.trim().strip_suffix('p')?.trim_end();
    let number = |text: &str| {
        text.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| text.parse::<usize>().ok())
            .flatten()
            .filter(|&line| line > 0)
    };
    let (first, last) = match script.split_once(',') {
        Some((first, "$")) => (number(first)?, None),
        Some((first, last)) => (number(first)?, Some(number(last)?)),
        None => (number(script)?, Some(number(script)?)),
    };
    (quiet && last.is_none_or(|last| last >= first)).then_some(Filter::Range(first, last))
}

/// The width a `cut -c1-N` or `cut -c-N` keeps of each line. A narrower width could
/// clip the path or name a report reads from the start of a line.
fn columns(args: &[&str]) -> Option<usize> {
    let list = match args {
        ["-c", list] => *list,
        [option] => option
            .strip_prefix("-c")
            .or_else(|| option.strip_prefix("--characters="))?,
        _ => return None,
    };
    let width = list.strip_prefix("1-").or_else(|| list.strip_prefix('-'))?;
    width
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| width.parse::<usize>().ok())
        .flatten()
        .filter(|&width| width >= 40)
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
        if arg.starts_with("--") {
            let (name, has_eq) = match arg.split_once('=') {
                Some((flag, _)) => (flag, true),
                None => (*arg, false),
            };
            if REJECTED.contains(&name) {
                return false;
            }
            if VALUED.contains(&name) && !has_eq && args.next().is_none() {
                return false;
            }
            patterns += usize::from(matches!(name, "--regexp" | "--file"));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The filters of `command`'s one pipeline, after its first command.
    fn filters(command: &str) -> Option<Vec<Filter>> {
        let segments = parse(command)?;
        segments[0][1..].iter().map(filter).collect()
    }

    /// The label of the plan for a line no other rule reads, if it gets one.
    fn loosely(command: &str) -> Option<String> {
        unknown_plan(command).map(|plan| plan.label)
    }

    #[test]
    fn a_line_no_rule_reads_is_previewed_unless_it_prints_file_content() {
        for (command, label) in [
            ("python3 report.py", "python3"),
            ("python3 report.py 2>&1 | tail -40", "python3; …"),
            ("cd tools && X=1 python3 report.py; echo done", "python3; …"),
            ("timeout 60 node build.js | sort | uniq -c", "node; …"),
            ("for i in 1 2 3; do ./bench.sh $i; done", "bench.sh; …"),
            ("if test -f a; then node report.js; fi", "node; …"),
            (
                "python3 - <<'PY'\nprint(open('cat').read())\nPY",
                "python3; …",
            ),
            ("# git log\ncurl -s https://example.test/api", "curl; …"),
            ("jq -r '.items[] | .name' data.json", "jq"),
            ("echo \"$(python3 report.py)\"", "python3; …"),
        ] {
            assert_eq!(loosely(command).as_deref(), Some(label), "{command}");
        }
        // A program a report is made for is recorded as `sh`, so a line that
        // mixes commands gets no report meant for one command's lines.
        let recorded = |command: &str| unknown_plan(command).map(|plan| plan.words.join(" "));
        assert_eq!(
            recorded("cd web && eslint -f json . | python3 tally.py").as_deref(),
            Some("sh")
        );
        assert_eq!(
            recorded("cd tools && python3 report.py; echo done").as_deref(),
            Some("python3")
        );
        for command in [
            // A reader as a word anywhere outside single quotes.
            "python3 report.py; cat report.txt",
            "for f in a b; do sed -n 1,40p $f; done",
            "python3 list.py | xargs cat",
            "while read line; do echo \"$line\"; done < log.txt",
            "echo \"$(cat notes.md)\"",
            "docker exec app cat /etc/app.conf",
            "python3 tool.py && git stash list",
            "python3 a.py > out.txt; diff out.txt want.txt",
            "python3 report.py | cut -c1-80 | cat",
            // A command a hook supports, a search or a listing starts a pipeline.
            "cargo test 2>&1 | python3 filter.py",
            "for f in src/*; do grep -n needle $f; done",
            "find . -name '*.py' | xargs wc -l",
            "(cd web && ls -la)",
            // A shell given its commands as text, which may read a file.
            "bash -c 'cat a.rs | grep x'",
            "cd web && sh -ec 'npm run report'",
            "ssh host 'uptime' && eval \"$CMD\"",
            // Justless itself, and lines of notes alone.
            "./target/debug/justless stats",
            "echo one; printf two",
            "cd src && true",
        ] {
            assert_eq!(loosely(command), None, "{command}");
        }
    }

    #[test]
    fn display_filters_say_which_lines_they_keep() {
        use Filter::*;
        for (command, kept) in [
            ("x | head", Some(vec![Head(Some(10))])),
            (
                "x | head -n 40 | tail -5",
                Some(vec![Head(Some(40)), Tail(Some(5))]),
            ),
            (
                "x | head -c 300 | tail -n +3",
                Some(vec![Head(None), Tail(None)]),
            ),
            ("x | sed -n 89,236p", Some(vec![Range(89, Some(236))])),
            ("x | sed -n '7p' | cat", Some(vec![Range(7, Some(7)), Pass])),
            ("x | sed -ne '3,$p'", Some(vec![Range(3, None)])),
            (
                "x | cut -c1-160 | sort | sort -u",
                Some(vec![Columns(160), Pass, Lines]),
            ),
            (
                "x | grep -v '^ ' | cut -c-120",
                Some(vec![Lines, Columns(120)]),
            ),
            ("x | sed -n 9,3p", None),
            ("x | sed -n 0,3p", None),
            ("x | sed 1,3p", None),
            ("x | sed -n '1,3p;5p'", None),
            ("x | sed -n /re/,/end/p", None),
            ("x | sed -n 1,3p file", None),
            ("x | cut -c5-80", None),
            ("x | cut -c1-20", None),
            ("x | cut -f1 -d:", None),
            ("x | head -n 4 file", None),
            ("x | awk 'NR<9'", None),
        ] {
            assert_eq!(filters(command), kept, "{command}");
        }
    }

    #[test]
    fn a_window_that_reached_its_limit_may_have_cut_the_output() {
        // The pipeline, the lines it passed on, and whether it may have cut the
        // output's start and end.
        for (command, lines, cut) in [
            ("x | head -n 200", 120, (false, false)),
            ("x | head -n 200", 198, (false, true)),
            ("x | head -c 900", 3, (false, true)),
            ("x | tail -40", 40, (true, false)),
            ("x | tail -n +5", 3, (true, false)),
            ("x | sed -n 1,400p | cat", 120, (false, false)),
            ("x | sed -n 9,400p", 120, (true, false)),
            ("x | sed -n 1,400p | grep -v '^ '", 120, (false, true)),
            ("x | sed -n 1,10p", 10, (false, true)),
            ("x | grep fn | sort", 5000, (false, false)),
        ] {
            let plan = Plan {
                words: Vec::new(),
                command_words: Vec::new(),
                program: String::new(),
                label: String::new(),
                cd: PathBuf::new(),
                filters: filters(command)
                    .unwrap()
                    .into_iter()
                    .map(|filter| (filter, String::new()))
                    .collect(),
                later: Vec::new(),
                profile: None,
            };
            assert_eq!(
                plan.cut(lines),
                (cut.0, cut.1, cut.1),
                "{command} with {lines} lines"
            );
        }
    }

    fn test_workspace(config: &str) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join(".justless.toml"), config).unwrap();
        let ws = Workspace {
            state: root.join(".justless"),
            origin: root.clone(),
            cwd: root.clone(),
            root,
        };
        (dir, ws)
    }

    fn post_bash(ws: &Workspace, command: &str, stdout: &str) -> Option<String> {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "session_id": "unit-postroute",
            "cwd": ws.root,
            "tool_input": { "command": command },
            "tool_response": {
                "stdout": stdout,
                "stderr": "",
                "interrupted": false,
                "isImage": false,
            }
        });
        claude_bash(ws, &input)?["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .map(str::to_owned)
    }

    fn run_id_from(shown: &str) -> String {
        shown
            .rsplit("[justless ")
            .next()
            .and_then(|tail| tail.split([' ', ']']).next())
            .unwrap_or_else(|| panic!("missing run marker in:\n{shown}"))
            .to_owned()
    }

    fn assert_archive_exact_and_accounted(ws: &Workspace, shown: &str, raw: &str) {
        let id = run_id_from(shown);
        let dir = ws.state.join("runs").join(&id);
        let archived = fs::read(dir.join("stdout")).unwrap();
        assert_eq!(archived, raw.as_bytes(), "exact archive replay mismatch");
        let record: Value =
            serde_json::from_slice(&fs::read(dir.join("record.json")).unwrap()).unwrap();
        let accounted =
            record["preview_bytes"].as_u64().unwrap() + record["footer_bytes"].as_u64().unwrap();
        assert_eq!(
            accounted,
            shown.len() as u64,
            "record.json bytes ({accounted}) != shown len ({})\n{record:#}",
            shown.len()
        );
        let total_lines = raw.lines().count();
        for prefix in ["[stdout:", "in stdout:"] {
            for span in shown.split(prefix).skip(1) {
                let range = span.split(']').next().unwrap();
                let (a, b) = range.split_once('-').unwrap_or((range, range));
                let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                assert!(
                    1 <= a && a <= b && b <= total_lines,
                    "pointer {prefix}{range}] out of bounds (1..={total_lines})\n{shown}"
                );
            }
        }
    }

    #[test]
    fn classifiers_recognize_devops_inventories_and_log_commands() {
        assert_eq!(
            extra_inventory_kind(&["kubectl", "-n", "prod", "get", "events"]),
            Some(ExtraInventory::Events)
        );
        assert_eq!(
            extra_inventory_kind(&["kubectl", "get", "ev", "-A"]),
            Some(ExtraInventory::Events)
        );
        assert_eq!(
            extra_inventory_kind(&["oc", "--context=prod", "get", "svc"]),
            Some(ExtraInventory::Resource("svc"))
        );
        assert_eq!(
            extra_inventory_kind(&["kubectl", "get", "ingresses.networking.k8s.io"]),
            Some(ExtraInventory::Resource("ingresses.networking.k8s.io"))
        );
        assert_eq!(
            extra_inventory_kind(&["kubectl", "get", "pvc", "-A"]),
            Some(ExtraInventory::Resource("pvc"))
        );
        assert_eq!(
            extra_inventory_kind(&["kubectl", "get", "cronjobs"]),
            Some(ExtraInventory::Resource("cronjobs"))
        );
        assert_eq!(
            extra_inventory_kind(&["helm", "-n", "apps", "ls", "-a"]),
            Some(ExtraInventory::HelmList)
        );
        assert_eq!(
            extra_inventory_kind(&["docker", "image", "list"]),
            Some(ExtraInventory::Images)
        );
        assert_eq!(
            extra_inventory_kind(&["podman", "images"]),
            Some(ExtraInventory::Images)
        );
        // Machine formats, watches, and quiet modes stay out of table inventories.
        for rejected in [
            &["kubectl", "get", "events", "-o", "json"][..],
            &["kubectl", "get", "events", "-ojson"][..],
            &["kubectl", "get", "svc", "--output=yaml"][..],
            &["kubectl", "get", "events", "-w"][..],
            &["kubectl", "get", "svc", "--no-headers"][..],
            &["helm", "ls", "-o", "json"][..],
            &["helm", "list", "-q"][..],
            &["docker", "images", "--format", "{{json .}}"][..],
            &["podman", "images", "-q"][..],
        ] {
            assert_eq!(extra_inventory_kind(rejected), None, "{rejected:?}");
        }

        // Log command recognition, including flags before subcommands.
        for log_cmd in [
            &["docker", "logs", "api"][..],
            &["docker", "compose", "-f", "compose.prod.yml", "logs", "api"][..],
            &["docker-compose", "-p", "shop", "logs", "--tail=500"][..],
            &["podman", "container", "logs", "web"][..],
            &["kubectl", "-n", "prod", "logs", "deploy/api"][..],
            &["oc", "-n", "prod", "logs", "pod/api-1"][..],
            &["journalctl", "-u", "nginx", "-n", "500"][..],
            &["aws", "logs", "tail", "/aws/lambda/fn"][..],
            &["stern", "api-"][..],
        ] {
            assert!(is_log_command(log_cmd), "{log_cmd:?}");
        }
        for not_log in [
            &["journalctl", "-o", "json"][..],
            &["journalctl", "--output=json-pretty"][..],
            &["aws", "logs", "get-log-events", "--log-group-name", "g"][..],
            &["aws", "logs", "tail", "g", "--format", "json"][..],
            &["docker", "ps"][..],
            &["kubectl", "get", "pods"][..],
        ] {
            assert!(!is_log_command(not_log), "{not_log:?}");
        }
    }

    #[test]
    fn post_run_logview_compacts_container_and_unrouted_logs_with_redaction() {
        let (_dir, ws) = test_workspace("");
        let mut compose_logs = String::new();
        for i in 0..180 {
            compose_logs.push_str(&format!(
                "api-1  | 2026-11-25T10:00:{:02}.{:03}Z INFO  GET /api/orders/{i} 200 1{}ms req=req-{i:04}\n",
                i % 60,
                i % 1000,
                i % 9
            ));
        }
        compose_logs.push_str(
            "api-1  | 2026-11-25T10:03:01.000Z ERROR database connection refused host=db-primary:5432\n",
        );
        for i in 180..260 {
            compose_logs.push_str(&format!(
                "api-1  | 2026-11-25T10:04:{:02}.{:03}Z INFO  GET /api/orders/{i} 200 1{}ms req=req-{i:04}\n",
                i % 60,
                i % 1000,
                i % 9
            ));
        }

        // 1. Docker compose with `-f` before `logs` (not matched by filters::profile).
        let shown = post_bash(
            &ws,
            "docker compose -f compose.prod.yml logs api",
            &compose_logs,
        )
        .expect("compose logs compacted via logview");
        assert!(shown.contains("database connection refused"), "{shown}");
        assert!(shown.contains("[stdout:181]"), "{shown}");
        assert!(shown.len() * 4 < compose_logs.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &compose_logs);

        // 2. Unrouted `python3 server.py` emitting noisy timestamped multi-line logs.
        let mut app_logs = String::new();
        for i in 0..220 {
            app_logs.push_str(&format!(
                "2026-11-25T11:00:{:02}.{:03}Z INFO worker processed job_id=job-{i:04} latency={}ms\n",
                i % 60,
                i % 1000,
                5 + (i % 7)
            ));
        }
        app_logs.push_str(
            "2026-11-25T11:04:01.100Z ERROR worker failed to charge card: gateway timeout\n",
        );
        let shown = post_bash(&ws, "python3 server.py", &app_logs)
            .expect("unrouted structured logs compacted via logview");
        assert!(shown.contains("gateway timeout"), "{shown}");
        assert!(shown.contains("[stdout:221]"), "{shown}");
        assert!(!shown.contains("[output clipped"), "{shown}");
        assert!(shown.len() * 4 < app_logs.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &app_logs);

        // 3. Redaction safety: secret in a log line is never shown, archive keeps raw bytes.
        let token = format!("{}{}{}", "gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let secret_logs =
            app_logs.replace("gateway timeout", &format!("gateway timeout auth={token}"));
        let shown = post_bash(&ws, "python3 server.py", &secret_logs)
            .expect("secret-bearing logs compacted");
        assert!(
            !shown.contains(&token),
            "secret leaked into preview: {shown}"
        );
        assert!(shown.contains("[redacted:"), "{shown}");
        assert!(shown.contains("secret redacted"), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &secret_logs);
    }

    #[test]
    fn post_run_devops_inventories_compact_events_helm_images_and_resources() {
        let (_dir, ws) = test_workspace("");

        // 1. `kubectl -n prod get events | head -n 500` (global flag before `get`).
        let mut events =
            String::from("LAST SEEN   TYPE      REASON      OBJECT             MESSAGE\n");
        for i in 0..140 {
            events.push_str(&format!(
                "{}m          Normal    Scheduled   pod/web-{i:03}        Successfully assigned prod/web-{i:03} to node-{}\n",
                i % 59 + 1,
                i % 4
            ));
        }
        events.push_str(
            "2m          Warning   BackOff     pod/checkout-7     Back-off restarting failed container api\n",
        );
        let shown = post_bash(&ws, "kubectl -n prod get events | head -n 500", &events)
            .expect("kubectl events compacted");
        assert!(shown.contains("BackOff"), "{shown}");
        assert!(shown.contains("pod/checkout-7"), "{shown}");
        assert!(shown.contains("Normal"), "{shown}");
        assert!(shown.len() * 4 < events.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &events);

        // Redaction safety inside Warning event message.
        let token = format!("{}{}{}", "gh", "p_", "Z9y8X7w6V5u4T3s2R1q0P9o8N7m6L5k4J3i2");
        let secret_events = events.replace(
            "restarting failed container api",
            &format!("token={token} failed container api"),
        );
        let shown = post_bash(&ws, "kubectl -n prod get events", &secret_events)
            .expect("secret-bearing events compacted");
        assert!(!shown.contains(&token), "secret leaked: {shown}");
        assert!(shown.contains("[redacted:"), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &secret_events);

        // 2. `helm ls -A | head -n 500` (`ls` alias not in known::ROUTED).
        let mut helm =
            String::from("NAME\tNAMESPACE\tREVISION\tUPDATED\tSTATUS\tCHART\tAPP VERSION\n");
        for i in 0..120 {
            helm.push_str(&format!(
                "svc-{i:03}\tprod\t3\t2026-11-25 10:00:00 +0000 UTC\tdeployed\tservice-1.2.0\t1.2.0\n"
            ));
        }
        helm.push_str(
            "billing\tprod\t7\t2026-11-25 10:05:00 +0000 UTC\tfailed\tbilling-2.0.1\t2.0.1\n",
        );
        let shown = post_bash(&ws, "helm ls -A | head -n 500", &helm).expect("helm ls compacted");
        assert!(shown.contains("billing"), "{shown}");
        assert!(shown.contains("failed"), "{shown}");
        assert!(shown.len() * 4 < helm.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &helm);

        // 3. `docker image list | head -n 500` (`image list` alias).
        let mut images =
            String::from("REPOSITORY              TAG       IMAGE ID       CREATED        SIZE\n");
        for i in 0..120 {
            images.push_str(&format!(
                "<none>                  <none>    {:012x}   {} days ago    48.2MB\n",
                0xabc000000000u64 + i as u64,
                i % 30 + 1
            ));
        }
        for i in 0..15 {
            images.push_str(&format!(
                "ghcr.io/acme/api        v1.0.{i}    {:012x}   2 days ago     120MB\n",
                0xdef000000000u64 + i as u64
            ));
        }
        let shown = post_bash(&ws, "docker image list | head -n 500", &images)
            .expect("docker images compacted");
        assert!(
            shown.contains("dangling") || shown.contains("<none>"),
            "{shown}"
        );
        assert!(shown.contains("ghcr.io/acme/api"), "{shown}");
        assert!(shown.len() * 4 < images.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &images);

        // 4. `kubectl get pvc -A | head -n 500` (`inventory::resources`).
        let mut pvcs = String::from(
            "NAMESPACE   NAME        STATUS    VOLUME      CAPACITY   ACCESS MODES   STORAGECLASS   AGE\n",
        );
        for i in 0..120 {
            pvcs.push_str(&format!(
                "prod        data-{i:03}    Bound     pv-{i:04}     50Gi       RWO            gp3            12d\n"
            ));
        }
        pvcs.push_str(
            "prod        wal-000     Pending                                         gp3            4m\n",
        );
        let shown = post_bash(&ws, "kubectl get pvc -A | head -n 500", &pvcs)
            .expect("kubectl pvc compacted");
        assert!(shown.contains("wal-000"), "{shown}");
        assert!(shown.contains("Pending"), "{shown}");
        assert!(shown.len() * 4 < pvcs.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &pvcs);
    }

    #[test]
    fn post_run_stacktrace_collapses_external_frames_and_keeps_workspace_verbatim() {
        let (_dir, ws) = test_workspace("");
        let root_str = ws.root.display().to_string();
        let mut trace = String::from("Traceback (most recent call last):\n");
        trace.push_str(&format!(
            "  File \"{root_str}/src/cli.py\", line 14, in main\n    run_server()\n"
        ));
        for i in 0..70 {
            trace.push_str(&format!(
                "  File \"/usr/lib/python3.12/site-packages/uvicorn/middleware/layer_{i:02}.py\", line {}, in dispatch_{i}\n    return await self.app(scope, receive, send)\n",
                10 + i
            ));
        }
        trace.push_str(&format!(
            "  File \"{root_str}/src/checkout.py\", line 88, in compute_total\n    raise ValueError(\"negative subtotal\")\n"
        ));
        trace.push_str("ValueError: negative subtotal\n");
        assert!(trace.len() > 8_000, "trace len = {}", trace.len());

        let shown =
            post_bash(&ws, "python3 src/cli.py", &trace).expect("traceback-heavy output compacted");
        assert!(shown.contains("src/cli.py"), "{shown}");
        assert!(shown.contains("src/checkout.py"), "{shown}");
        assert!(shown.contains("ValueError: negative subtotal"), "{shown}");
        assert!(shown.contains("in stdout:4-143]"), "{shown}");
        assert!(
            !shown.contains("[output clipped. Full log retained.]"),
            "should collapse frames without clipping: {shown}"
        );
        assert!(shown.len() * 4 < trace.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &trace);

        // Redaction safety in stacktrace output.
        let token = format!("{}{}{}", "gh", "p_", "Q1w2E3r4T5y6U7i8O9p0A1s2D3f4G5h6J7k8");
        let secret_trace = trace.replace("negative subtotal", &format!("bad token {token}"));
        let shown = post_bash(&ws, "python3 src/cli.py", &secret_trace)
            .expect("secret-bearing trace compacted");
        assert!(!shown.contains(&token), "secret leaked: {shown}");
        assert!(shown.contains("[redacted:"), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &secret_trace);
    }

    #[test]
    fn post_run_deep_json_payloads_compact_inline_and_persisted() {
        let (_dir, ws) = test_workspace("");
        let items: Vec<String> = (0..180)
            .map(|i| {
                format!(
                    r#"{{"id":{i},"status":"active","owner":{{"login":"dev{}","role":"member"}},"tags":[{{"name":"backend","priority":1}}]}}"#,
                    i % 4
                )
            })
            .collect();
        let payload = format!("[{}]\n", items.join(","));
        assert!(payload.len() > 15_000);

        for command in [
            "curl -s https://api.example.com/v1/items",
            "gh api repos/acme/widget/items",
            "aws ec2 describe-instances",
            "kubectl get pods -o json | jq '.items'",
        ] {
            let shown = post_bash(&ws, command, &payload)
                .unwrap_or_else(|| panic!("{command} should compact via jsonview"));
            assert!(
                shown.starts_with("[JSON array as a table:"),
                "{command}: {shown}"
            );
            assert!(shown.contains("{..} nested"), "{command}: {shown}");
            assert!(shown.len() * 2 < payload.len(), "{command}: {shown}");
            assert_archive_exact_and_accounted(&ws, &shown, &payload);
        }

        // Persisted large JSON payload (>30 KB) from an unknown command compacts inline
        // when jsonview fits inside Claude Code's 2,000-character preview.
        let results = ws.root.join("tool-results");
        fs::create_dir_all(&results).unwrap();
        let big_items: Vec<String> = (0..40)
            .map(|i| {
                let pad = " ".repeat(700);
                format!(
                    "  {{\n{pad}\"id\": {i},\n{pad}\"status\": \"ok\",\n{pad}\"meta\": {{\"team\": \"core\", \"tier\": 1}}\n  }}"
                )
            })
            .collect();
        let big_json = format!("[\n{}\n]\n", big_items.join(",\n"));
        assert!(big_json.len() > 30_000);
        let saved = results.join("gh-api.txt");
        fs::write(&saved, &big_json).unwrap();
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "session_id": "unit-persisted-json",
            "cwd": ws.root,
            "tool_input": { "command": "gh api repos/acme/widget/items" },
            "tool_response": {
                "stdout": &big_json[..2000],
                "stderr": "",
                "interrupted": false,
                "isImage": false,
                "persistedOutputPath": saved,
                "persistedOutputSize": big_json.len(),
            }
        });
        let reply = claude_bash(&ws, &input).expect("persisted JSON compacted");
        let updated = &reply["hookSpecificOutput"]["updatedToolOutput"];
        assert!(updated.get("persistedOutputPath").is_none());
        let shown = updated["stdout"].as_str().unwrap();
        assert!(shown.starts_with("[JSON array as a table:"), "{shown}");
        assert!(shown.len() <= PREVIEW_CHARS, "len={}: {shown}", shown.len());

        // Persisted unstructured unknown output still stays untouched (returns None).
        let unstructured: String = (0..600)
            .map(|i| format!("row-{i:04} payload-alpha-{i} payload-beta-{i} payload-gamma-{i}\n"))
            .collect();
        let saved_plain = results.join("plain.txt");
        fs::write(&saved_plain, &unstructured).unwrap();
        let plain_input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "session_id": "unit-persisted-plain",
            "cwd": ws.root,
            "tool_input": { "command": "python3 dump.py" },
            "tool_response": {
                "stdout": &unstructured[..2000],
                "stderr": "",
                "interrupted": false,
                "isImage": false,
                "persistedOutputPath": saved_plain,
                "persistedOutputSize": unstructured.len(),
            }
        });
        assert!(claude_bash(&ws, &plain_input).is_none());
    }

    #[test]
    fn post_run_cli_views_compact_kubectl_describe_gh_audits_and_tf_state() {
        let (_dir, ws) = test_workspace("");

        // 1. `kubectl -n prod describe pod ...` (including flag before subcommand & redaction safety)
        let last_applied = format!(
            "{{\"apiVersion\":\"v1\",\"spec\":{{\"env\":[{}]}}}}",
            (0..60)
                .map(|i| format!("{{\"name\":\"K_{i}\",\"value\":\"v-{i:04}\"}}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut describe = format!(
            "\
Name:             api-7f8d9c6b5-abcde
Namespace:        prod
Node:             worker-01/10.0.0.12
Annotations:      kubectl.kubernetes.io/last-applied-configuration: {last_applied}
                  prometheus.io/scrape: true
Status:           Running
Controlled By:    ReplicaSet/api-7f8d9c6b5
Containers:
  app:
    Image:          ghcr.io/acme/api:v2
    State:          Waiting
      Reason:       CrashLoopBackOff
    Last State:     Terminated
      Reason:       OOMKilled
      Exit Code:    137
    Ready:          False
    Restart Count:  5
    Environment:
"
        );
        for i in 0..45 {
            describe.push_str(&format!("      APP_VAR_{i:02}:  value-{i:04}\n"));
        }
        describe.push_str(
            "\
Conditions:
  Type              Status
  Initialized       True 
  Ready             False 
  PodScheduled      True 
Events:
  Type     Reason     Age   From               Message
  ----     ------     ----  ----               -------
  Normal   Scheduled  10m   default-scheduler  Successfully assigned prod/api-7f8d9c6b5-abcde
  Warning  BackOff    1m    kubelet            Back-off restarting failed container app
",
        );
        assert!(describe.len() > 4_096);
        let shown = post_bash(
            &ws,
            "kubectl -n prod describe pod api-7f8d9c6b5-abcde",
            &describe,
        )
        .expect("kubectl describe compacted via postroute");
        assert!(shown.contains("CrashLoopBackOff"), "{shown}");
        assert!(shown.contains("OOMKilled"), "{shown}");
        assert!(shown.contains("Warning  BackOff"), "{shown}");
        assert!(
            shown.contains("Annotations: 2 entries (includes last-applied-configuration)"),
            "{shown}"
        );
        assert!(
            !shown.contains("[output clipped. Full log retained.]"),
            "{shown}"
        );
        assert!(shown.len() * 3 < describe.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &describe);

        // Redaction safety inside kubectl describe Warning events.
        let token = format!("{}{}{}", "gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let secret_describe = describe.replace(
            "Back-off restarting failed container app",
            &format!("Back-off restarting failed container app token={token}"),
        );
        let shown = post_bash(
            &ws,
            "kubectl -n prod describe pod api-7f8d9c6b5-abcde",
            &secret_describe,
        )
        .expect("secret-bearing kubectl describe compacted");
        assert!(!shown.contains(&token), "secret leaked: {shown}");
        assert!(shown.contains("[redacted:"), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &secret_describe);

        // 2. `gh pr checks`
        let mut checks = String::new();
        for i in 1..=60 {
            checks.push_str(&format!(
                "ci / job-check-runner-{i:02}\tpass\t{}s\thttps://github.com/acme/repo/actions/runs/77/job/{i}\n",
                10 + i
            ));
        }
        checks.push_str(
            "ci / unit-tests\tfail\t2m10s\thttps://github.com/acme/repo/actions/runs/77/job/61\n",
        );
        assert!(checks.len() > 4_096);
        let shown = post_bash(&ws, "gh pr checks 42", &checks)
            .expect("gh pr checks compacted via postroute");
        assert!(shown.contains("60 passed [stdout:1-60]"), "{shown}");
        assert!(shown.contains("ci / unit-tests  fail"), "{shown}");
        assert!(shown.len() * 8 < checks.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &checks);

        // 3. `pnpm audit` box-art dump
        let mut audit = String::new();
        for i in 0..8 {
            audit.push_str(&format!(
                "\
┌───────────────┬──────────────────────────────────────────────────────────────┐
│ high          │ Prototype Pollution in lodash                                │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Package       │ lodash                                                       │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Patched in    │ >=4.17.21                                                    │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Dependency of │ pkg-{i:02}                                                       │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ More info     │ https://github.com/advisories/GHSA-jf85-cpcp-j695            │
└───────────────┴──────────────────────────────────────────────────────────────┘
"
            ));
        }
        audit.push_str("8 vulnerabilities found\n");
        assert!(audit.len() > 8_000);
        let shown = post_bash(&ws, "pnpm audit", &audit).expect("pnpm audit compacted");
        assert!(
            shown.contains("[high] lodash (GHSA-jf85-cpcp-j695)"),
            "{shown}"
        );
        assert!(shown.contains("(×8 paths)"), "{shown}");
        assert!(shown.len() * 15 < audit.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &audit);

        // 4. `terraform state list`
        let mut tf_state = String::new();
        for i in 0..120 {
            tf_state.push_str(&format!("module.network.aws_subnet.private[{i}]\n"));
        }
        for i in 0..40 {
            tf_state.push_str(&format!("aws_instance.worker[{i}]\n"));
        }
        assert!(tf_state.len() > 4_096);
        let shown = post_bash(&ws, "terraform state list", &tf_state)
            .expect("terraform state list compacted");
        assert!(
            shown.contains("module.network.aws_subnet ×120 [stdout:1-120]"),
            "{shown}"
        );
        assert!(
            shown.contains("aws_instance ×40 [stdout:121-160]"),
            "{shown}"
        );
        assert!(shown.len() * 15 < tf_state.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &tf_state);
    }

    #[test]
    fn post_run_cloud_views_compact_kube_diff_helm_s3_and_cloud_tables() {
        let (_dir, ws) = test_workspace("");

        // 1. `kubectl -n prod diff -f k8s/` with metadata noise + redaction safety.
        let mut diff = String::from(
            "\
diff -u -N /tmp/LIVE-1/apps.v1.Deployment.prod.api /tmp/MERGED-1/apps.v1.Deployment.prod.api
--- /tmp/LIVE-1/apps.v1.Deployment.prod.api\t2026-01-15 10:00:00 +0000
+++ /tmp/MERGED-1/apps.v1.Deployment.prod.api\t2026-01-15 10:00:00 +0000
@@ -1,120 +1,120 @@
 apiVersion: apps/v1
 kind: Deployment
 metadata:
   managedFields:
   - apiVersion: apps/v1
     fieldsType: FieldsV1
     fieldsV1:
",
        );
        for i in 0..160 {
            diff.push_str(&format!(
                "       f:noise-managed-field-{i:03}: {{\"f:subkey\": {{}}}}\n"
            ));
        }
        diff.push_str(concat!(
            "   name: api\n",
            "   namespace: prod\n",
            "-  resourceVersion: \"100\"\n",
            "+  resourceVersion: \"101\"\n",
            " spec:\n",
            "-  replicas: 2\n",
            "+  replicas: 4\n",
            "   template:\n",
            "     spec:\n",
            "       containers:\n",
            "       - name: api\n",
            "-        image: ghcr.io/acme/api:1.4.1\n",
            "+        image: ghcr.io/acme/api:1.4.2\n",
        ));
        assert!(diff.len() > 4_096);
        let shown = post_bash(&ws, "kubectl -n prod diff -f k8s/", &diff)
            .expect("kubectl diff compacted via postroute");
        assert!(shown.contains("+  replicas: 4"), "{shown}");
        assert!(
            shown.contains("+        image: ghcr.io/acme/api:1.4.2"),
            "{shown}"
        );
        assert!(shown.contains("folded metadata:"), "{shown}");
        assert!(!shown.contains("f:noise-managed-field-050"), "{shown}");
        assert!(shown.len() * 4 < diff.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &diff);

        // Redaction safety inside `kubectl diff` real change line.
        let token = format!("{}{}{}", "gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let secret_diff = diff.replace(
            "+        image: ghcr.io/acme/api:1.4.2",
            &format!("+        image: ghcr.io/acme/api:1.4.2\n+        token: {token}"),
        );
        let shown = post_bash(&ws, "kubectl -n prod diff -f k8s/", &secret_diff)
            .expect("secret-bearing kubectl diff compacted");
        assert!(!shown.contains(&token), "secret leaked: {shown}");
        assert!(shown.contains("[redacted:"), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &secret_diff);

        // 2. `helm template` (not pre-routed in known::ROUTED, compacted post-run).
        let mut helm = String::new();
        for i in 1..=8 {
            helm.push_str(&format!(
                "\
---
# Source: app/templates/deployment-{i}.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api-{i:02}
  namespace: prod
spec:
  replicas: 3
  template:
    spec:
      containers:
        - name: app
          image: ghcr.io/acme/api-{i:02}:2.0.0
          env:
            - name: ENV_A
              value: val-a
            - name: ENV_B
              value: val-b
            - name: ENV_C
              value: val-c
---
# Source: app/templates/configmap-{i}.yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: cfg-{i:02}
  namespace: prod
data:
  K1: v1
  K2: v2
  K3: v3
  K4: v4
  K5: v5
"
            ));
        }
        assert!(helm.len() > 4_096);
        let shown = post_bash(&ws, "helm template app ./charts/app", &helm)
            .expect("helm template compacted via postroute");
        assert!(
            shown.contains("Deployment/api-01 (3 replicas, image=ghcr.io/acme/api-01:2.0.0)"),
            "{shown}"
        );
        assert!(shown.contains("ConfigMap/cfg-01 (5 keys)"), "{shown}");
        assert!(shown.len() * 4 < helm.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &helm);

        // 3. `aws s3 ls --recursive` (not pre-routed in known::ROUTED, compacted post-run).
        let mut s3 = String::from("                           PRE archive/\n");
        for i in 1..=120 {
            s3.push_str(&format!(
                "2026-01-15 10:22:{:02}    {:>9} warehouse/events/part-{i:05}.parquet\n",
                i % 60,
                262_144 + i * 512
            ));
        }
        assert!(s3.len() > 6_000);
        let shown = post_bash(&ws, "aws s3 ls s3://acme-data/ --recursive", &s3)
            .expect("aws s3 ls compacted via postroute");
        assert!(shown.contains("PRE archive/ [stdout:1]"), "{shown}");
        assert!(shown.contains("warehouse/events/"), "{shown}");
        assert!(shown.contains(".parquet ×120"), "{shown}");
        assert!(shown.len() * 15 < s3.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &s3);

        // 4. `gcloud compute instances list` (unhealthy pinned, healthy grouped).
        let mut gce = String::from(
            "NAME                 ZONE           MACHINE_TYPE   PREEMPTIBLE  INTERNAL_IP   EXTERNAL_IP    STATUS\n",
        );
        for i in 1..=50 {
            gce.push_str(&format!(
                "web-prod-{i:02}          us-central1-a  e2-standard-4               10.128.0.{:<3}  34.120.1.{:<3}   RUNNING\n",
                i + 1,
                i + 10
            ));
        }
        gce.push_str(
            "worker-dead-01       us-central1-b  n2-standard-8               10.128.1.10   34.120.2.10    TERMINATED\n",
        );
        assert!(gce.len() > 4_096);
        let shown = post_bash(&ws, "gcloud compute instances list", &gce)
            .expect("gcloud compute instances list compacted via postroute");
        assert!(shown.contains("worker-dead-01"), "{shown}");
        assert!(shown.contains("TERMINATED"), "{shown}");
        assert!(
            shown.contains("RUNNING (us-central1-a, e2-standard-4) ×50"),
            "{shown}"
        );
        assert!(shown.len() * 8 < gce.len(), "{shown}");
        assert_archive_exact_and_accounted(&ws, &shown, &gce);
    }

    #[test]
    fn post_run_flutter_dart_and_nonzero_exit_compaction_preserve_signals() {
        let (_dir, ws) = test_workspace("");
        let post_bash_with_exit = |command: &str, stdout: &str, exit_code: i32| -> Option<Value> {
            let input = json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "session_id": "unit-flutter-exit",
                "cwd": ws.root,
                "tool_input": { "command": command },
                "tool_response": {
                    "stdout": stdout,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                    "exitCode": exit_code,
                }
            });
            claude_bash(&ws, &input)
        };

        // 1. `flutter test` with non-zero exitCode: preserves failure detail,
        //    exitCode in updatedToolOutput, `· exit 1` footer, and exact archive.
        let mut flutter_test = String::new();
        for i in 1..=140 {
            flutter_test.push_str(&format!(
                "00:{:02} +{i}: test/widgets/item_{i:03}_test.dart: widget_{i:03} renders cleanly\n",
                i % 60
            ));
        }
        flutter_test.push_str(
            "\
00:25 +140 -1: test/checkout_test.dart: rejects negative totals [E]
  Expected: <42>
    Actual: <-1>
  test/checkout_test.dart 28:7  main.<fn>
  package:flutter_test/src/widget_tester.dart 145:5  WidgetTester.pumpWidget
00:26 +140 -1: Some tests failed.
",
        );
        assert!(flutter_test.len() > 6_000);
        let reply = post_bash_with_exit("flutter test", &flutter_test, 1)
            .expect("failing flutter test compacted");
        let updated = &reply["hookSpecificOutput"]["updatedToolOutput"];
        assert_eq!(updated["exitCode"], 1);
        let shown = updated["stdout"].as_str().unwrap();
        assert!(shown.contains("rejects negative totals"), "{shown}");
        assert!(shown.contains("Expected: <42>"), "{shown}");
        assert!(shown.contains("· exit 1]"), "{shown}");
        assert!(shown.len() * 3 < flutter_test.len(), "{shown}");
        let id = run_id_from(shown);
        let rec: Value = serde_json::from_slice(
            &fs::read(ws.state.join("runs").join(&id).join("record.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(rec["status"], 1);
        assert_archive_exact_and_accounted(&ws, shown, &flutter_test);

        // 2. `dart analyze` & `flutter --no-color analyze` with non-zero exit
        //    and pipe: groups findings by rule and keeps `· exit 1`.
        let mut analyze = String::from("Analyzing app...\n\n");
        for i in 1..=90 {
            analyze.push_str(&format!(
                "   info • Prefer const with constant constructors • lib/ui/card_{i:02}.dart:{}:12 • prefer_const_constructors\n",
                10 + i
            ));
        }
        analyze.push_str(
            "  error • The argument type 'String' can't be assigned to 'int' • lib/billing/total.dart:44:19 • argument_type_not_assignable\n\n91 issues found. (ran in 2.4s)\n",
        );
        assert!(analyze.len() > 6_000);
        for cmd in ["dart analyze | head -n 500", "flutter --no-color analyze"] {
            let reply = post_bash_with_exit(cmd, &analyze, 1)
                .unwrap_or_else(|| panic!("{cmd} should compact"));
            let updated = &reply["hookSpecificOutput"]["updatedToolOutput"];
            assert_eq!(updated["exitCode"], 1, "{cmd}");
            let shown = updated["stdout"].as_str().unwrap();
            assert!(
                shown.contains("argument_type_not_assignable"),
                "{cmd}: {shown}"
            );
            assert!(
                shown.contains("prefer_const_constructors"),
                "{cmd}: {shown}"
            );
            assert!(
                shown.contains("lib/billing/total.dart:44"),
                "{cmd}: {shown}"
            );
            assert!(shown.contains("· exit 1]"), "{cmd}: {shown}");
            assert!(shown.len() * 3 < analyze.len(), "{cmd}: {shown}");
            assert_archive_exact_and_accounted(&ws, shown, &analyze);
        }

        // 3. `dart pub deps` & `flutter pub deps`: deduplicates repeated subtrees.
        let mut pub_deps = String::from("Dart SDK 3.7.0\nFlutter SDK 3.29.0\nshop_app 1.0.0\n");
        for i in 1..=35 {
            pub_deps.push_str(&format!(
                "|-- feature_{i:02} 1.0.0\n|   |-- shared_core 2.1.0\n|   |   |-- meta 1.16.0\n|   |   |-- collection 1.19.1\n|   |   '-- async 2.12.0\n|   '-- http 1.3.0\n"
            ));
        }
        pub_deps.push_str("'-- logger 2.5.0\n");
        assert!(pub_deps.len() > 4_096);
        for cmd in ["dart pub deps", "flutter pub deps | head -n 500"] {
            let shown =
                post_bash(&ws, cmd, &pub_deps).unwrap_or_else(|| panic!("{cmd} should compact"));
            assert!(shown.contains("shared_core 2.1.0"), "{cmd}: {shown}");
            assert!(shown.contains("[stdout:"), "{cmd}: {shown}");
            assert!(shown.len() * 2 < pub_deps.len(), "{cmd}: {shown}");
            assert_archive_exact_and_accounted(&ws, &shown, &pub_deps);
        }

        // 4. Machine formats stay rejected from dart/flutter planning.
        for rejected in [
            &["dart", "analyze", "--format=machine"][..],
            &["flutter", "analyze", "--write=out.txt"][..],
            &["dart", "test", "--machine"][..],
            &["flutter", "test", "--reporter=json"][..],
            &["dart", "pub", "deps", "--json"][..],
            &["flutter", "pub", "deps", "--json"][..],
        ] {
            assert_eq!(dart_flutter_kind(rejected), None, "{rejected:?}");
        }
    }
}
