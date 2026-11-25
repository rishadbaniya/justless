use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempDir};

use crate::history;
use crate::summary::{Parser, Summary};
use crate::workspace::{Location, Workspace, lock_state, private_dir, write_json};

pub(crate) static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("ANSI pattern"));
static SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:-->|:::|at)\s+)?([^\r\n]+?):(\d+)(?::\d+)?")
        .expect("source location pattern")
});
static PYTHON_SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*File "([^"]+)", line (\d+)"#).expect("Python traceback pattern")
});
static TYPESCRIPT_SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*([^()\r\n]+)\((\d+),\s*\d+\):").expect("TypeScript diagnostic pattern")
});
/// A compiler or checker error after a `path:line[:col]:` or `path(line,col):` prefix.
static LOCATED_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:[A-Za-z]:)?[^\s:()]+(?:(?::\d+){1,2}:|\(\d+(?:,\s*\d+)?\):)\s*(?:fatal error|error)\b",
    )
    .expect("located error pattern")
});
/// An exception line such as `ValueError: bad input` or `TypeError: x is undefined`.
static EXCEPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[A-Za-z_][\w.]*\.)?(?:[A-Z]\w*)?(?:Error|Exception):(?:\s|$)")
        .expect("exception pattern")
});
/// A numbered source line under a rustc diagnostic, such as `42 |     call();`.
static RUST_GUTTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+\s+\|").expect("source gutter pattern"));
const SCAN_LINE_LIMIT: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    pub(crate) const BOTH: [Self; 2] = [Self::Stdout, Self::Stderr];

    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }

    pub fn select(name: &str) -> &'static [Self] {
        match name {
            "stdout" => &[Self::Stdout],
            "stderr" => &[Self::Stderr],
            _ => &Self::BOTH,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }

    fn print(self, bytes: &[u8]) -> io::Result<()> {
        match self {
            // Flush a final partial line now, so a shell's `2>&1` keeps it ahead of
            // the stderr preview and the footer.
            Self::Stdout => {
                let mut stdout = io::stdout().lock();
                stdout.write_all(bytes).and_then(|()| stdout.flush())
            }
            Self::Stderr => io::stderr().lock().write_all(bytes),
        }
    }
}

pub fn diagnostic_excerpt(directory: &Path, diagnostic: &Diagnostic) -> Result<Vec<String>> {
    let file = File::open(directory.join(diagnostic.stream.name()))?;
    let start = diagnostic.log_line.saturating_sub(2).max(1);
    let end = diagnostic.log_line.saturating_add(2);
    let mut excerpt = Vec::new();
    let mut file = crate::redact::Reader::new(file, crate::redact::for_archive(directory));
    let mut raw = Vec::new();
    for line in 1..=end {
        if !read_prefix(&mut file, &mut raw, SCAN_LINE_LIMIT)? {
            break;
        }
        if line >= start {
            let text = String::from_utf8_lossy(&raw);
            let text = ANSI.replace_all(&text, "");
            let diagnostic_text =
                cargo_message(&text).and_then(|message| rendered_message(&message));
            let text = diagnostic_text.as_deref().unwrap_or(&text).trim_end();
            let first = text.lines().next().unwrap_or("");
            let stop = first.floor_char_boundary(first.len().min(220));
            excerpt.push(format!(
                "  {}:{line}: {}",
                diagnostic.stream.name(),
                &first[..stop]
            ));
        }
        raw.clear();
    }
    Ok(excerpt)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    pub location: Location,
    pub log_line: usize,
    pub stream: Stream,
}

/// A verified command's portion of archived stdout. Byte offsets are half-open;
/// line pointers are one-based and inclusive. Empty pieces have no line pointer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunPiece {
    pub command_index: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub first_line: Option<usize>,
    pub last_line: Option<usize>,
}

#[derive(Serialize, Deserialize)]
pub struct Run {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pieces: Vec<RunPiece>,
    pub id: String,
    pub status: i32,
    pub raw_bytes: u64,
    pub preview_bytes: usize,
    #[serde(default)]
    pub completed_ms: u64,
    #[serde(default)]
    pub family: String,
    /// The program and any known subcommand, such as `git log`, without arguments.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub command_key: String,
    #[serde(default)]
    pub digests: Vec<String>,
    #[serde(default)]
    pub summary: Summary,
    #[serde(default)]
    pub unchanged: bool,
    /// Bytes of the footer printed after the preview, so savings count everything
    /// the agent received.
    #[serde(default)]
    pub footer_bytes: usize,
    /// The run was routed by an agent hook rather than typed by a person.
    #[serde(default)]
    pub hook: bool,
    /// How output that Justless did not capture itself reached the archive, such
    /// as a Claude Code Bash result recorded after the command ran.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// The command line, its directory relative to the root (empty at the root) and
    /// the session, so `learn` can pair a failure with its correction. A command line
    /// that may carry a credential is left empty and marked withheld.
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub argv_withheld: bool,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub session: String,
    /// Secrets replaced in the previews; absent when redaction was off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction: Option<crate::redact::Found>,
    pub diagnostics: Vec<Diagnostic>,
    pub source_digests: BTreeMap<String, String>,
    /// Bytes each stream held when the command exited. A background process can keep
    /// writing to the archive afterwards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_bytes: Vec<u64>,
    /// Bytes the command wrote to each stream, kept only when a stream outgrew
    /// `max_run_bytes` and its archive holds just the first bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produced_bytes: Vec<u64>,
    /// Set on the record written before the command starts. It stays set when
    /// Justless stopped before the command finished.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unfinished: bool,
    /// A background process still held a stream when the command exited, and a
    /// drainer can keep adding its output to the archive.
    #[serde(default, skip_serializing_if = "is_false")]
    pub drained: bool,
    /// Which stream preview(s) a structured report or view replaced (`"stdout"`
    /// or `"both"`). Absent when plain previews were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced: Option<String>,
    /// Preview byte budget applied when formatting this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_bytes: Option<usize>,
    /// Set when the budget was supplied explicitly (such as `run --max-bytes`)
    /// rather than chosen by the adaptive preview advisor.
    #[serde(default, skip_serializing_if = "is_false")]
    pub explicit_budget: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Run {
    /// Whether the run failed or may have. A pipe or a list that the Bash
    /// post-command hook recorded keeps the whole line's status, which can hide
    /// a failure.
    pub(crate) fn may_have_failed(&self) -> bool {
        self.status != 0 || crate::postroute::exit_status(self) != self.status.to_string()
    }

    fn unfinished(id: String, family: &str, label: &str, hook: bool) -> Self {
        Self {
            id,
            pieces: Vec::new(),
            status: -1,
            raw_bytes: 0,
            preview_bytes: 0,
            completed_ms: history::now_ms(),
            family: family.to_owned(),
            label: label.to_owned(),
            command_key: String::new(),
            digests: Vec::new(),
            summary: Summary::default(),
            unchanged: false,
            footer_bytes: 0,
            hook,
            note: String::new(),
            argv: Vec::new(),
            argv_withheld: false,
            cwd: String::new(),
            session: String::new(),
            redaction: None,
            diagnostics: Vec::new(),
            source_digests: BTreeMap::new(),
            stream_bytes: Vec::new(),
            produced_bytes: Vec::new(),
            unfinished: true,
            drained: false,
            replaced: None,
            budget_bytes: None,
            explicit_budget: false,
        }
    }

    /// One line per stream whose archive stopped at the cap, such as
    /// `[stdout archive truncated at 256.0 MB of 1228.8 MB]`.
    fn truncation(&self) -> String {
        truncation(&self.stream_bytes, &self.produced_bytes)
    }
}

fn truncation(archived: &[u64], produced: &[u64]) -> String {
    let mut text = String::new();
    for ((stream, &kept), &produced) in Stream::BOTH.iter().zip(archived).zip(produced) {
        if produced > kept {
            text.push_str(&format!(
                "[{} archive truncated at {} of {}]\n",
                stream.name(),
                history::human_bytes(kept),
                history::human_bytes(produced)
            ));
        }
    }
    text
}

pub fn source_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// How a capture is labelled, started and previewed. `execute` passes `run`'s flags.
pub struct Shape<'a> {
    pub max_bytes: usize,
    pub raw: bool,
    pub session: &'a str,
    pub fresh: bool,
    /// `run --window`: a filter after the command shows the agent only part of
    /// the report, so the run is never compared with another, in either direction.
    pub window: bool,
    /// The run was routed by an agent hook rather than typed by a person.
    pub hook: bool,
    /// Print the agent footer, as after a hook-routed run: nothing when every byte was
    /// shown and the command succeeded, otherwise one short line naming the run.
    pub agent_footer: bool,
    /// Label for stats, history and resume, such as `exec python3`.
    pub family: Option<String>,
    /// `run --merge`, for a command that ended in `2>&1`: the command writes both
    /// streams to one pipe, archived as stdout, unless `merge::joins` keeps them
    /// apart.
    pub merge: bool,
    /// Replaces the preview of a stdout longer than `max_bytes`, given the stdout file
    /// and the run ID. When it returns `None`, the usual preview stays.
    pub stdout_preview: Option<StdoutPreview<'a>>,
    /// Starts the process, given the run directory and the write ends of its stdout
    /// and stderr pipes, and returns its exit status once every process it started
    /// has stopped. Justless copies the pipes into the archive within
    /// `max_run_bytes`, as for any captured run.
    pub spawn: Option<Spawn<'a>>,
}

pub type Spawn<'a> = Box<dyn FnOnce(&Path, Stdio, Stdio) -> Result<i32> + 'a>;
pub type StdoutPreview<'a> = Box<dyn FnOnce(&Path, &str) -> Result<Option<Vec<u8>>> + 'a>;

#[allow(clippy::too_many_arguments)]
pub fn execute(
    workspace: &Workspace,
    args: &[OsString],
    max_bytes: usize,
    raw: bool,
    session: &str,
    (fresh, window): (bool, bool),
    hook: bool,
    merge: bool,
) -> Result<i32> {
    execute_shaped(
        workspace,
        args,
        Shape {
            max_bytes,
            raw,
            session,
            fresh,
            window,
            hook,
            agent_footer: hook,
            family: None,
            merge,
            stdout_preview: None,
            spawn: None,
        },
    )
}

pub fn execute_shaped(workspace: &Workspace, args: &[OsString], shape: Shape<'_>) -> Result<i32> {
    let Shape {
        max_bytes,
        raw,
        session,
        fresh,
        window,
        hook,
        agent_footer,
        family,
        merge,
        stdout_preview,
        spawn,
    } = shape;
    let mut command = Command::new(&args[0]);
    command.current_dir(&workspace.cwd);
    if raw {
        command.args(&args[1..]);
        return Ok(uncaptured(&mut command, &args[0]));
    }
    let explicit_budget = !crate::adaptive::take_advised(workspace, args, max_bytes);

    let settings = workspace.settings()?;
    let selection = crate::filters::Selection::for_run(args, &settings.filters)?;
    let redactor = crate::redact::for_run(workspace)?;
    let words: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
    // A recursive grep would otherwise read the archived logs, including the stdout
    // file it is writing.
    if let Some(exclude) = crate::search::exclude_state(&words) {
        command.arg(exclude);
    }
    command.args(&args[1..]);
    let family = family.unwrap_or_else(|| family_of(args, &selection));
    // A read-only checkout or sandbox must not stop the command itself.
    let mut archive = match Archive::create(workspace, &family, &history::label(&words), hook) {
        Ok(archive) => archive,
        // A process that `spawn` starts needs its run directory.
        Err(error) if spawn.is_some() => return Err(error.context("cannot record this run")),
        Err(error) => {
            eprintln!("justless: cannot record this run ({error:#}); running it without capture");
            return Ok(uncaptured(&mut command, &args[0]));
        }
    };
    let merged = crate::merge::joins(merge, selection.profile);
    let (status, exited) = match spawn {
        Some(spawn) => (archive.run_with(spawn, settings.max_run_bytes)?, None),
        None => {
            let exited = match archive.run(&mut command, settings.max_run_bytes, merged) {
                Ok(Ok(status)) => Some(status),
                Ok(Err(error)) => {
                    eprintln!("justless: wait for {}: {error}", args[0].to_string_lossy());
                    None
                }
                Err(error) => return Ok(spawn_failure(&args[0], &error)),
            };
            (exited.map_or(1, exit_code), exited)
        }
    };

    // The command has run. From here on its output and exit status always reach the
    // caller, even when the archive cannot be read back or saved.
    let capture = Capture {
        workspace,
        args,
        words: &words,
        selection: &selection,
        max_bytes,
        explicit_budget,
        session,
        fresh: fresh || window,
        window,
        hook,
        agent_footer,
        merge,
        merged,
        redactor,
        retention: (settings.max_runs, settings.max_archive_bytes),
        origin: None,
        pieces: &[],
        json_grouping: settings.json_grouping,
        ends_early: false,
    };
    match capture.record(&mut archive, family, status, stdout_preview) {
        Ok(((previews, _), footer, _)) => {
            for (stream, preview) in previews {
                if let Err(error) = stream.print(&preview)
                    && error.kind() != io::ErrorKind::BrokenPipe
                {
                    eprintln!("justless: could not display {}: {error}", stream.name());
                }
            }
            // The footer goes to stderr, so a preview line still buffered on stdout
            // must reach the caller first.
            let _ = io::stdout().flush();
            if !footer.is_empty() {
                eprint!("{footer}");
            }
        }
        Err(error) => {
            eprintln!("justless: could not save run, restoring full output: {error:#}");
            let last = archive.replay(redactor);
            let truncation = archive.truncation();
            if !truncation.is_empty() {
                eprint!("{}{truncation}", separator(last));
            }
        }
    }
    signals::reraise(exited);
    Ok(status)
}

/// How stats, history and resume label a run of `args`.
fn family_of(args: &[OsString], selection: &crate::filters::Selection) -> String {
    match selection.profile {
        Some(
            crate::filters::Profile::Lint
            | crate::filters::Profile::Search
            | crate::filters::Profile::Listing,
        ) => crate::filters::tool_label(args),
        Some(profile) => profile.label().to_owned(),
        None => crate::summary::family(args),
    }
}

/// A command that ran elsewhere, such as in Claude Code's own shell, whose output
/// Justless archives after the fact.
pub struct Origin {
    /// The whole command line. It keys the run apart from runs Justless started, so
    /// a piped `cargo test | tail` never supersedes or compares with `cargo test`.
    /// It is hashed into the key and not stored.
    pub command: String,
    /// How `history` and `resume` name the run, such as `cargo test | tail`.
    pub label: String,
    /// Stored in the run record: how the output was captured and what it may lack.
    pub note: String,
    /// A verified source-range outline and the bytes the client would have received.
    pub outline: Option<SourceOutline>,
}

pub struct SourceOutline {
    pub text: String,
    /// Bytes the shell client showed before compaction, including any client-side trim.
    pub baseline: usize,
    /// Claude trims the trailing newline from Bash results sent to the model.
    pub trim_footer: bool,
}

/// What a recorded run shows the agent: each stream's preview, with secrets
/// replaced as in any run's preview, and the footer.
pub struct Recorded {
    pub run: Run,
    pub previews: Vec<(Stream, Vec<u8>)>,
    pub footer: String,
    /// Whether a report of the output, such as a test, patch, lint, search or
    /// listing report, replaced the stdout preview.
    pub report: bool,
}

/// Archive output that another program captured and compact it exactly as a
/// hook-routed `execute` of `args` would, without rerun comparisons. `output` is
/// stored as stdout within `max_run_bytes`, and `status` is the exit status
/// reported with it. The run is staged under `runs/.staged` and moved into place
/// once its record is written, so a hook stopped at its timeout leaves nothing in
/// `runs/`. Hooks finish within seconds, so a staged run older than a minute was
/// left by one and is removed.
pub fn record(
    workspace: &Workspace,
    args: &[OsString],
    output: &[u8],
    status: i32,
    max_bytes: usize,
    session: &str,
    origin: &Origin,
) -> Result<Recorded> {
    record_with_pieces(
        workspace,
        args,
        output,
        status,
        max_bytes,
        session,
        (origin, &[]),
    )
}

pub(crate) fn record_with_pieces(
    workspace: &Workspace,
    args: &[OsString],
    output: &[u8],
    status: i32,
    max_bytes: usize,
    session: &str,
    (origin, pieces): (&Origin, &[RunPiece]),
) -> Result<Recorded> {
    record_cut(
        workspace,
        args,
        output,
        status,
        max_bytes,
        session,
        (origin, pieces, false),
    )
}

/// `record_with_pieces`, for output that can stop in the middle of an item, as
/// `| head` can leave a patch (`ends_early`). A patch report then marks a last
/// hunk that stops short of its header's line counts.
pub(crate) fn record_cut(
    workspace: &Workspace,
    args: &[OsString],
    output: &[u8],
    status: i32,
    max_bytes: usize,
    session: &str,
    (origin, pieces, ends_early): (&Origin, &[RunPiece], bool),
) -> Result<Recorded> {
    ensure!(!args.is_empty(), "no command to record");
    let settings = workspace.settings()?;
    let selection = crate::filters::Selection::for_run(args, &settings.filters)?;
    let redactor = crate::redact::for_run(workspace)?;
    let words: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
    let family = if origin.outline.is_some() {
        origin.label.clone()
    } else {
        family_of(args, &selection)
    };
    workspace.prepare_state()?;
    let runs = workspace.state.join("runs");
    private_dir(&runs)?;
    let staging = runs.join(".staged");
    private_dir(&staging)?;
    for entry in fs::read_dir(&staging)?.flatten() {
        if entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|time| time.elapsed().is_ok_and(|age| age.as_secs() > 60))
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let explicit_budget = !crate::adaptive::take_advised(workspace, args, max_bytes);
    let mut archive = Archive::create_in(&staging, &family, &origin.label, true)?;
    archive.destination = Some(runs);
    archive.store(output, settings.max_run_bytes)?;
    let capture = Capture {
        workspace,
        args,
        words: &words,
        selection: &selection,
        max_bytes,
        explicit_budget,
        session,
        fresh: true,
        window: false,
        hook: true,
        agent_footer: true,
        merge: false,
        merged: false,
        redactor,
        retention: (settings.max_runs, settings.max_archive_bytes),
        origin: Some(origin),
        pieces,
        json_grouping: settings.json_grouping,
        ends_early,
    };
    match capture.record(&mut archive, family, status, None) {
        Ok(((previews, report), footer, run)) => Ok(Recorded {
            run,
            previews,
            footer,
            report,
        }),
        Err(error) => {
            // A run that could not be recorded or moved into place is not kept.
            let _ = fs::remove_dir_all(&archive.directory);
            Err(error)
        }
    }
}

