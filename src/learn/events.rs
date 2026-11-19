//! Shell commands an agent ran, read from Claude Code transcripts, Codex rollouts and
//! the run archive.
//!
//! Each event is one command with its directory relative to the workspace root, its
//! exit status and a bounded copy of its output. Commands are normalized without
//! running anything, and their output is scanned, never printed.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, UNIX_EPOCH};

use regex::Regex;
use serde_json::Value;

use crate::capture::source_digest;
use crate::workspace::Workspace;

const HEAD: usize = 16 * 1024;
const TAIL: usize = 4 * 1024;
const TEST_SCAN: u64 = 8 * 1024 * 1024;
const RUN_CAP: u64 = 5_000;
const MAX_WORDS: usize = 32;
const MAX_WORD: usize = 128;
/// `api-request-blob` entries replay earlier request bodies without a cwd or timestamp.
const BLOB: &str = "{\"type\":\"api-request-blob\"";
/// Pipeline stages that only display output, so the exit status is theirs.
const FILTERS: &[&str] = &[
    "head", "tail", "wc", "sort", "uniq", "cat", "less", "more", "cut", "tr", "column", "grep",
    "rg",
];
const REDIRECTS: &[&str] = &[
    "2>&1",
    "2>/dev/null",
    ">/dev/null",
    "1>/dev/null",
    "&>/dev/null",
    ">&2",
];

// A local copy of capture's pattern, which is private there.
static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("ANSI pattern"));
/// Every footer form: `[justless ID] exit N`, `[justless ID · exit N]` and
/// `[justless ID · X of Y shown · exit N]`.
static LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[justless (run-[A-Za-z0-9]+)(?:\]| ·)").expect("run link pattern")
});
static EXIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Exit code (\d+)$").expect("exit code pattern"));
static RUNNING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^running (\d+) tests?$").expect("cargo running pattern"));
static FILTERED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^test result: .*\b(\d+) filtered out").expect("cargo result pattern")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Code(i32),
    /// Claude Code showed a nonzero exit as a non-error, such as a search that found
    /// nothing. Such an event is never a failure, a success or a correction.
    Unknown,
}

/// One command after normalization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    /// The nested checkout that holds the directory, relative to the root; empty for
    /// the main tree.
    pub checkout: String,
    /// The directory relative to that checkout's root; empty at its root.
    pub dir: String,
    pub argv: Vec<String>,
    /// The exit status belongs to a display filter or a `|| true` fallback.
    pub masked: bool,
}

impl Cmd {
    pub fn program(&self) -> &str {
        &self.argv[0]
    }

    pub fn same_place(&self, other: &Self) -> bool {
        self.checkout == other.checkout && self.dir == other.dir
    }
}

pub enum Shape {
    Simple(Cmd),
    Chain(Vec<Cmd>),
    Opaque,
}

/// Output scanned for signatures. Archived stderr and stdout come first, with the run
/// that holds them; the transcript text, when there is one, comes last.
pub struct Text {
    pub run: Option<String>,
    pub stream: &'static str,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pointer {
    Run { id: String, stream: &'static str },
    Transcript { file: String, line: usize },
}

/// Test counts from cargo's `running N tests` and `test result:` lines.
#[derive(Clone, Copy, Default)]
pub struct Tests {
    pub reports: u32,
    pub ran: bool,
    pub filtered: bool,
}

impl Tests {
    fn observe(&mut self, line: &str) {
        if let Some(captures) = RUNNING.captures(line) {
            self.reports += 1;
            self.ran |= captures[1].parse::<u64>().is_ok_and(|n| n > 0);
        } else if let Some(captures) = FILTERED.captures(line) {
            self.filtered |= captures[1].parse::<u64>().is_ok_and(|n| n > 0);
        }
    }

    fn of(text: &str) -> Self {
        let mut tests = Self::default();
        for line in text.lines() {
            tests.observe(line.trim_end());
        }
        tests
    }
}

pub struct Event {
    pub thread: String,
    pub session: String,
    /// Read from an agent transcript or rollout, which holds the text the agent read.
    pub transcript: bool,
    /// Read from a Codex rollout rather than a Claude Code transcript.
    pub codex: bool,
    pub time_ms: u64,
    /// The command as the agent wrote it. Used for privacy and operand checks only.
    pub raw: String,
    /// Leading `NAME=value` words, kept for the privacy check.
    pub env: Vec<String>,
    pub shape: Shape,
    pub status: Status,
    pub texts: Vec<Text>,
    pub tests: Tests,
    pub failure_bytes: u64,
    pub pointer: Pointer,
}

impl Event {
    pub fn simple(&self) -> Option<&Cmd> {
        match &self.shape {
            Shape::Simple(cmd) => Some(cmd),
            _ => None,
        }
    }

