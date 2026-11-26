//! Single-file shell commands that read an inclusive source range, `cat`, and
//! `git show REV:path`, which prints a file as a revision or the index holds it.
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::capture::{self, Origin, SourceOutline};
use crate::history;
use crate::source::Snapshot;
use crate::workspace::{Workspace, shell_quote};

/// One window a read prints: lines `first..=last`, or `first` to the end of
/// the file. With `from_end`, `first` counts lines back from the end, as
/// `tail -n N` does.
#[derive(Clone, Copy)]
struct Span {
    first: usize,
    last: Option<usize>,
    from_end: bool,
}

/// The windows of a recognized read, in the order the command prints them.
#[derive(Clone)]
pub(crate) struct LineRange {
    spans: Vec<Span>,
    /// The operand names a Git object, such as `HEAD~1:src/a.rs`, rather than
    /// a file in the working tree.
    object: bool,
    /// A `cat` of the whole file.
    whole: bool,
    /// `cat -n FILE | …` or `nl -ba FILE | …`: each line after its number.
    numbered: Option<Numbering>,
}

/// How a read numbers its lines: as `cat -n` and `nl -ba` both print them, the
/// number right-aligned in six columns and a tab before the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Numbering {
    Cat,
    /// `nl` also starts a new section, and its numbers again, at a line that is
    /// only `\:`, `\:\:` or `\:\:\:`.
    Nl,
}

impl Numbering {
    fn command(self) -> [&'static str; 2] {
        match self {
            Self::Cat => ["cat", "-n"],
            Self::Nl => ["nl", "-ba"],
        }
    }
}

fn decimal(text: &str) -> Option<usize> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl Span {
    fn closed(text: &str, separator: char) -> Option<Self> {
        let (first, last) = text.split_once(separator)?;
        let (first, last) = (decimal(first)?, decimal(last)?);
        (first <= last).then_some(Self {
            first,
            last: Some(last),
            from_end: false,
        })
    }

    fn tail(text: &str) -> Option<Self> {
        Some(Self {
            first: decimal(text.strip_prefix('+')?)?,
            last: None,
            from_end: false,
        })
    }
}

impl LineRange {
    fn new(spans: Vec<Span>) -> Self {
        Self {
            spans,
            object: false,
            whole: false,
            numbered: None,
        }
    }

    /// A `sed -n` script of `A,Bp`, `A,$p`, or single-line `Ap` addresses, one
    /// or several joined by `;`, such as `12,107p;209,260p`, `10,$p`, or `42p`.
    fn sed(script: &str) -> Option<Self> {
        let mut iter = script.split(';').peekable();
        let mut spans = Vec::new();
        while let Some(part) = iter.next() {
            let is_last = iter.peek().is_none();
            let body = part.strip_suffix('p')?;
            let span = if let Some(first) = body.strip_suffix(",$") {
                if !is_last {
                    return None;
                }
                Span {
                    first: decimal(first)?,
                    last: None,
                    from_end: false,
                }
            } else if body.contains(',') {
                Span::closed(body, ',')?
            } else if spans.is_empty() && is_last {
                let line = decimal(body)?;
                Span {
                    first: line,
                    last: Some(line),
                    from_end: false,
                }
            } else {
                return None;
            };
            spans.push(span);
        }
        (!spans.is_empty()).then(|| Self::new(spans))
    }

    /// The `read --shell-range` argument: `A-B` and `+N` windows joined by commas.
    fn parse(text: &str) -> Option<Self> {
        let spans = text
            .split(',')
            .map(|part| Span::tail(part).or_else(|| Span::closed(part, '-')))
            .collect::<Option<Vec<_>>>()?;
        Some(Self::new(spans))
    }

    /// A `bat`/`batcat` `-r`/`--line-range` argument: `A:B`, `A:`, or `:B`.
    fn bat_range(text: &str) -> Option<Self> {
        let (start, end) = text.split_once(':')?;
        let span = match (start.is_empty(), end.is_empty()) {
            (true, true) => return None,
            (true, false) => Span {
                first: 1,
                last: Some(decimal(end)?),
                from_end: false,
            },
            (false, true) => Span {
                first: decimal(start)?,
                last: None,
                from_end: false,
            },
            (false, false) => {
                let first = decimal(start)?;
                let last = decimal(end)?;
                (first <= last).then_some(Span {
                    first,
                    last: Some(last),
                    from_end: false,
                })?
            }
        };
        Some(Self::new(vec![span]))
    }
}

/// The object name in `git show REV:path`, `git show REV:./path` or
/// `git show :path` (the index), also after safe Git display global flags such
/// as `--no-pager`, `-P` or `--no-optional-locks`.
pub(crate) fn show(words: &[String]) -> Option<&str> {
    if words.first().map(String::as_str) != Some("git") {
        return None;
    }
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    show_refs(&refs)
}

fn show_refs<'a>(words: &[&'a str]) -> Option<&'a str> {
    let ["git", rest @ ..] = words else {
        return None;
    };
    let ["show", spec] = crate::filters::git_display_subcommand(rest) else {
        return None;
    };
    blob_path(spec).map(|_| *spec)
}

/// The path part of an object name that names a file, such as `src/a.rs` in
/// `HEAD~1:./src/a.rs`. The revision is a plain name with `~`, `^` and `@`, so
/// a `:/text` message search, a `:N:path` stage or a `^{type}` peel is not one.
pub(crate) fn blob_path(spec: &str) -> Option<&str> {
    let (rev, path) = spec.split_once(':')?;
    if rev.starts_with('-')
        || !rev
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/~^@".contains(&b))
    {
        return None;
    }
    let path = path.strip_prefix("./").unwrap_or(path);
    let normal = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    (normal
        && !path.is_empty()
        && !path.starts_with('-')
        && !path.ends_with('/')
        && !path.contains(':')
        && !path.chars().any(char::is_control))
    .then_some(path)
}