/// Run without recording, with the same exit statuses a shell reports.
fn uncaptured(command: &mut Command, program: &OsStr) -> i32 {
    match supervise(command) {
        Ok(Ok(status)) => {
            signals::reraise(Some(status));
            exit_code(status)
        }
        Ok(Err(error)) => {
            eprintln!("justless: wait for {}: {error}", program.to_string_lossy());
            1
        }
        Err(error) => spawn_failure(program, &error),
    }
}

/// Start the command and wait for it, forwarding termination signals to it. The
/// outer error is a failure to start; the inner one a failure to wait.
fn supervise(command: &mut Command) -> io::Result<io::Result<ExitStatus>> {
    signals::install();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            signals::restore();
            return Err(error);
        }
    };
    signals::watch(child.id());
    // Stop forwarding before the command is reaped, so a late signal can never
    // reach a process that reuses its ID.
    signals::settle(child.id());
    Ok(child.wait())
}

/// A shell exits 127 when it cannot find a command and 126 when it cannot run one.
fn spawn_failure(program: &OsStr, error: &io::Error) -> i32 {
    let program = program.to_string_lossy();
    if error.kind() == io::ErrorKind::NotFound {
        if program.contains('/') {
            eprintln!("justless: {program}: No such file or directory");
        } else {
            eprintln!("justless: {program}: command not found");
        }
        127
    } else {
        eprintln!("justless: {program}: {error}");
        126
    }
}

/// The file in a run directory that the Justless process recording the run keeps
/// locked until it exits. A run with an unfinished record and a free lock belongs
/// to a Justless process that was killed.
pub const ACTIVE_LOCK: &str = "active.lock";

/// A run directory and its streams. The command writes into pipes that Justless
/// copies to the archive, so Justless decides how much of each stream is stored.
/// Justless writes and reads through handles opened before the command starts, so
/// the output stays readable even if the directory is deleted while the command runs.
struct Archive {
    directory: PathBuf,
    temporary: TempDir,
    writers: Vec<File>,
    readers: Vec<File>,
    captured: Vec<Captured>,
    /// A background process still held a stream open when the command exited, and
    /// a drainer keeps adding its output to the archive.
    late: bool,
    /// The locked `ACTIVE_LOCK`, held until Justless exits.
    active: Option<File>,
    /// The runs directory a staged run moves into once its record is saved.
    destination: Option<PathBuf>,
}

/// What the command wrote to one stream.
#[derive(Clone, Copy, Default)]
struct Captured {
    /// Bytes stored in the archive by the time the command exited.
    archived: u64,
    /// Bytes the command wrote, including any past the cap.
    produced: u64,
}

impl Archive {
    fn create(workspace: &Workspace, family: &str, label: &str, hook: bool) -> Result<Self> {
        workspace.prepare_state()?;
        let runs = workspace.state.join("runs");
        private_dir(&runs)?;
        Self::create_in(&runs, family, label, hook)
    }

    /// A new run directory under `runs`.
    fn create_in(runs: &Path, family: &str, label: &str, hook: bool) -> Result<Self> {
        let temporary = Builder::new()
            .prefix("run-")
            .rand_bytes(12)
            .tempdir_in(runs)
            .with_context(|| format!("create a run in {}", runs.display()))?;
        let directory = temporary.path().to_path_buf();
        // Lock before the first record exists, so `prune` never sees a record of a
        // live run without its lock held.
        let active = lock_active(&directory.join(ACTIVE_LOCK));
        let mut writers = Vec::new();
        let mut readers = Vec::new();
        for stream in Stream::BOTH {
            let path = directory.join(stream.name());
            writers
                .push(File::create(&path).with_context(|| format!("create {}", path.display()))?);
            readers.push(File::open(&path)?);
        }
        // A record written before the command starts keeps the run listed and
        // replayable if Justless is killed before the command finishes.
        let id = id_of(&directory);
        let _ = write_json(
            &directory.join("record.json"),
            &Run::unfinished(id, family, label, hook),
        );
        Ok(Self {
            directory,
            temporary,
            writers,
            readers,
            captured: vec![Captured::default(); 2],
            late: false,
            active,
            destination: None,
        })
    }

    /// Store output that another program captured as stdout, at most `cap` bytes of
    /// it, and leave stderr empty.
    fn store(&mut self, output: &[u8], cap: u64) -> Result<()> {
        let mut writers = std::mem::take(&mut self.writers).into_iter();
        let Some(mut stdout) = writers.next() else {
            anyhow::bail!("the output is already stored");
        };
        let kept = &output[..output.len().min(usize::try_from(cap).unwrap_or(usize::MAX))];
        stdout.write_all(kept).context("archive stdout")?;
        self.captured[Stream::Stdout.index()] = Captured {
            archived: kept.len() as u64,
            produced: output.len() as u64,
        };
        Ok(())
    }

    /// Move a staged run into the runs directory, once its record is saved.
    fn publish(&mut self) -> Result<()> {
        let Some(runs) = self.destination.take() else {
            return Ok(());
        };
        let target = runs.join(self.id());
        fs::rename(&self.directory, &target)
            .with_context(|| format!("move the run into {}", runs.display()))?;
        self.directory = target;
        Ok(())
    }

    /// The final record is saved, so the run no longer needs its lock file. The
    /// lock itself is held until Justless exits.
    fn finish(&self) {
        if self.active.is_some() {
            let _ = fs::remove_file(self.directory.join(ACTIVE_LOCK));
        }
    }

    /// Run the command into the archive, storing at most `cap` bytes of each stream.
    /// Output past the cap is still read and counted, so the command never blocks
    /// and its exit status is unchanged. Termination signals sent to Justless reach
    /// the command, and Justless finishes the record once the command exits. With
    /// `merged`, the command writes both streams to one pipe, as a shell's `2>&1`
    /// gives it, and the stdout archive keeps them in the order they were written,
    /// under the same cap. The stderr archive then stays empty.
    fn run(
        &mut self,
        command: &mut Command,
        cap: u64,
        merged: bool,
    ) -> io::Result<io::Result<ExitStatus>> {
        let mut writers = std::mem::take(&mut self.writers).into_iter();
        let (Some(stdout_file), Some(stderr_file)) = (writers.next(), writers.next()) else {
            return Err(io::Error::other("the command already ran"));
        };
        let shared = if merged {
            let (reader, writer) = io::pipe()?;
            command.stdout(writer.try_clone()?).stderr(writer);
            Some(reader)
        } else {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            None
        };
        signals::install();
        let spawned = command.spawn();
        if shared.is_some() {
            // `Command` keeps the write ends it was given. Close them here, so the
            // pipe closes once the command and its children are done with it.
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                signals::restore();
                return Err(error);
            }
        };
        signals::watch(child.id());
        let exited = Arc::new(AtomicBool::new(false));
        // A merged run has one pipe, which fills the stdout archive.
        let (shared, stdout_file) = match shared {
            Some(pipe) => {
                let exited = Arc::clone(&exited);
                let pumped = std::thread::spawn(move || {
                    pump(pipe, stdout_file, cap, Stream::Stdout, &exited)
                });
                (Some(pumped), None)
            }
            None => (None, Some(stdout_file)),
        };
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let stdout = stdout.zip(stdout_file).map(|(pipe, file)| {
            let exited = Arc::clone(&exited);
            std::thread::spawn(move || pump(pipe, file, cap, Stream::Stdout, &exited))
        });
        let stderr = stderr.map(|pipe| {
            let exited = Arc::clone(&exited);
            std::thread::spawn(move || pump(pipe, stderr_file, cap, Stream::Stderr, &exited))
        });
        // Stop forwarding before the command is reaped, so a late signal can never
        // reach a process that reuses its ID.
        signals::settle(child.id());
        exited.store(true, Ordering::SeqCst);
        let status = child.wait();
        // A pipe that a background process still holds goes to a drainer, so that
        // process can keep writing, within the cap, after Justless exits.
        if let Some(pumped) = shared.map(joined) {
            self.collect(Stream::Stdout, pumped, cap);
        }
        if let Some(pumped) = stdout.map(joined) {
            self.collect(Stream::Stdout, pumped, cap);
        }
        if let Some(pumped) = stderr.map(joined) {
            self.collect(Stream::Stderr, pumped, cap);
        }
        Ok(status)
    }

    /// Run a process that `spawn` starts on the write ends of the stream pipes, storing
    /// at most `cap` bytes of each stream. The pipes are read while `spawn` runs, and
    /// once it returns they are read as `run` reads them after its command exits.
    /// `spawn` handles signals and stops what it started.
    fn run_with(&mut self, spawn: Spawn<'_>, cap: u64) -> Result<i32> {
        let mut writers = std::mem::take(&mut self.writers).into_iter();
        let (Some(stdout_file), Some(stderr_file)) = (writers.next(), writers.next()) else {
            anyhow::bail!("the command already ran");
        };
        let (stdout_pipe, stdout_end) = io::pipe().context("create the stdout pipe")?;
        let (stderr_pipe, stderr_end) = io::pipe().context("create the stderr pipe")?;
        let exited = Arc::new(AtomicBool::new(false));
        let stdout = {
            let exited = Arc::clone(&exited);
            std::thread::spawn(move || pump(stdout_pipe, stdout_file, cap, Stream::Stdout, &exited))
        };
        let stderr = {
            let exited = Arc::clone(&exited);
            std::thread::spawn(move || pump(stderr_pipe, stderr_file, cap, Stream::Stderr, &exited))
        };
        // `spawn` owns the write ends, so they are closed once it returns.
        let status = spawn(&self.directory, stdout_end.into(), stderr_end.into());
        exited.store(true, Ordering::SeqCst);
        self.collect(Stream::Stdout, joined(stdout), cap);
        self.collect(Stream::Stderr, joined(stderr), cap);
        status
    }

    fn collect<R: Into<Stdio>>(&mut self, stream: Stream, pumped: Pumped<R>, cap: u64) {
        let Pumped {
            file,
            captured,
            open,
        } = pumped;
        self.captured[stream.index()] = captured;
        if let Some(pipe) = open {
            self.late |= hand_off(pipe, file, cap.saturating_sub(captured.archived));
        }
    }

    fn id(&self) -> String {
        id_of(&self.directory)
    }

    /// Stream sizes when the command exited. Later writes by background processes
    /// stay in the archive but are not part of the record.
    fn lengths(&self) -> Vec<u64> {
        self.captured.iter().map(|c| c.archived).collect()
    }

    /// What each stream produced, when a stream outgrew the cap.
    fn produced(&self) -> Vec<u64> {
        if self.captured.iter().all(|c| c.produced == c.archived) {
            return Vec::new();
        }
        self.captured.iter().map(|c| c.produced).collect()
    }

    fn truncation(&self) -> String {
        truncation(&self.lengths(), &self.produced())
    }

    fn keep(&mut self) {
        self.temporary.disable_cleanup(true);
    }

    /// Store an unchanged run's streams once: link them to the identical streams of
    /// the run it matched instead of keeping a second copy. A stream that a drainer
    /// can still extend is never linked, so neither run takes on the other's late
    /// output.
    fn share(&self, prior: &Path, earlier: &Run, run: &Run) {
        if earlier.drained || run.drained || earlier.stream_bytes != run.stream_bytes {
            return;
        }
        for (stream, &length) in Stream::BOTH.into_iter().zip(&run.stream_bytes) {
            let source = prior.join(stream.name());
            let target = self.directory.join(stream.name());
            let same_length = |path: &Path| fs::metadata(path).is_ok_and(|m| m.len() == length);
            if length == 0 || !same_length(&source) || !same_length(&target) {
                continue;
            }
            let link = self.directory.join(format!(".{}.link", stream.name()));
            if fs::hard_link(&source, &link).is_ok() && fs::rename(&link, &target).is_err() {
                let _ = fs::remove_file(&link);
            }
        }
    }

    /// Print both complete streams from the open handles, with secrets replaced as
    /// in a preview. Returns the last byte printed.
    fn replay(&self, redactor: Option<&crate::redact::Redactor>) -> Option<u8> {
        let mut last = None;
        for (stream, mut file) in Stream::BOTH.into_iter().zip(&self.readers) {
            match file
                .seek(SeekFrom::Start(0))
                .and_then(|_| copy_out(&mut crate::redact::Reader::new(file, redactor), stream))
            {
                Ok((byte, _)) => last = byte.or(last),
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
                Err(error) => eprintln!("justless: could not replay {}: {error}", stream.name()),
            }
        }
        last
    }
}

