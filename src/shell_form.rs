//! The shell words a command hook keeps around a command it wraps.
//!
//! Leading `NAME=value` assignments stay in front of `justless run`, so the variables
//! reach both Justless and the command, while the command words alone choose the
//! output profile. A final `2>&1` stays at the end, so it merges the stdout and
//! stderr previews Justless prints, the way it merged the command's own streams.
//! `RUST_BACKTRACE=1 cargo test 2>&1` becomes
//! `RUST_BACKTRACE=1 justless run --hook -- cargo test 2>&1`.
use std::ops::Range;

/// The redirection that merges stderr into stdout.
const MERGE: &str = "2>&1";

/// One simple command, split around the words a hook wraps.
pub struct Form {
    /// Leading `NAME=value` assignments, as written.
    assignments: Vec<Range<usize>>,
    /// The command words, from the program to the last argument, without a final
    /// `2>&1`.
    pub command: Range<usize>,
}

/// Whether `bytes[at]` is the `>` of a `2>&1` that ends its command: it follows a
/// blank, and only blanks lie between it and the end of the line, a `;`, or a
/// blank-separated `&&` or `||`. A command list may contain it there, and `parse`
/// keeps it out of the command words. Anywhere else, such as before `cd` in
/// `2>&1 cd /tmp`, the list passes through.
pub fn merge_at(bytes: &[u8], at: usize) -> bool {
    let blank = |byte: &u8| matches!(byte, b' ' | b'\t');
    if at < 2 || !blank(&bytes[at - 2]) || !bytes[at - 1..].starts_with(MERGE.as_bytes()) {
        return false;
    }
    let rest = &bytes[at + 3..];
    let next = &rest[rest
        .iter()
        .position(|byte| !blank(byte))
        .unwrap_or(rest.len())..];
    rest.first().is_none_or(|byte| blank(byte) || *byte == b';')
        && (next.is_empty()
            || next[0] == b';'
            || next.starts_with(b"&&")
            || next.starts_with(b"||"))
}

/// Split a trimmed simple command into its assignments and command words, leaving
/// out a final `2>&1`. Returns `None` for unbalanced quotes or when nothing but
/// assignments remains. A `2>&1` anywhere else stays in the command words, where
/// the hooks' literal-command check rejects it.
pub fn parse(segment: &str) -> Option<Form> {
    let mut words = words(segment)?;
    if words.len() > 1
        && words
            .last()
            .is_some_and(|word| &segment[word.clone()] == MERGE)
    {
        words.pop();
    }
    let program = words
        .iter()
        .position(|word| !assignment(&segment[word.clone()]))?;
    Some(Form {
        assignments: words[..program].to_vec(),
        command: words[program].start..words.last()?.end,
    })
}

impl Form {
    /// Whether Justless itself runs unchanged under these assignments. `PATH`, and
    /// `path`, which zsh ties to it, decide which `justless` the shell starts,
    /// `LD_*` and `DYLD_*` load code into it, and `JUSTLESS_*` configure it, so a
    /// command with any of them is left whole.
    pub fn keeps_justless(&self, segment: &str) -> bool {
        self.variables(segment).all(|(name, _)| {
            !name.is_empty()
                && name != "PATH"
                && name != "path"
                && !name.starts_with("LD_")
                && !name.starts_with("DYLD_")
                && !name.starts_with("JUSTLESS_")
        })
    }

    /// Whether a client that may run the rewritten command without asking can take
    /// these assignments: display and diagnostic settings with plain values only.
    /// Variables such as `RUSTFLAGS`, `NODE_OPTIONS`, `PYTEST_ADDOPTS` or
    /// `GIT_EXTERNAL_DIFF` can run other code or write elsewhere, so they are left
    /// to the client's own approval, as is a value with a path.
    pub fn approvable(&self, segment: &str) -> bool {
        self.variables(segment).all(|(name, value)| {
            (DISPLAY.contains(&name.as_str()) || name.starts_with("LC_"))
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.,:=@%+-".contains(c))
        })
    }

    /// Each assignment's name and its value after quote removal. An assignment the
    /// shell would not take literally yields an empty name, which no rule accepts.
    fn variables<'a>(&'a self, segment: &'a str) -> impl Iterator<Item = (String, String)> + 'a {
        self.assignments.iter().map(|range| {
            match shlex::split(&segment[range.clone()]).as_deref() {
                Some([word]) => word
                    .split_once('=')
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .unwrap_or_default(),
                _ => Default::default(),
            }
        })
    }
}

/// Variables that only change how a tool reports, colors, localizes or paces its
/// output, so setting them cannot make a build or test run other code.
const DISPLAY: &[&str] = &[
    "CI",
    "NO_COLOR",
    "FORCE_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "TERM",
    "COLUMNS",
    "LINES",
    "LANG",
    "LANGUAGE",
    "RUST_BACKTRACE",
    "RUST_LIB_BACKTRACE",
    "RUST_LOG",
    "RUST_LOG_STYLE",
    "RUST_TEST_THREADS",
    "RUST_TEST_NOCAPTURE",
    "CARGO_TERM_COLOR",
    "CARGO_TERM_VERBOSE",
    "CARGO_TERM_QUIET",
    "CARGO_INCREMENTAL",
    "CARGO_BUILD_JOBS",
    "NODE_ENV",
    "PYTHONUNBUFFERED",
    "PYTHONDONTWRITEBYTECODE",
    "PYTHONHASHSEED",
    "PYTHONFAULTHANDLER",
    "PYTHONUTF8",
    "PYTHONIOENCODING",
    "GOTRACEBACK",
    "GOMAXPROCS",
];

/// Whether a word assigns a variable: an unquoted shell name followed by `=`.
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The byte range of each word, split at blanks outside quotes.
fn words(text: &str) -> Option<Vec<Range<usize>>> {
    let mut words = Vec::new();
    let mut start = None;
    let mut quote = None;
    for (index, character) in text.char_indices() {
        match quote {
            Some(active) if character == active => quote = None,
            Some(_) => {}
            None if matches!(character, ' ' | '\t') => {
                if let Some(start) = start.take() {
                    words.push(start..index);
                }
                continue;
            }
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None => {}
        }
        start.get_or_insert(index);
    }
    if quote.is_some() {
        return None;
    }
    if let Some(start) = start {
        words.push(start..text.len());
    }
    Some(words)
}