/// A Git invocation that only reads: no optional locks, no fsmonitor hook, and
/// no fetch of an object a partial clone left out.
fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["--no-pager", "-c", "core.fsmonitor=false"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// The text of blob `id`, as `git show` prints it: no text conversion, at most
/// `cap` bytes, UTF-8 without NUL bytes. A tree, a submodule commit or any
/// other object type fails `cat-file blob`.
fn blob(dir: &Path, id: &str, cap: u64) -> Option<String> {
    if ![40, 64].contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut child = git(dir)
        .args(["cat-file", "blob", id])
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()?
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() as u64 > cap {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    if !child.wait().ok()?.success() || bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// The blob that `git show SPEC` prints in `dir`, and its id.
pub(crate) fn object(dir: &Path, spec: &str, cap: u64) -> Option<(String, String)> {
    blob_path(spec)?;
    let output = git(dir)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options", spec])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8(output.stdout).ok()?;
    let id = id.strip_suffix('\n')?.to_owned();
    let text = blob(dir, &id, cap)?;
    Some((id, text))
}

/// A plain, undecorated `bat`/`batcat` single-file read (`--plain`/`-p`/`-pp`/
/// `--style=plain`, optional `--paging=never`/`--color=never`/`--decorations=never`
/// and optional single `-r`/`--line-range`). Any other flag, bare `bat`, or
/// multiple operands return `None`.
fn bat_plain_command<'a>(words: &[&'a str]) -> Option<(&'a str, LineRange)> {
    let [program, rest @ ..] = words else {
        return None;
    };
    if !matches!(*program, "bat" | "batcat") {
        return None;
    }
    let mut plain = false;
    let mut range: Option<LineRange> = None;
    let mut file: Option<&'a str> = None;
    let mut iter = rest.iter().copied();
    while let Some(arg) = iter.next() {
        match arg {
            "--plain" | "-p" | "-pp" | "--style=plain" => plain = true,
            "--style" => {
                if iter.next() != Some("plain") {
                    return None;
                }
                plain = true;
            }
            "--paging=never" | "--color=never" | "--decorations=never" => {}
            "--paging" | "--color" | "--decorations" => {
                if iter.next() != Some("never") {
                    return None;
                }
            }
            "-r" | "--line-range" => {
                if range.is_some() {
                    return None;
                }
                range = Some(LineRange::bat_range(iter.next()?)?);
            }
            _ if arg.starts_with("--line-range=") => {
                if range.is_some() {
                    return None;
                }
                range = Some(LineRange::bat_range(&arg["--line-range=".len()..])?);
            }
            _ if arg.starts_with("-r") && arg.len() > 2 => {
                if range.is_some() {
                    return None;
                }
                range = Some(LineRange::bat_range(&arg[2..])?);
            }
            _ if arg.starts_with('-') => return None,
            _ => {
                if file.is_some() {
                    return None;
                }
                file = Some(arg);
            }
        }
    }
    if !plain {
        return None;
    }
    let file = file?;
    let range = range.unwrap_or_else(|| LineRange {
        whole: true,
        ..LineRange::new(vec![Span::tail("+1").expect("line 1")])
    });
    Some((file, range))
}

/// A standalone single-file read: `sed -n 'A,Bp[;C,Dp…]' FILE` (or `sed -ne` /
/// `sed -n -e`), `head -n N FILE`, `tail -n N FILE`, `tail -n +N FILE`,
/// `cat FILE`, plain `bat`/`batcat`, or `git show REV:path`, whose operand is
/// then the object name.
#[cfg(test)]
fn command(words: &[String]) -> Option<(&str, LineRange)> {
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    command_words(&refs)
}

fn command_words<'a>(words: &[&'a str]) -> Option<(&'a str, LineRange)> {
    let whole = || LineRange::new(vec![Span::tail("+1").expect("line 1")]);
    if let Some(spec) = show_refs(words) {
        let mut range = whole();
        range.object = true;
        return Some((spec, range));
    }
    let (file, range) = match words {
        ["cat", file] => (
            *file,
            LineRange {
                whole: true,
                ..whole()
            },
        ),
        ["sed", "-n" | "-ne", address, file] | ["sed", "-n", "-e", address, file] => {
            (*file, LineRange::sed(address)?)
        }
        ["head", "-n", address, file] => (
            *file,
            LineRange::new(vec![Span {
                first: 1,
                last: Some(decimal(address)?),
                from_end: false,
            }]),
        ),
        ["tail", "-n", address, file] => (
            *file,
            LineRange::new(vec![Span::tail(address).or_else(|| {
                Some(Span {
                    first: decimal(address)?,
                    last: None,
                    from_end: true,
                })
            })?]),
        ),
        ["bat" | "batcat", ..] => bat_plain_command(words)?,
        _ => return None,
    };
    if file.is_empty() || file.starts_with(['-', '=']) || file.chars().any(char::is_control) {
        return None;
    }
    Some((file, range))
}

/// Split a command line at unquoted `&&` and `|`, each part with the operator
/// after it. Any other operator, expansion or escape fails `literal_command`.
fn split(command: &str) -> Option<Vec<(Range<usize>, &'static str)>> {
    let mut parts = Vec::new();
    let (mut quote, mut start, mut at) = (None, 0, 0);
    let bytes = command.as_bytes();
    while let Some(&byte) = bytes.get(at) {
        let operator = match quote {
            Some(open) if byte == open => {
                quote = None;
                None
            }
            None if matches!(byte, b'\'' | b'"') => {
                quote = Some(byte);
                None
            }
            None if bytes[at..].starts_with(b"&&") => Some("&&"),
            None if bytes[at..].starts_with(b"||") || byte == b'&' => return None,
            None if byte == b'|' => Some("|"),
            _ => None,
        };
        match operator {
            Some(operator) => {
                parts.push((start..at, operator));
                at += operator.len();
                start = at;
            }
            None => at += 1,
        }
    }
    if quote.is_some() {
        return None;
    }
    parts.push((start..command.len(), ""));
    Some(parts)
}

/// Zero-allocation pre-check for [`parse`]: any recognized read must begin with
/// one of its known command words (or a quote that may wrap one), and a `git`
/// read must contain `:` for `REV:path`.
fn may_start_read(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let bytes = trimmed.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    if matches!(first, b'\'' | b'"') {
        return true;
    }
    let end = bytes
        .iter()
        .position(|&b| matches!(b, b' ' | b'\t' | b'|' | b'&' | b'\'' | b'"'))
        .unwrap_or(bytes.len());
    if end < bytes.len() && matches!(bytes[end], b'\'' | b'"') {
        return true;
    }
    match &trimmed[..end] {
        "cd" | "cat" | "head" | "tail" | "sed" | "nl" | "bat" | "batcat" => true,
        "git" => bytes.contains(&b':'),
        _ => false,
    }
}