fn joined<T>(thread: std::thread::JoinHandle<T>) -> T {
    thread
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// A stream's pipe after the command exited.
struct Pumped<R> {
    file: File,
    captured: Captured,
    /// The pipe, when a background process still held it open.
    open: Option<R>,
}

#[cfg(unix)]
trait Source: Read + std::os::fd::AsRawFd {}
#[cfg(unix)]
impl<T: Read + std::os::fd::AsRawFd> Source for T {}
#[cfg(not(unix))]
trait Source: Read {}
#[cfg(not(unix))]
impl<T: Read> Source for T {}

/// How long an empty pipe may stay open after the command exited before it counts
/// as held by a background process. macOS can show the end of the pipe several
/// milliseconds after the exit under heavy load.
const CLOSE_GRACE_MILLISECONDS: i32 = 250;

/// Copy a stream's pipe into its archive file until the pipe closes, storing at
/// most `cap` bytes and counting the rest. Each chunk is written at once, so a
/// killed Justless leaves what it read on disk. Once `exited` is set, only the
/// bytes already waiting in the pipe are still the command's. After them, a pipe
/// that a background process still holds is returned instead of waited on, even
/// when that process keeps writing. An empty pipe gets `CLOSE_GRACE_MILLISECONDS`
/// to close first.
fn pump<R: Source>(
    mut pipe: R,
    mut file: File,
    cap: u64,
    stream: Stream,
    exited: &AtomicBool,
) -> Pumped<R> {
    let mut captured = Captured::default();
    let mut buffer = vec![0; 64 * 1024];
    let mut failed = false;
    // The command's bytes still in the pipe, counted when its exit is first seen.
    let mut left = None;
    loop {
        let done = exited.load(Ordering::SeqCst);
        if done && left.is_none() {
            left = pending(&pipe);
        }
        let ready = match left {
            // Readable with nothing waiting means every writer closed the pipe. The
            // kernel can report the exit a moment before the command's end of the
            // pipe closes, so a pipe still open waits a little for that.
            Some(0) => match pending(&pipe) {
                Some(0) => readable(&pipe, CLOSE_GRACE_MILLISECONDS)
                    .map(|ready| ready && pending(&pipe) == Some(0)),
                _ => Ok(false),
            },
            Some(_) => Ok(true),
            None => readable(&pipe, if done { 0 } else { 50 }),
        };
        match ready {
            Ok(true) => {}
            Ok(false) if done => {
                return Pumped {
                    file,
                    captured,
                    open: Some(pipe),
                };
            }
            Ok(false) => continue,
            Err(_) => break,
        }
        let limit = left.map_or(buffer.len(), |left| {
            usize::try_from(left).map_or(buffer.len(), |left| left.clamp(1, buffer.len()))
        });
        let read = match pipe.read(&mut buffer[..limit]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if let Some(left) = &mut left {
            *left = left.saturating_sub(read as u64);
        }
        captured.produced += read as u64;
        let room = usize::try_from(cap.saturating_sub(captured.archived)).unwrap_or(usize::MAX);
        let mut chunk = &buffer[..read.min(room)];
        while !failed && !chunk.is_empty() {
            match file.write(chunk) {
                Ok(0) => failed = true,
                Ok(written) => {
                    captured.archived += written as u64;
                    chunk = &chunk[written..];
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    eprintln!(
                        "justless: could not archive {}: {error}; the rest is not stored",
                        stream.name()
                    );
                    failed = true;
                }
            }
        }
    }
    Pumped {
        file,
        captured,
        open: None,
    }
}

/// Whether the pipe has data or has closed, waiting up to `milliseconds`.
#[cfg(unix)]
fn readable(pipe: &impl std::os::fd::AsRawFd, milliseconds: libc::c_int) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd: pipe.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one initialized pollfd for a descriptor this thread owns.
        let ready = unsafe { libc::poll(&mut poll, 1, milliseconds) };
        if ready >= 0 {
            return Ok(ready > 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Without `poll`, a read waits for data, so the pipe is read until it closes.
#[cfg(not(unix))]
fn readable<T>(_: &T, _: i32) -> io::Result<bool> {
    Ok(true)
}

/// The bytes waiting in the pipe.
#[cfg(unix)]
fn pending(pipe: &impl std::os::fd::AsRawFd) -> Option<u64> {
    let mut count: libc::c_int = 0;
    // SAFETY: FIONREAD stores the byte count in the initialized integer.
    let result = unsafe { libc::ioctl(pipe.as_raw_fd(), libc::FIONREAD, &mut count) };
    (result == 0).then(|| u64::try_from(count).unwrap_or(0))
}

#[cfg(not(unix))]
fn pending<T>(_: &T) -> Option<u64> {
    None
}

/// Start `justless drain` with the pipe as its input and the archive file as its
/// output, in its own process group so a terminal interrupt meant for the command
/// does not stop it. Returns whether it started; otherwise the pipe closes.
#[cfg(unix)]
fn hand_off(pipe: impl Into<Stdio>, file: File, room: u64) -> bool {
    use std::os::unix::process::CommandExt;
    let Ok(program) = std::env::current_exe() else {
        return false;
    };
    Command::new(program)
        .args(["drain", &room.to_string()])
        .current_dir("/")
        .stdin(pipe)
        .stdout(file)
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .is_ok()
}

#[cfg(not(unix))]
fn hand_off(_: impl Into<Stdio>, _: File, _: u64) -> bool {
    false
}

/// `justless drain ROOM`: copy up to ROOM bytes from standard input to standard
/// output, then read and discard the rest until every writer closes the pipe. It
/// stores output that a background process writes after its command exited.
pub fn drain(room: u64) -> Result<i32> {
    #[cfg(unix)]
    let mut output = File::from(std::os::fd::AsFd::as_fd(&io::stdout()).try_clone_to_owned()?);
    #[cfg(not(unix))]
    let mut output = io::stdout().lock();
    let mut input = io::stdin().lock();
    let mut room = usize::try_from(room).unwrap_or(usize::MAX);
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        let keep = read.min(room);
        if keep > 0 {
            if output.write_all(&buffer[..keep]).is_err() {
                room = 0;
            } else {
                room -= keep;
            }
        }
    }
    output.flush()?;
    Ok(0)
}

/// Create and lock a run's `ACTIVE_LOCK`. The handle is not inherited by the
/// command, so the lock ends with Justless. Where locks are unavailable, the file is
/// removed and `prune` judges the run by its age instead.
fn lock_active(path: &Path) -> Option<File> {
    let file = File::create(path).ok()?;
    if file.lock().is_ok() {
        return Some(file);
    }
    drop(file);
    let _ = fs::remove_file(path);
    None
}

/// Whether a run whose record is unfinished or missing is still being recorded.
/// `None` means the run has no lock file, from a Justless that could not lock it
/// or that did not create one, or because its directory was created a moment ago.
pub fn recording(directory: &Path) -> Option<bool> {
    let file = File::open(directory.join(ACTIVE_LOCK)).ok()?;
    match file.try_lock() {
        Ok(()) => Some(false),
        Err(fs::TryLockError::WouldBlock) => Some(true),
        Err(fs::TryLockError::Error(_)) => None,
    }
}

fn id_of(directory: &Path) -> String {
    directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(unix)]
mod signals {
    //! Forward SIGTERM and SIGHUP to the command at once. The command shares
    //! Justless's process group, so a terminal interrupt or a SIGINT or SIGQUIT
    //! sent to the whole group already reaches it. Justless forwards those only
    //! if the command is still running 200 ms after Justless received one, so a
    //! signal sent to Justless alone still stops the command, and a command that
    //! exits promptly from a group interrupt is not interrupted again. One that
    //! takes longer to exit receives it twice. Like a shell waiting for a
    //! foreground job, Justless waits for the command to exit and then finishes
    //! the record. Once the command has exited, the default dispositions return,
    //! so another signal stops Justless as usual. Any signal that stopped the
    //! command, along with group interrupts (`SIGINT` and `SIGQUIT`), are the
    //! exception: Justless's own copy can arrive just after the command exited
    //! (even when the command handled the interrupt and exited cleanly), so they
    //! are only recorded until the preview is out.
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

    static CHILD: AtomicI32 = AtomicI32::new(0);
    static RECEIVED: AtomicI32 = AtomicI32::new(0);
    /// The interrupt to forward when the grace period ends.
    static PENDING: AtomicI32 = AtomicI32::new(0);
    static INSTALLED: AtomicU64 = AtomicU64::new(0);
    /// Signals whose handlers stay until `reraise`: any signal that stopped the
    /// command, plus `SIGINT` and `SIGQUIT`, which a process-group interrupt can
    /// still deliver to Justless after a command catches its copy and exits.
    static HELD: AtomicU64 = AtomicU64::new(0);
    const FORWARDED: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGHUP];
    /// SIGALRM ends the grace period of an interrupt, so its handler goes first.
    const OBSERVED: [libc::c_int; 5] = [
        libc::SIGALRM,
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
    ];
    /// How long the command may take to exit from an interrupt it received itself.
    const GRACE_MICROSECONDS: libc::suseconds_t = 200_000;

    extern "C" fn handle(signal: libc::c_int) {
        let child = CHILD.load(Ordering::SeqCst);
        if signal == libc::SIGALRM {
            let pending = PENDING.swap(0, Ordering::SeqCst);
            if child > 0 && pending != 0 {
                // SAFETY: kill is async-signal-safe, and the command is not yet reaped.
                unsafe { libc::kill(child, pending) };
            }
            return;
        }
        RECEIVED.store(signal, Ordering::SeqCst);
        if FORWARDED.contains(&signal) {
            if child > 0 {
                // SAFETY: kill is async-signal-safe, and the command is not yet reaped.
                unsafe { libc::kill(child, signal) };
            }
        } else {
            PENDING.store(signal, Ordering::SeqCst);
            if child > 0 {
                arm(GRACE_MICROSECONDS);
            }
        }
    }

    /// Deliver SIGALRM after `microseconds`, or cancel it with 0. The timer is set
    /// only while the SIGALRM handler is installed.
    fn arm(microseconds: libc::suseconds_t) {
        if INSTALLED.load(Ordering::SeqCst) & (1 << libc::SIGALRM) == 0 {
            return;
        }
        // SAFETY: setitimer is a plain system call with no shared state in libc,
        // and the structure is fully initialized.
        unsafe {
            let mut timer: libc::itimerval = std::mem::zeroed();
            timer.it_value.tv_usec = microseconds;
            libc::setitimer(libc::ITIMER_REAL, &timer, std::ptr::null_mut());
        }
    }

    pub fn install() {
        HELD.store(0, Ordering::SeqCst);
        for signal in OBSERVED {
            // SAFETY: the handler only touches atomics and calls kill and
            // setitimer. A signal that is already ignored, as under nohup, stays
            // ignored so the command inherits that disposition.
            unsafe {
                let mut current: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, std::ptr::null(), &mut current) != 0
                    || current.sa_sigaction == libc::SIG_IGN
                {
                    continue;
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handle as extern "C" fn(libc::c_int) as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, std::ptr::null_mut()) == 0 {
                    INSTALLED.fetch_or(1 << signal, Ordering::SeqCst);
                }
            }
        }
    }

    pub fn watch(pid: u32) {
        let Ok(pid) = i32::try_from(pid) else {
            return;
        };
        CHILD.store(pid, Ordering::SeqCst);
        // A signal that arrived while the command was starting.
        let early = RECEIVED.load(Ordering::SeqCst);
        if FORWARDED.contains(&early) {
            // SAFETY: pid is the command just started, and it is not yet reaped.
            unsafe { libc::kill(pid, early) };
        } else if PENDING.load(Ordering::SeqCst) != 0 {
            arm(GRACE_MICROSECONDS);
        }
    }

    /// Wait until the command exits without reaping it, then stop forwarding.
    pub fn settle(pid: u32) {
        let mut ended = 0;
        loop {
            // SAFETY: WNOWAIT leaves the exited command for Child::wait to reap, and
            // si_status is read only from a child state change that waitid filled.
            let (waited, info) = unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                let waited =
                    libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT);
                (waited, info)
            };
            if waited == 0 {
                if matches!(info.si_code, libc::CLD_KILLED | libc::CLD_DUMPED) {
                    // SAFETY: as above.
                    let signal = unsafe { info.si_status() };
                    let held = signal != libc::SIGALRM && OBSERVED.contains(&signal);
                    ended = if held { signal } else { 0 };
                }
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
        // A group signal reaches Justless and the command together, but Justless
        // can take its copy after the command is gone, whether the command died
        // from the signal or caught it and exited. Hold group interrupts and any
        // fatal signal until `reraise`, so a late copy cannot stop Justless
        // before the preview is out.
        let mut held = (1u64 << libc::SIGINT) | (1u64 << libc::SIGQUIT);
        if ended > 0 {
            held |= 1u64 << ended;
        }
        HELD.store(held, Ordering::SeqCst);
        restore();
    }

    /// Exit from the signal that stopped the command when Justless received it
    /// too, after the preview is out, so a calling shell stops its script just as
    /// it would for the command itself.
    pub fn reraise(status: Option<std::process::ExitStatus>) {
        use std::os::unix::process::ExitStatusExt;
        let held = HELD.swap(0, Ordering::SeqCst);
        if let Some(signal) = status.and_then(|status| status.signal())
            && RECEIVED.load(Ordering::SeqCst) == signal
        {
            let _ = std::io::Write::flush(&mut std::io::stdout());
            // SAFETY: restores the default action and raises the signal on Justless.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
        }
        let clear = INSTALLED.fetch_and(!held, Ordering::SeqCst) & held;
        for signal in OBSERVED {
            if clear & (1u64 << signal) != 0 {
                // SAFETY: restores the default action for signals `settle` held.
                unsafe { libc::signal(signal, libc::SIG_DFL) };
            }
        }
    }

    /// Restore the default action of each signal set by `install`, except the ones
    /// `settle` holds. SIGALRM keeps its handler, which does nothing once the
    /// command is gone, since a grace period can end just before it is cancelled.
    pub fn restore() {
        CHILD.store(0, Ordering::SeqCst);
        PENDING.store(0, Ordering::SeqCst);
        arm(0);
        let kept = (1u64 << libc::SIGALRM) | HELD.load(Ordering::SeqCst);
        let installed = INSTALLED.fetch_and(kept, Ordering::SeqCst) & !kept;
        for signal in OBSERVED {
            if installed & (1 << signal) != 0 {
                // SAFETY: restores the default action for a signal set above.
                unsafe { libc::signal(signal, libc::SIG_DFL) };
            }
        }
    }
}

#[cfg(not(unix))]
mod signals {
    pub fn install() {}
    pub fn watch(_: u32) {}
    pub fn settle(_: u32) {}
    pub fn reraise(_: Option<std::process::ExitStatus>) {}
    pub fn restore() {}
}

/// The preview of each stream, in display order.
type Previews = Vec<(Stream, Vec<u8>)>;

/// The settings of one captured run.
struct Capture<'a> {
    workspace: &'a Workspace,
    args: &'a [OsString],
    words: &'a [&'a str],
    selection: &'a crate::filters::Selection,
    max_bytes: usize,
    /// `max_bytes` was supplied explicitly rather than chosen by `adaptive`.
    explicit_budget: bool,
    session: &'a str,
    fresh: bool,
    /// Keep no baseline for a later run to compare with (see `Shape::window`).
    window: bool,
    hook: bool,
    agent_footer: bool,
    /// `run --merge` was given.
    merge: bool,
    /// The command wrote both streams to the stdout archive.
    merged: bool,
    /// Replaces secrets in what the agent sees; the archive keeps the original bytes.
    redactor: Option<&'a crate::redact::Redactor>,
    /// The most runs and archive bytes to keep.
    retention: (usize, u64),
    /// Output another program captured, recorded after its command ran.
    origin: Option<&'a Origin>,
    pieces: &'a [RunPiece],
    /// `json_grouping` from the settings.
    json_grouping: bool,
    /// The output can stop in the middle of an item.
    ends_early: bool,
}

