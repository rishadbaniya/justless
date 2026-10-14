//! The shell words a command hook keeps around a command it wraps.
//!
//! Leading `NAME=value` assignments stay in front of `justless run`, so the variables
//! reach both Justless and the command, while the command words alone choose the
//! output profile. A final `2>&1` stays at the end, so it merges the stdout and
//! stderr Justless prints, and the hook adds `--merge`, so the command writes both
//! streams in order to one archive, the way the original `2>&1` merged them.
//! `RUST_BACKTRACE=1 cargo test 2>&1` becomes
//! `RUST_BACKTRACE=1 justless run --hook --merge -- cargo test 2>&1`.
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

/// Whether the text that follows a wrapped command's words starts with the final
/// `2>&1` that `parse` left out of them.
pub fn merges(rest: &str) -> bool {
    rest.trim_start_matches([' ', '\t'])
        .strip_prefix(MERGE)
        .is_some_and(|after| after.is_empty() || after.starts_with([' ', '\t', ';']))
}

/// Whether a command of a list may change, for the commands after it, a variable
/// that decides which Justless runs or how. That is a command made only of
/// assignments, such as `PATH=/x` or `PATH=/x 2>&1`, an assignment before a POSIX
/// special builtin such as `:`, which the shell keeps, or `export`, `declare`,
/// `typeset`, `readonly`, `local` or `unset` naming such a variable, also behind
/// `builtin` or `command`. A hook then passes the whole list through, as for a
/// change of directory.
pub fn steers_justless(segment: &str) -> bool {
    let Some(words) = words(segment) else {
        return true;
    };
    // Each word with its quotes removed.
    let words: Vec<String> = words
        .iter()
        .map(|word| {
            shlex::split(&segment[word.clone()])
                .and_then(|word| word.into_iter().next())
                .unwrap_or_default()
        })
        .collect();
    let words = match words.as_slice() {
        [rest @ .., last] if last == MERGE => rest,
        all => all,
    };
    // The variable a word names, as in `NAME=value`, or `NAME+=value`, which appends
    // in bash and zsh.
    let name = |word: &str| {
        let name = word.split_once('=')?.0;
        let name = name.strip_suffix('+').unwrap_or(name);
        (name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| name.to_owned())
    };
    let program = words.iter().position(|word| name(word).is_none());
    let assigned = words[..program.unwrap_or(words.len())]
        .iter()
        .filter_map(|word| name(word))
        .any(|name| steers(&name));
    let Some(program) = program else {
        return assigned;
    };
    let mut rest = words[program..]
        .iter()
        .map(String::as_str)
        .skip_while(|word| matches!(*word, "builtin" | "command"));
    let Some(program) = rest.next() else {
        return assigned;
    };
    let declares = matches!(
        program,
        "export" | "declare" | "typeset" | "readonly" | "local" | "unset"
    );
    (assigned && SPECIAL.contains(&program))
        || (declares
            && rest.any(|word| {
                let name = word.split_once('=').map_or(word, |(name, _)| name);
                steers(name.strip_suffix('+').unwrap_or(name))
            }))
}

/// POSIX special builtins, after which a shell keeps the assignments that precede
/// them.
const SPECIAL: &[&str] = &[
    ":", ".", "break", "continue", "eval", "exec", "exit", "export", "readonly", "return", "set",
    "shift", "times", "trap", "unset",
];

/// Whether a variable decides which Justless the shell starts or how it runs:
/// `PATH`, and `path`, which zsh ties to it, `LD_*` and `DYLD_*`, which load code
/// into it, and `JUSTLESS_*`, which configure it. An empty name stands for an
/// assignment the shell would not take literally.
fn steers(name: &str) -> bool {
    name.is_empty()
        || name == "PATH"
        || name == "path"
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("JUSTLESS_")
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
    /// Whether Justless itself runs unchanged under these assignments. A command
    /// with a variable that `steers` Justless is left whole.
    pub fn keeps_justless(&self, segment: &str) -> bool {
        self.variables(segment).all(|(name, _)| !steers(&name))
    }

    /// Whether a client that may run the rewritten command without asking can take
    /// these assignments: display and diagnostic settings with plain values only.
    /// Variables such as `RUSTFLAGS`, `NODE_OPTIONS`, `PYTEST_ADDOPTS` or
    /// `GIT_EXTERNAL_DIFF` can run other code or write elsewhere, so they are left
    /// to the client's own approval, as is a value with a path. `make` reads every
    /// environment variable as a make variable, which a recipe may use, so it takes
    /// no assignment, as it takes no `NAME=value` argument.
    pub fn approvable(&self, segment: &str) -> bool {
        let make = || {
            shlex::split(&segment[self.command.clone()]).is_none_or(|words| {
                let words: Vec<&str> = words.iter().map(String::as_str).collect();
                crate::filters::unwrap_tool(&words)
                    .first()
                    .is_some_and(|program| {
                        let name = std::path::Path::new(program).file_name();
                        name.is_some_and(|name| name == "make" || name == "gmake")
                    })
            })
        };
        (self.assignments.is_empty() || !make())
            && self.variables(segment).all(|(name, value)| {
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
/// output, so setting them cannot make a build or test run other code. `CI` and
/// `NODE_ENV` are left out: builds, test setups and Makefiles branch on them, for
/// example to choose a production configuration or skip a step.
const DISPLAY: &[&str] = &[
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