/// Normalize `head -N`, `head`/`tail` attached `-nN`, `-n+N`, `--lines=N`,
/// `--lines=+N` and `--lines N`/`+N` into `head|tail -n ARG …`.
fn head_count(mut words: Vec<String>) -> Vec<String> {
    let valid_tail = |arg: &str| {
        arg.strip_prefix('+')
            .map_or_else(|| decimal(arg).is_some(), |n| decimal(n).is_some())
    };
    match words.as_slice() {
        [head, opt, ..] if head == "head" => {
            if let Some(arg) = opt
                .strip_prefix("-n")
                .or_else(|| opt.strip_prefix("--lines="))
                .or_else(|| opt.strip_prefix('-'))
                && decimal(arg).is_some()
            {
                let arg = arg.to_owned();
                words.splice(1..2, ["-n".to_owned(), arg]);
            } else if opt == "--lines" && words.get(2).is_some_and(|arg| decimal(arg).is_some()) {
                words[1] = "-n".to_owned();
            }
        }
        [tail, opt, ..] if tail == "tail" => {
            if let Some(arg) = opt
                .strip_prefix("-n")
                .or_else(|| opt.strip_prefix("--lines="))
                && valid_tail(arg)
            {
                let arg = arg.to_owned();
                words.splice(1..2, ["-n".to_owned(), arg]);
            } else if opt == "--lines" && words.get(2).is_some_and(|arg| valid_tail(arg)) {
                words[1] = "-n".to_owned();
            }
        }
        _ => {}
    }
    words
}

/// A recognized read: one of the forms `command` takes, or `cat FILE` piped
/// into one `head -n N`, `tail -n N`, `tail -n +N` or `sed -n` script, each
/// after an optional `cd DIR &&` and before an optional `| cat`.
pub(crate) struct ShellRead {
    /// The `cd DIR && ` that starts the command line, as written.
    prefix: String,
    /// The directory that `cd` enters.
    dir: Option<String>,
    file: String,
    range: LineRange,
    /// Where the read ends when a `| cat` follows it.
    unpiped: Option<usize>,
}

/// Whether a read of `lines` lines and `bytes` bytes keeps its text instead of an
/// outline: at most 150 lines and 6 KiB, or, when the client shows it whole, at
/// most `whole` lines, the `read_outline_lines` setting. In 30 days of this
/// project's Claude Code transcripts, a compacted shell read of at most 600
/// lines was read again 67% to 83% of the time and left the agent with 95% to
/// 127% of the bytes of the raw read (`scripts/read_followups.py`, 2026-11-20).
pub(crate) fn small(lines: usize, bytes: usize, whole: Option<u64>) -> bool {
    lines <= 150 && bytes <= 6144 || whole.is_some_and(|whole| lines as u64 <= whole)
}

/// The read a command line runs, if it is one of the forms `ShellRead` describes.
pub(crate) fn parse(line: &str) -> Option<ShellRead> {
    if !may_start_read(line) {
        return None;
    }
    let mut parts = split(line)?;
    // A last `| cat` or `| cat -` prints the read's bytes unchanged.
    let mut unpiped = None;
    if let [.., (_, "|"), (last, "")] = parts.as_slice()
        && crate::agents::literal_command(&line[last.clone()])
            .is_some_and(|words| words == ["cat"] || words == ["cat", "-"])
    {
        parts.pop();
        let read = parts.last_mut()?;
        read.1 = "";
        unpiped = Some(read.0.end);
    }
    let words = |at: usize| crate::agents::literal_command(&line[parts.get(at)?.0.clone()]);
    let (prefix, dir, first) = match parts.first()?.1 {
        "&&" => match words(0)?.as_slice() {
            [cd, dir] if cd == "cd" && !dir.starts_with('-') && !dir.contains(char::is_control) => {
                (
                    format!("{} ", &line[..parts[1].0.start]),
                    Some(dir.clone()),
                    1,
                )
            }
            _ => return None,
        },
        _ => (String::new(), None, 0),
    };
    let read = head_count(words(first)?);
    let read_refs: Vec<&str> = read.iter().map(String::as_str).collect();
    let (file, range) = match &parts[first..] {
        [(_, "")] => command_words(&read_refs)?,
        [(_, "|"), (_, "")] => {
            let filter = head_count(words(first + 1)?);
            let (program, flag, address) = match filter.as_slice() {
                [program, flag, address] => (program.as_str(), flag.as_str(), address.as_str()),
                [program, flag, e_flag, address]
                    if program == "sed" && flag == "-n" && e_flag == "-e" =>
                {
                    ("sed", "-n", address.as_str())
                }
                _ => return None,
            };
            // `cat -n` and `nl -ba` number every line, `nl` with sections.
            let (file, numbered) = match read_refs.as_slice() {
                ["cat", file] => (*file, None),
                ["cat", "-n", file] => (*file, Some(Numbering::Cat)),
                ["nl", "-ba", file] => (*file, Some(Numbering::Nl)),
                ["bat" | "batcat", ..] => {
                    let (file, range) = bat_plain_command(&read_refs)?;
                    if !range.whole {
                        return None;
                    }
                    (file, None)
                }
                _ => return None,
            };
            let filters: &[&str] = if numbered.is_some() {
                &["head", "sed"]
            } else {
                &["head", "tail", "sed"]
            };
            if !filters.contains(&program) {
                return None;
            }
            let (_, mut range) = command_words(&[program, flag, address, file])?;
            range.numbered = numbered;
            (file, range)
        }
        _ => return None,
    };
    Some(ShellRead {
        prefix,
        dir,
        file: file.to_owned(),
        range,
        unpiped,
    })
}