impl Capture<'_> {
    /// Scan the archived streams, save the record and return the previews and footer.
    /// `stdout_preview` can replace the preview of a stdout longer than `max_bytes`.
    /// A panic in a summary or view is an error like any other, so the caller still
    /// restores the command's output and exit status.
    fn record(
        &self,
        archive: &mut Archive,
        family: String,
        status: i32,
        stdout_preview: Option<StdoutPreview<'_>>,
    ) -> Result<((Previews, bool), String, Run)> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.record_unguarded(archive, family, status, stdout_preview)
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("an internal error stopped the preview")))
    }

    fn record_unguarded(
        &self,
        archive: &mut Archive,
        family: String,
        status: i32,
        stdout_preview: Option<StdoutPreview<'_>>,
    ) -> Result<((Previews, bool), String, Run)> {
        let Self {
            workspace,
            args,
            words,
            selection,
            max_bytes,
            explicit_budget,
            session,
            fresh,
            window,
            hook,
            agent_footer,
            merge,
            merged,
            redactor,
            retention,
            origin,
            pieces,
            json_grouping,
            ends_early,
        } = *self;
        let directory = archive.directory.clone();
        let lengths = archive.lengths();
        let argv: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let argv_withheld = crate::learn::withhold_command(&argv);
        let mut run = Run {
            pieces: pieces.to_vec(),
            status,
            raw_bytes: lengths.iter().sum(),
            stream_bytes: lengths.clone(),
            produced_bytes: archive.produced(),
            unfinished: false,
            drained: archive.late,
            budget_bytes: Some(max_bytes),
            explicit_budget,
            label: origin.map_or_else(|| history::label(words), |o| o.label.clone()),
            note: origin.map(|o| o.note.clone()).unwrap_or_default(),
            argv: if argv_withheld { Vec::new() } else { argv },
            argv_withheld,
            cwd: workspace
                .cwd
                .strip_prefix(&workspace.root)
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default(),
            session: session.to_owned(),
            redaction: redactor.map(|r| crate::redact::Found::new(r.fingerprint())),
            ..Run::unfinished(archive.id(), &family, "", hook)
        };
        // A file's content, as `git show REV:PATH` prints it, is no patch or commit
        // whatever it holds.
        let prints_file = crate::floor::prints_file(words);
        let mut parser = Parser::new(
            if prints_file { "" } else { &run.family },
            selection.profile,
        );
        if ends_early {
            parser.ends_early();
        }
        let mut lint = (selection.profile == Some(crate::filters::Profile::Lint))
            .then(|| crate::lint::Lint::new(&workspace.root));
        let mut grouping = match selection.profile {
            Some(crate::filters::Profile::Search) => Some(Grouping::Search(
                crate::search::Search::new(&workspace.cwd, words),
            )),
            Some(crate::filters::Profile::Listing) => {
                Some(Grouping::Listing(crate::listing::Listing::new(words)))
            }
            // A joined stderr, output another program cut or recorded, or a
            // stream cut short would make the counts of a log or status wrong.
            None if !merged && origin.is_none() && !ends_early => {
                crate::gitview::View::select(words, max_bytes).map(Grouping::Git)
            }
            _ => None,
        };
        let mut locations = Locations::default();
        let mut previews = Vec::new();
        // Output that reaches the agent directly, so a reformatted stream breaks no
        // program that parses it, and that is not a file's content.
        let direct = hook && !prints_file;
        // The JSON that a hook-routed read such as `gh api` or `kubectl get -o json`
        // prints stays byte for byte when it is shown whole, unless a view that
        // decodes to the same data is smaller (see `json_view`), so the floor leaves
        // its stdout. Claude Code's post-run hook, which records such a command after
        // it ran (`origin`), keeps the floor.
        let json = origin.is_none() && crate::filters::json_output(words);
        // Whether each stream, after redaction, fits the budget and is shown whole,
        // and how many secrets it held.
        let mut fit_whole = [true; 2];
        let mut secrets = [0; 2];
        for (index, stream) in Stream::BOTH.into_iter().enumerate() {
            let before = run.redaction.as_ref().map_or(0, |found| found.count);
            let (preview, fits) = scan(
                workspace,
                &archive.readers[index],
                lengths[index],
                stream,
                max_bytes,
                redactor,
                &mut run,
                &mut Observers {
                    parser: &mut parser,
                    selection,
                    lint: &mut lint,
                    grouping: &mut grouping,
                    locations: &mut locations,
                },
            )
            .with_context(|| format!("read {}", stream.name()))?;
            // A stream shown whole gets the floor too when it reaches the agent
            // directly, since a typed run may feed a pipe, and when that saves more
            // than the footer that then names the run.
            let preview = match std::str::from_utf8(&preview)
                .ok()
                .filter(|_| direct && fits && !(json && index == 0))
                .and_then(|text| crate::floor::compact_stream(text, &workspace.root, stream.name()))
            {
                Some(compact) if compact.len() + 64 < preview.len() => compact.into_bytes(),
                _ => preview,
            };
            fit_whole[index] = fits;
            secrets[index] = run.redaction.as_ref().map_or(0, |found| found.count) - before;
            previews.push((stream, preview));
        }
        locations.finish(&mut run);
        let stdout_fits = fit_whole[0];

        run.summary = parser.finish();
        // A truncated archive holds only a prefix, whose totals can add up and still
        // leave out what came after it.
        if !run.produced_bytes.is_empty() {
            run.summary.cargo = None;
            run.summary.js = None;
        }
        let mut key = Sha256::new();
        for field in std::iter::once(workspace.cwd.as_os_str().as_encoded_bytes())
            .chain(args.iter().map(|arg| arg.as_encoded_bytes()))
            .chain(origin.map(|o| o.command.as_bytes()))
        {
            key.update((field.len() as u64).to_le_bytes());
            key.update(field);
        }
        run.command_key = format!("{:x}", key.clone().finalize());
        key.update(session.as_bytes());
        key.update(max_bytes.to_le_bytes());
        let baselines = workspace.state.join("baselines");
        let baseline = baselines.join(format!("{:x}", key.finalize()));
        let guard = private_dir(&baselines)
            .and_then(|()| lock_state(&workspace.state.join("capture.lock")))
            .map_err(|err| {
                eprintln!("justless: could not track this baseline: {err:#}");
                err
            })
            .ok();
        let previous = if fresh || guard.is_none() {
            None
        } else {
            fs::read_to_string(&baseline)
                .ok()
                .and_then(|id| load(workspace, id.trim()).ok())
                .map(|(_, run)| run)
                .filter(|prior| !prior.unfinished && !run.summary.other_commits(&prior.summary))
        };
        // A truncated archive holds only a prefix, so its digests cannot show that
        // two runs printed the same output.
        run.unchanged = run.produced_bytes.is_empty()
            && previous.as_ref().is_some_and(|p| {
                p.produced_bytes.is_empty() && p.digests == run.digests && p.status == run.status
            });
        // A clean build or lint run, in one line, when every line it printed is
        // Cargo's status.
        let clean = lint
            .as_ref()
            .filter(|_| status == 0 && run.produced_bytes.is_empty())
            .and_then(|lint| lint.clean(&crate::filters::tool_label(args)));
        let lint = lint.and_then(|lint| {
            lint.finish(
                &crate::filters::tool_label(args),
                max_bytes,
                previous.as_ref().map(|r| (r.id.as_str(), &r.summary)),
            )
        });
        if let Some((_, summary)) = &lint {
            run.summary = summary.clone();
        }
        // A Git log or status view has no comparison record, and replaces the
        // preview only when it is smaller, so it takes the room the preview took.
        let grouped = grouping.and_then(|grouping| {
            let previous = previous.as_ref().map(|r| (r.id.as_str(), &r.summary));
            match grouping {
                Grouping::Search(search) => search
                    .finish(&run.family, max_bytes, previous, true)
                    .map(|(groups, summary)| (groups, Some(summary))),
                Grouping::Listing(listing) => listing
                    .finish(&run.family, max_bytes, previous, true)
                    .map(|(groups, summary)| (groups, Some(summary))),
                // Output that fits takes the path it would take without one.
                Grouping::Git(view) => (!stdout_fits)
                    .then(|| view.finish(max_bytes.min(previews[0].1.len().saturating_sub(1))))
                    .flatten()
                    .map(|view| (view, None)),
            }
        });
        if let Some((_, Some(summary))) = &grouped {
            run.summary = summary.clone();
        }
        let shown: usize = previews.iter().map(|(_, bytes)| bytes.len()).sum();
        // `cargo test` totals and structured JS/TS test reports account for every
        // line of stdout, and of stderr when they cover it, so they replace what
        // they cover.
        let whole = run.summary.cargo.is_some() || run.summary.js.is_some();
        let covers_stderr = run
            .summary
            .cargo
            .as_ref()
            .is_some_and(|totals| totals.stderr)
            || run.summary.js.as_ref().is_some_and(|report| report.stderr);
        let covered = if run.summary.js.is_some() {
            let stdout_raw = usize::try_from(lengths[0]).unwrap_or(usize::MAX);
            let stderr_raw = if covers_stderr {
                usize::try_from(lengths[1]).unwrap_or(usize::MAX)
            } else {
                0
            };
            stdout_raw.saturating_add(stderr_raw)
        } else {
            previews[0].1.len()
                + if covers_stderr {
                    previews[1].1.len()
                } else {
                    0
                }
        };
        // The Cargo.lock sections of a patch, described by their dependency changes.
        let locks = (!merged && run.produced_bytes.is_empty() && crate::lockdiff::applies(words))
            .then(|| read_text(&archive.readers[0], lengths[0], crate::lockdiff::LIMIT))
            .flatten()
            .map(|patch| {
                let sections = crate::lockdiff::sections(&patch, &workspace.cwd);
                (patch, sections)
            })
            .filter(|(_, sections)| !sections.is_empty());
        // The patch with those sections described instead, when it fits and is
        // smaller than the preview. A rerun keeps its comparison with the earlier run.
        let lock_view = locks
            .as_ref()
            .filter(|_| previous.is_none())
            .map(|(patch, sections)| crate::lockdiff::view(patch, sections))
            .filter(|view| (view.len() as u64) < lengths[0] && view.len() <= max_bytes);
        let mut flags_shown = false;
        let mut notice = false;
        let mut report = false;
        let plan_fits = run.summary.kind == "plan"
            && (stdout_fits
                || (selection.built_in
                    && previews[0].1.starts_with(b"[")
                    && !previews[0]
                        .1
                        .windows("[… full stream in log …]".len())
                        .any(|w| w == "[… full stream in log …]".as_bytes())
                    && !previews[0]
                        .1
                        .windows("[output clipped.".len())
                        .any(|w| w == "[output clipped.".as_bytes())));
        if let Some(prior) = &previous
            && run.unchanged
        {
            if shown > 64 {
                notice = true;
                let target = usize::from(previews[0].1.is_empty());
                for (_, bytes) in &mut previews {
                    bytes.clear();
                }
                previews[target].1 = format!("Output unchanged since {}.\n", prior.id).into_bytes();
            }
        } else if let Some((findings, _)) = lint {
            // Findings replace both streams: the grouped report names every rule and
            // location, and each group links to its original lines in the full log.
            // Output that fits the budget and is smaller than the report stays
            // verbatim, even on a rerun.
            let lines = fit(findings, max_bytes);
            let clipped = fit_whole.contains(&false);
            if lines.len() < shown || clipped {
                previews[0].1 = lines.into_bytes();
                previews[1].1.clear();
                report = true;
                run.replaced = Some("both".to_owned());
            }
        } else if let Some(line) = clean.filter(|line| line.len() + 64 < shown) {
            // Like findings, the line covers both streams. It saves more than the
            // footer that then names the run, as for the floor.
            previews[0].1 = line.into_bytes();
            previews[1].1.clear();
            report = true;
            run.replaced = Some("both".to_owned());
        } else if let Some((groups, summary)) = grouped {
            // Search and listing groups replace a large stdout only. Smaller output stays
            // verbatim, and stderr keeps messages such as unreadable directories.
            if !stdout_fits
                && !groups.is_empty()
                && (summary.is_some() || groups.len() < previews[0].1.len())
            {
                previews[0].1 = fit(groups, max_bytes).into_bytes();
                report = true;
                run.replaced = Some("stdout".to_owned());
            }
        } else if (!merged || whole)
            && (!plan_fits || previous.is_some())
            && let Some(summary) = run.summary.render(
                previous.as_ref().map(|r| (r.id.as_str(), &r.summary)),
                run.status,
            )
            && (covers_stderr && !fit_whole[1]
                || !stdout_fits
                || previous.is_some()
                || (run.summary.kind != "plan" && summary.len() < covered))
        {
            // A merged stdout also holds the command's stderr, which a test, diff or
            // inventory summary of stdout would drop, so that run keeps its preview,
            // unless the summary accounts for every line, as `cargo test` totals do.
            let summary = match &locks {
                Some((_, sections)) if previous.is_none() && run.summary.kind == "diff" => {
                    flags_shown = true;
                    crate::lockdiff::summarize(&run.summary, sections, max_bytes / 4)
                }
                _ => summary,
            };
            previews[0].1 = fit(summary, max_bytes).into_bytes();
            if covers_stderr {
                previews[1].1.clear();
            }
            report = true;
            run.replaced = Some(if covers_stderr { "both" } else { "stdout" }.to_owned());
        } else if run.produced_bytes.is_empty()
            && is_log_stream(words, selection.profile)
            && let Some(folded) =
                log_view(&archive.readers, &lengths, redactor, &secrets, max_bytes)
            && (fit_whole.contains(&false) || folded.len() + 64 < shown)
            && (folded.len() < shown || fit_whole.contains(&false))
        {
            previews[0].1 = folded.into_bytes();
            previews[1].1.clear();
            report = true;
            run.replaced = Some("both".to_owned());
        } else if !merged
            && run.produced_bytes.is_empty()
            && selection.profile.is_none()
            && run.summary.kind.is_empty()
            && let Some(view) = extra_inventory_view(
                words,
                &archive.readers[0],
                lengths[0],
                redactor,
                secrets[0] > 0,
                max_bytes,
            )
            && (!stdout_fits || view.len() + 64 < previews[0].1.len())
        {
            previews[0].1 = view.into_bytes();
            report = true;
            run.replaced = Some("stdout".to_owned());
        } else if !merged
            && run.produced_bytes.is_empty()
            && selection.profile.is_none()
            && run.summary.kind.is_empty()
            && let Some(view) = cli_view(
                words,
                &archive.readers[0],
                lengths[0],
                redactor,
                secrets[0] > 0,
                max_bytes,
            )
            && (!stdout_fits || view.len() + 64 < previews[0].1.len())
        {
            previews[0].1 = view.into_bytes();
            report = true;
            run.replaced = Some("stdout".to_owned());
        } else if !merged
            && selection.profile.is_none()
            && run.summary.kind.is_empty()
            && run.produced_bytes.is_empty()
            && origin.is_none_or(|o| o.outline.is_none())
            // JSON reformatted for reading would break a program that parses it, so
            // only output that reaches the agent directly, or that is clipped anyway,
            // is shown as tables. A stream that fits takes a view only when it saves
            // more than the footer that then names the run, as for the floor.
            && (direct || !stdout_fits)
            && let Some(view) = json_view(
                &archive.readers[0],
                lengths[0],
                redactor.filter(|_| secrets[0] > 0),
                max_bytes,
                json_grouping,
            )
            && (!stdout_fits || view.len() + 64 < previews[0].1.len())
        {
            previews[0].1 = view;
            run.replaced = Some("stdout".to_owned());
        }
        if let Some(view) = lock_view
            && !run.unchanged
            && view.len() < previews[0].1.len()
        {
            flags_shown = true;
            previews[0].1 = view.into_bytes();
            report = true;
            run.replaced = Some("stdout".to_owned());
        }
        // A source or checksum that changed on a locked version is always named, also
        // above a verbatim patch or a clipped preview. The flags take at most a
        // quarter of the budget, and the preview below them gives way.
        if let Some((_, sections)) = &locks
            && !flags_shown
        {
            let flags = crate::lockdiff::flag_lines(sections, max_bytes / 4);
            if !flags.is_empty() {
                let index = usize::from(previews[0].1.is_empty() && !previews[1].1.is_empty());
                let preview = &mut previews[index].1;
                preview.splice(0..0, flags.into_bytes());
                fit_lines(preview, max_bytes);
            }
        }
        if let Some(shape_stdout) = stdout_preview
            && !run.unchanged
            && !stdout_fits
        {
            // The shaper cuts lines and windows out of stdout, and a cut through a
            // secret can leave a part that no longer looks like one. When stdout held
            // a secret, it reads a redacted copy, whose line numbers match the archive.
            let copy = redactor
                .filter(|_| secrets[0] > 0)
                .map(|redactor| {
                    redacted_copy(&archive.readers[0], lengths[0], &directory, redactor)
                })
                .transpose();
            let shaped = copy.and_then(|copy| {
                let path = copy.as_ref().map_or_else(
                    || directory.join("stdout"),
                    |copy| copy.path().to_path_buf(),
                );
                shape_stdout(&path, &run.id)
            });
            match shaped {
                Ok(Some(shaped)) if shaped.len() <= max_bytes => {
                    previews[0].1 = shaped;
                    if run.replaced.is_none() {
                        run.replaced = Some("stdout".to_owned());
                    }
                }
                Ok(Some(_)) => eprintln!("justless: the shaped stdout preview exceeded its budget"),
                Ok(None) => {}
                Err(error) => eprintln!("justless: {error:#}"),
            }
        }
        let justless = workspace.justless();
        let source_outline = origin.and_then(|o| o.outline.as_ref());
        if let Some(outline) = source_outline {
            previews[0].1 =
                format!("{}Exact: {justless} log {}\n", outline.text, run.id).into_bytes();
            run.replaced = Some("stdout".to_owned());
        }
        if let Some(redactor) = redactor {
            // Reports can quote an earlier record, such as a rerun's previous failures.
            for (_, preview) in &mut previews {
                if let Cow::Owned(redacted) = redactor.bytes(preview) {
                    *preview = redacted;
                }
            }
            // A shaped preview's pointers start with `justless --root ROOT`. A root
            // named like a secret is no secret of the command's output, and the
            // footer prints it as it is, so restore it where it was hidden and the
            // pointer still runs.
            if let Cow::Owned(hidden) = redactor.text(&justless) {
                let hidden = regex::bytes::Regex::new(&regex::escape(&hidden))?;
                for (_, preview) in &mut previews {
                    if let Cow::Owned(restored) =
                        hidden.replace_all(preview, regex::bytes::NoExpand(justless.as_bytes()))
                    {
                        *preview = restored;
                    }
                }
            }
        }
        run.preview_bytes = previews.iter().map(|(_, s)| s.len()).sum();
        // A `--merge` search or listing kept its streams apart, so its stderr follows
        // stdout rather than interleaving with it.
        let split = merge && !merged && previews.iter().all(|(_, bytes)| !bytes.is_empty());
        let mut footer = footer(
            &run,
            &previews,
            &archive.readers,
            Shown {
                agent: agent_footer,
                selection,
                redactor,
                notice,
                split,
                justless: &justless,
            },
        )?;
        if let Some(outline) = source_outline {
            run.raw_bytes = outline.baseline as u64;
            if outline.trim_footer && footer.ends_with('\n') {
                footer.pop();
            }
        }
        run.footer_bytes = footer.len();
        write_json(&directory.join("record.json"), &run).context("save run record")?;
        archive.keep();
        archive.finish();
        archive.publish()?;
        if let Some(prior) = previous.as_ref().filter(|_| run.unchanged) {
            archive.share(&workspace.state.join("runs").join(&prior.id), prior, &run);
        }
        if guard.is_some() {
            if !window && let Err(error) = crate::workspace::write_text(&baseline, &run.id) {
                eprintln!("justless: could not update run baseline: {error:#}");
            }
            history::prune(workspace, &run.id, retention.0, retention.1);
        }
        drop(guard);
        Ok(((previews, report), footer, run))
    }
}

/// The first `length` bytes of a stream with its secrets replaced, in a temporary
/// file in the run directory. Redaction keeps every line break, so line numbers
/// match the archive.
fn redacted_copy(
    mut file: &File,
    length: u64,
    directory: &Path,
    redactor: &crate::redact::Redactor,
) -> Result<tempfile::NamedTempFile> {
    let mut copy = Builder::new()
        .prefix(".redacted-")
        .tempfile_in(directory)
        .context("create a redacted copy of stdout")?;
    file.seek(SeekFrom::Start(0))?;
    io::copy(
        &mut crate::redact::Reader::new(file.take(length), Some(redactor)),
        copy.as_file_mut(),
    )
    .context("write a redacted copy of stdout")?;
    Ok(copy)
}

/// How a run's previews were produced, for its footer.
struct Shown<'a> {
    /// Print the agent footer.
    agent: bool,
    selection: &'a crate::filters::Selection,
    redactor: Option<&'a crate::redact::Redactor>,
    /// The unchanged notice replaced the previews.
    notice: bool,
    /// Both streams have output that a `--merge` run kept apart.
    split: bool,
    /// How a command that reopens the run starts. See [`Workspace::justless`].
    justless: &'a str,
}

/// The line printed after the preview. A command typed by a person keeps the full
/// footer. A hook-routed command or `exec` script whose output reached the agent
/// verbatim prints nothing, since the agent already has every byte; otherwise one
/// short marker names the run so the agent can recover what was left out. Shown
/// secrets are always counted, so the agent knows Justless wrote the markers and
/// which run holds the originals.
fn footer(
    run: &Run,
    previews: &[(Stream, Vec<u8>)],
    readers: &[File],
    shown: Shown,
) -> Result<String> {
    let id = &run.id;
    let secrets = run.redaction.as_ref().map_or(0, |found| found.count);
    let redacted = match secrets {
        _ if shown.notice => String::new(),
        0 => String::new(),
        1 => " · 1 secret redacted".to_owned(),
        n => format!(" · {n} secrets redacted"),
    };
    // A stream past `max_run_bytes` is never shown in full, so the marker always
    // follows, even after a hook-routed run.
    let truncation = run.truncation();
    if !shown.agent {
        return Ok(format!(
            "\n[justless {id}] exit {} · {} of {} output bytes{redacted} · {} log {id}\n{truncation}",
            run.status, run.preview_bytes, run.raw_bytes, shown.justless
        ));
    }
    // Compare the printed bytes with what the command wrote before it exited: equal
    // sizes alone can hide a replaced or regrouped stream. A background process can
    // keep adding to the archive, so only the bytes counted at exit are read. With
    // secrets, compare with the redacted archive.
    let redactor = shown.redactor.filter(|_| secrets > 0);
    let mut verbatim =
        truncation.is_empty() && (secrets > 0 || run.preview_bytes as u64 == run.raw_bytes);
    for (stream, preview) in previews {
        if !verbatim {
            break;
        }
        let index = stream.index();
        let (Some(&length), Some(mut file)) = (run.stream_bytes.get(index), readers.get(index))
        else {
            verbatim = false;
            break;
        };
        file.seek(SeekFrom::Start(0))?;
        verbatim = match redactor {
            None => {
                length == preview.len() as u64 && {
                    let mut stored = Vec::with_capacity(preview.len());
                    file.take(length).read_to_end(&mut stored)?;
                    stored == *preview
                }
            }
            Some(redactor) => redactor.matches(file.take(length), preview)?,
        };
    }
    // A search that matched nothing exits 1 with no output; that is an answer, not a failure.
    let empty_search = run.status == 1
        && run.raw_bytes == 0
        && shown.selection.profile == Some(crate::filters::Profile::Search);
    let separator = separator(
        previews
            .iter()
            .rev()
            .find_map(|(_, bytes)| bytes.last().copied()),
    );
    let only_count = || {
        if redacted.is_empty() {
            String::new()
        } else {
            format!("{separator}[justless {id}{redacted}]\n")
        }
    };
    let line = if verbatim && shown.split {
        let exit = if run.status == 0 {
            String::new()
        } else {
            format!(" · exit {}", run.status)
        };
        format!("{separator}[justless {id}{redacted} · stderr after stdout{exit}]\n")
    } else if verbatim && (run.status == 0 || empty_search) {
        only_count()
    } else if verbatim || run.unchanged {
        if run.status == 0 {
            only_count()
        } else {
            format!(
                "{separator}[justless {id}{redacted} · exit {}]\n",
                run.status
            )
        }
    } else {
        let exit = if run.status == 0 {
            String::new()
        } else {
            format!(" · exit {}", run.status)
        };
        format!(
            "{separator}[justless {id} · {} of {} shown{redacted}{exit}]\n",
            history::human_bytes(run.preview_bytes as u64),
            history::human_bytes(run.raw_bytes)
        )
    };
    // The marker names the run, and `log`, `search` and `context --run` look for it
    // in the workspace found from where the agent works. When that is another one,
    // as for a hook-routed command inside a nested repository, say how to reach it.
    let reopen = if line.is_empty() || shown.justless == "justless" {
        String::new()
    } else {
        format!("[reopen with {} log {id}]\n", shown.justless)
    };
    Ok(line + &reopen + &truncation)
}