    /// The commands of a simple command or a chain.
    pub fn commands(&self) -> &[Cmd] {
        match &self.shape {
            Shape::Simple(cmd) => std::slice::from_ref(cmd),
            Shape::Chain(cmds) => cmds,
            Shape::Opaque => &[],
        }
    }

    /// The text the agent read in the transcript, as opposed to archived streams.
    pub fn transcript_text(&self) -> &str {
        self.texts
            .iter()
            .find(|text| text.run.is_none())
            .map_or("", |text| text.body.as_str())
    }
}

#[derive(Default)]
pub struct TranscriptStats {
    pub available: bool,
    pub directory: Option<PathBuf>,
    pub files: u64,
    pub sessions: BTreeSet<String>,
    pub threads: BTreeSet<String>,
    pub results: u64,
    pub not_run: u64,
    pub status_unknown: u64,
}

#[derive(Default)]
pub struct RunStats {
    pub available: bool,
    pub scanned: u64,
    pub without_command: u64,
    pub unreadable: u64,
    pub linked: u64,
    pub covered_by_transcripts: u64,
    pub capped: bool,
    /// Runs whose command line the archive withheld for a possible credential.
    pub withheld: u64,
}

pub struct Transcripts {
    pub events: Vec<Event>,
    /// Run IDs named by a footer in an accepted result.
    pub linked: BTreeSet<String>,
}

struct Pending {
    command: String,
    cwd: String,
    time_ms: u64,
    session: String,
}

/// The session digest hooks pass to `justless run --session`.
pub fn session_key(id: &str) -> String {
    source_digest(id.as_bytes())[..24].to_owned()
}

pub fn transcripts(
    workspace: &Workspace,
    since_ms: u64,
    stats: &mut TranscriptStats,
) -> Transcripts {
    let mut result = Transcripts {
        events: Vec::new(),
        linked: BTreeSet::new(),
    };
    let Some(dir) = crate::discover::transcript_dir(&workspace.root) else {
        return result;
    };
    stats.directory = Some(dir.clone());
    if !dir.is_dir() {
        return result;
    }
    stats.available = true;
    let mut files = Vec::new();
    // Depth 4 reaches workflow subagents at <session>/subagents/workflows/<id>/.
    crate::discover::transcripts(
        &dir,
        UNIX_EPOCH + Duration::from_millis(since_ms),
        4,
        &mut files,
    );
    files.retain(|file| file.file_name().is_some_and(|name| name != "journal.jsonl"));
    files.sort();
    let root = workspace.root.to_string_lossy().into_owned();
    let mut normalizer = Normalizer::new(&workspace.root);
    let mut seen = HashSet::new();
    for file in files {
        stats.files += 1;
        let thread = file
            .strip_prefix(&dir)
            .unwrap_or(&file)
            .to_string_lossy()
            .into_owned();
        let Ok(handle) = File::open(&file) else {
            continue;
        };
        let mut pending: HashMap<String, Pending> = HashMap::new();
        for (index, line) in BufReader::new(handle).lines().enumerate() {
            let Ok(line) = line else {
                break;
            };
            // Parse only a line that can start a shell call or finish a pending one;
            // other tools' calls and results are most of a transcript.
            let starts = line.contains("\"tool_use\"") && line.contains("\"Bash\"");
            let finishes = !pending.is_empty() && line.contains("\"tool_result\"");
            if line.starts_with(BLOB) || !starts && !finishes {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(parts) = entry["message"]["content"].as_array() else {
                continue;
            };
            match entry["type"].as_str() {
                Some("assistant") => {
                    let (Some(cwd), Some(time_ms)) = (
                        entry["cwd"].as_str(),
                        entry["timestamp"].as_str().and_then(parse_timestamp),
                    ) else {
                        continue;
                    };
                    if time_ms < since_ms || cwd != root && !cwd.starts_with(&format!("{root}/")) {
                        continue;
                    }
                    let session = session_key(entry["sessionId"].as_str().unwrap_or_default());
                    for part in parts {
                        if part["type"] != "tool_use" || part["name"] != "Bash" {
                            continue;
                        }
                        if let (Some(id), Some(command)) =
                            (part["id"].as_str(), part["input"]["command"].as_str())
                        {
                            pending.insert(
                                id.to_owned(),
                                Pending {
                                    command: command.to_owned(),
                                    cwd: cwd[root.len()..].trim_start_matches('/').to_owned(),
                                    time_ms,
                                    session: session.clone(),
                                },
                            );
                        }
                    }
                }
                Some("user") => {
                    for part in parts {
                        if part["type"] != "tool_result" {
                            continue;
                        }
                        let Some(id) = part["tool_use_id"].as_str() else {
                            continue;
                        };
                        let Some(call) = pending.remove(id) else {
                            continue;
                        };
                        // Resumed sessions replay earlier results.
                        if !seen.insert(id.to_owned()) {
                            continue;
                        }
                        let event = accept(
                            workspace,
                            &mut normalizer,
                            &entry["toolUseResult"],
                            part,
                            call,
                            &thread,
                            index + 1,
                            stats,
                            &mut result.linked,
                        );
                        result.events.extend(event);
                    }
                }
                _ => {}
            }
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn accept(
    workspace: &Workspace,
    normalizer: &mut Normalizer,
    outcome: &Value,
    part: &Value,
    call: Pending,
    thread: &str,
    line: usize,
    stats: &mut TranscriptStats,
    linked: &mut BTreeSet<String>,
) -> Option<Event> {
    // Interrupted, backgrounded and timed-out calls did not finish.
    if let Some(fields) = outcome.as_object()
        && (fields.get("interrupted") == Some(&Value::Bool(true))
            || fields.contains_key("backgroundTaskId")
            || fields.contains_key("timedOutAfterMs"))
    {
        stats.not_run += 1;
        return None;
    }
    let content = content_text(&part["content"]);
    let status = if part["is_error"] == true {
        // Permission denials, rejections, isolation notices and tool errors carry no
        // exit code: the command did not run.
        let Some(code) = EXIT
            .captures(content.lines().next().unwrap_or_default())
            .and_then(|captures| captures[1].parse().ok())
        else {
            stats.not_run += 1;
            return None;
        };
        Status::Code(code)
    } else if outcome
        .as_object()
        .is_some_and(|fields| fields.contains_key("returnCodeInterpretation"))
    {
        stats.status_unknown += 1;
        Status::Unknown
    } else {
        Status::Code(0)
    };
    let event = event(
        workspace, normalizer, &content, status, call, thread, stats, linked,
    );
    Some(Event {
        pointer: Pointer::Transcript {
            file: thread.to_owned(),
            line,
        },
        ..event
    })
}

/// Shell commands from Codex rollouts of threads that started in this workspace,
/// read like transcript results. A rule they support goes to AGENTS.md, which
/// Codex reads.
pub fn rollouts(
    workspace: &Workspace,
    since_ms: u64,
    stats: &mut TranscriptStats,
    result: &mut Transcripts,
) {
    let mut normalizer = Normalizer::new(&workspace.root);
    let scan = crate::rollouts::scan(&workspace.root, since_ms, |shell| {
        // A call a hook or the sandbox refused never ran.
        let Some(code) = shell.status else {
            stats.not_run += 1;
            return;
        };
        let cwd = Path::new(&shell.cwd)
            .strip_prefix(&workspace.root)
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file = shell.file.to_string_lossy().into_owned();
        let call = Pending {
            command: shell.command,
            cwd,
            time_ms: shell.time_ms,
            session: session_key(&shell.session),
        };
        let event = event(
            workspace,
            &mut normalizer,
            &shell.output,
            Status::Code(code),
            call,
            &file,
            stats,
            &mut result.linked,
        );
        result.events.push(Event {
            codex: true,
            pointer: Pointer::Transcript {
                file,
                line: shell.line,
            },
            ..event
        });
    });
    stats.directory = scan.home.map(|home| home.join("sessions"));
    stats.available = stats.directory.as_ref().is_some_and(|dir| dir.is_dir());
    stats.files = scan.files;
}

/// An event from the text an agent read and the exit status it was given. The
/// caller sets the pointer.
#[allow(clippy::too_many_arguments)]
fn event(
    workspace: &Workspace,
    normalizer: &mut Normalizer,
    content: &str,
    status: Status,
    call: Pending,
    thread: &str,
    stats: &mut TranscriptStats,
    linked: &mut BTreeSet<String>,
) -> Event {
    stats.results += 1;
    stats.sessions.insert(call.session.clone());
    stats.threads.insert(thread.to_owned());

    let body = bounded(content);
    let mut texts = Vec::new();
    let mut tests = None;
    for captures in LINK.captures_iter(&body) {
        let id = captures[1].to_owned();
        linked.insert(id.clone());
        let Ok((directory, _)) = crate::capture::load(workspace, &id) else {
            continue;
        };
        for stream in ["stderr", "stdout"] {
            if let Some(text) = read_bounded(&directory.join(stream)) {
                texts.push(Text {
                    run: Some(id.clone()),
                    stream,
                    body: text,
                });
            }
        }
        tests.get_or_insert_with(|| count_tests(&directory.join("stdout")));
    }
    let tests = tests.unwrap_or_else(|| Tests::of(&body));
    texts.push(Text {
        run: None,
        stream: "",
        body,
    });
    release(&mut texts, status);
    let (shape, env) = normalizer.shell(&call.cwd, &call.command);
    Event {
        thread: thread.to_owned(),
        session: call.session,
        transcript: true,
        codex: false,
        time_ms: call.time_ms,
        raw: call.command,
        env,
        shape,
        status,
        texts,
        tests,
        failure_bytes: content.len() as u64,
        pointer: Pointer::Transcript {
            file: String::new(),
            line: 0,
        },
    }
}

/// Archived runs from every client whose hooks route commands through `justless run`,
/// and from the terminal. Runs already seen in a transcript are left out.
pub fn runs(
    workspace: &Workspace,
    since_ms: u64,
    transcript_sessions: &BTreeSet<String>,
    linked: &BTreeSet<String>,
    stats: &mut RunStats,
) -> Vec<Event> {
    let mut events = Vec::new();
    stats.available = workspace.state.join("runs").is_dir();
    let records = match crate::history::records(workspace, false) {
        Ok(records) => records,
        Err(_) => {
            stats.unreadable += 1;
            return events;
        }
    };
    let mut normalizer = Normalizer::new(&workspace.root);
    for record in records {
        let Ok(run) = record else {
            stats.unreadable += 1;
            continue;
        };
        if run.completed_ms < since_ms {
            break;
        }
        // A run whose Justless stopped before the command finished has no exit
        // status or final command line to learn from.
        if run.unfinished {
            continue;
        }
        // `justless exec` records a script the agent wrote, not a command line.
        if run.family.starts_with("exec ") {
            continue;
        }
        // Output the Bash post-command hook recorded after the command ran carries
        // the whole command line's status, such as a pipe's, not that of the command
        // its words name.
        if !run.note.is_empty() {
            continue;
        }
        if stats.scanned == RUN_CAP {
            stats.capped = true;
            break;
        }
        stats.scanned += 1;
        if run.argv_withheld {
            stats.withheld += 1;
            continue;
        }
        if run.argv.is_empty() {
            stats.without_command += 1;
            continue;
        }
        if linked.contains(&run.id) {
            stats.linked += 1;
            continue;
        }
        // A hooked success that reached the agent verbatim prints no footer, so its
        // transcript result cannot name it.
        if run.hook && transcript_sessions.contains(&run.session) {
            stats.covered_by_transcripts += 1;
            continue;
        }
        let (shape, env) = normalizer.argv(&run.cwd, &run.argv);
        let runner = match &shape {
            Shape::Simple(cmd) => runner(&cmd.argv).is_some(),
            _ => false,
        };
        let directory = workspace.state.join("runs").join(&run.id);
        let mut texts = Vec::new();
        if run.status != 0 || runner {
            for stream in ["stderr", "stdout"] {
                if let Some(text) = read_bounded(&directory.join(stream)) {
                    texts.push(Text {
                        run: Some(run.id.clone()),
                        stream,
                        body: text,
                    });
                }
            }
        }
        let tests = if runner {
            count_tests(&directory.join("stdout"))
        } else {
            Tests::default()
        };
        release(&mut texts, Status::Code(run.status));
        events.push(Event {
            thread: format!("run:{}", run.session),
            session: run.session.clone(),
            transcript: false,
            codex: false,
            time_ms: run.completed_ms,
            raw: shlex::try_join(run.argv.iter().map(String::as_str))
                .unwrap_or_else(|_| run.argv.join(" ")),
            env,
            shape,
            status: Status::Code(run.status),
            texts,
            tests,
            failure_bytes: (run.preview_bytes + run.footer_bytes) as u64,
            pointer: Pointer::Run {
                id: run.id.clone(),
                stream: "stderr",
            },
        });
    }
    // Oldest first, so a correction follows its failure.
    events.sort_by(|a, b| a.time_ms.cmp(&b.time_ms));
    events
}

/// Free the output a success cannot use, once its test counts are read. A success
/// matters only through a signature or a self-evident error in its text, so a body
/// without any of their words is emptied; the run and stream stay for evidence
/// pointers. A failure keeps everything, and an unknown status is never read.
fn release(texts: &mut [Text], status: Status) {
    if matches!(status, Status::Code(code) if code != 0) {
        return;
    }
    for text in texts {
        if status == Status::Unknown || !super::patterns::may_signal(&text.body) {
            text.body = String::new();
        }
    }
}

/// A test runner whose output or exit status reports how many tests ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runner {
    Cargo,
    Pytest,
    NodeTest,
}

pub fn runner(argv: &[String]) -> Option<Runner> {
    let words = unwrap(argv);
    let program = basename(words.first()?);
    match (program, words.get(1).map(String::as_str)) {
        ("cargo", Some("test")) => Some(Runner::Cargo),
        ("pytest" | "py.test", _) => Some(Runner::Pytest),
        ("node", _) if words.iter().any(|w| w == "--test") => Some(Runner::NodeTest),
        _ => None,
    }
}

/// Skip package runners such as `uv run` and `npx`, and `python -m pytest`.
pub fn unwrap(argv: &[String]) -> &[String] {
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    let rest = crate::filters::unwrap_tool(&words);
    let mut start = argv.len() - rest.len();
    if let [python, flag, module, ..] = &argv[start..]
        && basename(python).starts_with("python")
        && flag == "-m"
        && module == "pytest"
    {
        start += 2;
    }
    &argv[start..]
}

pub fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The first 16 KiB and the last 4 KiB, without terminal escapes.
fn bounded(text: &str) -> String {
    let text = ANSI.replace_all(text, "");
    if text.len() <= HEAD + TAIL {
        return text.into_owned();
    }
    let head = text.floor_char_boundary(HEAD);
    let tail = text.ceil_char_boundary(text.len() - TAIL);
    format!("{}\n{}", &text[..head], &text[tail..])
}

fn read_bounded(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut bytes = Vec::new();
    (&mut file).take(HEAD as u64).read_to_end(&mut bytes).ok()?;
    if length > (HEAD + TAIL) as u64 {
        file.seek(SeekFrom::Start(length - TAIL as u64)).ok()?;
        bytes.push(b'\n');
    }
    file.read_to_end(&mut bytes).ok()?;
    Some(
        ANSI.replace_all(&String::from_utf8_lossy(&bytes), "")
            .into_owned(),
    )
}

fn count_tests(path: &Path) -> Tests {
    let mut tests = Tests::default();
    let Ok(file) = File::open(path) else {
        return tests;
    };
    for line in BufReader::new(file.take(TEST_SCAN)).split(b'\n') {
        let Ok(line) = line else {
            break;
        };
        let text = String::from_utf8_lossy(&line);
        tests.observe(ANSI.replace_all(text.trim_end(), "").as_ref());
    }
    tests
}

/// Milliseconds since the epoch for an RFC 3339 UTC timestamp such as
/// `2026-11-17T09:30:00.123Z`.
pub fn parse_timestamp(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let rest = &text[19..];
    let (fraction, zone) = match rest.strip_prefix('.') {
        Some(rest) => {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            (&rest[..digits], &rest[digits..])
        }
        None => ("", rest),
    };
    if zone != "Z"
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let millis: i64 = format!("{fraction:0<3}").get(..3)?.parse().ok()?;
    let days = days_from_civil(year, month, day);
    let total = days * 86_400_000 + ((hour * 60 + minute) * 60 + second) * 1000 + millis;
    u64::try_from(total).ok()
}

/// Days since 1970-01-01 in the proleptic Gregorian calendar, the inverse of
/// `history::utc_date`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

enum Token {
    Word(String, bool),
    Op(&'static str),
}

struct Piece {
    words: Vec<String>,
    /// The operator that follows this piece.
    op: Option<&'static str>,
}

enum Parsed {
    Simple(Seg),
    Chain(Vec<Seg>),
}

struct Seg {
    dir: String,
    argv: Vec<String>,
    masked: bool,
}

/// Turns command text into one command, a chain of commands, or nothing it can trust.
pub struct Normalizer<'a> {
    root: &'a Path,
    checkouts: HashMap<String, bool>,
}

impl<'a> Normalizer<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            checkouts: HashMap::new(),
        }
    }

    /// Normalize shell text the agent ran from `cwd`, relative to the root. Returns the
    /// shape and the leading environment assignments.
    pub fn shell(&mut self, cwd: &str, text: &str) -> (Shape, Vec<String>) {
        let mut env = Vec::new();
        let parsed = lex(text)
            .and_then(pieces)
            .and_then(|pieces| self.parse(cwd, &pieces, &mut env, 0));
        (self.shape(parsed), env)
    }

    /// Normalize an archived argv. Only `sh -c SCRIPT` is parsed as shell text.
    pub fn argv(&mut self, cwd: &str, argv: &[String]) -> (Shape, Vec<String>) {
        let mut env = Vec::new();
        let parsed = (argv.len() <= MAX_WORDS && argv.iter().all(|w| w.len() <= MAX_WORD))
            .then(|| self.command(cwd, argv, &mut env, 0))
            .flatten();
        (self.shape(parsed), env)
    }

    /// Split a root-relative directory into the nested checkout that holds it and the
    /// directory within that checkout, so `server/` means the same in a worktree.
    pub fn rebase(&mut self, dir: &str) -> (String, String) {
        let parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
        let mut split = 0;
        if parts.len() >= 3 && parts[0] == ".claude" && parts[1] == "worktrees" {
            split = 3;
        }
        for end in (split + 1..=parts.len()).rev() {
            let prefix = parts[..end].join("/");
            let root = self.root;
            let nested = *self
                .checkouts
                .entry(prefix.clone())
                .or_insert_with(|| root.join(&prefix).join(".git").exists());
            if nested {
                split = end;
                break;
            }
        }
        (parts[..split].join("/"), parts[split..].join("/"))
    }

    fn shape(&mut self, parsed: Option<Parsed>) -> Shape {
        let mut cmd = |seg: Seg| {
            let (checkout, dir) = self.rebase(&seg.dir);
            Cmd {
                checkout,
                dir,
                argv: seg.argv,
                masked: seg.masked,
            }
        };
        match parsed {
            None => Shape::Opaque,
            Some(Parsed::Simple(seg)) => Shape::Simple(cmd(seg)),
            Some(Parsed::Chain(segs)) => Shape::Chain(segs.into_iter().map(cmd).collect()),
        }
    }

    fn parse(
        &self,
        cwd: &str,
        pieces: &[Piece],
        env: &mut Vec<String>,
        depth: u8,
    ) -> Option<Parsed> {
        let mut dir = cwd.to_owned();
        let mut start = 0;
        while start < pieces.len() && pieces[start].words[0] == "cd" {
            if !matches!(pieces[start].op, Some("&&" | ";")) {
                return None;
            }
            dir = self.cd(&dir, &pieces[start].words)?;
            start += 1;
        }
        let rest = &pieces[start..];
        if rest.is_empty() {
            return None;
        }
        let mut end = 1;
        let mut masked = false;
        while end < rest.len() && rest[end - 1].op == Some("|") && filter(&rest[end].words) {
            masked = true;
            end += 1;
        }
        if end + 1 == rest.len() && rest[end - 1].op == Some("||") && fallback(&rest[end].words) {
            masked = true;
            end += 1;
        }
        if end == rest.len() {
            return Some(match self.command(&dir, &rest[0].words, env, depth)? {
                Parsed::Simple(mut seg) => {
                    seg.masked |= masked;
                    Parsed::Simple(seg)
                }
                Parsed::Chain(mut segs) => {
                    for seg in &mut segs {
                        seg.masked |= masked;
                    }
                    Parsed::Chain(segs)
                }
            });
        }
        let mut segs = Vec::new();
        for (index, piece) in rest.iter().enumerate() {
            if piece.words[0] == "cd" && matches!(piece.op, Some("&&" | ";")) {
                dir = self.cd(&dir, &piece.words)?;
                continue;
            }
            let masked = piece.op == Some("|")
                || piece.op == Some("||")
                    && rest
                        .get(index + 1)
                        .is_some_and(|next| fallback(&next.words));
            match self.command(&dir, &piece.words, env, depth)? {
                Parsed::Simple(mut seg) => {
                    seg.masked |= masked;
                    segs.push(seg);
                }
                Parsed::Chain(inner) => segs.extend(inner),
            }
        }
        Some(Parsed::Chain(segs))
    }

    fn command(
        &self,
        dir: &str,
        words: &[String],
        env: &mut Vec<String>,
        depth: u8,
    ) -> Option<Parsed> {
        let mut words = words;
        while let Some(word) = words.first()
            && assignment(word)
        {
            env.push(word.clone());
            words = &words[1..];
        }
        loop {
            let first = words.first()?;
            words = match basename(first) {
                "sudo" | "doas" | "su" => return None,
                "time" | "nohup" => &words[1..],
                "command" if words.get(1).is_some_and(|w| !w.starts_with('-')) => &words[1..],
                "env" => {
                    let mut rest = &words[1..];
                    while let Some(word) = rest.first()
                        && assignment(word)
                    {
                        env.push(word.clone());
                        rest = &rest[1..];
                    }
                    if rest.first()?.starts_with('-') {
                        return None;
                    }
                    rest
                }
                "nice" => match &words[1..] {
                    [flag, _, rest @ ..] if flag == "-n" => rest,
                    [next, ..] if next.starts_with('-') => return None,
                    rest => rest,
                },
                "timeout" => {
                    if words.get(1)?.starts_with('-') {
                        return None;
                    }
                    &words[2..]
                }
                "justless" if words.get(1).is_some_and(|w| w == "run") => {
                    match strip_run_flags(&words[2..]) {
                        Some(rest) => rest,
                        None => break,
                    }
                }
                "sh" | "bash" | "zsh" if words.len() == 3 && words[1] == "-c" => {
                    if depth >= 2 {
                        return None;
                    }
                    let pieces = lex(&words[2]).and_then(pieces)?;
                    return self.parse(dir, &pieces, env, depth + 1);
                }
                _ => break,
            };
        }
        let mut argv = words.to_vec();
        if argv.is_empty() {
            return None;
        }
        if Path::new(&argv[0]).is_absolute() {
            argv[0] = basename(&argv[0]).to_owned();
        }
        let mut dir = dir.to_owned();
        self.fold(&mut dir, &mut argv)?;
        Some(Parsed::Simple(Seg {
            dir,
            argv,
            masked: false,
        }))
    }

    /// Move a tool's directory flag into the directory, as in `make -C server test`.
    fn fold(&self, dir: &mut String, argv: &mut Vec<String>) -> Option<()> {
        let program = argv[0].clone();
        let flags: &[&str] = match program.as_str() {
            "make" | "gmake" => &["-C", "--directory"],
            "git" | "go" => &["-C"],
            "pnpm" => &["-C", "--dir"],
            "npm" => &["--prefix"],
            "yarn" => &["--cwd"],
            "cargo" => &["--manifest-path"],
            "just" => &["--working-directory"],
            _ => return Some(()),
        };
        // Git and Go take -C only before the subcommand; `git commit -C` names a commit.
        let leading = matches!(program.as_str(), "git" | "go" | "yarn");
        let mut index = 1;
        while index < argv.len() {
            let word = argv[index].clone();
            if word == "--" || leading && !word.starts_with('-') {
                break;
            }
            let (value, width) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") && flags.contains(&flag) => {
                    (value.to_owned(), 1)
                }
                _ if flags.contains(&word.as_str()) => (argv.get(index + 1)?.clone(), 2),
                _ => {
                    index += 1;
                    continue;
                }
            };
            let target = if program == "cargo" {
                let path = Path::new(&value);
                if path.file_name().is_none_or(|name| name != "Cargo.toml") {
                    index += width;
                    continue;
                }
                path.parent()?.to_string_lossy().into_owned()
            } else {
                value
            };
            *dir = self.join(dir, if target.is_empty() { "." } else { &target })?;
            argv.drain(index..index + width);
        }
        Some(())
    }

    fn cd(&self, dir: &str, words: &[String]) -> Option<String> {
        let [_, target] = words else {
            return None;
        };
        if target == "-" || target.starts_with('~') || target.is_empty() {
            return None;
        }
        self.join(dir, target)
    }

    /// Join lexically and stay inside the root.
    fn join(&self, base: &str, target: &str) -> Option<String> {
        let path = Path::new(target);
        let (mut parts, relative): (Vec<String>, &Path) = if path.is_absolute() {
            (Vec::new(), path.strip_prefix(self.root).ok()?)
        } else {
            (
                base.split('/')
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect(),
                path,
            )
        };
        for component in relative.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
                Component::CurDir => {}
                Component::ParentDir => {
                    parts.pop()?;
                }
                Component::RootDir | Component::Prefix(_) => return None,
            }
        }
        Some(parts.join("/"))
    }
}