impl ShellRead {
    /// `line`, the command this read was parsed from, without its last `| cat`,
    /// so every hook judges the read as it would the read alone.
    pub(crate) fn unpiped<'l>(&self, line: &'l str) -> &'l str {
        self.unpiped
            .map_or(line, |end| line[..end].trim_end_matches([' ', '\t']))
    }

    /// Whether the read is a `git show` of an object, which falls back to the
    /// ordinary run report when it cannot be outlined.
    pub(crate) fn object(&self) -> bool {
        self.range.object
    }

    /// Whether the read is a `cat` of the whole file.
    pub(crate) fn whole(&self) -> bool {
        self.range.whole
    }

    /// Whether the read stays in `root` when it runs from `cwd`, judged from the
    /// command alone as `directory` and `Selection::open` judge it, for a read
    /// recorded in a transcript whose files may have changed since.
    pub(crate) fn inside(&self, root: &Path, cwd: &Path) -> bool {
        let dir = crate::find::lexical(&cwd.join(self.dir.as_deref().unwrap_or("")));
        dir.starts_with(root)
            && (self.range.object
                || dir
                    .join(&self.file)
                    .strip_prefix(root)
                    .is_ok_and(|relative| {
                        !relative.components().any(|c| c == Component::ParentDir)
                    }))
    }

    /// The workspace directory the read runs in. Claude Code's main session keeps
    /// a `cd`, so once the command ran its `cwd` can already be the directory a
    /// relative `cd` entered, while a subagent's is where the command started.
    fn directory(&self, ws: &Workspace, cwd: &Path, ran: bool) -> Option<PathBuf> {
        let Some(dir) = &self.dir else {
            return Some(cwd.to_path_buf());
        };
        let entered: PathBuf = Path::new(dir)
            .components()
            .filter(|c| *c != Component::CurDir)
            .collect();
        let directory = match cwd.join(dir).canonicalize() {
            Ok(directory) if directory.is_dir() => directory,
            _ if ran
                && entered
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)))
                && cwd.ends_with(&entered) =>
            {
                cwd.to_path_buf()
            }
            _ => return None,
        };
        directory.starts_with(&ws.root).then_some(directory)
    }
}

/// One printed window, resolved against the file.
struct Window {
    first: usize,
    last: usize,
    bytes: Range<usize>,
}

struct Selection {
    source: Snapshot,
    /// The workspace file, or for a Git object its path, which picks the parser.
    file: String,
    /// For `git show`: the object name as the command gave it, and the blob id.
    object: Option<(String, String)>,
    windows: Vec<Window>,
    text: String,
    /// The last window stops at a requested line rather than at the end of
    /// the file, where sed and head implementations differ on a missing
    /// final newline.
    closed: bool,
    /// `text` numbers each line as `cat -n` or `nl -ba` does.
    numbered: Option<Numbering>,
}

impl Selection {
    fn new(source: Snapshot, file: &str, range: &LineRange) -> Option<Self> {
        let total = source.line_count();
        let mut windows: Vec<Window> = Vec::new();
        for span in &range.spans {
            let first = if span.from_end {
                total.checked_sub(span.first).map_or(1, |rest| rest + 1)
            } else {
                span.first
            };
            // sed prints overlapping or out-of-order addresses line by line,
            // repeating lines, so only ascending, disjoint windows are exact.
            if windows.last().is_some_and(|window| first <= window.last) {
                return None;
            }
            let (last, bytes) = source.line_window(first, span.last.unwrap_or(usize::MAX))?;
            windows.push(Window { first, last, bytes });
        }
        let mut text: String = windows
            .iter()
            .map(|window| &source.text[window.bytes.clone()])
            .collect();
        if let Some(numbering) = range.numbered {
            // Past six digits a number widens its column.
            let last = windows.last()?.last;
            if last > 999_999
                || numbering == Numbering::Nl
                    && source
                        .text
                        .lines()
                        .take(last)
                        .any(|line| matches!(line, "\\:" | "\\:\\:" | "\\:\\:\\:"))
            {
                return None;
            }
            text = windows
                .iter()
                .flat_map(|window| {
                    source.text[window.bytes.clone()]
                        .split_inclusive('\n')
                        .zip(window.first..)
                })
                .map(|(line, number)| format!("{number:>6}\t{line}"))
                .collect();
        }
        let closed = range.spans.last()?.last.is_some();
        Some(Self {
            source,
            file: file.to_owned(),
            object: None,
            windows,
            text,
            closed,
            numbered: range.numbered,
        })
    }

    /// The blob `git show SPEC` prints in `dir`, as `id` when the hook already
    /// resolved the name.
    fn object(
        ws: &Workspace,
        dir: &Path,
        spec: &str,
        id: Option<&str>,
        range: &LineRange,
    ) -> Option<Self> {
        let path = blob_path(spec)?;
        let cap = ws.settings().ok()?.max_file_bytes;
        let (id, text) = match id {
            Some(id) => (id.to_owned(), blob(dir, id, cap)?),
            None => object(dir, spec, cap)?,
        };
        let mut read = Self::new(Snapshot::from_text(path, text), path, range)?;
        read.object = Some((spec.to_owned(), id));
        Some(read)
    }

    /// How the outline header names the read.
    fn name(&self) -> &str {
        self.object.as_ref().map_or(&self.file, |(spec, _)| spec)
    }

    /// The windows as `read --shell-range` and the outline header name them.
    fn ranges(&self, separator: &str) -> String {
        let last = self.windows.len() - 1;
        self.windows
            .iter()
            .enumerate()
            .map(|(i, window)| {
                if i == last && !self.closed {
                    format!("+{}", window.first)
                } else {
                    format!("{}-{}", window.first, window.last)
                }
            })
            .collect::<Vec<_>>()
            .join(separator)
    }

    /// The `justless read --shell-range` command that prints this selection.
    fn shell_command(&self, ws: &Workspace, session: Option<&str>) -> String {
        let object = match &self.object {
            Some((_, id)) => format!(" --shell-object {id}"),
            None => String::new(),
        };
        let session = session
            .filter(|id| !id.is_empty())
            .map(|id| {
                format!(
                    " --shell-session {}",
                    &capture::source_digest(id.as_bytes())[..24]
                )
            })
            .unwrap_or_default();
        let numbered = match self.numbered {
            Some(Numbering::Cat) => " --shell-numbered cat",
            Some(Numbering::Nl) => " --shell-numbered nl",
            None => "",
        };
        format!(
            "justless --root {} read --shell-range {}{numbered}{object}{session} -- {}",
            shell_quote(&ws.root),
            self.ranges(","),
            shell_quote(Path::new(self.name()))
        )
    }