/// The first `length` bytes of a stream as text, when they are at most `limit` and
/// valid UTF-8.
fn read_text(mut file: &File, length: u64, limit: u64) -> Option<String> {
    if length > limit {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.take(length).read_to_end(&mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

/// Whether `words` / `profile` runs a structured container, Kubernetes or
/// systemd log command eligible for `logview` template folding.
fn is_log_stream(words: &[&str], profile: Option<crate::filters::Profile>) -> bool {
    if profile == Some(crate::filters::Profile::Logs) {
        return true;
    }
    let Some(unwrapped) = crate::wrappers::hook_effective(words) else {
        return false;
    };
    let refs: Vec<&str> = unwrapped.iter().map(String::as_str).collect();
    let tool = crate::filters::unwrap_tool(&refs);
    tool.first()
        .and_then(|p| Path::new(p).file_name()?.to_str())
        == Some("journalctl")
}

/// Fold structured container, Kubernetes, JSON/NDJSON or `journalctl` logs via
/// `logview`, reading redacted stream copies up to 2 MiB each.
fn log_view(
    readers: &[File],
    lengths: &[u64],
    redactor: Option<&crate::redact::Redactor>,
    secrets: &[u64; 2],
    max_bytes: usize,
) -> Option<String> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    if readers.len() < 2 || lengths.len() < 2 {
        return None;
    }
    let mut streams = [String::new(), String::new()];
    for i in 0..2 {
        if lengths[i] == 0 {
            continue;
        }
        let text = read_text(&readers[i], lengths[i], LIMIT)?;
        streams[i] = match redactor.filter(|_| secrets[i] > 0) {
            Some(redactor) => redactor.text(&text).into_owned(),
            None => text,
        };
    }
    crate::logview::format(&streams[0], &streams[1], max_bytes)
}

/// Format display-table DevOps inventories handled by `inventory::{events,
/// helm_list, images, resources}` (`kubectl`/`oc get events|svc|ing|pvc|cj`,
/// `helm list`, `docker`/`podman images`), redacting before formatting.
fn extra_inventory_view(
    words: &[&str],
    reader: &File,
    length: u64,
    redactor: Option<&crate::redact::Redactor>,
    has_secrets: bool,
    max_bytes: usize,
) -> Option<String> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    if length == 0 || length > LIMIT {
        return None;
    }
    let unwrapped = crate::wrappers::hook_effective(words)?;
    let refs: Vec<&str> = unwrapped.iter().map(String::as_str).collect();
    let tool = crate::filters::unwrap_tool(&refs);
    let (&program, mut args) = tool.split_first()?;
    let program = Path::new(program).file_name()?.to_str()?;
    let has_machine_flag = |tail: &[&str], blocked: &[&str]| {
        tail.iter().any(|arg| {
            let flag = arg.split_once('=').map_or(*arg, |(f, _)| f);
            blocked.contains(&flag)
        })
    };
    enum Kind<'a> {
        Events,
        HelmList,
        Images,
        Resource(&'a str),
    }
    let kind = match program {
        "kubectl" | "oc" => {
            while let Some((&arg, rest)) = args.split_first() {
                if ["--namespace", "-n", "--context"].contains(&arg)
                    && rest.first().is_some_and(|v| !v.starts_with('-'))
                {
                    args = &rest[1..];
                } else if arg.starts_with("--namespace=") || arg.starts_with("--context=") {
                    args = rest;
                } else {
                    break;
                }
            }
            let ["get", resource, tail @ ..] = args else {
                return None;
            };
            if has_machine_flag(tail, &["-o", "--output", "-w", "--watch", "--no-headers"]) {
                return None;
            }
            let base = resource.split('.').next().unwrap_or("");
            match base {
                "ev" | "event" | "events" => Kind::Events,
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
                | "cronjobs" => Kind::Resource(resource),
                _ => return None,
            }
        }
        "helm" => {
            let ["list" | "ls", tail @ ..] = args else {
                return None;
            };
            if has_machine_flag(tail, &["-o", "--output", "-q", "--short"]) {
                return None;
            }
            Kind::HelmList
        }
        "docker" | "podman" => {
            let tail = match args {
                ["images", tail @ ..] | ["image", "ls" | "list", tail @ ..] => tail,
                _ => return None,
            };
            if has_machine_flag(tail, &["--format", "-q", "--quiet"]) {
                return None;
            }
            Kind::Images
        }
        _ => return None,
    };
    let text = read_text(reader, length, LIMIT)?;
    let text = match redactor.filter(|_| has_secrets) {
        Some(redactor) => redactor.text(&text).into_owned(),
        None => text,
    };
    match kind {
        Kind::Events => crate::inventory::events(&text, max_bytes),
        Kind::HelmList => crate::inventory::helm_list(&text, max_bytes),
        Kind::Images => crate::inventory::images(&text, max_bytes),
        Kind::Resource(resource) => crate::inventory::resources(resource, &text, max_bytes),
    }
}

/// Format high-volume CLI views handled by `cli_views::format` (`kubectl describe`,
/// `gh pr checks`/`run view`/lists, security audits, `terraform state list`,
/// outdated/package lists), redacting before formatting.
fn cli_view(
    words: &[&str],
    reader: &File,
    length: u64,
    redactor: Option<&crate::redact::Redactor>,
    has_secrets: bool,
    max_bytes: usize,
) -> Option<String> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    if length == 0 || length > LIMIT {
        return None;
    }
    crate::cli_views::kind_tag(words)?;
    let text = read_text(reader, length, LIMIT)?;
    let text = match redactor.filter(|_| has_secrets) {
        Some(redactor) => redactor.text(&text).into_owned(),
        None => text,
    };
    crate::cli_views::format(words, &text, max_bytes)
}

/// Stdout shown as JSON tables or compact JSON, clipped at whole lines to
/// `max_bytes`, when a view is smaller and decodes to the same data. See
/// `jsonview`.
fn json_view(
    mut file: &File,
    length: u64,
    redactor: Option<&crate::redact::Redactor>,
    max_bytes: usize,
    group: bool,
) -> Option<Vec<u8>> {
    if length > crate::jsonview::LIMIT as u64 {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    let mut start = [0; 64];
    let read = file.take(length).read(&mut start).ok()?;
    if !start[..read]
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|&byte| byte == b'[' || byte == b'{')
    {
        return None;
    }
    bytes.extend_from_slice(&start[..read]);
    file.take(length - read as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    // Parse what the agent may see, so a secret never reaches a table cell.
    if let Some(redactor) = redactor {
        bytes = redactor.bytes(&bytes).into_owned();
    }
    let text = String::from_utf8(bytes).ok()?;
    // Cargo's compiler messages keep their rendered diagnostics.
    if text.contains("\"compiler-message\"") {
        return None;
    }
    // The smallest view that fits or can be cut: a compact document on one line
    // longer than the budget cannot be, and the tables then still can.
    crate::jsonview::views(&text, group)
        .into_iter()
        .find_map(|view| clip_view(view, max_bytes))
}

/// A view cut at a whole line to `max_bytes`, with a marker, unless that leaves
/// too little of it.
fn clip_view(view: String, max_bytes: usize) -> Option<Vec<u8>> {
    if view.len() <= max_bytes {
        return Some(view.into_bytes());
    }
    let lines = view.split_inclusive('\n').count();
    let mut shown = String::new();
    for (index, line) in view.split_inclusive('\n').enumerate() {
        let marker = format!(
            "[{} more lines of this view in the full log]\n",
            lines - index
        );
        if shown.len() + line.len() + marker.len() > max_bytes {
            // The legend and a table header alone tell the agent nothing, and
            // neither does a view cut before rows too long for the budget, such as
            // whole Kubernetes objects: the ordinary preview shows more of them.
            if index < 3 || shown.len() < max_bytes / 2 {
                return None;
            }
            shown.push_str(&marker);
            break;
        }
        shown.push_str(line);
    }
    Some(shown.into_bytes())
}

/// A preview cut at a line end to `max_bytes`, with a marker.
fn fit_lines(preview: &mut Vec<u8>, max_bytes: usize) {
    const MORE: &[u8] = b"[more in full log]\n";
    if preview.len() <= max_bytes {
        return;
    }
    let room = max_bytes.saturating_sub(MORE.len() + 1);
    let end = match preview[..room].iter().rposition(|&byte| byte == b'\n') {
        Some(at) => at + 1,
        // A first line longer than the budget is cut inside it, at a character.
        None => (0..=room)
            .rev()
            .find(|&at| preview[at] & 0xC0 != 0x80)
            .unwrap_or(0),
    };
    preview.truncate(end);
    if !preview.is_empty() && !preview.ends_with(b"\n") {
        preview.push(b'\n');
    }
    preview.extend_from_slice(MORE);
}

fn fit(mut report: String, max_bytes: usize) -> String {
    if report.len() > max_bytes {
        let tail = "\n[more in full log]\n";
        report.truncate(report.floor_char_boundary(max_bytes - tail.len()));
        report.push_str(tail);
    }
    report
}

// Hash the exact bytes during the same pass that extracts the bounded preview.
struct HashedFile<R> {
    file: R,
    digest: Sha256,
}
impl<R: Read> Read for HashedFile<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read(buffer)?;
        self.digest.update(&buffer[..count]);
        Ok(count)
    }
}

/// Line consumers that see every captured line during the single scan of a stream.
struct Observers<'a> {
    parser: &'a mut Parser,
    selection: &'a crate::filters::Selection,
    lint: &'a mut Option<crate::lint::Lint>,
    grouping: &'a mut Option<Grouping>,
    locations: &'a mut Locations,
}

/// Groups stdout of a search by file, or of a file listing by directory, or keeps
/// the newest commits of a log or groups a status by state.
enum Grouping {
    Search(crate::search::Search),
    Listing(crate::listing::Listing),
    Git(crate::gitview::View),
}

/// Read one stream once, through the redactor, and return its preview and whether
/// the redacted stream fit the budget whole.
#[allow(clippy::too_many_arguments)]
fn scan(
    workspace: &Workspace,
    mut file: &File,
    length: u64,
    stream: Stream,
    max_bytes: usize,
    redactor: Option<&crate::redact::Redactor>,
    run: &mut Run,
    observers: &mut Observers,
) -> Result<(Vec<u8>, bool)> {
    let Observers {
        parser,
        selection,
        lint,
        grouping,
        locations,
    } = observers;
    const DIAGNOSTICS: &[u8] = b"\n[diagnostics]\n";
    const CLIPPED: &[u8] = b"\n[output clipped. Full log retained.]\n";
    const PROGRESS_RESERVE: usize = 48;
    let mut truncated = length > max_bytes as u64;
    // Redaction can bring output under the budget, as when a private key body
    // collapses to one marker, so keep the redacted stream while it still fits.
    let mut whole = (truncated && redactor.is_some()).then(Vec::new);
    let progress_reserve = if truncated && selection.omit.is_some() {
        PROGRESS_RESERVE
    } else {
        0
    };
    let payload = max_bytes.saturating_sub(DIAGNOSTICS.len() + CLIPPED.len() + progress_reserve);
    let section_budget = payload / 2;
    // Store headroom so entries that end up omitted because they already appear
    // verbatim in `head` or `tail` never crowd out mid-stream diagnostics.
    let section_store_budget = section_budget.saturating_mul(2).saturating_add(payload / 2);
    file.seek(SeekFrom::Start(0))?;
    // Every consumer below sees redacted lines; the digest covers the raw bytes.
    let mut file = crate::redact::Reader::new(
        HashedFile {
            file: file.take(length),
            digest: Sha256::new(),
        },
        redactor,
    );
    let mut raw = Vec::new();
    let mut end = Vec::new();
    let mut preview = Vec::new();
    let mut head = Excerpt::new(payload / 6);
    let mut head_covered = LineRanges::default();
    let mut section = Section::default();
    let mut failures = Failures::default();
    let tail_limit = payload / 3;
    let mut tail: VecDeque<TailItem> = VecDeque::new();
    let mut tail_bytes = 0;
    let mut line_no = 0;
    let mut filter = crate::filters::LineFilter::new(selection, max_bytes);
    // Display-only compaction of the head and tail; see `stacktrace` and `floor`.
    let mut stack_stream =
        crate::stacktrace::StreamCollapser::new(Some(&workspace.root), Some(stream.name()));
    let mut stacked = Vec::new();
    let mut floor = crate::floor::Floor::default();
    let mut floored = Vec::new();
    let mut progress_omitted: usize = 0;
    let mut progress_context_until: usize = 0;

    // A stream that fits is shown whole, including lines that markers made longer.
    let line_limit = if truncated {
        SCAN_LINE_LIMIT
    } else {
        usize::MAX
    };
    while let Some(cut) = read_line(&mut file, &mut raw, line_limit, &mut end, tail_limit)? {
        line_no += 1;
        let text = String::from_utf8_lossy(&raw);
        let text = ANSI.replace_all(&text, "");
        filter.observe(&text, line_no);
        if let Some(lint) = lint.as_mut() {
            lint.observe(&text, line_no, stream);
        }
        if matches!(stream, Stream::Stdout) {
            parser.observe(&text, line_no);
            match grouping.as_mut() {
                Some(Grouping::Search(search)) => search.observe(&text, line_no),
                Some(Grouping::Listing(listing)) => listing.observe(&text, line_no),
                Some(Grouping::Git(view)) => view.observe(&text, line_no),
                None => {}
            }
        } else {
            parser.observe_stderr(&text, line_no);
        }
        let structured = cargo_message(&text);
        let rendered = structured.as_ref().and_then(rendered_message);
        let shown = rendered.as_deref().unwrap_or(&text);
        let line = shown.trim_end_matches(['\n', '\r']);
        let failure = failures.observe(line, shown, line_no, |block| {
            if truncated {
                section.add(block, &workspace.root, stream.name(), section_store_budget);
            }
        });
        let patch = failures.patch;
        let found = if patch {
            Vec::new()
        } else if let Some(message) = &structured {
            json_locations(workspace, locations, message)
        } else {
            source_location(workspace, locations, &text)
                .into_iter()
                .collect()
        };
        let has_location = !found.is_empty();
        for location in found {
            locations.add(workspace, location, line_no, stream, failure);
        }

        if let Some(buffer) = whole.as_mut() {
            if !cut && buffer.len() + raw.len() <= max_bytes {
                buffer.extend_from_slice(&raw);
            } else {
                whole = None;
            }
        }
        if !truncated {
            preview.extend_from_slice(&raw);
        } else {
            let trimmed = line.trim_start();
            let diag_body = crate::filters::log_dedup_key(trimmed);
            let is_warning = diag_body.starts_with("warning")
                || diag_body.starts_with("WARNING")
                || diag_body.starts_with("WARN ")
                || diag_body.starts_with("WARN:")
                || diag_body.starts_with("[WARN");
            if !failure && !patch && (has_location || is_warning) {
                section.add(
                    Block::new(Tier::Note, line, shown, line_no),
                    &workspace.root,
                    stream.name(),
                    section_store_budget,
                );
            }
            let is_diag = failure || crate::filters::diagnostic(trimmed);
            if is_diag {
                progress_context_until = line_no.saturating_add(4);
            }
            let omit_as_progress = !cut
                && !is_diag
                && !patch
                && !has_location
                && line_no > progress_context_until
                && selection
                    .omit
                    .as_ref()
                    .is_some_and(|re| re.is_match(line.trim()));
            if omit_as_progress {
                progress_omitted += 1;
                stack_stream.flush(&mut stacked);
                emit_stacked(
                    &mut stacked,
                    &mut floor,
                    &mut floored,
                    &mut head,
                    &mut head_covered,
                    &mut tail,
                    &mut tail_bytes,
                    tail_limit,
                );
            } else if cut {
                stack_stream.flush(&mut stacked);
                emit_stacked(
                    &mut stacked,
                    &mut floor,
                    &mut floored,
                    &mut head,
                    &mut head_covered,
                    &mut tail,
                    &mut tail_bytes,
                    tail_limit,
                );
                floor.flush(&mut floored);
                for (line, repeats) in floored.drain(..) {
                    head.push(&line);
                    keep_tail(
                        &mut tail,
                        &mut tail_bytes,
                        tail_limit,
                        &line,
                        false,
                        repeats,
                        None,
                    );
                }
                // Only the start of an enormous line was scanned. Show its real end;
                // the tail ends where the stream ends, even inside such a line.
                head.push(shown);
                let wanted = tail_limit.saturating_sub(end.len());
                let mut suffix = raw[raw.len().saturating_sub(wanted)..].to_vec();
                suffix.extend_from_slice(&end);
                let suffix = String::from_utf8_lossy(&suffix);
                let suffix = ANSI.replace_all(&suffix, "");
                keep_tail(
                    &mut tail,
                    &mut tail_bytes,
                    tail_limit,
                    &suffix,
                    true,
                    false,
                    None,
                );
            } else {
                stack_stream.push(shown, line_no, failure, &mut stacked);
                emit_stacked(
                    &mut stacked,
                    &mut floor,
                    &mut floored,
                    &mut head,
                    &mut head_covered,
                    &mut tail,
                    &mut tail_bytes,
                    tail_limit,
                );
            }
        }
        raw.clear();
        end.clear();
    }
    if let Some(open) = failures.block.take()
        && truncated
    {
        section.add(open, &workspace.root, stream.name(), section_store_budget);
    }
    if truncated {
        stack_stream.flush(&mut stacked);
        emit_stacked(
            &mut stacked,
            &mut floor,
            &mut floored,
            &mut head,
            &mut head_covered,
            &mut tail,
            &mut tail_bytes,
            tail_limit,
        );
        floor.flush(&mut floored);
        for (line, repeats) in floored.drain(..) {
            head.push(&line);
            keep_tail(
                &mut tail,
                &mut tail_bytes,
                tail_limit,
                &line,
                false,
                repeats,
                None,
            );
        }
    }

    if let Some(buffer) = whole {
        truncated = false;
        preview = buffer;
    }
    if truncated {
        let mut covered = head_covered;
        for item in &tail {
            if let Some(range) = item.range {
                covered.add(range);
            }
        }
        if progress_omitted > 0 {
            let note = if progress_omitted == 1 {
                "[1 progress line omitted]\n".to_owned()
            } else {
                format!("[{progress_omitted} progress lines omitted]\n")
            };
            preview.extend_from_slice(note.as_bytes());
        }
        preview.extend_from_slice(head.text.as_bytes());
        let diagnostics = section.render(section_budget, stream.name(), &covered);
        if !diagnostics.is_empty() {
            preview.extend_from_slice(DIAGNOSTICS);
            preview.extend_from_slice(diagnostics.as_bytes());
        }
        preview.extend_from_slice(CLIPPED);
        preview.extend(tail.into_iter().flat_map(|item| item.text.into_bytes()));
    }
    if let Some(found) = run.redaction.as_mut() {
        found.count += file.found().count;
    }
    run.digests
        .push(format!("{:x}", file.into_inner().digest.finalize()));
    // A tool's built-in progress lines only ever shorten a preview: when the
    // other lines still do not fit, the ordinary preview, which puts failures
    // before warnings (and now omits progress lines from head/tail too), is shown.
    if let Some((filtered, whole)) = filter.finish()
        && (filtered.len() < preview.len() || truncated && filtered.len() <= max_bytes)
        && (whole || !selection.built_in)
    {
        return Ok((filtered, !truncated));
    }
    Ok((preview, !truncated))
}

/// One entry in the tail ring buffer, carrying the 1-based archive line range
/// it represents verbatim (or as a collapsed stack summary) when un-cut.
struct TailItem {
    text: String,
    range: Option<(usize, usize)>,
}

/// Sorted, merged set of 1-based inclusive archive line ranges already shown in
/// `head` or `tail`, used to suppress duplicate `[diagnostics]` entries.
#[derive(Default)]
struct LineRanges {
    ranges: Vec<(usize, usize)>,
}

impl LineRanges {
    fn add(&mut self, (start, end): (usize, usize)) {
        if start == 0 || end < start {
            return;
        }
        if let Some(last) = self.ranges.last_mut()
            && start <= last.1.saturating_add(1)
            && end >= last.0
        {
            last.0 = last.0.min(start);
            last.1 = last.1.max(end);
            return;
        }
        self.ranges.push((start, end));
    }

