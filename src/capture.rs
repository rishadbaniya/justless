use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
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

static ANSI: LazyLock<Regex> =
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
    const BOTH: [Self; 2] = [Self::Stdout, Self::Stderr];

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
            Self::Stdout => io::stdout().lock().write_all(bytes),
            Self::Stderr => io::stderr().lock().write_all(bytes),
        }
    }
}

pub fn diagnostic_excerpt(directory: &Path, diagnostic: &Diagnostic) -> Result<Vec<String>> {
    let file = File::open(directory.join(diagnostic.stream.name()))?;
    let start = diagnostic.log_line.saturating_sub(2).max(1);
    let end = diagnostic.log_line.saturating_add(2);
    let mut excerpt = Vec::new();
    let mut file = BufReader::new(file);
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

#[derive(Serialize, Deserialize)]
pub struct Run {
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
    /// The command line, its directory relative to the root (empty at the root) and
    /// the session, so `learn` can pair a failure with its correction.
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub session: String,
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
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Run {
    fn unfinished(id: String, family: &str, label: &str, hook: bool) -> Self {
        Self {
            id,
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
            argv: Vec::new(),
            cwd: String::new(),
            session: String::new(),
            diagnostics: Vec::new(),
            source_digests: BTreeMap::new(),
            stream_bytes: Vec::new(),
            produced_bytes: Vec::new(),
            unfinished: true,
            drained: false,
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

pub fn execute(
    workspace: &Workspace,
    args: &[OsString],
    max_bytes: usize,
    raw: bool,
    session: &str,
    fresh: bool,
    hook: bool,
) -> Result<i32> {
    let mut command = Command::new(&args[0]);
    command.current_dir(&workspace.cwd);
    if raw {
        command.args(&args[1..]);
        return Ok(uncaptured(&mut command, &args[0]));
    }

    let settings = workspace.settings()?;
    let selection = crate::filters::Selection::for_run(args, &settings.filters)?;
    let words: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
    // A recursive grep would otherwise read the archived logs, including the stdout
    // file it is writing.
    if let Some(exclude) = crate::search::exclude_state(&words) {
        command.arg(exclude);
    }
    command.args(&args[1..]);
    let family = match selection.profile {
        Some(
            crate::filters::Profile::Lint
            | crate::filters::Profile::Search
            | crate::filters::Profile::Listing,
        ) => crate::filters::tool_label(args),
        Some(profile) => profile.label().to_owned(),
        None => crate::summary::family(args),
    };
    // A read-only checkout or sandbox must not stop the command itself.
    let mut archive = match Archive::create(workspace, &family, &history::label(&words), hook) {
        Ok(archive) => archive,
        Err(error) => {
            eprintln!("justless: cannot record this run ({error:#}); running it without capture");
            return Ok(uncaptured(&mut command, &args[0]));
        }
    };
    let exited = match archive.run(&mut command, settings.max_run_bytes) {
        Ok(Ok(status)) => Some(status),
        Ok(Err(error)) => {
            eprintln!("justless: wait for {}: {error}", args[0].to_string_lossy());
            None
        }
        Err(error) => return Ok(spawn_failure(&args[0], &error)),
    };
    let status = exited.map_or(1, exit_code);

    // The command has run. From here on its output and exit status always reach the
    // caller, even when the archive cannot be read back or saved.
    let capture = Capture {
        workspace,
        args,
        words: &words,
        selection: &selection,
        max_bytes,
        session,
        fresh,
        hook,
        retention: (settings.max_runs, settings.max_archive_bytes),
    };
    match capture.record(&mut archive, family, status) {
        Ok((previews, footer, _)) => {
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
            let last = archive.replay();
            let truncation = archive.truncation();
            if !truncation.is_empty() {
                eprint!("{}{truncation}", separator(last));
            }
        }
    }
    signals::reraise(exited);
    Ok(status)
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
        let temporary = Builder::new()
            .prefix("run-")
            .rand_bytes(12)
            .tempdir_in(&runs)
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
        })
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
    /// the command, and Justless finishes the record once the command exits.
    fn run(&mut self, command: &mut Command, cap: u64) -> io::Result<io::Result<ExitStatus>> {
        let mut writers = std::mem::take(&mut self.writers).into_iter();
        let (Some(stdout_file), Some(stderr_file)) = (writers.next(), writers.next()) else {
            return Err(io::Error::other("the command already ran"));
        };
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        signals::install();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                signals::restore();
                return Err(error);
            }
        };
        signals::watch(child.id());
        let exited = Arc::new(AtomicBool::new(false));
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let stdout = stdout.map(|pipe| {
            let exited = Arc::clone(&exited);
            std::thread::spawn(move || pump(pipe, stdout_file, cap, Stream::Stdout, &exited))
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
        if let Some(pumped) = stdout.map(joined) {
            self.collect(Stream::Stdout, pumped, cap);
        }
        if let Some(pumped) = stderr.map(joined) {
            self.collect(Stream::Stderr, pumped, cap);
        }
        Ok(status)
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

    /// Print both complete streams from the open handles. Returns the last byte
    /// printed.
    fn replay(&self) -> Option<u8> {
        let mut last = None;
        for (stream, mut file) in Stream::BOTH.into_iter().zip(&self.readers) {
            match file
                .seek(SeekFrom::Start(0))
                .and_then(|_| copy_out(&mut file, stream))
            {
                Ok(byte) => last = byte.or(last),
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

/// Copy a stream's pipe into its archive file until the pipe closes, storing at
/// most `cap` bytes and counting the rest. Each chunk is written at once, so a
/// killed Justless leaves what it read on disk. Once `exited` is set, only the
/// bytes already waiting in the pipe are still the command's. After them, a pipe
/// that a background process still holds is returned instead of waited on, even
/// when that process keeps writing.
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
            // Readable with nothing waiting means every writer closed the pipe.
            Some(0) => match pending(&pipe) {
                Some(0) => readable(&pipe, 0),
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
    //! so another signal stops Justless as usual.
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

    static CHILD: AtomicI32 = AtomicI32::new(0);
    static RECEIVED: AtomicI32 = AtomicI32::new(0);
    /// The interrupt to forward when the grace period ends.
    static PENDING: AtomicI32 = AtomicI32::new(0);
    static INSTALLED: AtomicU64 = AtomicU64::new(0);
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
        loop {
            // SAFETY: WNOWAIT leaves the exited command for Child::wait to reap.
            let waited = unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT)
            };
            if waited == 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
        restore();
    }

    /// Exit from the signal that stopped the command when Justless received it
    /// too, after the preview is out, so a calling shell stops its script just as
    /// it would for the command itself.
    pub fn reraise(status: Option<std::process::ExitStatus>) {
        use std::os::unix::process::ExitStatusExt;
        let Some(signal) = status.and_then(|status| status.signal()) else {
            return;
        };
        if RECEIVED.load(Ordering::SeqCst) != signal {
            return;
        }
        let _ = std::io::Write::flush(&mut std::io::stdout());
        // SAFETY: restores the default action and raises the signal on Justless.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    pub fn restore() {
        CHILD.store(0, Ordering::SeqCst);
        PENDING.store(0, Ordering::SeqCst);
        // Cancel a grace period while its handler is still installed.
        arm(0);
        let installed = INSTALLED.swap(0, Ordering::SeqCst);
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
    session: &'a str,
    fresh: bool,
    hook: bool,
    /// The most runs and archive bytes to keep.
    retention: (usize, u64),
}

impl Capture<'_> {
    /// Scan the archived streams, save the record and return the previews and footer.
    fn record(
        &self,
        archive: &mut Archive,
        family: String,
        status: i32,
    ) -> Result<(Previews, String, Run)> {
        let Self {
            workspace,
            args,
            words,
            selection,
            max_bytes,
            session,
            fresh,
            hook,
            retention,
        } = *self;
        let directory = archive.directory.clone();
        let lengths = archive.lengths();
        let mut run = Run {
            status,
            raw_bytes: lengths.iter().sum(),
            stream_bytes: lengths.clone(),
            produced_bytes: archive.produced(),
            unfinished: false,
            drained: archive.late,
            label: history::label(words),
            argv: args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            cwd: workspace
                .cwd
                .strip_prefix(&workspace.root)
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default(),
            session: session.to_owned(),
            ..Run::unfinished(archive.id(), &family, "", hook)
        };
        let mut parser = Parser::new(&run.family, selection.profile);
        let mut lint = (selection.profile == Some(crate::filters::Profile::Lint))
            .then(|| crate::lint::Lint::new(&workspace.root));
        let mut grouping = match selection.profile {
            Some(crate::filters::Profile::Search) => Some(Grouping::Search(
                crate::search::Search::new(&workspace.cwd, words),
            )),
            Some(crate::filters::Profile::Listing) => {
                Some(Grouping::Listing(crate::listing::Listing::new(words)))
            }
            _ => None,
        };
        let mut locations = Locations::default();
        let mut previews = Vec::new();
        for (index, stream) in Stream::BOTH.into_iter().enumerate() {
            let preview = scan(
                workspace,
                &archive.readers[index],
                lengths[index],
                stream,
                max_bytes,
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
            previews.push((stream, preview));
        }
        locations.finish(&mut run);
        let stdout_bytes = lengths[0];

        run.summary = parser.finish();
        let mut key = Sha256::new();
        for field in std::iter::once(workspace.cwd.as_os_str().as_encoded_bytes())
            .chain(args.iter().map(|arg| arg.as_encoded_bytes()))
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
                .filter(|run| !run.unfinished)
        };
        // A truncated archive holds only a prefix, so its digests cannot show that
        // two runs printed the same output.
        run.unchanged = run.produced_bytes.is_empty()
            && previous.as_ref().is_some_and(|p| {
                p.produced_bytes.is_empty() && p.digests == run.digests && p.status == run.status
            });
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
        let grouped = grouping.and_then(|grouping| {
            let previous = previous.as_ref().map(|r| (r.id.as_str(), &r.summary));
            match grouping {
                Grouping::Search(search) => search.finish(&run.family, max_bytes, previous, true),
                Grouping::Listing(listing) => {
                    listing.finish(&run.family, max_bytes, previous, true)
                }
            }
        });
        if let Some((_, summary)) = &grouped {
            run.summary = summary.clone();
        }
        let shown: usize = previews.iter().map(|(_, bytes)| bytes.len()).sum();
        if let Some(prior) = &previous
            && run.unchanged
        {
            if shown > 64 {
                let target = usize::from(previews[0].1.is_empty());
                for (_, bytes) in &mut previews {
                    bytes.clear();
                }
                previews[target].1 = format!("Output unchanged since {}.\n", prior.id).into_bytes();
            }
        } else if let Some((report, _)) = lint {
            // Findings replace both streams: the grouped report names every rule and
            // location, and each group links to its original lines in the full log.
            // Output that fits the budget and is smaller than the report stays
            // verbatim, even on a rerun.
            let report = fit(report, max_bytes);
            let clipped = lengths.iter().any(|&length| length > max_bytes as u64);
            if report.len() < shown || clipped {
                previews[0].1 = report.into_bytes();
                previews[1].1.clear();
            }
        } else if let Some((report, _)) = grouped {
            // Search and listing groups replace a large stdout only. Smaller output stays
            // verbatim, and stderr keeps messages such as unreadable directories.
            if stdout_bytes > max_bytes as u64 && !report.is_empty() {
                previews[0].1 = fit(report, max_bytes).into_bytes();
            }
        } else if let Some(summary) = run.summary.render(
            previous.as_ref().map(|r| (r.id.as_str(), &r.summary)),
            run.status,
        ) && (stdout_bytes > max_bytes as u64
            || previous.is_some()
            || summary.len() < previews[0].1.len())
        {
            previews[0].1 = fit(summary, max_bytes).into_bytes();
        }
        run.preview_bytes = previews.iter().map(|(_, s)| s.len()).sum();
        let footer = footer(&run, &previews, &archive.readers, hook, selection)?;
        run.footer_bytes = footer.len();
        write_json(&directory.join("record.json"), &run).context("save run record")?;
        archive.keep();
        archive.finish();
        if let Some(prior) = previous.as_ref().filter(|_| run.unchanged) {
            archive.share(&workspace.state.join("runs").join(&prior.id), prior, &run);
        }
        if guard.is_some() {
            if let Err(error) = crate::workspace::write_text(&baseline, &run.id) {
                eprintln!("justless: could not update run baseline: {error:#}");
            }
            history::prune(workspace, &run.id, retention.0, retention.1);
        }
        drop(guard);
        Ok((previews, footer, run))
    }
}

/// The line printed after the preview. A command typed by a person keeps the full
/// footer. A hook-routed command that reached the agent verbatim prints nothing, since
/// the agent already has every byte; otherwise one short marker names the run so the
/// agent can recover what was left out.
fn footer(
    run: &Run,
    previews: &[(Stream, Vec<u8>)],
    readers: &[File],
    hook: bool,
    selection: &crate::filters::Selection,
) -> Result<String> {
    let id = &run.id;
    // A stream past `max_run_bytes` is never shown in full, so the marker always
    // follows, even after a hook-routed run.
    let truncation = run.truncation();
    if !hook {
        return Ok(format!(
            "\n[justless {id}] exit {} · {} of {} output bytes · justless log {id}\n{truncation}",
            run.status, run.preview_bytes, run.raw_bytes
        ));
    }
    // Compare the printed bytes with what the command wrote before it exited: equal
    // sizes alone can hide a replaced or regrouped stream. A background process can
    // keep adding to the archive, so only the bytes counted at exit are read.
    let mut verbatim = truncation.is_empty() && run.preview_bytes as u64 == run.raw_bytes;
    for (stream, preview) in previews {
        if !verbatim {
            break;
        }
        let index = stream.index();
        verbatim = run.stream_bytes.get(index) == Some(&(preview.len() as u64))
            && match readers.get(index) {
                Some(mut file) => {
                    let mut stored = Vec::with_capacity(preview.len());
                    file.seek(SeekFrom::Start(0))?;
                    file.take(preview.len() as u64).read_to_end(&mut stored)?;
                    stored == *preview
                }
                None => false,
            };
    }
    // A search that matched nothing exits 1 with no output; that is an answer, not a failure.
    let empty_search = run.status == 1
        && run.raw_bytes == 0
        && selection.profile == Some(crate::filters::Profile::Search);
    let separator = separator(
        previews
            .iter()
            .rev()
            .find_map(|(_, bytes)| bytes.last().copied()),
    );
    let line = if verbatim && (run.status == 0 || empty_search) {
        String::new()
    } else if verbatim || run.unchanged {
        if run.status == 0 {
            String::new()
        } else {
            format!("{separator}[justless {id} · exit {}]\n", run.status)
        }
    } else {
        let exit = if run.status == 0 {
            String::new()
        } else {
            format!(" · exit {}", run.status)
        };
        format!(
            "{separator}[justless {id} · {} of {} shown{exit}]\n",
            history::human_bytes(run.preview_bytes as u64),
            history::human_bytes(run.raw_bytes)
        )
    };
    Ok(line + &truncation)
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

/// Groups stdout of a search by file, or of a file listing by directory.
enum Grouping {
    Search(crate::search::Search),
    Listing(crate::listing::Listing),
}

fn scan(
    workspace: &Workspace,
    mut file: &File,
    length: u64,
    stream: Stream,
    max_bytes: usize,
    run: &mut Run,
    observers: &mut Observers,
) -> Result<Vec<u8>> {
    let Observers {
        parser,
        selection,
        lint,
        grouping,
        locations,
    } = observers;
    const DIAGNOSTICS: &[u8] = b"\n[diagnostics]\n";
    const CLIPPED: &[u8] = b"\n[output clipped. Full log retained.]\n";
    let truncated = length > max_bytes as u64;
    let payload = max_bytes - DIAGNOSTICS.len() - CLIPPED.len();
    file.seek(SeekFrom::Start(0))?;
    let mut file = BufReader::new(HashedFile {
        file: file.take(length),
        digest: Sha256::new(),
    });
    let mut raw = Vec::new();
    let mut end = Vec::new();
    let mut preview = Vec::new();
    let mut head = Excerpt::new(payload / 6);
    let mut section = Section::default();
    let mut block: Option<Block> = None;
    let mut patch = false;
    let tail_limit = payload / 3;
    let mut tail = VecDeque::new();
    let mut tail_bytes = 0;
    let mut line_no = 0;
    let mut filter = crate::filters::LineFilter::new(selection, max_bytes);

    let line_limit = if truncated {
        SCAN_LINE_LIMIT
    } else {
        max_bytes
    };
    loop {
        let Some(cut) = read_line(&mut file, &mut raw, line_limit, &mut end, tail_limit)? else {
            break;
        };
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
                None => {}
            }
        }
        let structured = cargo_message(&text);
        let rendered = structured.as_ref().and_then(rendered_message);
        let shown = rendered.as_deref().unwrap_or(&text);
        let line = shown.trim_end_matches(['\n', '\r']);
        // Lines inside a patch hunk are file content, not diagnostics.
        patch = line.starts_with("@@ ")
            || patch && (line.is_empty() || line.starts_with([' ', '+', '-', '\\']));

        // A failure keeps the lines that explain it: a traceback through its
        // exception line, a panic's message, or a compiler error's location.
        let mut failure = false;
        if let Some(open) = block.as_mut() {
            match open.extend(line, shown) {
                Extend::More => failure = true,
                Extend::Last => {
                    failure = true;
                    section.add(block.take().unwrap(), payload / 2);
                }
                Extend::Stop => section.add(block.take().unwrap(), payload / 2),
            }
        }
        if !failure
            && !patch
            && let Some(tier) = failure_tier(line)
        {
            failure = true;
            if let Some(earlier) = block.replace(Block::new(tier, line, shown)) {
                section.add(earlier, payload / 2);
            }
        }
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

        if !truncated {
            preview.extend_from_slice(&raw);
        } else {
            let trimmed = line.trim_start();
            if !failure && !patch && (has_location || trimmed.starts_with("warning")) {
                section.add(Block::new(Tier::Note, line, shown), payload / 2);
            }
            head.push(shown);
            // The tail ends where the stream ends, even inside an enormous line.
            let mut excerpt = Excerpt::new(tail_limit);
            if cut {
                // Only the start of an enormous line was scanned. Show its real end.
                let wanted = tail_limit.saturating_sub(end.len());
                let mut suffix = raw[raw.len().saturating_sub(wanted)..].to_vec();
                suffix.extend_from_slice(&end);
                let suffix = String::from_utf8_lossy(&suffix);
                excerpt.push_end(&ANSI.replace_all(&suffix, ""), true);
            } else {
                excerpt.push_end(shown, false);
            }
            if !excerpt.text.is_empty() {
                tail_bytes += excerpt.text.len();
                tail.push_back(excerpt.text);
                while tail_bytes > tail_limit {
                    tail_bytes -= tail.pop_front().unwrap().len();
                }
            }
        }
        raw.clear();
        end.clear();
    }
    if let Some(open) = block.take() {
        section.add(open, payload / 2);
    }

    if truncated {
        preview.extend_from_slice(head.text.as_bytes());
        let diagnostics = section.render(payload / 2);
        if !diagnostics.is_empty() {
            preview.extend_from_slice(DIAGNOSTICS);
            preview.extend_from_slice(diagnostics.as_bytes());
        }
        preview.extend_from_slice(CLIPPED);
        preview.extend(tail.into_iter().flat_map(String::into_bytes));
    }
    run.digests
        .push(format!("{:x}", file.into_inner().digest.finalize()));
    if let Some(filtered) = filter.finish()
        && (filtered.len() < preview.len() || truncated && filtered.len() <= max_bytes)
    {
        return Ok(filtered);
    }
    Ok(preview)
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
    indent: usize,
    kind: Follow,
    lines: usize,
    frames: VecDeque<String>,
    skipped: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    /// A Python traceback runs through its first line at the header's indentation.
    Traceback,
    /// A panic message follows `panicked at path:line:col:` until a blank line.
    Panic,
    /// Indented location and source lines under a compiler error.
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

impl Block {
    fn new(tier: Tier, line: &str, shown: &str) -> Self {
        let trimmed = line.trim_start();
        let kind = if tier == Tier::Note {
            Follow::Nothing
        } else if trimmed.starts_with("Traceback") {
            Follow::Traceback
        } else if trimmed.contains(" panicked at ") && trimmed.ends_with(':') {
            Follow::Panic
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
            indent: indentation(line),
            kind,
            lines: 0,
            frames: VecDeque::new(),
            skipped: 0,
        }
    }

    fn extend(&mut self, line: &str, shown: &str) -> Extend {
        let indent = indentation(line);
        let blank = line.trim().is_empty();
        let next = match self.kind {
            Follow::Nothing => return Extend::Stop,
            Follow::Traceback if blank || indent < self.indent => return Extend::Stop,
            Follow::Traceback if indent > self.indent => {
                self.frames.push_back(with_newline(shown));
                if self.frames.len() > TRACEBACK_FRAMES {
                    self.frames.pop_front();
                    self.skipped += 1;
                }
                return Extend::More;
            }
            Follow::Traceback => {
                // The exception line, such as `ValueError: bad input`.
                self.finish_frames();
                self.text.push_str(&with_newline(shown));
                return Extend::Last;
            }
            Follow::Panic => {
                let trimmed = line.trim_start();
                !blank
                    && !trimmed.starts_with("note:")
                    && !trimmed.starts_with("stack backtrace:")
                    && !trimmed.starts_with("---- ")
            }
            Follow::Indented => !blank && (indent > self.indent || RUST_GUTTER.is_match(line)),
        };
        if !next || self.lines >= FOLLOW_LINES {
            return Extend::Stop;
        }
        self.lines += 1;
        self.text.push_str(&with_newline(shown));
        Extend::More
    }

    fn finish_frames(&mut self) {
        if self.skipped > 0 {
            self.text.push_str(&format!(
                "{}{CUT} {} earlier frame line(s)\n",
                " ".repeat(self.indent + 2),
                self.skipped
            ));
            self.skipped = 0;
        }
        for frame in self.frames.drain(..) {
            self.text.push_str(&frame);
        }
    }
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

/// The [diagnostics] section of a clipped stream: failures first, then indented
/// error lines, then located lines and warnings. Repeated entries are counted
/// rather than repeated.
#[derive(Default)]
struct Section {
    entries: [Vec<(String, usize)>; 3],
    bytes: [usize; 3],
    index: HashMap<String, (usize, usize)>,
}

impl Section {
    fn add(&mut self, mut block: Block, budget: usize) {
        block.finish_frames();
        let tier = block.tier as usize;
        if let Some(&(tier, position)) = self.index.get(&block.text) {
            self.entries[tier][position].1 += 1;
            return;
        }
        // An entry past the budget could not be shown, so it is not kept either.
        if self.bytes[tier] >= budget {
            return;
        }
        self.bytes[tier] += block.text.len();
        self.index
            .insert(block.text.clone(), (tier, self.entries[tier].len()));
        self.entries[tier].push((block.text, 1));
    }

    fn render(&self, budget: usize) -> String {
        let mut out = Excerpt::new(budget);
        for (text, count) in self.entries.iter().flatten() {
            let mut lines = text.split_inclusive('\n');
            let first = lines.next().unwrap_or_default();
            let first = if *count > 1 {
                format!("{}  [×{count}]\n", first.trim_end_matches(['\n', '\r']))
            } else {
                first.to_owned()
            };
            for line in std::iter::once(first.as_str()).chain(lines) {
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
    // These prefixes mean a failure wherever they appear, including in indented
    // build script output.
    if [
        "Traceback",
        "FAILED",
        "thread '",
        "assertion ",
        "left:",
        "right:",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
        || trimmed.trim_end().ends_with("... FAILED")
        || LOCATED_ERROR.is_match(line)
    {
        return Some(Tier::Failure);
    }
    let error = [
        "error:",
        "error[",
        "ERROR",
        "fatal:",
        "failures:",
        "test result: FAILED",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix));
    if trimmed.len() == line.len() {
        (error || EXCEPTION.is_match(line)).then_some(Tier::Failure)
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
        if !self.source_digests.contains_key(&location.file)
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

fn exit_code(status: ExitStatus) -> i32 {
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

pub fn log(directory: &Path, streams: &[Stream], from: usize, to: usize) -> Result<()> {
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
    for &stream in streams {
        let path = directory.join(stream.name());
        let last = replay_file(&path, stream, from, to)?;
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
            eprint!("{}{notes}", separator(last));
        }
    }
    Ok(())
}

/// Replay a stream file, or a range of its lines, and flush it. Returns the last
/// byte printed.
fn replay_file(path: &Path, stream: Stream, from: usize, to: usize) -> Result<Option<u8>> {
    let mut file = File::open(path)?;
    if from == 1 && to == usize::MAX {
        return Ok(copy_out(&mut file, stream)?);
    }
    let mut file = BufReader::new(file);
    let mut line = Vec::new();
    let mut last = None;
    for number in 1..=to {
        if file.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if number >= from {
            stream.print(&line)?;
            last = line.last().copied().or(last);
        }
        line.clear();
    }
    if stream == Stream::Stdout {
        io::stdout().flush()?;
    }
    Ok(last)
}

/// Copy a stream to Justless's own stdout or stderr and flush it, so a note that
/// follows on stderr comes after it even when both go to one file. Returns the
/// last byte copied.
fn copy_out(reader: &mut impl Read, stream: Stream) -> io::Result<Option<u8>> {
    fn copy(reader: &mut impl Read, mut output: impl Write) -> io::Result<Option<u8>> {
        let mut buffer = vec![0; 64 * 1024];
        let mut last = None;
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            output.write_all(&buffer[..read])?;
            last = Some(buffer[read - 1]);
        }
        output.flush()?;
        Ok(last)
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

pub fn search(directory: &Path, streams: &[Stream], needle: &str, limit: usize) -> Result<()> {
    let mut remaining = limit;
    matches(directory, streams, needle, |stream, n, line| {
        println!("{}:{n}: {line}", stream.name());
        remaining -= 1;
        remaining > 0
    })
}

/// Keep one overlapping window so even a single enormous log line is searchable.
pub fn matches(
    directory: &Path,
    streams: &[Stream],
    needle: &str,
    mut emit: impl FnMut(Stream, usize, &str) -> bool,
) -> Result<()> {
    for &stream in streams {
        let mut file = BufReader::new(File::open(directory.join(stream.name()))?);
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
                    if !emit(stream, line_number, text[start..end].trim_end()) {
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