    fn open(ws: &Workspace, cwd: &Path, file: &str, range: &LineRange) -> Option<Self> {
        if range.object {
            return cwd
                .starts_with(&ws.root)
                .then(|| Self::object(ws, cwd, file, None, range))?;
        }
        let path = cwd.join(file);
        let relative = path.strip_prefix(&ws.root).ok()?;
        if relative.components().any(|c| c == Component::ParentDir) {
            return None;
        }
        let relative: std::path::PathBuf = relative.components().collect();
        let file = relative.to_str()?;
        Self::new(Snapshot::open(ws, file).ok()?, file, range)
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn observe(&self, ws: &Workspace, outline: bool) {
        // A blob from a revision or the index is not the working file.
        if self.object.is_some() {
            return;
        }
        let digest = capture::source_digest(self.source.text.as_bytes());
        for window in &self.windows {
            history::saw_range(
                ws,
                &self.file,
                window.first,
                window.last,
                digest.clone(),
                outline,
            );
        }
    }

    /// A read that keeps its text (see [`small`]). `shown` is whether Claude Code
    /// shows the result whole, not as a preview of a file it saved. The line
    /// threshold was measured on Claude Code, so other clients keep the earlier
    /// rule and pass false.
    fn small(&self, ws: &Workspace, shown: bool) -> bool {
        let whole = ws
            .settings()
            .ok()
            .filter(|_| shown)
            .map(|settings| settings.read_outline_lines);
        let lines: usize = self
            .windows
            .iter()
            .map(|window| window.last + 1 - window.first)
            .sum();
        small(lines, self.text.len(), whole)
    }

    fn outline(&mut self, ws: &Workspace, baseline: usize, claude: bool) -> Option<String> {
        let settings = ws.settings().ok()?;
        let bytes = self.text.len() as u64;
        // Claude Code drops the final newlines of a result it shows whole. A
        // baseline smaller than that is the preview of a result it saved to a file.
        let shown = claude && baseline >= self.text.trim_end_matches('\n').len();
        if self.small(ws, shown)
            || bytes > settings.max_run_bytes
            || settings.max_runs == 0
            || bytes > settings.max_archive_bytes
        {
            return None;
        }
        // Leave room for the exact command and the capture footer, including
        // the root when a nested project needs it.
        let budget = settings
            .context_budget
            .min(baseline / 2)
            .checked_sub(160 + 2 * ws.justless().len())?;
        self.source.index(&self.file).ok()?;
        if crate::agents::uncovered_code(&self.file, &self.source)?
            .iter()
            .any(|&i| {
                self.windows
                    .iter()
                    .any(|window| (window.first - 1..window.last).contains(&i))
            })
        {
            return None;
        }
        let shown = self
            .windows
            .iter()
            .map(|window| format!("{}-{}", window.first, window.last))
            .collect::<Vec<_>>()
            .join(", ");
        let mut outline = format!("{} lines {shown}\n", self.name());
        let mut count = 0;
        let mut shown = 0;
        for (name, kind, a, b) in self.source.symbols() {
            if kind == "text" {
                return None;
            }
            // A row for each symbol a window shows part of, not one that only
            // encloses a whole window, such as the impl around a method.
            let listed = self.windows.iter().any(|window| {
                a <= window.last && b >= window.first && !(a <= window.first && b >= window.last)
            });
            if kind == "import" || !listed {
                continue;
            }
            let row = format!("  L{a}-{b} {kind} {name}\n");
            count += 1;
            if shown + 1 == count && outline.len() + row.len() + 32 <= budget {
                outline.push_str(&row);
                shown += 1;
            }
        }
        if shown < 2 {
            return None;
        }
        if count > shown {
            outline.push_str(&format!("  … {} more symbols\n", count - shown));
        }
        // A digest or name cut out of a sensitive line must not escape redaction.
        if let Some(redactor) = crate::redact::for_run(ws).ok()?
            && matches!(redactor.text(self.text()), std::borrow::Cow::Owned(_))
        {
            return None;
        }
        Some(outline)
    }

    fn compact(
        &mut self,
        ws: &Workspace,
        baseline: usize,
        session: &str,
        claude: bool,
    ) -> Option<String> {
        let outline = self.outline(ws, baseline, claude)?;
        let words = self.command();
        let args: Vec<_> = words.iter().map(OsString::from).collect();
        let mut command = shlex::try_join(words.iter().map(String::as_str)).ok()?;
        if self.numbered.is_some() {
            let script = shlex::try_quote(&self.sed_script()).ok()?.into_owned();
            command = format!("{command} | sed -n {script}");
        }
        let origin = Origin {
            command,
            // Kept apart from `git show` patches, whose preview budget adapts
            // to how often their clipped runs are replayed.
            label: if self.object.is_some() {
                "git show REV:path".into()
            } else {
                words[0].clone()
            },
            note: String::new(),
            outline: Some(SourceOutline {
                text: outline,
                baseline,
                trim_footer: claude,
            }),
        };
        let result = capture::record(
            ws,
            &args,
            self.text().as_bytes(),
            0,
            ws.settings().ok()?.context_budget,
            session,
            &origin,
        )
        .ok()?;
        let mut text = String::new();
        for (_, bytes) in result.previews {
            text.push_str(std::str::from_utf8(&bytes).ok()?);
        }
        text.push_str(&result.footer);
        self.observe(ws, true);
        Some(text)
    }

    /// The `sed -n` script that prints these windows.
    fn sed_script(&self) -> String {
        let last = self.windows.len() - 1;
        self.windows
            .iter()
            .enumerate()
            .map(|(i, window)| {
                if i == last && !self.closed {
                    format!("{},$p", window.first)
                } else {
                    format!("{},{}p", window.first, window.last)
                }
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    /// The command that prints these bytes, which keys the archived run.
    fn command(&self) -> Vec<String> {
        if let Some((spec, _)) = &self.object {
            return vec!["git".into(), "show".into(), spec.clone()];
        }
        if let Some(numbering) = self.numbered {
            let [program, flag] = numbering.command();
            return [program, flag, &self.file].map(str::to_owned).to_vec();
        }
        let (program, address) = match (&self.windows[..], self.closed) {
            ([window], false) if window.first == 1 => return vec!["cat".into(), self.file.clone()],
            ([window], false) => ("tail", format!("+{}", window.first)),
            _ => ("sed", self.sed_script()),
        };
        [program, "-n", &address, &self.file]
            .map(str::to_owned)
            .to_vec()
    }
}

/// Other shell clients run a pure workspace read after the same `cd`. Leave the
/// native sed in charge where implementations disagree on an unterminated last
/// line. A `git show` is pinned to the blob its name resolves to now, and like a
/// `cat` rerouted only when an outline will shorten it. Claude Code runs the
/// command itself and `claude` checks it after, except a wide `cat` that the
/// range outline cannot show, which every client reroutes to the outline
/// `justless read` gives the whole file when there is one.
pub(crate) fn rewrite(
    ws: &Workspace,
    cwd: &Path,
    shell: &ShellRead,
    session: Option<&str>,
    claude: bool,
) -> Option<String> {
    // A count from the end is resolved here, before the command runs, so a file
    // that grows in between would print other lines. Only the post-run hook,
    // which checks the bytes the command printed, takes `tail -n N`.
    if (claude && !shell.whole()) || shell.range.spans.iter().any(|span| span.from_end) {
        return None;
    }
    let mut read = Selection::open(
        ws,
        &shell.directory(ws, cwd, false)?,
        &shell.file,
        &shell.range,
    )?;
    if read.closed && !read.text().ends_with('\n') {
        return None;
    }
    let small = read.small(ws, claude);
    if shell.whole() && (small || read.outline(ws, read.text.len(), claude).is_none()) {
        return (!small)
            .then(|| crate::agents::read_instead(ws, &ws.root, &read.file))?
            .map(|instead| format!("{}{instead}", shell.prefix));
    }
    if claude {
        return None;
    }
    if read.object.is_some() {
        read.outline(ws, read.text.len(), false)?;
    }
    Some(format!(
        "{}{}",
        shell.prefix,
        read.shell_command(ws, session)
    ))
}

/// A read a Codex command list runs as one of its commands, as `parse` takes it,
/// that `piece` can print before it runs: not a `git show` blob, which takes the
/// run route there, and not counted from the end of the file.
pub(crate) fn parse_script(line: &str) -> Option<ShellRead> {
    parse(line).filter(|read| {
        read.prefix.is_empty()
            && !read.range.object
            && !read.range.spans.iter().any(|span| span.from_end)
    })
}

/// The `justless read` that replaces a read in a Codex command list run from
/// `cwd`. Nothing in a list Codex approves may run as written, so a small file,
/// which a single read leaves alone, gets the command too, and it prints the same
/// bytes. A read counted from the end, a file outside the workspace, one that
/// cannot be read as text and a closed range that ends without a newline leave the
/// list as written.
pub(crate) fn piece(
    ws: &Workspace,
    cwd: &Path,
    shell: &ShellRead,
    session: Option<&str>,
) -> Option<String> {
    if shell.range.object || shell.range.spans.iter().any(|span| span.from_end) {
        return None;
    }
    let mut read = Selection::open(
        ws,
        &shell.directory(ws, cwd, false)?,
        &shell.file,
        &shell.range,
    )?;
    if read.closed && !read.text().ends_with('\n') {
        return None;
    }
    if shell.whole()
        && !read.small(ws, false)
        && read.outline(ws, read.text.len(), false).is_none()
        && let Some(instead) = crate::agents::read_instead(ws, &ws.root, &read.file)
    {
        return Some(instead);
    }
    Some(read.shell_command(ws, session))
}

/// `read --shell-range`: `file` is a workspace file, or with `object` the name
/// a `git show` gave blob `object`.
pub(crate) fn read(
    ws: &Workspace,
    file: &str,
    object: Option<&str>,
    (range, numbered): (&str, Option<Numbering>),
    session: &str,
) -> Result<()> {
    let mut range = LineRange::parse(range)
        .context("expected positive line ranges A-B or +N, joined by commas")?;
    range.numbered = numbered;
    let read = match object {
        Some(id) => Some(
            Selection::object(ws, &ws.cwd, file, Some(id), &range)
                .with_context(|| format!("read Git blob {id} as {file}"))?,
        ),
        None => Selection::new(Snapshot::open(ws, file)?, file, &range),
    };
    let Some(mut read) = read else {
        return Ok(());
    };
    let mut out = io::stdout().lock();
    if let Some(text) = read.compact(ws, read.text.len(), session, false) {
        out.write_all(text.as_bytes())?;
    } else {
        out.write_all(read.text().as_bytes())?;
        read.observe(ws, false);
    }
    Ok(())
}

/// The selection a Claude Code Bash result printed, its session's directory and
/// the bytes Claude Code would show, when the result is exactly those bytes.
fn ran(ws: &Workspace, input: &Value, shell: &ShellRead) -> Option<(Selection, PathBuf, usize)> {
    let response = &input["tool_response"];
    if !response["stderr"].as_str()?.is_empty()
        || response["returnCodeInterpretation"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    {
        return None;
    }
    let cwd = input["cwd"]
        .as_str()
        .map(Path::new)
        .unwrap_or(&ws.cwd)
        .canonicalize()
        .ok()?;
    let read = Selection::open(
        ws,
        &shell.directory(ws, &cwd, true)?,
        &shell.file,
        &shell.range,
    )?;
    let stdout = response["stdout"].as_str()?;
    let persisted = response["persistedOutputPath"].as_str();
    let stored;
    let (output, baseline) = if let Some(path) = persisted {
        stored = crate::postroute::read_persisted(path, &response["persistedOutputSize"], stdout)?;
        (
            std::str::from_utf8(&stored).ok()?,
            stdout
                .chars()
                .take(crate::postroute::PREVIEW_CHARS)
                .map(char::len_utf8)
                .sum(),
        )
    } else {
        (stdout, stdout.len())
    };
    let selected = read.text();
    if output != selected && (persisted.is_some() || selected.trim_end_matches('\n') != output) {
        return None;
    }
    // BSD and GNU sed differ at an unterminated EOF. A post hook can keep
    // either client's original result without reconstructing that final byte.
    if read.closed && !selected.ends_with('\n') {
        return None;
    }
    Some((read, cwd, baseline))
}

/// Claude executes the original command itself and checks permissions on the original command.
/// Accept the result only if it matches the selected source bytes. A result this
/// cannot outline, other than a small read that keeps its text, gets `fallback`.
pub(crate) fn claude(
    ws: &Workspace,
    input: &Value,
    shell: &ShellRead,
    fallback: impl FnOnce() -> Option<Value>,
) -> Option<Value> {
    let Some((mut read, cwd, baseline)) = ran(ws, input, shell) else {
        return fallback();
    };
    let session = input["session_id"]
        .as_str()
        .map(|s| capture::source_digest(s.as_bytes())[..24].to_owned());
    let Some(text) = read.compact(
        &ws.working_in(&cwd),
        baseline,
        session.as_deref().unwrap_or("terminal"),
        true,
    ) else {
        let shown = !input["tool_response"]["persistedOutputPath"].is_string();
        if !read.small(ws, shown)
            && let Some(other) = fallback()
        {
            return Some(other);
        }
        // Claude Code shows a result it saved to a file as a preview of its first
        // characters. The agent did not see the lines whole, so the read is
        // recorded as an outline of them, which never counts as a repeated read.
        let saved = input["tool_response"]["persistedOutputPath"].is_string();
        read.observe(ws, saved);
        return None;
    };
    let mut updated = input["tool_response"].as_object()?.clone();
    updated.insert("stdout".into(), json!(text));
    for key in [
        "persistedOutputPath",
        "persistedOutputSize",
        "stdoutHoldsOutputEnd",
    ] {
        updated.remove(key);
    }
    Some(json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":updated}}))
}

#[cfg(test)]
mod tests {
    use super::{Span, blob_path, command, parse};

    #[test]
    fn git_show_names_a_file_in_a_revision_or_the_index() {
        let words = |text: &str| shlex::split(text).unwrap();
        for (spec, path) in [
            ("HEAD:src/a.rs", "src/a.rs"),
            ("HEAD~1:./a.rs", "a.rs"),
            (":a.rs", "a.rs"),
            ("main^:a", "a"),
            ("@~2:a/b.py", "a/b.py"),
        ] {
            assert_eq!(blob_path(spec), Some(path), "{spec}");
        }
        for spec in [
            "HEAD",
            "HEAD:",
            "HEAD:./",
            ":/fix",
            ":0:a.rs",
            "HEAD:../a",
            "HEAD^{tree}:a",
            "-p:a",
            "HEAD:src/",
            "a b:c",
        ] {
            assert_eq!(blob_path(spec), None, "{spec}");
        }
        let unpaged = words("git --no-pager show HEAD:a.rs");
        let (spec, range) = command(&unpaged).unwrap();
        assert!(spec == "HEAD:a.rs" && range.object);
        for text in [
            "git show HEAD",
            "git show HEAD:a.rs b.rs",
            "git show -p HEAD:a.rs",
            "git -C x show HEAD:a.rs",
        ] {
            assert!(command(&words(text)).is_none(), "{text}");
        }
    }

    #[test]
    fn cat_cd_and_piped_reads_are_the_single_file_read() {
        let shown = |line: &str| {
            parse(line).map(|read| {
                let spans = read.range.spans.iter().map(|span| match span {
                    Span { from_end: true, .. } => format!("-{}", span.first),
                    Span { last: None, .. } => format!("+{}", span.first),
                    Span {
                        last: Some(last), ..
                    } => format!("{}-{last}", span.first),
                });
                let kind = match read.range.numbered {
                    Some(numbering) => numbering.command().join(" ").replace(' ', ""),
                    None if read.whole() => "cat".into(),
                    None => String::new(),
                };
                let kind = if kind.is_empty() {
                    kind
                } else {
                    format!(" {kind}")
                };
                let kind = if read.object() { " object" } else { &kind };
                format!(
                    "{}{}:{} {}{kind}",
                    read.prefix,
                    read.dir.as_deref().unwrap_or(""),
                    read.file,
                    spans.collect::<Vec<_>>().join(",")
                )
            })
        };
        for (line, read) in [
            ("cat src/a.rs", ":src/a.rs +1 cat"),
            ("cat 'a|b && c.rs'", ":a|b && c.rs +1 cat"),
            ("cd src && cat a.rs", "cd src && src:a.rs +1 cat"),
            (
                "cd 'my dir'&&sed -n '5,9p' a.rs",
                "cd 'my dir'&& my dir:a.rs 5-9",
            ),
            ("cd ./x && head -n 40 a.rs", "cd ./x && ./x:a.rs 1-40"),
            (
                "cd x && git show HEAD:./a.rs",
                "cd x && x:HEAD:./a.rs +1 object",
            ),
            ("cat a.rs | head -n 40", ":a.rs 1-40"),
            ("cat ä\u{3000}.rs | head -n 2", ":ä\u{3000}.rs 1-2"),
            ("cat a.rs | tail -n 30", ":a.rs -30"),
            ("cat a.rs|tail -n +30", ":a.rs +30"),
            ("cat a.rs | sed -n '1,2p;8,9p'", ":a.rs 1-2,8-9"),
            ("cd x && cat a.rs | sed -n 3,4p", "cd x && x:a.rs 3-4"),
            ("sed -n '1,200p' a.rs | cat", ":a.rs 1-200"),
            ("cd x && head -n 30 a.rs | cat -", "cd x && x:a.rs 1-30"),
            ("cat a.rs | head -n 40 | cat", ":a.rs 1-40"),
            ("cat a.rs|cat", ":a.rs +1 cat"),
            ("cat -n a.rs | sed -n '5,9p'", ":a.rs 5-9 cat-n"),
            ("cat -n a.rs | head -n 40", ":a.rs 1-40 cat-n"),
            ("cat a.rs | head -40", ":a.rs 1-40"),
            ("head -25 a.rs", ":a.rs 1-25"),
            ("head -n5 a.rs", ":a.rs 1-5"),
            ("cat a | head -n5", ":a 1-5"),
            ("head --lines=12 a.rs", ":a.rs 1-12"),
            ("head --lines 12 a.rs", ":a.rs 1-12"),
            ("cat a.rs | head --lines=12", ":a.rs 1-12"),
            ("tail -n20 a.rs", ":a.rs -20"),
            ("tail -n+30 a.rs", ":a.rs +30"),
            ("tail --lines=20 a.rs", ":a.rs -20"),
            ("tail --lines=+30 a.rs", ":a.rs +30"),
            ("tail --lines +30 a.rs", ":a.rs +30"),
            ("cat a.rs | tail -n+10", ":a.rs +10"),
            ("cat a.rs | tail -n10", ":a.rs -10"),
            ("sed -n '10,$p' a.rs", ":a.rs +10"),
            ("sed -n '1,5p;10,$p' a.rs", ":a.rs 1-5,+10"),
            ("cat a.rs | sed -n '10,$p'", ":a.rs +10"),
            ("nl -ba a.rs | sed -n '10,$p'", ":a.rs +10 nl-ba"),
            ("cat -n a.rs | head -7", ":a.rs 1-7 cat-n"),
            (
                "nl -ba a.rs | sed -n '5,9p;20,30p'",
                ":a.rs 5-9,20-30 nl-ba",
            ),
            (
                "cd x && nl -ba a.rs | head -n 3 | cat",
                "cd x && x:a.rs 1-3 nl-ba",
            ),
            ("git show HEAD:a.rs | cat", ":HEAD:a.rs +1 object"),
            ("git -P show HEAD:a.rs", ":HEAD:a.rs +1 object"),
            (
                "git --no-pager --no-optional-locks show HEAD:./a.rs",
                ":HEAD:./a.rs +1 object",
            ),
        ] {
            assert_eq!(shown(line).as_deref(), Some(read), "{line}");
        }
        for line in [
            "cat a b",
            "cat -n a",
            "cat -A a",
            "cat -- a",
            "cat *.rs",
            "cat a > b",
            "cat <<EOF",
            "cat a | head",
            "cat a | head -05",
            "head -n05 a.rs",
            "head -n+5 a.rs",
            "head --lines=-5 a.rs",
            "tail -n+0 a.rs",
            "tail -c5 a.rs",
            "sed -n '10,$p;20,30p' a.rs",
            "sed -n '0,$p' a.rs",
            "cat a | grep -n x",
            "cat a | head -n 5 | tail -n 2",
            "cat a b | head -n 1",
            "sed -n 1,2p a | head -n 1",
            "cat -n a | tail -n 2",
            "cat -n a | head",
            "cat -b a | head -n 2",
            "nl a | sed -n 1,2p",
            "nl -b a a | sed -n 1,2p",
            "sed -n 1,2p a | nl -ba",
            "cat -n a b | head -n 2",
            "cat a |& head -n 2",
            "cat a &",
            "cat a && cat b",
            "true && cat a",
            "cd x; cat a",
            "cd x || cat a",
            "cd x && cd y && cat a",
            "cd && cat a",
            "cd - && cat a",
            "cd $HOME && cat a",
            "cd ~/x && cat a",
            "cat ~/a",
            "cat a | cat | cat",
            "cat a | cat -n",
            "cat a | cat -u",
            "cat a | cat b",
            "cat a b | cat",
            "cat a && cat",
            "cd x && cat",
            "| cat",
        ] {
            assert_eq!(shown(line), None, "{line}");
        }
    }

    #[test]
    fn bat_plain_sed_flags_and_fail_closed_reads() {
        let shown = |line: &str| {
            parse(line).map(|read| {
                let spans = read
                    .range
                    .spans
                    .iter()
                    .map(|span| match span {
                        Span { from_end: true, .. } => format!("-{}", span.first),
                        Span { last: None, .. } => format!("+{}", span.first),
                        Span {
                            last: Some(last), ..
                        } => format!("{}-{last}", span.first),
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{}{}:{} {}{}",
                    read.prefix,
                    read.dir.as_deref().unwrap_or(""),
                    read.file,
                    spans,
                    if read.whole() { " whole" } else { "" }
                )
            })
        };
        for (line, expected) in [
            ("bat --plain src/a.rs", ":src/a.rs +1 whole"),
            ("bat -p src/a.rs", ":src/a.rs +1 whole"),
            (
                "bat -pp --paging=never --color=never src/a.rs",
                ":src/a.rs +1 whole",
            ),
            (
                "batcat --style=plain 'my dir/a.rs'",
                ":my dir/a.rs +1 whole",
            ),
            ("bat -p -r 10:50 a.rs", ":a.rs 10-50"),
            ("bat --plain --line-range=10: a.rs", ":a.rs +10"),
            ("batcat -p -r :25 a.rs", ":a.rs 1-25"),
            ("bat --plain a.rs | head -n 40", ":a.rs 1-40"),
            (
                "cd src && bat -p \"a b.rs\" | cat",
                "cd src && src:a b.rs +1 whole",
            ),
            ("sed -ne '1,20p' a.rs", ":a.rs 1-20"),
            ("sed -n -e '42p' a.rs", ":a.rs 42-42"),
            ("cat a.rs | sed -ne '5,10p'", ":a.rs 5-10"),
        ] {
            assert_eq!(shown(line).as_deref(), Some(expected), "{line}");
        }
        for rejected in [
            "bat src/a.rs",
            "bat -n src/a.rs",
            "bat -p -n src/a.rs",
            "bat -p --color=always src/a.rs",
            "bat -p --style=full src/a.rs",
            "bat -p a.rs b.rs",
            "bat -p *.rs",
            "bat -p a.rs > out.txt",
            "bat -p a.rs | grep foo",
            "bat -p -r 20:10 a.rs",
            "bat -p -r 0:5 a.rs",
            "bat -p -r 1:5 -r 10:20 a.rs",
            "bat -p -r 1:5 a.rs | head -n 2",
            "cat 'unclosed",
            "cargo test --locked",
            "git status",
        ] {
            assert_eq!(shown(rejected), None, "{rejected}");
        }
    }
}