    fn covers(&self, start: usize, end: usize) -> bool {
        if start == 0 || end < start {
            return false;
        }
        let mut cursor = start;
        // `ranges` is almost always in ascending order (head then tail), but
        // scan cleanly so overlapping/adjacent ranges always merge logically.
        loop {
            let mut advanced = false;
            for &(a, b) in &self.ranges {
                if a <= cursor && b >= cursor {
                    if b >= end {
                        return true;
                    }
                    cursor = b.saturating_add(1);
                    advanced = true;
                }
            }
            if !advanced {
                return false;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_stacked(
    stacked: &mut Vec<crate::stacktrace::OutputLine>,
    floor: &mut crate::floor::Floor,
    floored: &mut Vec<crate::floor::Shown>,
    head: &mut Excerpt,
    head_covered: &mut LineRanges,
    tail: &mut VecDeque<TailItem>,
    tail_bytes: &mut usize,
    tail_limit: usize,
) {
    for st in stacked.drain(..) {
        floor.push(&st.text, st.marked, floored);
        let emitted = floored.len();
        for (idx, (line, repeats)) in floored.drain(..).enumerate() {
            let range = (!repeats && idx + 1 == emitted && line == st.text).then_some(st.range);
            if head.push(&line)
                && let Some(r) = range
            {
                head_covered.add(r);
            }
            keep_tail(tail, tail_bytes, tail_limit, &line, false, repeats, range);
        }
    }
}

/// Add a line to the tail excerpt, dropping its oldest lines past `limit`. `cut`
/// marks a line whose start was left out, and `repeats` the floor's count of
/// repeats of the line before, which leaves the tail with that line.
fn keep_tail(
    tail: &mut VecDeque<TailItem>,
    bytes: &mut usize,
    limit: usize,
    line: &str,
    cut: bool,
    repeats: bool,
    range: Option<(usize, usize)>,
) {
    let mut excerpt = Excerpt::new(limit);
    excerpt.push_end(line, cut);
    if !excerpt.text.is_empty() {
        let verbatim = !cut && line.len() <= limit;
        let item_range = verbatim.then_some(range).flatten();
        *bytes += excerpt.text.len();
        match tail.back_mut() {
            Some(last) if repeats => last.text.push_str(&excerpt.text),
            _ => tail.push_back(TailItem {
                text: excerpt.text,
                range: item_range,
            }),
        }
        while *bytes > limit {
            *bytes -= tail.pop_front().unwrap().text.len();
        }
    }
}

/// Read one line. A line longer than `limit` keeps its first `limit` bytes in
/// `prefix` and its last `end_limit` bytes in `end`, and returns `Some(true)`.
fn read_line(
    file: &mut impl BufRead,
    prefix: &mut Vec<u8>,
    limit: usize,
    end: &mut Vec<u8>,
    end_limit: usize,
) -> io::Result<Option<bool>> {
    let mut read = false;
    let mut cut = false;
    loop {
        let chunk = file.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        read = true;
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let length = newline.map_or(chunk.len(), |index| index + 1);
        let kept = length.min(limit.saturating_sub(prefix.len()));
        prefix.extend_from_slice(&chunk[..kept]);
        if kept < length {
            cut = true;
            end.extend_from_slice(&chunk[kept..length]);
            if end.len() > end_limit.saturating_mul(2).max(4096) {
                end.drain(..end.len() - end_limit);
            }
        }
        file.consume(length);
        if newline.is_some() {
            break;
        }
    }
    if end.len() > end_limit {
        end.drain(..end.len() - end_limit);
    }
    Ok(read.then_some(cut))
}

/// Marks a line that was cut to fit.
const CUT: &str = "…";

/// Whole lines up to a byte limit. The first line that does not fit is cut and
/// marked, and nothing follows it, so the excerpt never skips a line.
struct Excerpt {
    text: String,
    limit: usize,
    closed: bool,
}

impl Excerpt {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            closed: false,
        }
    }

    fn push(&mut self, line: &str) -> bool {
        if self.closed {
            return false;
        }
        if self.text.len() + line.len() <= self.limit {
            self.text.push_str(line);
            return true;
        }
        self.closed = true;
        let room = self.limit.saturating_sub(self.text.len() + CUT.len() + 1);
        if room >= 16 {
            self.text.push_str(&line[..line.floor_char_boundary(room)]);
            self.text.push_str(CUT);
            self.text.push('\n');
        }
        false
    }

    /// Keep the end of a line, marking when its start was left out.
    fn push_end(&mut self, line: &str, cut: bool) {
        if line.len() <= self.limit && !cut {
            self.text.push_str(line);
        } else if line.len() + CUT.len() <= self.limit {
            self.text.push_str(CUT);
            self.text.push_str(line);
        } else if self.limit > CUT.len() {
            let start = line.ceil_char_boundary(line.len() - (self.limit - CUT.len()));
            self.text.push_str(CUT);
            self.text.push_str(&line[start..]);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// Errors, panics, tracebacks and failed tests.
    Failure,
    /// An indented error line, which can also be a field in a debug dump.
    Indented,
    /// A located line or a warning.
    Note,
}

/// A failure line and the lines that explain it.
struct Block {
    tier: Tier,
    text: String,
    first_line: usize,
    last_line: usize,
    indent: usize,
    kind: Follow,
    lines: usize,
    trace: Vec<(usize, String)>,
    trace_overflow: usize,
    /// Optional blank line sitting between `panic:` and `goroutine N [running]:`.
    pending_blank: Option<(usize, String)>,
    /// Compiler diagnostic carrying an embedded source location (`path:line: error:`
    /// or `error[E...]` followed by `--> path:line:col`).
    compiler_located: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    /// A Python traceback runs through its first line at the header's indentation.
    Traceback,
    /// A Rust panic message follows `panicked at path:line:col:` until a blank line,
    /// or upgrades to `RustBacktrace` when `stack backtrace:` appears.
    Panic,
    /// A Rust `stack backtrace:` section collecting numbered frames and `at` lines.
    RustBacktrace,
    /// A Go panic (`panic:` / `fatal error:`) and its goroutine stack trace.
    GoPanic {
        saw_goroutine: bool,
    },
    /// Node/V8/Deno/Bun or Java/Kotlin/Scala stack frames (+ `Caused by:` chains).
    StackFrames,
    /// Indented location and source lines under a compiler or runtime error.
    /// Upgrades to `StackFrames` as soon as an inline stack frame arrives.
    Indented,
    Nothing,
}

enum Extend {
    More,
    Last,
    Stop,
}

const FOLLOW_LINES: usize = 4;
const TRACEBACK_FRAMES: usize = 10;
/// Upper bound on stack-trace lines collected inside one diagnostic block so
/// workspace frames sitting behind dozens of runtime/library frames are always
/// captured before workspace-aware collapsing.
const MAX_TRACE_LINES: usize = 512;

impl Block {
    fn new(tier: Tier, line: &str, shown: &str, line_no: usize) -> Self {
        let trimmed = line.trim_start();
        let body = crate::filters::log_dedup_key(trimmed);
        let kind = if tier == Tier::Note {
            Follow::Nothing
        } else if body.starts_with("Traceback") {
            Follow::Traceback
        } else if body.contains(" panicked at ") && body.trim_end().ends_with(':') {
            Follow::Panic
        } else if body.starts_with("stack backtrace:") {
            Follow::RustBacktrace
        } else if body.starts_with("panic:") || body.starts_with("fatal error:") {
            Follow::GoPanic {
                saw_goroutine: false,
            }
        } else if tier == Tier::Failure {
            Follow::Indented
        } else {
            Follow::Nothing
        };
        let mut text = shown.to_owned();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        Self {
            tier,
            text,
            first_line: line_no,
            last_line: line_no,
            indent: indentation(line),
            kind,
            lines: 0,
            trace: Vec::new(),
            trace_overflow: 0,
            pending_blank: None,
            compiler_located: LOCATED_ERROR.is_match(line),
        }
    }

    fn push_trace(&mut self, line_no: usize, shown: &str) {
        if self.trace.len() < MAX_TRACE_LINES {
            self.trace.push((line_no, with_newline(shown)));
        } else {
            self.trace_overflow += 1;
        }
        self.last_line = line_no;
    }

    fn extend(&mut self, line: &str, shown: &str, line_no: usize) -> Extend {
        let indent = indentation(line);
        let blank = line.trim().is_empty();
        let trimmed = line.trim_start();
        match self.kind {
            Follow::Nothing => Extend::Stop,
            Follow::Traceback if blank || indent < self.indent => Extend::Stop,
            Follow::Traceback if indent > self.indent => {
                self.push_trace(line_no, shown);
                Extend::More
            }
            Follow::Traceback => {
                // The exception line, such as `ValueError: bad input`.
                self.trace.push((line_no, with_newline(shown)));
                self.last_line = line_no;
                Extend::Last
            }
            Follow::Panic => {
                if trimmed.starts_with("stack backtrace:") {
                    self.kind = Follow::RustBacktrace;
                    self.push_trace(line_no, shown);
                    return Extend::More;
                }
                let next = !blank
                    && !trimmed.starts_with("note:")
                    && !trimmed.starts_with("---- ")
                    && !trimmed.starts_with("--- ");
                if !next || self.lines >= FOLLOW_LINES {
                    return Extend::Stop;
                }
                self.lines += 1;
                self.last_line = line_no;
                self.text.push_str(&with_newline(shown));
                Extend::More
            }
            Follow::RustBacktrace => {
                if !blank
                    && (crate::stacktrace::is_rust_frame_start(line, true)
                        || crate::stacktrace::is_rust_at_line(line))
                {
                    self.push_trace(line_no, shown);
                    return Extend::More;
                }
                Extend::Stop
            }
            Follow::GoPanic { saw_goroutine } => {
                if !saw_goroutine {
                    if blank {
                        if self.pending_blank.is_none() {
                            self.pending_blank = Some((line_no, with_newline(shown)));
                            return Extend::More;
                        }
                        self.pending_blank = None;
                        return Extend::Stop;
                    }
                    if trimmed.starts_with("goroutine ") {
                        if let Some(b) = self.pending_blank.take() {
                            self.trace.push(b);
                        }
                        self.kind = Follow::GoPanic {
                            saw_goroutine: true,
                        };
                        self.push_trace(line_no, shown);
                        return Extend::More;
                    }
                    if self.pending_blank.is_some() {
                        self.pending_blank = None;
                        return Extend::Stop;
                    }
                    if trimmed.starts_with("[signal ") || self.lines < FOLLOW_LINES {
                        self.lines += 1;
                        self.push_trace(line_no, shown);
                        return Extend::More;
                    }
                    return Extend::Stop;
                }
                if blank {
                    return Extend::Stop;
                }
                if crate::stacktrace::is_go_location_line(line)
                    || crate::stacktrace::is_go_func_line(line)
                    || trimmed.starts_with("...")
                {
                    self.push_trace(line_no, shown);
                    return Extend::More;
                }
                Extend::Stop
            }
            Follow::StackFrames => {
                if !blank
                    && (crate::stacktrace::is_inline_frame(line)
                        || trimmed.starts_with("Caused by:")
                        || trimmed.starts_with("Suppressed:")
                        || trimmed.starts_with("[cause]:"))
                {
                    self.push_trace(line_no, shown);
                    return Extend::More;
                }
                Extend::Stop
            }
            Follow::Indented => {
                if !blank && crate::stacktrace::is_inline_frame(line) {
                    self.kind = Follow::StackFrames;
                    self.compiler_located = false;
                    self.push_trace(line_no, shown);
                    return Extend::More;
                }
                let next = !blank && (indent > self.indent || RUST_GUTTER.is_match(line));
                if !next || self.lines >= FOLLOW_LINES {
                    return Extend::Stop;
                }
                if self.lines == 0
                    && trimmed.starts_with("--> ")
                    && self.text.trim_start().starts_with("error[")
                {
                    self.compiler_located = true;
                }
                self.lines += 1;
                self.last_line = line_no;
                self.text.push_str(&with_newline(shown));
                Extend::More
            }
        }
    }

    fn finish(&mut self, workspace_root: &Path, stream: &str) {
        self.pending_blank = None;
        if self.trace.is_empty() {
            return;
        }
        if let Some(collapsed) =
            crate::stacktrace::collapse_numbered(&self.trace, Some(workspace_root), Some(stream))
        {
            self.text.push_str(&collapsed);
            self.trace.clear();
        } else if self.kind == Follow::Traceback && self.trace.len() > TRACEBACK_FRAMES * 3 {
            let keep = TRACEBACK_FRAMES + 1;
            let skip = self.trace.len() - keep;
            self.text.push_str(&format!(
                "{}{CUT} {skip} earlier frame line(s)\n",
                " ".repeat(self.indent + 2)
            ));
            for (_, line) in self.trace.drain(skip..) {
                self.text.push_str(&line);
            }
            self.trace.clear();
        } else {
            for (_, line) in self.trace.drain(..) {
                self.text.push_str(&line);
            }
        }
        if self.trace_overflow > 0 {
            self.text.push_str(&format!(
                "{}{CUT} {} additional frame line(s) in {stream}:{}-{}\n",
                " ".repeat(self.indent + 2),
                self.trace_overflow,
                self.first_line,
                self.last_line,
            ));
            self.trace_overflow = 0;
        }
    }
}

/// Follows the failure blocks of a stream line by line, as the `[diagnostics]`
/// section reads them, and whether a line is inside a patch hunk.
#[derive(Default)]
struct Failures {
    block: Option<Block>,
    patch: bool,
}

impl Failures {
    /// Whether `line`, without its line break, starts or continues a failure block. A
    /// block that ends goes to `done`.
    fn observe(
        &mut self,
        line: &str,
        shown: &str,
        line_no: usize,
        mut done: impl FnMut(Block),
    ) -> bool {
        // Lines inside a patch hunk are file content, not diagnostics.
        self.patch = crate::floor::in_hunk(self.patch, line);
        // A failure keeps the lines that explain it: a traceback through its
        // exception line, a panic's message/backtrace, a stack trace, or a
        // compiler error's location.
        let mut failure = false;
        if let Some(open) = self.block.as_mut() {
            match open.extend(line, shown, line_no) {
                Extend::More => failure = true,
                Extend::Last => {
                    failure = true;
                    done(self.block.take().unwrap());
                }
                Extend::Stop => done(self.block.take().unwrap()),
            }
        }
        if !failure
            && !self.patch
            && let Some(tier) = failure_tier(line)
        {
            failure = true;
            if let Some(earlier) = self.block.replace(Block::new(tier, line, shown, line_no)) {
                done(earlier);
            }
        }
        failure
    }
}

/// Whether each line of `text` belongs to a failure block, as a clipped preview's
/// scan reads it, so the floor leaves it whole.
pub(crate) fn failure_lines(text: &str) -> Vec<bool> {
    let mut failures = Failures::default();
    text.split_inclusive('\n')
        .enumerate()
        .map(|(idx, raw)| {
            let shown = ANSI.replace_all(raw, "");
            let line = shown.trim_end_matches(['\n', '\r']);
            failures.observe(line, &shown, idx + 1, drop)
        })
        .collect()
}

fn with_newline(text: &str) -> String {
    let mut text = text.to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Normalize a block's text for `[diagnostics]` deduplication: strip leading
/// timestamps via `filters::log_dedup_key` and strip archive line ranges on
/// collapsed stack-trace summary lines.
fn block_dedup_key(text: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut stripped = false;
    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        let body = if let Some(norm) = crate::stacktrace::normalize_summary_for_dedup(trimmed) {
            stripped = true;
            Cow::Owned(norm)
        } else {
            let key = crate::filters::log_dedup_key(trimmed);
            if key.len() != trimmed.len() {
                stripped = true;
            }
            Cow::Borrowed(key)
        };
        out.push_str(indent);
        out.push_str(&body);
        if raw.ends_with('\n') {
            out.push('\n');
        }
    }
    (out, stripped)
}

/// Maximum individual occurrence ranges tracked on a deduplicated entry to
/// decide whether all occurrences were already shown in `head` / `tail`.
const MAX_TRACKED_OCCURRENCES: usize = 32;

struct Entry {
    tier: Tier,
    text: String,
    first_line: usize,
    last_line: usize,
    count: usize,
    timestamp_dedup: bool,
    compiler_located: bool,
    occurrences: Vec<(usize, usize)>,
    occurrences_overflow: bool,
}

impl Entry {
    fn is_covered_by(&self, covered: &LineRanges) -> bool {
        !self.occurrences_overflow
            && !self.occurrences.is_empty()
            && self
                .occurrences
                .iter()
                .all(|&(start, end)| covered.covers(start, end))
    }
}

/// The [diagnostics] section of a clipped stream: failures first, then indented
/// error lines, then located lines and warnings. Repeated entries (including
/// ones differing only by leading timestamps) are counted rather than repeated,
/// linked to their 1-based archive line range, and omitted when already printed
/// verbatim in `head` or `tail`.
#[derive(Default)]
struct Section {
    entries: [Vec<Entry>; 3],
    bytes: [usize; 3],
    index: HashMap<String, (usize, usize)>,
}

impl Section {
    fn add(&mut self, mut block: Block, workspace_root: &Path, stream: &str, budget: usize) {
        block.finish(workspace_root, stream);
        let tier = block.tier as usize;
        let (key, stripped) = block_dedup_key(&block.text);
        if let Some(&(tier, position)) = self.index.get(&key) {
            let entry = &mut self.entries[tier][position];
            entry.count += 1;
            if stripped || block.text != entry.text {
                entry.timestamp_dedup = true;
            }
            if entry.occurrences.len() < MAX_TRACKED_OCCURRENCES {
                entry.occurrences.push((block.first_line, block.last_line));
            } else {
                entry.occurrences_overflow = true;
            }
            return;
        }
        // An entry past the budget could not be shown, so it is not kept either.
        if self.bytes[tier] >= budget {
            return;
        }
        self.bytes[tier] += block.text.len();
        self.index.insert(key, (tier, self.entries[tier].len()));
        self.entries[tier].push(Entry {
            tier: block.tier,
            text: block.text,
            first_line: block.first_line,
            last_line: block.last_line,
            count: 1,
            timestamp_dedup: stripped,
            compiler_located: block.compiler_located,
            occurrences: vec![(block.first_line, block.last_line)],
            occurrences_overflow: false,
        });
    }

    fn render(&self, budget: usize, stream: &str, covered: &LineRanges) -> String {
        let mut out = Excerpt::new(budget);
        for entry in self.entries.iter().flatten() {
            if entry.is_covered_by(covered) {
                continue;
            }
            let lines: Vec<&str> = entry.text.split_inclusive('\n').collect();
            let Some((&first, rest)) = lines.split_first() else {
                continue;
            };
            let first_trimmed = first.trim_end_matches(['\n', '\r']);
            let pointer = if entry.first_line == entry.last_line {
                format!("{stream}:{}", entry.first_line)
            } else {
                format!("{stream}:{}-{}", entry.first_line, entry.last_line)
            };
            let mut rendered: Vec<Cow<'_, str>> = Vec::with_capacity(lines.len());
            if entry.count > 1 {
                if entry.timestamp_dedup || entry.tier != Tier::Indented {
                    rendered.push(Cow::Owned(format!(
                        "{first_trimmed}  [×{}, first {stream}:{}]\n",
                        entry.count, entry.first_line
                    )));
                } else {
                    rendered.push(Cow::Owned(format!("{first_trimmed}  [×{}]\n", entry.count)));
                }
                rendered.extend(rest.iter().copied().map(Cow::Borrowed));
            } else if entry.compiler_located {
                rendered.push(Cow::Borrowed(first));
                if rest.len() >= 2 {
                    let (mid, last) = rest.split_at(rest.len() - 1);
                    rendered.extend(mid.iter().copied().map(Cow::Borrowed));
                    let last_trimmed = last[0].trim_end_matches(['\n', '\r']);
                    rendered.push(Cow::Owned(format!("{last_trimmed}  [{pointer}]\n")));
                } else {
                    rendered.extend(rest.iter().copied().map(Cow::Borrowed));
                }
            } else {
                rendered.push(Cow::Owned(format!("{first_trimmed}  [{pointer}]\n")));
                rendered.extend(rest.iter().copied().map(Cow::Borrowed));
            }
            for line in &rendered {
                if !out.push(line) {
                    return out.text;
                }
            }
        }
        out.text
    }
}

/// Classify a line that can explain why a command failed.
fn failure_tier(line: &str) -> Option<Tier> {
    let trimmed = line.trim_start();
    let body = crate::filters::log_dedup_key(trimmed);
    // These prefixes mean a failure wherever they appear, including in indented
    // build script output.
    if [
        "Traceback",
        "FAILED",
        "FAIL [",
        "TIMEOUT [",
        "--- FAIL:",
        "FAIL\t",
        "thread '",
        "assertion ",
        "left:",
        "right:",
        "panic:",
        "fatal error:",
        "Exception in thread ",
        "Caused by:",
    ]
    .iter()
    .any(|prefix| body.starts_with(prefix))
        || body.trim_end().ends_with("... FAILED")
        || LOCATED_ERROR.is_match(line)
    {
        return Some(Tier::Failure);
    }
    let error = [
        "error:",
        "error[",
        "ERROR",
        "[ERROR]",
        "FATAL",
        "[FATAL]",
        "fatal:",
        "failures:",
        "test result: FAILED",
    ]
    .iter()
    .any(|prefix| body.starts_with(prefix));
    if trimmed.len() == line.len() {
        (error || EXCEPTION.is_match(body)).then_some(Tier::Failure)
    } else {
        // Indented `error:` lines are also struct fields and diff context.
        error.then_some(Tier::Indented)
    }
}

fn read_prefix(file: &mut impl BufRead, prefix: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut read = false;
    loop {
        let chunk = file.fill_buf()?;
        if chunk.is_empty() {
            return Ok(read);
        }
        read = true;
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let length = newline.map_or(chunk.len(), |index| index + 1);
        let kept = length.min(limit.saturating_sub(prefix.len()));
        prefix.extend_from_slice(&chunk[..kept]);
        file.consume(length);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

fn source_location(
    workspace: &Workspace,
    locations: &mut Locations,
    text: &str,
) -> Option<Location> {
    if let Some(captures) = PYTHON_SOURCE
        .captures(text)
        .or_else(|| TYPESCRIPT_SOURCE.captures(text))
    {
        let start = captures[2].parse().ok()?;
        return locate(workspace, locations, &captures[1], start, start);
    }
    let candidate = if text.contains("panicked at ") {
        text.split_once("panicked at ")?.1
    } else if text.trim_start().starts_with("at ")
        && let Some((_, path)) = text.rsplit_once(" (")
    {
        path
    } else {
        text
    };
    let captures = SOURCE.captures(candidate)?;
    let start: usize = captures[2].parse().ok()?;
    let name = captures.get(1)?.as_str().trim();
    // A marker such as `[redacted:secret:1d0d]` looks like `name:line`.
    if name.contains("[redacted:") {
        return None;
    }
    locate(
        workspace,
        locations,
        name.strip_prefix("file://").unwrap_or(name),
        start,
        start,
    )
}

fn locate(
    workspace: &Workspace,
    locations: &mut Locations,
    name: &str,
    start: usize,
    end: usize,
) -> Option<Location> {
    if start == 0 || end < start {
        return None;
    }
    let file = locations.resolve(workspace, name)?;
    Some(Location { file, start, end })
}

/// The most source locations of each kind that a run record keeps.
const KEPT_LOCATIONS: usize = 100;
/// The most log lines of one kind a run record keeps for one source location.
/// Progress output and the real error often name the same line, so a failure is
/// kept even when other lines already named its location.
const LINES_PER_LOCATION: usize = 4;

/// Source locations named in the output, for `context --run`, packs and handoffs.
/// Each location keeps a few log lines, those in failures first, so the record
/// stays far smaller than output that names a location on every line.
#[derive(Default)]
struct Locations {
    names: HashMap<String, Option<String>>,
    /// Log lines kept for each location, by whether they were failures.
    seen: BTreeMap<(Location, bool), usize>,
    failures: Vec<Diagnostic>,
    others: Vec<Diagnostic>,
    source_digests: BTreeMap<String, String>,
}

impl Locations {
    /// The workspace file a printed path names, resolved once per distinct path.
    fn resolve(&mut self, workspace: &Workspace, name: &str) -> Option<String> {
        if let Some(file) = self.names.get(name) {
            return file.clone();
        }
        let path = Path::new(name);
        let candidates = [workspace.cwd.join(path), workspace.root.join(path)];
        let file = candidates.iter().find_map(|path| {
            let canonical = path.canonicalize().ok()?;
            workspace.relative_file(&canonical)
        });
        if self.names.len() < 8192 {
            self.names.insert(name.to_owned(), file.clone());
        }
        file
    }

    fn add(
        &mut self,
        workspace: &Workspace,
        location: Location,
        log_line: usize,
        stream: Stream,
        failure: bool,
    ) {
        let kept = if failure {
            self.failures.len()
        } else {
            self.others.len()
        };
        let lines = self
            .seen
            .get(&(location.clone(), failure))
            .copied()
            .unwrap_or(0);
        if kept >= KEPT_LOCATIONS || lines >= LINES_PER_LOCATION {
            return;
        }
        // A script `exec` saved under the state directory never changes, and the
        // index leaves that directory out, so a digest would always look stale.
        if !self.source_digests.contains_key(&location.file)
            && !Path::new(&location.file).starts_with(".justless")
            && let Ok(source) = workspace
                .source_path(&location.file)
                .and_then(|p| fs::read(p).context("read diagnostic source"))
        {
            self.source_digests
                .insert(location.file.clone(), source_digest(&source));
        }
        self.seen.insert((location.clone(), failure), lines + 1);
        let diagnostic = Diagnostic {
            location,
            log_line,
            stream,
        };
        if failure {
            self.failures.push(diagnostic);
        } else {
            self.others.push(diagnostic);
        }
    }

    fn finish(self, run: &mut Run) {
        run.diagnostics = self.failures;
        run.diagnostics.extend(self.others);
        run.source_digests = self.source_digests;
    }
}

fn cargo_message(text: &str) -> Option<serde_json::Value> {
    if !text.starts_with('{') || !text.contains("\"compiler-message\"") {
        return None;
    }
    let mut record: serde_json::Value = serde_json::from_str(text).ok()?;
    (record.get("reason")?.as_str()? == "compiler-message").then(|| record["message"].take())
}

fn rendered_message(message: &serde_json::Value) -> Option<String> {
    if let Some(rendered) = message.get("rendered").and_then(serde_json::Value::as_str) {
        return Some(ANSI.replace_all(rendered, "").into_owned());
    }
    Some(format!(
        "{}: {}\n",
        message.get("level")?.as_str()?,
        message.get("message")?.as_str()?
    ))
}

fn json_locations(
    workspace: &Workspace,
    locations: &mut Locations,
    message: &serde_json::Value,
) -> Vec<Location> {
    let Some(spans) = message.get("spans").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    spans
        .iter()
        .filter_map(|span| {
            if !span.get("is_primary")?.as_bool()? {
                return None;
            }
            let file = span.get("file_name")?.as_str()?;
            let start = usize::try_from(span.get("line_start")?.as_u64()?).ok()?;
            let end = usize::try_from(span.get("line_end")?.as_u64()?).ok()?;
            locate(workspace, locations, file, start, end)
        })
        .collect()
}

pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

pub fn load(workspace: &Workspace, id: &str) -> Result<(PathBuf, Run)> {
    let suffix = id.strip_prefix("run-").unwrap_or("");
    ensure!(
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()),
        "invalid run ID"
    );
    let directory = workspace.state.join("runs").join(id);
    ensure!(
        directory.canonicalize().ok().as_ref() == Some(&directory),
        "unknown run ID: {id}"
    );
    let run = read_record(&directory, id)?;
    ensure!(run.id == id, "run record does not match its directory");
    Ok((directory, run))
}

/// Decode a run record in one read. A run directory with streams but no record
/// belongs to a run that Justless could not finish, and stays replayable.
fn read_record(directory: &Path, id: &str) -> Result<Run> {
    match fs::read(directory.join("record.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("decode run record"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let started = fs::metadata(directory.join("stdout"))
                .and_then(|meta| meta.modified())
                .with_context(|| format!("open recorded run {id}"))?;
            let mut run = Run::unfinished(id.to_owned(), "unknown", "", false);
            run.completed_ms = started
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.as_millis() as u64);
            Ok(run)
        }
        Err(error) => Err(error).with_context(|| format!("open recorded run {id}")),
    }
}

/// Replays a run's streams, or a range of their lines, with any notes on stderr.
/// Returns the bytes printed, notes included, which is what the agent receives.
pub fn log(directory: &Path, streams: &[Stream], from: usize, to: usize) -> Result<u64> {
    ensure!(
        from > 0 && from <= to,
        "expected a positive, ordered line range"
    );
    #[derive(Default, Deserialize)]
    struct Lengths {
        #[serde(default)]
        stream_bytes: Vec<u64>,
        #[serde(default)]
        produced_bytes: Vec<u64>,
    }
    let Lengths {
        stream_bytes: recorded,
        produced_bytes: produced,
    } = fs::read(directory.join("record.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Lengths>(&bytes).ok())
        .unwrap_or_default();
    let mut printed = 0;
    for &stream in streams {
        let path = directory.join(stream.name());
        let (last, bytes) = replay_file(&path, stream, from, to)?;
        printed += bytes;
        let mut notes = String::new();
        if let (Some(&kept), Some(&total)) =
            (recorded.get(stream.index()), produced.get(stream.index()))
            && total > kept
        {
            notes.push_str(&format!(
                "[justless: {} archive truncated at {} of {}; the rest was not stored]\n",
                stream.name(),
                history::human_bytes(kept),
                history::human_bytes(total)
            ));
        }
        // A background process can keep writing after the command exits. Those
        // bytes stay in the archive, but no preview or record described them.
        if from == 1
            && to == usize::MAX
            && let Some(&expected) = recorded.get(stream.index())
            && let Ok(meta) = fs::metadata(&path)
            && meta.len() > expected
        {
            notes.push_str(&format!(
                "[justless: the last {} byte(s) of {} were written after the command exited]\n",
                meta.len() - expected,
                stream.name()
            ));
        }
        if !notes.is_empty() {
            let separator = separator(last);
            eprint!("{separator}{notes}");
            printed += (separator.len() + notes.len()) as u64;
        }
    }
    Ok(printed)
}

/// Replay a stream file, or a range of its lines, and flush it. Returns the last
/// byte printed and how many bytes were printed.
fn replay_file(path: &Path, stream: Stream, from: usize, to: usize) -> Result<(Option<u8>, u64)> {
    let redactor = path.parent().and_then(crate::redact::for_archive);
    let mut file = crate::redact::Reader::new(File::open(path)?, redactor);
    if from == 1 && to == usize::MAX {
        return Ok(copy_out(&mut file, stream)?);
    }
    let mut line = Vec::new();
    let mut last = None;
    let mut printed = 0;
    for number in 1..=to {
        if file.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if number >= from {
            stream.print(&line)?;
            printed += line.len() as u64;
            last = line.last().copied().or(last);
        }
        line.clear();
    }
    if stream == Stream::Stdout {
        io::stdout().flush()?;
    }
    Ok((last, printed))
}

/// Copy a stream to Justless's own stdout or stderr and flush it, so a note that
/// follows on stderr comes after it even when both go to one file. Returns the
/// last byte copied and how many bytes were copied.
fn copy_out(reader: &mut impl Read, stream: Stream) -> io::Result<(Option<u8>, u64)> {
    fn copy(reader: &mut impl Read, mut output: impl Write) -> io::Result<(Option<u8>, u64)> {
        let mut buffer = vec![0; 64 * 1024];
        let mut last = None;
        let mut copied = 0;
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            output.write_all(&buffer[..read])?;
            copied += read as u64;
            last = Some(buffer[read - 1]);
        }
        output.flush()?;
        Ok((last, copied))
    }
    match stream {
        Stream::Stdout => copy(reader, io::stdout().lock()),
        Stream::Stderr => copy(reader, io::stderr().lock()),
    }
}

/// What starts a note on stderr after output that ended with `last`, so a line the
/// archive cut short does not run into the note.
fn separator(last: Option<u8>) -> &'static str {
    match last {
        Some(byte) if byte != b'\n' => "\n",
        _ => "",
    }
}

/// A writer that counts the bytes written through it.
pub(crate) struct Counted<W> {
    inner: W,
    pub(crate) written: u64,
}

impl<W: Write> Counted<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Lists the lines of a run that contain `needle`, up to `limit`, and ends with a
/// count of the matches it left out. `failed` is whether the run failed.
pub fn search(
    directory: &Path,
    streams: &[Stream],
    needle: &str,
    limit: usize,
    failed: bool,
) -> Result<u64> {
    let found = Found::scan(directory, streams, needle, failed, limit)?;
    let chosen = found.choose(limit);
    let mut tally = found.tally();
    let mut out = Counted::new(io::stdout().lock());
    for &(stream, line, text) in &chosen {
        writeln!(out, "{}:{line}: {text}", stream.name())?;
        tally.show(stream);
    }
    if tally.shown() < tally.total() {
        // MCP takes no `limit` above its cap, so past it the hint narrows the
        // search instead.
        let total = tally.total();
        let hint = if total <= crate::mcp::MAX_LIMIT {
            format!("--limit {total} shows all")
        } else if Stream::BOTH.iter().all(|&stream| tally.of(stream).1 > 0) {
            "narrow TEXT or pick one --stream".to_owned()
        } else {
            "narrow TEXT".to_owned()
        };
        writeln!(
            out,
            "[{} of {total} matches shown ({tally}); {hint}]",
            tally.shown()
        )?;
    }
    Ok(out.written)
}

/// A word that marks a matching line as a failure wherever it appears, such as
/// `FAILED`, `TimeoutError`, `panicked`, mocha's `2 failing`, a signal such as
/// `Segmentation fault` or `Killed`, or TAP's `not ok`. Matching too much only
/// changes which lines a cut keeps first.
static FAILURE_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:errors?|exceptions?)\b|\b(?:fail(?:s|ed|ing|ure|ures)?|panic(?:s|ked)?|traceback|fatal|segfault|segmentation fault|abort(?:s|ed)?|killed|timed out|not ok)\b",
    )
    .expect("failure word pattern")
});

