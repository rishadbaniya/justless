use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::LazyLock;

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
    pub diagnostics: Vec<Diagnostic>,
    pub source_digests: BTreeMap<String, String>,
    /// Bytes each stream held when the command exited. A background process can keep
    /// writing to the archive afterwards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_bytes: Vec<u64>,
    /// Set on the record written before the command starts. It stays set when
    /// Justless stopped before the command finished.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unfinished: bool,
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
            diagnostics: Vec::new(),
            source_digests: BTreeMap::new(),
            stream_bytes: Vec::new(),
            unfinished: true,
        }
    }
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

    let selection = crate::filters::Selection::for_run(args, &workspace.settings()?.filters)?;
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
    let status = match archive.run(&mut command) {
        Ok(status) => exit_code(status),
        Err(error) => return Ok(spawn_failure(&args[0], &error)),
    };

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
            if !footer.is_empty() {
                eprint!("{footer}");
            }
        }
        Err(error) => {
            eprintln!("justless: could not save run, restoring full output: {error:#}");
            archive.replay();
        }
    }
    Ok(status)
}

/// Run without recording, with the same exit statuses a shell reports.
fn uncaptured(command: &mut Command, program: &OsStr) -> i32 {
    signals::install();
    match command.spawn() {
        Ok(mut child) => {
            signals::watch(child.id());
            let status = child.wait();
            signals::release();
            match status {
                Ok(status) => exit_code(status),
                Err(error) => {
                    eprintln!("justless: wait for {}: {error}", program.to_string_lossy());
                    1
                }
            }
        }
        Err(error) => spawn_failure(program, &error),
    }
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

/// A run directory and its streams. The command writes through its own handles
/// while Justless reads through handles opened before the command starts, so the
/// output stays readable even if the directory is deleted while the command runs.
struct Archive {
    directory: PathBuf,
    temporary: TempDir,
    writers: Vec<File>,
    readers: Vec<File>,
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
        })
    }

    /// Run the command into the archive. Termination signals sent to Justless
    /// reach the command, and Justless finishes the record once the command exits.
    fn run(&mut self, command: &mut Command) -> io::Result<ExitStatus> {
        let mut writers = std::mem::take(&mut self.writers).into_iter();
        let (Some(stdout), Some(stderr)) = (writers.next(), writers.next()) else {
            return Err(io::Error::other("the command already ran"));
        };
        command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        signals::install();
        let mut child = command.spawn()?;
        signals::watch(child.id());
        let status = child.wait();
        signals::release();
        status
    }

    fn id(&self) -> String {
        id_of(&self.directory)
    }

    /// Stream sizes when the command exited. Later writes by background processes
    /// stay in the archive but are not part of the record.
    fn lengths(&self) -> io::Result<Vec<u64>> {
        self.readers
            .iter()
            .map(|file| file.metadata().map(|m| m.len()))
            .collect()
    }

    fn keep(&mut self) {
        self.temporary.disable_cleanup(true);
    }

    /// Print both complete streams from the open handles.
    fn replay(&self) {
        for (stream, mut file) in Stream::BOTH.into_iter().zip(&self.readers) {
            let copied = file.seek(SeekFrom::Start(0)).and_then(|_| match stream {
                Stream::Stdout => io::copy(&mut file, &mut io::stdout().lock()),
                Stream::Stderr => io::copy(&mut file, &mut io::stderr().lock()),
            });
            if let Err(error) = copied
                && error.kind() != io::ErrorKind::BrokenPipe
            {
                eprintln!("justless: could not replay {}: {error}", stream.name());
            }
        }
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
    //! Forward SIGTERM and SIGHUP to the command. The command shares Justless's
    //! process group, so a terminal interrupt or a signal to the whole group
    //! already reaches it. Like a shell waiting for a foreground job, Justless
    //! then waits for the command to exit and finishes the record.
    use std::sync::atomic::{AtomicI32, Ordering};

    static CHILD: AtomicI32 = AtomicI32::new(0);
    static RECEIVED: AtomicI32 = AtomicI32::new(0);
    const FORWARDED: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGHUP];
    const OBSERVED: [libc::c_int; 4] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT];

    extern "C" fn handle(signal: libc::c_int) {
        RECEIVED.store(signal, Ordering::SeqCst);
        let child = CHILD.load(Ordering::SeqCst);
        if child > 0 && FORWARDED.contains(&signal) {
            // SAFETY: kill is async-signal-safe.
            unsafe { libc::kill(child, signal) };
        }
    }

    pub fn install() {
        for signal in OBSERVED {
            // SAFETY: the handler only touches atomics and calls kill. A signal
            // that is already ignored, as under nohup, stays ignored so the
            // command inherits that disposition.
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
                libc::sigaction(signal, &action, std::ptr::null_mut());
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
            // SAFETY: pid is the command we just started and have not reaped.
            unsafe { libc::kill(pid, early) };
        }
    }

    pub fn release() {
        CHILD.store(0, Ordering::SeqCst);
    }
}