/// Skip `justless run` options up to `--`; `None` when the words are not a run.
fn strip_run_flags(mut words: &[String]) -> Option<&[String]> {
    loop {
        let word = words.first()?.as_str();
        words = match word {
            "--" => return Some(&words[1..]),
            "--hook" | "--merge" | "--fresh" | "--raw" => &words[1..],
            "--session" | "--max-bytes" => words.get(2..)?,
            _ if word.starts_with("--session=") || word.starts_with("--max-bytes=") => &words[1..],
            _ => return None,
        };
    }
}

fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn filter(words: &[String]) -> bool {
    FILTERS.contains(&basename(&words[0]))
}

fn fallback(words: &[String]) -> bool {
    matches!(words[0].as_str(), "true" | ":") && words.len() == 1 || words[0] == "echo"
}

/// Split shell text into words and the list and pipe operators, refusing anything
/// whose meaning depends on expansion, grouping or redirection beyond the plain forms.
fn lex(text: &str) -> Option<Vec<Token>> {
    if text.contains(['\n', '\r', '`', '$'])
        || text.contains("<<")
        || text.contains("<(")
        || text.contains(">(")
    {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut redirect = false;
    let mut quote = None;
    let mut index = 0;
    let flush = |word: &mut String, redirect: &mut bool, tokens: &mut Vec<Token>| {
        if !word.is_empty() {
            tokens.push(Token::Word(std::mem::take(word), *redirect));
        }
        *redirect = false;
    };
    while index < chars.len() {
        let c = chars[index];
        let next = chars.get(index + 1).copied();
        if let Some(active) = quote {
            word.push(c);
            if c == active {
                quote = None;
            } else if active == '"' && c == '\\' {
                word.push(next?);
                index += 1;
            }
            index += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                word.push(c);
            }
            '\\' => {
                word.push(c);
                word.push(next?);
                index += 1;
            }
            c if c.is_whitespace() => flush(&mut word, &mut redirect, &mut tokens),
            '&' if next == Some('&') => {
                flush(&mut word, &mut redirect, &mut tokens);
                tokens.push(Token::Op("&&"));
                index += 1;
            }
            '|' if next == Some('|') => {
                flush(&mut word, &mut redirect, &mut tokens);
                tokens.push(Token::Op("||"));
                index += 1;
            }
            '|' | ';' => {
                if matches!(next, Some('&' | ';')) {
                    return None;
                }
                flush(&mut word, &mut redirect, &mut tokens);
                tokens.push(Token::Op(if c == '|' { "|" } else { ";" }));
            }
            '<' | '>' => {
                word.push(c);
                redirect = true;
            }
            '&' if word.ends_with('>') || next == Some('>') => {
                word.push(c);
                redirect = true;
            }
            '&' | '*' | '?' | '[' | ']' | '{' | '}' | '~' | '!' | '#' | '(' | ')' => return None,
            _ => word.push(c),
        }
        index += 1;
    }
    if quote.is_some() {
        return None;
    }
    flush(&mut word, &mut redirect, &mut tokens);
    Some(tokens)
}