/// The kinds of matching line, in the order a cut gives them places. Only a
/// failed run has failure and stderr lines; every match of a run that passed
/// is `PLAIN`.
const FAILURE: usize = 0;
const STDERR: usize = 1;
const PLAIN: usize = 2;
const KINDS: usize = 3;

fn kind_of(failed: bool, stream: Stream, text: &str) -> usize {
    if !failed {
        PLAIN
    } else if FAILURE_WORD.is_match(text) || failure_tier(text).is_some() {
        FAILURE
    } else if stream == Stream::Stderr {
        STDERR
    } else {
        PLAIN
    }
}

/// Matching lines shown and found per stream, for the notice that counts what a
/// listing left out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    shown: [usize; 2],
    total: [usize; 2],
}

impl Tally {
    pub(crate) fn found(&mut self, stream: Stream, count: usize) {
        self.total[stream.index()] += count;
    }

    pub(crate) fn show(&mut self, stream: Stream) {
        self.shown[stream.index()] += 1;
    }

    pub(crate) fn add(&mut self, other: &Self) {
        for index in 0..2 {
            self.shown[index] += other.shown[index];
            self.total[index] += other.total[index];
        }
    }

    /// The same counts with every match shown.
    pub(crate) fn every_shown(&self) -> Self {
        Self {
            shown: self.total,
            total: self.total,
        }
    }

    pub(crate) fn shown(&self) -> usize {
        self.shown.iter().sum()
    }

    pub(crate) fn total(&self) -> usize {
        self.total.iter().sum()
    }

    /// Shown and found for one stream.
    pub(crate) fn of(&self, stream: Stream) -> (usize, usize) {
        (self.shown[stream.index()], self.total[stream.index()])
    }
}

/// `stdout 19 of 25, stderr 1 of 1`, leaving out a stream without matches.
impl std::fmt::Display for Tally {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for stream in Stream::BOTH {
            let (shown, total) = self.of(stream);
            if total == 0 {
                continue;
            }
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            write!(f, "{} {shown} of {total}", stream.name())?;
        }
        Ok(())
    }
}

/// The lines of a run's streams that contain a text. Every match is counted,
/// and the first `keep` of each kind are kept in line order, enough to choose
/// the lines for any limit up to `keep`.
pub(crate) struct Found {
    failed: bool,
    streams: Vec<Stream>,
    counts: Vec<[usize; KINDS]>,
    kept: Vec<[Vec<(usize, String)>; KINDS]>,
}

impl Found {
    /// Reads `streams` to the end. `failed` is whether the run failed.
    pub(crate) fn scan(
        directory: &Path,
        streams: &[Stream],
        needle: &str,
        failed: bool,
        keep: usize,
    ) -> Result<Self> {
        let mut found = Self {
            failed,
            streams: streams.to_vec(),
            counts: vec![[0; KINDS]; streams.len()],
            kept: streams.iter().map(|_| Default::default()).collect(),
        };
        matches(directory, streams, needle, |stream, line, text, whole| {
            let index = found
                .streams
                .iter()
                .position(|&searched| searched == stream)
                .unwrap_or_default();
            // A failure word can lie outside the excerpt, as `FAILED` does
            // before a long test ID.
            let kind = kind_of(failed, stream, whole);
            found.counts[index][kind] += 1;
            let kept = &mut found.kept[index][kind];
            if kept.len() < keep {
                kept.push((line, text.to_owned()));
            }
            true
        })?;
        Ok(found)
    }

    pub(crate) fn total(&self) -> usize {
        self.counts.iter().flatten().sum()
    }