#[cfg(not(unix))]
mod signals {
    pub fn install() {}
    pub fn watch(_: u32) {}
    pub fn release() {}
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
        } = *self;
        let directory = archive.directory.clone();
        let lengths = archive.lengths().context("read the captured streams")?;
        let mut run = Run {
            status,
            raw_bytes: lengths.iter().sum(),
            stream_bytes: lengths.clone(),
            unfinished: false,
            label: history::label(words),
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
                },
            )
            .with_context(|| format!("read {}", stream.name()))?;
            previews.push((stream, preview));
        }
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
        run.unchanged = previous
            .as_ref()
            .is_some_and(|p| p.digests == run.digests && p.status == run.status);
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
            if report.len() < shown || previous.is_some() {
                previews[0].1 = fit(report, max_bytes).into_bytes();
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
        let footer = footer(&run, &previews, &directory, hook, selection)?;
        run.footer_bytes = footer.len();
        write_json(&directory.join("record.json"), &run).context("save run record")?;
        archive.keep();
        if guard.is_some()
            && let Err(error) = crate::workspace::write_text(&baseline, &run.id)
        {
            eprintln!("justless: could not update run baseline: {error:#}");
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
    directory: &Path,
    hook: bool,
    selection: &crate::filters::Selection,
) -> Result<String> {
    let id = &run.id;
    if !hook {
        return Ok(format!(
            "\n[justless {id}] exit {} · {} of {} output bytes · justless log {id}\n",
            run.status, run.preview_bytes, run.raw_bytes
        ));
    }
    // Compare the printed bytes with the archive itself: equal sizes alone can hide
    // a replaced or regrouped stream.
    let mut verbatim = run.preview_bytes as u64 == run.raw_bytes;
    for (stream, preview) in previews {
        if !verbatim {
            break;
        }
        let path = directory.join(stream.name());
        verbatim =
            fs::metadata(&path)?.len() == preview.len() as u64 && fs::read(&path)? == *preview;
    }
    // A search that matched nothing exits 1 with no output; that is an answer, not a failure.
    let empty_search = run.status == 1
        && run.raw_bytes == 0
        && selection.profile == Some(crate::filters::Profile::Search);
    let separator = match previews.iter().rev().find(|(_, bytes)| !bytes.is_empty()) {
        Some((_, bytes)) if !bytes.ends_with(b"\n") => "\n",
        _ => "",
    };
    Ok(if verbatim && (run.status == 0 || empty_search) {
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
    })
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
    let mut preview = Vec::new();
    let mut head = Vec::new();
    let mut failures = Vec::new();
    let mut notes = Vec::new();
    let mut tail = VecDeque::new();
    let mut tail_bytes = 0;
    let mut line_no = 0;
    let mut filter = crate::filters::LineFilter::new(selection, max_bytes);

    let line_limit = if truncated {
        SCAN_LINE_LIMIT
    } else {
        max_bytes
    };
    while read_prefix(&mut file, &mut raw, line_limit)? {
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
        let locations: Vec<Location> = if let Some(message) = &structured {
            json_locations(workspace, message)
        } else {
            source_location(workspace, &text).into_iter().collect()
        };
        let has_location = !locations.is_empty();
        for location in locations {
            if !run.source_digests.contains_key(&location.file)
                && let Ok(source) = workspace
                    .source_path(&location.file)
                    .and_then(|p| fs::read(p).context("read diagnostic source"))
            {
                run.source_digests
                    .insert(location.file.clone(), source_digest(&source));
            }
            run.diagnostics.push(Diagnostic {
                location,
                log_line: line_no,
                stream,
            });
        }

        if !truncated {
            preview.extend_from_slice(&raw);
        } else {
            let rendered = structured.as_ref().and_then(rendered_message);
            let text = rendered.as_deref().unwrap_or(&text);
            let line = text.trim_start();
            let summary_limit = payload / 2;
            if is_failure(line) {
                append_excerpt(&mut failures, text, summary_limit);
            } else if has_location || line.starts_with("warning") {
                append_excerpt(&mut notes, text, summary_limit);
            }
            append_excerpt(&mut head, text, payload / 6);
            let mut excerpt = Vec::new();
            let tail_limit = payload / 3;
            if text.len() > tail_limit {
                let start = text.ceil_char_boundary(text.len() - tail_limit);
                append_excerpt(&mut excerpt, &text[start..], tail_limit);
            } else {
                append_excerpt(&mut excerpt, text, tail_limit);
            }
            if !excerpt.is_empty() {
                tail_bytes += excerpt.len();
                tail.push_back(excerpt);
                while tail_bytes > tail_limit {
                    tail_bytes -= tail.pop_front().unwrap().len();
                }
            }
        }
        raw.clear();
    }

    if truncated {
        preview.extend_from_slice(&head);
        if !failures.is_empty() || !notes.is_empty() {
            preview.extend_from_slice(DIAGNOSTICS);
            let available = payload / 2 - failures.len();
            let notes = std::str::from_utf8(&notes).expect("UTF-8 excerpt");
            let end = notes.floor_char_boundary(available.min(notes.len()));
            preview.extend_from_slice(&failures);
            preview.extend_from_slice(&notes.as_bytes()[..end]);
        }
        preview.extend_from_slice(CLIPPED);
        preview.extend(tail.into_iter().flatten());
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

fn append_excerpt(output: &mut Vec<u8>, line: &str, limit: usize) {
    let available = limit.saturating_sub(output.len());
    let end = line.floor_char_boundary(available.min(line.len()));
    output.extend_from_slice(&line.as_bytes()[..end]);
}

fn is_failure(line: &str) -> bool {
    [
        "error:",
        "error[",
        "ERROR",
        "fatal:",
        "Traceback",
        "FAILED",
        "failures:",
        "test result: FAILED",
        "thread '",
        "assertion ",
        "left:",
        "right:",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
        || line.trim_end().ends_with("... FAILED")
}

fn source_location(workspace: &Workspace, text: &str) -> Option<Location> {
    if let Some(captures) = PYTHON_SOURCE
        .captures(text)
        .or_else(|| TYPESCRIPT_SOURCE.captures(text))
    {
        let start = captures[2].parse().ok()?;
        return locate(workspace, &captures[1], start, start);
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
        name.strip_prefix("file://").unwrap_or(name),
        start,
        start,
    )
}

fn locate(workspace: &Workspace, name: &str, start: usize, end: usize) -> Option<Location> {
    let path = Path::new(name);
    if start == 0 || end < start {
        return None;
    }
    let candidates = [workspace.cwd.join(path), workspace.root.join(path)];
    let file = candidates.iter().find_map(|path| {
        let canonical = path.canonicalize().ok()?;
        workspace.relative_file(&canonical)
    })?;
    Some(Location { file, start, end })
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

fn json_locations(workspace: &Workspace, message: &serde_json::Value) -> Vec<Location> {
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
            locate(workspace, file, start, end)
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
    #[derive(Deserialize)]
    struct Lengths {
        #[serde(default)]
        stream_bytes: Vec<u64>,
    }
    let recorded = fs::read(directory.join("record.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Lengths>(&bytes).ok())
        .map(|lengths| lengths.stream_bytes)
        .unwrap_or_default();
    for &stream in streams {
        let path = directory.join(stream.name());
        replay_file(&path, stream, from, to)?;
        // A background process can keep writing after the command exits. Those
        // bytes stay in the archive, but no preview or record described them.
        if from == 1
            && to == usize::MAX
            && let Some(&expected) = recorded.get(stream.index())
            && let Ok(meta) = fs::metadata(&path)
            && meta.len() > expected
        {
            eprintln!(
                "[justless: the last {} byte(s) of {} were written after the command exited]",
                meta.len() - expected,
                stream.name()
            );
        }
    }
    Ok(())
}

fn replay_file(path: &Path, stream: Stream, from: usize, to: usize) -> Result<()> {
    let file = File::open(path)?;
    if from == 1 && to == usize::MAX {
        let mut file = file;
        match stream {
            Stream::Stdout => io::copy(&mut file, &mut io::stdout().lock())?,
            Stream::Stderr => io::copy(&mut file, &mut io::stderr().lock())?,
        };
    } else {
        let mut file = BufReader::new(file);
        let mut line = Vec::new();
        for number in 1..=to {
            if file.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if number >= from {
                stream.print(&line)?;
            }
            line.clear();
        }
    }
    Ok(())
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