fn pieces(tokens: Vec<Token>) -> Option<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut raw: Vec<String> = Vec::new();
    let mut count = 0;
    let mut finish = |raw: &mut Vec<String>, op| -> Option<Piece> {
        let words = shlex::split(&raw.join(" "))?;
        raw.clear();
        count += words.len();
        if words.is_empty() || count > MAX_WORDS || words.iter().any(|w| w.len() > MAX_WORD) {
            return None;
        }
        Some(Piece { words, op })
    };
    for token in tokens {
        match token {
            Token::Word(word, true) if REDIRECTS.contains(&word.as_str()) => {}
            Token::Word(_, true) => return None,
            Token::Word(word, false) => raw.push(word),
            Token::Op(op) => pieces.push(finish(&mut raw, Some(op))?),
        }
    }
    if !raw.is_empty() {
        pieces.push(finish(&mut raw, None)?);
    } else if let Some(last) = pieces.last_mut() {
        // A trailing `;` is valid shell; a trailing `&&`, `||` or `|` is not complete.
        if last.op != Some(";") {
            return None;
        }
        last.op = None;
    }
    (!pieces.is_empty()).then_some(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple(cwd: &str, text: &str) -> Option<Cmd> {
        let root = Path::new("/nonexistent/root");
        match Normalizer::new(root).shell(cwd, text).0 {
            Shape::Simple(cmd) => Some(cmd),
            _ => None,
        }
    }

    /// A simple command as `DIR: ARGV`, with ` (masked)` when filters hid its status.
    fn shown(cwd: &str, text: &str) -> String {
        let cmd = simple(cwd, text).unwrap();
        assert!(cmd.argv.iter().all(|word| !word.contains(' ')), "{text}");
        let masked = if cmd.masked { " (masked)" } else { "" };
        format!("{}: {}{masked}", cmd.dir, cmd.argv.join(" "))
    }

    #[test]
    fn normalizes_directories_wrappers_and_filters() {
        for (cwd, text, expected) in [
            (
                "",
                "cd server && make test 2>&1 | tail -5",
                "server: make test (masked)",
            ),
            ("", "make -C server test", "server: make test"),
            (
                "web",
                "justless run --hook --session abc -- env CI=1 timeout 60 npm --prefix ../api test",
                "api: npm test",
            ),
            // A hook keeps assignments and a final `2>&1` around the words it wraps.
            (
                "",
                "RUST_BACKTRACE=1 justless run --hook --merge --session abc -- cargo test 2>&1",
                ": cargo test",
            ),
            (
                "",
                "cargo test --manifest-path core/Cargo.toml -p x || true",
                "core: cargo test -p x (masked)",
            ),
            (
                "",
                "bash -c 'cd api && /usr/bin/python3 -m pytest x'",
                "api: python3 -m pytest x",
            ),
            ("", "git -C lib commit -C HEAD", "lib: git commit -C HEAD"),
        ] {
            assert_eq!(shown(cwd, text), expected, "{text}");
        }
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        for text in [
            "echo $HOME",
            "ls *.rs",
            "make test &",
            "cd ~ && make",
            "cd .. && make",
            "sudo make install",
            "make > out.txt",
            "cat <<EOF",
            "echo 'open",
            "(make)",
            "make test &&",
            "a\nb",
        ] {
            assert!(simple("", text).is_none(), "{text}");
        }
        let root = Path::new("/r");
        let (shape, env) = Normalizer::new(root).shell("", "TOKEN=x make test && echo done");
        assert!(matches!(shape, Shape::Chain(ref cmds) if cmds.len() == 2));
        assert_eq!(env, ["TOKEN=x"]);
    }

    #[test]
    fn parses_utc_timestamps() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp("2026-11-18T01:02:03.456Z"),
            Some(1_794_963_723_456)
        );
        assert_eq!(
            crate::history::utc_date(parse_timestamp("2024-02-29T23:59:59.9Z").unwrap()),
            "2024-02-29"
        );
        assert_eq!(parse_timestamp("2026-11-18 01:02:03Z"), None);
        assert_eq!(parse_timestamp("2026-11-18T01:02:03+01:00"), None);
    }
}