    /// Every match counted, none shown yet.
    pub(crate) fn tally(&self) -> Tally {
        let mut tally = Tally::default();
        for (&stream, counts) in self.streams.iter().zip(&self.counts) {
            tally.found(stream, counts.iter().sum());
        }
        tally
    }

    /// The line of `stream` a cut shows first: on a failed run its first
    /// failure line, otherwise its first match.
    pub(crate) fn first(&self, stream: Stream) -> Option<(Stream, usize, &str)> {
        let index = self
            .streams
            .iter()
            .position(|&searched| searched == stream)?;
        let (line, text) = self.kept[index].iter().find_map(|kept| kept.first())?;
        Some((stream, *line, text.as_str()))
    }

    /// The lines to show within `limit`, at most the `keep` it was scanned
    /// with, as stream, line number and text. When every match fits, they come
    /// stream by stream in line order. Otherwise each stream with a match keeps
    /// a place. On a failed run, failure lines take the places first, then
    /// stderr lines, and the streams holding them come first. Lines stay in
    /// order within a stream.
    pub(crate) fn choose(&self, limit: usize) -> Vec<(Stream, usize, &str)> {
        let cut = self.total() > limit;
        let mut quota = vec![[0usize; KINDS]; self.streams.len()];
        if cut {
            let best = |counts: &[usize; KINDS]| (0..KINDS).find(|&kind| counts[kind] > 0);
            let mut reserved = self
                .counts
                .iter()
                .enumerate()
                .filter_map(|(index, counts)| Some((best(counts)?, index)))
                .collect::<Vec<_>>();
            reserved.sort_unstable();
            let mut left = limit;
            for (kind, index) in reserved.into_iter().take(limit) {
                quota[index][kind] += 1;
                left -= 1;
            }
            for kind in 0..KINDS {
                for (index, counts) in self.counts.iter().enumerate() {
                    let take = (counts[kind] - quota[index][kind]).min(left);
                    quota[index][kind] += take;
                    left -= take;
                }
            }
        } else {
            quota.clone_from(&self.counts);
        }
        let mut order = (0..self.streams.len()).collect::<Vec<_>>();
        if cut && self.failed {
            order.sort_by_key(|&index| {
                let first = (0..KINDS).find(|&kind| quota[index][kind] > 0);
                (first.unwrap_or(KINDS), index)
            });
        }
        let mut chosen = Vec::new();
        for index in order {
            let mut lines = (0..KINDS)
                .flat_map(|kind| self.kept[index][kind].iter().take(quota[index][kind]))
                .map(|(line, text)| (self.streams[index], *line, text.as_str()))
                .collect::<Vec<_>>();
            lines.sort_by_key(|&(_, line, _)| line);
            chosen.extend(lines);
        }
        chosen
    }
}

/// Calls `emit` with the stream, line number, excerpt around the match and the
/// whole line of each line that contains `needle`. Keep one overlapping window so
/// even a single enormous log line is searchable; for such a line, the whole line
/// is the window read so far.
pub fn matches(
    directory: &Path,
    streams: &[Stream],
    needle: &str,
    mut emit: impl FnMut(Stream, usize, &str, &str) -> bool,
) -> Result<()> {
    for &stream in streams {
        let mut file = crate::redact::Reader::new(
            File::open(directory.join(stream.name()))?,
            crate::redact::for_archive(directory),
        );
        let mut window = Vec::new();
        let mut line_number = 1;
        let mut matched = false;
        loop {
            let read = file
                .by_ref()
                .take(SCAN_LINE_LIMIT as u64)
                .read_until(b'\n', &mut window)?;
            if read == 0 {
                break;
            }
            let newline = window.ends_with(b"\n");
            if !matched {
                let text = String::from_utf8_lossy(&window);
                let text = ANSI.replace_all(&text, "");
                if let Some(pos) = text.find(needle) {
                    let start = text.ceil_char_boundary(pos.saturating_sub(100));
                    let end = text.floor_char_boundary((start + 400).min(text.len()));
                    if !emit(stream, line_number, text[start..end].trim_end(), &text) {
                        return Ok(());
                    }
                    matched = true;
                }
            }
            if newline {
                line_number += 1;
                matched = false;
                window.clear();
            } else if matched {
                window.clear();
            } else {
                let drop = window
                    .len()
                    .saturating_sub(needle.len().saturating_add(256));
                window.drain(..drop);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use Stream::{Stderr, Stdout};

    fn found(failed: bool, lines: &[(Stream, usize, &str)]) -> Found {
        let mut found = Found {
            failed,
            streams: Stream::BOTH.to_vec(),
            counts: vec![[0; KINDS]; 2],
            kept: vec![Default::default(), Default::default()],
        };
        for &(stream, line, text) in lines {
            let kind = kind_of(failed, stream, text);
            found.counts[stream.index()][kind] += 1;
            found.kept[stream.index()][kind].push((line, text.to_owned()));
        }
        found
    }

    #[test]
    fn a_cut_keeps_a_place_for_each_stream_and_lines_in_order() {
        let lines = [
            (Stdout, 1, "a"),
            (Stdout, 2, "b"),
            (Stdout, 3, "c"),
            (Stderr, 1, "warning d"),
        ];
        // Within the limit, streams and lines keep their order.
        assert_eq!(found(true, &lines).choose(4), lines);
        // A run that passed keeps stdout first.
        assert_eq!(
            found(false, &lines).choose(2),
            [(Stdout, 1, "a"), (Stderr, 1, "warning d")]
        );
        assert_eq!(found(false, &lines).choose(1), [(Stdout, 1, "a")]);
        // A failed run gives stderr the first place.
        assert_eq!(
            found(true, &lines).choose(3),
            [(Stderr, 1, "warning d"), (Stdout, 1, "a"), (Stdout, 2, "b")]
        );
        assert_eq!(found(true, &lines).choose(1), [(Stderr, 1, "warning d")]);
        // A failure line outranks stderr, and stays in line order.
        let lines = [
            (Stdout, 1, "a"),
            (Stdout, 2, "b"),
            (Stdout, 3, "test result: FAILED. 1 failed"),
            (Stderr, 1, "warning d"),
        ];
        assert_eq!(
            found(true, &lines).choose(2),
            [
                (Stdout, 3, "test result: FAILED. 1 failed"),
                (Stderr, 1, "warning d")
            ]
        );
        assert_eq!(
            found(true, &lines).choose(3),
            [
                (Stdout, 1, "a"),
                (Stdout, 3, "test result: FAILED. 1 failed"),
                (Stderr, 1, "warning d")
            ]
        );
    }

    #[test]
    fn failure_words_signals_and_timeouts_count_anywhere_in_a_failed_runs_line() {
        for text in [
            "order-42 FAILED at settlement",
            "raised TimeoutError: slow",
            "thread 'main' panicked at src/lib.rs:3:5",
            "found 2 errors",
            "Traceback (most recent call last):",
            "fatal: not a repository",
            "  2 failing",
            "Segmentation fault (core dumped)",
            "segfault at 0 ip 0000",
            "Aborted (core dumped)",
            "process killed by signal 9",
            "Killed",
            "request timed out after 30s",
            "not ok 3 - settles order",
        ] {
            assert_eq!(kind_of(true, Stdout, text), FAILURE, "{text}");
        }
        for text in [
            "order-42 progress 3",
            "fail_fast = true",
            "passed",
            "killed_count = 0",
            "nothing failing_here",
        ] {
            assert_eq!(kind_of(true, Stdout, text), PLAIN, "{text}");
        }
        assert_eq!(kind_of(true, Stderr, "Compiling demo"), STDERR);
        assert_eq!(kind_of(false, Stderr, "error: demo"), PLAIN);
    }

    #[test]
    fn a_tally_names_only_streams_with_matches() {
        let mut tally = Tally::default();
        tally.found(Stdout, 25);
        assert_eq!(tally.to_string(), "stdout 0 of 25");
        tally.found(Stderr, 1);
        tally.show(Stderr);
        assert_eq!(tally.to_string(), "stdout 0 of 25, stderr 1 of 1");
        assert_eq!(
            tally.every_shown().to_string(),
            "stdout 25 of 25, stderr 1 of 1"
        );
    }

    #[test]
    fn fit_lines_cuts_at_a_line_end_or_inside_a_long_first_line() {
        for (preview, expected) in [
            ("short\n".to_owned(), "short\n".to_owned()),
            (
                format!("! flag\n{}\n", "x".repeat(60)),
                "! flag\n[more in full log]\n".to_owned(),
            ),
            // The cut falls inside the `é`, so it moves before it.
            (
                format!("! {}é{}\n", "f".repeat(25), "g".repeat(30)),
                format!("! {}\n[more in full log]\n", "f".repeat(25)),
            ),
        ] {
            let mut bytes = preview.clone().into_bytes();
            fit_lines(&mut bytes, 48);
            assert_eq!(String::from_utf8(bytes).unwrap(), expected, "{preview:?}");
        }
    }

    #[test]
    fn failure_tier_recognizes_nextest_and_go_test_failures() {
        let log = concat!(
            "        PASS [   0.004s] demo::unit test_ok\n",
            "        FAIL [   0.012s] demo::unit test_bad\n",
            "     TIMEOUT [  60.001s] demo::unit test_hang\n",
            "--- FAIL: TestOrder (0.01s)\n",
            "FAIL\tgithub.com/example/demo\t0.123s\n",
            "ok  \tgithub.com/example/other\t0.010s\n",
        );
        assert_eq!(
            failure_lines(log),
            vec![false, true, true, true, true, false]
        );
    }

    #[cfg(unix)]
    fn test_workspace(root: &Path) -> Workspace {
        Workspace {
            root: root.to_path_buf(),
            cwd: root.to_path_buf(),
            state: root.join(".justless"),
            origin: root.to_path_buf(),
        }
    }

    #[cfg(unix)]
    fn fake_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn merged_docker_logs_gets_logview_folding_and_valid_stdout_replay() {
        let dir = tempfile::tempdir().unwrap();
        let ws = test_workspace(dir.path());
        let docker = fake_script(
            dir.path(),
            "docker",
            r#"i=1
while [ $i -le 45 ]; do
    echo "2026-11-25T10:00:$(printf '%02d' $((i % 60)))Z INFO api: handled GET /healthz status=200 latency=1ms req_id=req-$i"
    i=$((i + 1))
done
echo "2026-11-25T10:01:00Z ERROR api: ConnectionRefusedError: dial tcp 10.0.0.5:5432 failed" >&2
echo "    at pool.connect (/app/db.js:42:11)" >&2
i=46
while [ $i -le 90 ]; do
    echo "2026-11-25T10:01:$(printf '%02d' $((i % 60)))Z INFO api: handled GET /healthz status=200 latency=2ms req_id=req-$i"
    i=$((i + 1))
done"#,
        );
        let args = vec![
            docker.as_os_str().to_os_string(),
            OsString::from("logs"),
            OsString::from("api"),
            OsString::from("--tail"),
            OsString::from("500"),
        ];
        let words: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        let settings = ws.settings().unwrap();
        let selection = crate::filters::Selection::for_run(&args, &settings.filters).unwrap();
        assert_eq!(selection.profile, Some(crate::filters::Profile::Logs));
        let merged = crate::merge::joins(true, selection.profile);
        assert!(merged, "docker logs --merge must join streams");

        let budget = crate::adaptive::preview_budget(&ws, &args).unwrap();
        let explicit_budget = !crate::adaptive::take_advised(&ws, &args, budget);
        assert!(!explicit_budget);

        let family = family_of(&args, &selection);
        let mut archive = Archive::create(&ws, &family, &history::label(&words), true).unwrap();
        let mut cmd = Command::new(&args[0]);
        cmd.current_dir(&ws.cwd).args(&args[1..]);
        let status = exit_code(
            archive
                .run(&mut cmd, settings.max_run_bytes, merged)
                .unwrap()
                .unwrap(),
        );
        let capture = Capture {
            workspace: &ws,
            args: &args,
            words: &words,
            selection: &selection,
            max_bytes: budget,
            explicit_budget,
            session: "merged-log-test",
            fresh: true,
            window: false,
            hook: true,
            agent_footer: true,
            merge: true,
            merged,
            redactor: None,
            retention: (settings.max_runs, settings.max_archive_bytes),
            origin: None,
            pieces: &[],
            json_grouping: settings.json_grouping,
            ends_early: false,
        };
        let ((previews, report), _footer, run) =
            capture.record(&mut archive, family, status, None).unwrap();

        assert!(report, "merged docker logs should use logview report");
        assert_eq!(run.replaced.as_deref(), Some("both"));
        assert_eq!(run.budget_bytes, Some(budget));
        assert!(!run.explicit_budget);
        assert_eq!(run.stream_bytes.get(1).copied(), Some(0));
        assert!(previews[1].1.is_empty());

        let shown = String::from_utf8(previews[0].1.clone()).unwrap();
        assert!(
            shown.len() * 3 < run.raw_bytes as usize,
            "expected strong folding, got {} of {} bytes:\n{shown}",
            shown.len(),
            run.raw_bytes
        );
        assert!(
            shown.contains("ConnectionRefusedError"),
            "error from merged stderr must be preserved:\n{shown}"
        );
        assert!(
            !shown.contains("[stderr:"),
            "merged logview must emit only stdout pointers:\n{shown}"
        );

        // Verify every [stdout:A] or [stdout:A-B] pointer resolves cleanly in the archived stdout.
        let archived =
            fs::read_to_string(ws.state.join("runs").join(&run.id).join("stdout")).unwrap();
        let archived_lines: Vec<&str> = archived.lines().collect();
        let ptr_re = Regex::new(r"\[stdout:(\d+)(?:-(\d+))?\]").unwrap();
        let mut pointer_count = 0;
        let mut error_replayed = false;
        for cap in ptr_re.captures_iter(&shown) {
            pointer_count += 1;
            let from: usize = cap[1].parse().unwrap();
            let to: usize = cap.get(2).map_or(from, |m| m.as_str().parse().unwrap());
            assert!(
                from >= 1 && to >= from && to <= archived_lines.len(),
                "invalid pointer [stdout:{from}-{to}] for {} lines",
                archived_lines.len()
            );
            let slice = archived_lines[from - 1..to].join("\n");
            if slice.contains("ConnectionRefusedError") {
                error_replayed = true;
            }
        }
        assert!(pointer_count >= 2, "expected stdout pointers:\n{shown}");
        assert!(
            error_replayed,
            "error line pointer must replay exact ConnectionRefusedError line:\n{shown}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn record_persists_replaced_budget_and_explicit_flag_across_views() {
        let dir = tempfile::tempdir().unwrap();
        let ws = test_workspace(dir.path());
        let latest_run = |ws: &Workspace| -> Run {
            let mut entries: Vec<_> = fs::read_dir(ws.state.join("runs"))
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("run-"))
                .collect();
            entries.sort_by_key(|e| {
                serde_json::from_slice::<Run>(&fs::read(e.path().join("record.json")).unwrap())
                    .unwrap()
                    .completed_ms
            });
            let path = entries.last().unwrap().path().join("record.json");
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
        };

        // 1. Adaptive vs explicit budget handoff via execute().
        let noisy = fake_script(
            dir.path(),
            "noisy",
            "i=0; while [ $i -lt 300 ]; do echo \"line $i of verbose output payload\"; i=$((i+1)); done",
        );
        let noisy_args = vec![noisy.as_os_str().to_os_string()];
        let advised = crate::adaptive::preview_budget(&ws, &noisy_args).unwrap();
        assert_eq!(
            execute(
                &ws,
                &noisy_args,
                advised,
                false,
                "s",
                (true, false),
                true,
                false
            )
            .unwrap(),
            0
        );
        let adaptive_run = latest_run(&ws);
        assert_eq!(adaptive_run.replaced, None);
        assert_eq!(adaptive_run.budget_bytes, Some(advised));
        assert!(!adaptive_run.explicit_budget);

        // Calling execute directly (as `run --max-bytes 3500` does) sets explicit_budget = true.
        assert_eq!(
            execute(
                &ws,
                &noisy_args,
                3500,
                false,
                "s",
                (true, false),
                true,
                false
            )
            .unwrap(),
            0
        );
        let explicit_run = latest_run(&ws);
        assert_eq!(explicit_run.budget_bytes, Some(3500));
        assert!(explicit_run.explicit_budget);

        // 2. json_view sets replaced = Some("stdout").
        let mut rows = String::from("[");
        for i in 0..80 {
            if i > 0 {
                rows.push(',');
            }
            rows.push_str(&format!(
                "{{\"id\":{i},\"service\":\"api-{i}\",\"status\":\"running\",\"region\":\"us-east-1\"}}"
            ));
        }
        rows.push(']');
        let json_cmd = fake_script(
            dir.path(),
            "emit_json",
            &format!("printf '%s\\n' '{}'", rows),
        );
        let json_args = vec![json_cmd.as_os_str().to_os_string()];
        let budget = crate::adaptive::preview_budget(&ws, &json_args).unwrap();
        assert_eq!(
            execute(
                &ws,
                &json_args,
                budget,
                false,
                "s",
                (true, false),
                true,
                false
            )
            .unwrap(),
            0
        );
        let json_run = latest_run(&ws);
        assert_eq!(json_run.replaced.as_deref(), Some("stdout"));
        assert_eq!(json_run.budget_bytes, Some(budget));
        assert!(!json_run.explicit_budget);

        // 3. Stderr-heavy jest summary (covers_stderr == true) sets replaced = Some("both").
        let mut jest_err = String::new();
        for i in 0..80 {
            jest_err.push_str(&format!("PASS src/mod_{i:02}.test.ts\n"));
        }
        jest_err.push_str(
            "\nTest Suites: 80 passed, 80 total\nTests:       240 passed, 240 total\nSnapshots:   0 total\nTime:        2.41 s\n",
        );
        let jest_file = dir.path().join("jest.err");
        fs::write(&jest_file, &jest_err).unwrap();
        let jest = fake_script(
            dir.path(),
            "jest",
            &format!("cat '{}' >&2", jest_file.display()),
        );
        let jest_args = vec![jest.as_os_str().to_os_string()];
        let budget = crate::adaptive::preview_budget(&ws, &jest_args).unwrap();
        assert_eq!(
            execute(
                &ws,
                &jest_args,
                budget,
                false,
                "s",
                (true, false),
                true,
                false
            )
            .unwrap(),
            0
        );
        let jest_run = latest_run(&ws);
        assert_eq!(jest_run.replaced.as_deref(), Some("both"));
        assert_eq!(jest_run.budget_bytes, Some(budget));
        assert!(!jest_run.explicit_budget);
    }
}
