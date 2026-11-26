//! Group search output by file and select the search and listing profiles.
//!
//! `rg`, `grep -r`, `git grep`, `ag` and `ack` print one record per line as
//! `path:line:text` or `path:text`. A path is accepted only when it names an existing
//! file, so output without paths, such as a single-file grep, keeps the ordinary
//! preview. Deep common directory prefixes hoist into the header when strictly
//! smaller, ultra-long single lines (> 200 bytes) center a window on the match,
//! identical trimmed lines repeating 3+ times inside a file collapse to
//! `L12,18,25: <line>`, and pure-boilerplate files sharing an identical line
//! deduplicate while keeping every path, line number and `[stdout:A-B]` link.
//!
//! With context options, matches use `path:line:text`, context lines
//! `path-line-text`, and blocks of them are separated by `--`. Block mode reads
//! those, the numbered lines of one file without its name and `rg --heading`, and
//! reports blocks instead of matching lines.
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::filters::Profile;
use crate::summary::{Item, Summary, short};
use crate::workspace::Workspace;

const SHOWN: usize = 8;
const LINE_BYTES: usize = 200;
const WINDOW_SNIPPET_BYTES: usize = 168;
const MAX_SLOTS: usize = 16;
const MAX_FOLDED_RANGES: usize = 24;
const MAX_LOC_STR_BYTES: usize = 72;
const INLINE_KEY_BYTES: usize = 512;
const INTRA_FOLD_MIN: usize = 3;
const CROSS_DEDUP_MIN_FILES: usize = 3;
const CROSS_DEDUP_MIN_LEN: usize = 10;
const MAX_FILES: usize = 50_000;
const MAX_CHECKS: usize = 200_000;
/// Groups kept in a run record for rerun comparisons.
pub(crate) const STORED: usize = 256;

/// Accepted options for one tool. Any other option selects no profile, so a new
/// flag or output format reaches the ordinary preview.
/// Each field is a space-separated list.
pub(crate) struct Spec {
    flags: &'static str,
    valued: &'static str,
    /// Options accepted bare or with `=value`, such as grep's `--color`.
    optional: &'static str,
    /// Options that print file names instead of matching lines.
    lists: &'static str,
}

const RG: Spec = Spec {
    flags: "-i --ignore-case -s --case-sensitive -S --smart-case -w --word-regexp -x \
        --line-regexp -F --fixed-strings --no-fixed-strings -n --line-number -N --no-line-number \
        -H --with-filename --hidden --no-hidden -. -u --unrestricted --no-ignore --no-ignore-vcs \
        --no-ignore-parent --no-ignore-dot --no-ignore-global --no-ignore-exclude \
        --no-ignore-files --no-ignore-messages --no-require-git --require-git --ignore \
        --ignore-dot --ignore-exclude --ignore-files --ignore-global --ignore-messages \
        --ignore-parent --ignore-vcs --messages --glob-case-insensitive \
        --ignore-file-case-insensitive -L --follow --no-follow -U --multiline --no-multiline \
        --multiline-dotall --no-multiline-dotall -P --pcre2 --no-pcre2 --auto-hybrid-regex \
        --no-auto-hybrid-regex --unicode --no-unicode -o --only-matching --no-only-matching \
        -v --invert-match --no-invert-match --column -a --text --no-text --no-heading --heading \
        --no-messages --trim --no-trim --crlf --no-crlf --sort-files --no-sort-files --binary \
        --no-binary --no-config --one-file-system --no-one-file-system --vimgrep \
        --max-columns-preview --no-max-columns-preview -l --files-with-matches \
        --files-without-match --files",
    valued: "-e --regexp -f --file -g --glob --iglob -t --type -T --type-not --type-add -m \
        --max-count -A --after-context -B --before-context -C --context -d --max-depth \
        -M --max-columns -j --threads -E --encoding --engine --max-filesize --sort \
        --sortr --ignore-file -r --replace --color --colors",
    optional: "",
    lists: "-l --files-with-matches --files-without-match --files",
};

/// Every ripgrep 15 option, as `rg --help` lists it with the `--no-` forms that
/// turn one off, except those that run another program: `--pre` and
/// `--pre-glob`, which run a preprocessor on each file, `-z`/`--search-zip`, which
/// runs decompression programs, and `--hostname-bin`. Hooks route ripgrep with
/// any option, so a client that approves the rewrite checks it against this list.
const RG_READS: Spec = Spec {
    flags: "-s --case-sensitive --crlf -F --fixed-strings -i --ignore-case -v --invert-match \
        -x --line-regexp --mmap -U --multiline --multiline-dotall --no-unicode --unicode \
        --null-data -P --pcre2 -S --smart-case --stop-on-nonmatch -a --text -w --word-regexp \
        --auto-hybrid-regex --no-pcre2-unicode --pcre2-unicode --binary -L --follow \
        --glob-case-insensitive -. --hidden --ignore-file-case-insensitive --no-ignore \
        --no-ignore-dot --no-ignore-exclude --no-ignore-files --no-ignore-global \
        --no-ignore-parent --no-ignore-vcs --no-require-git --one-file-system -u \
        --unrestricted --block-buffered -b --byte-offset --column --heading -h --help \
        --include-zero --line-buffered -n --line-number -N --no-line-number \
        --max-columns-preview -0 --null -o --only-matching --passthru -p --pretty -q --quiet \
        --trim --vimgrep -H --with-filename -I --no-filename --sort-files -c --count \
        --count-matches -l --files-with-matches --files-without-match --json --debug \
        --no-ignore-messages --no-messages --stats --trace --files --no-config \
        --pcre2-version --type-list -V --version --ignore --ignore-dot --ignore-exclude \
        --ignore-files --ignore-global --ignore-messages --ignore-parent --ignore-vcs \
        --messages --require-git --no-auto-hybrid-regex --no-binary --no-block-buffered \
        --no-byte-offset --no-column --no-context-separator --no-crlf --no-encoding \
        --no-fixed-strings --no-follow --no-glob-case-insensitive --no-heading --no-hidden \
        --no-ignore-file-case-insensitive --no-include-zero --no-invert-match --no-json \
        --no-line-buffered --no-max-columns-preview --no-mmap --no-multiline \
        --no-multiline-dotall --no-one-file-system --no-pcre2 --no-pre --no-search-zip \
        --no-sort-files --no-stats --no-text --no-trim",
    valued: "-e --regexp -f --file --dfa-size-limit -E --encoding --engine -m --max-count \
        --regex-size-limit -j --threads -g --glob --iglob --ignore-file -d --max-depth \
        --max-filesize -t --type -T --type-not --type-add --type-clear -A --after-context -B \
        --before-context --color --colors -C --context --context-separator \
        --field-context-separator --field-match-separator --hyperlink-format -M --max-columns \
        --path-separator -r --replace --sort --sortr --generate",
    optional: "",
    lists: "",
};

const GREP: Spec = Spec {
    flags: "-r -R --recursive --dereference-recursive -n --line-number -i --ignore-case \
        --no-ignore-case -y -w --word-regexp -x --line-regexp -v --invert-match -E \
        --extended-regexp -F --fixed-strings -G --basic-regexp -P --perl-regexp -o \
        --only-matching -s --no-messages -I -a --text -H --with-filename -b \
        --byte-offset -U --binary --line-buffered -l --files-with-matches -L \
        --files-without-match",
    valued: "-e --regexp -f --file -m --max-count -A --after-context -B --before-context -C \
        --context --include --exclude --exclude-dir --exclude-from -d --directories -D \
        --devices --binary-files",
    optional: "--color --colour",
    lists: "-l --files-with-matches -L --files-without-match",
};

const GIT_GREP: Spec = Spec {
    flags: "-n --line-number -i --ignore-case -w --word-regexp -v --invert-match -E \
        --extended-regexp -F --fixed-strings -G --basic-regexp -P --perl-regexp -I -a \
        --text --untracked --cached --exclude-standard --recurse-submodules -o \
        --only-matching --column --full-name -r --recursive --and --or --not --all-match \
        --no-color -l --files-with-matches --name-only -L --files-without-match",
    valued: "-e -f -A --after-context -B --before-context -C --context --max-depth -m \
        --max-count --threads",
    optional: "--color",
    lists: "-l --files-with-matches --name-only -L --files-without-match",
};

const AG: Spec = Spec {
    flags: "-i --ignore-case -s --case-sensitive -S --smart-case -w --word-regexp -Q \
        --literal -v --invert-match -o --only-matching -u --unrestricted -U \
        --skip-vcs-ignores --hidden -a --all-types -t --all-text -f --follow --color \
        --nocolor --noheading --nogroup --numbers --column --vimgrep --search-binary -l \
        --files-with-matches -L --files-without-matches",
    valued: "-A --after -B --before -C --context -G --file-search-regex -g --ignore \
        --ignore-dir --depth -m --max-count -p --path-to-ignore",
    optional: "",
    lists: "-l --files-with-matches -L --files-without-matches -g",
};

const ACK: Spec = Spec {
    flags: "-i --ignore-case -I --no-ignore-case -S --smart-case -w --word-regexp -x \
        --line-regexp -Q --literal -v --invert-match -o -H --with-filename --nocolor \
        --nogroup --noheading --column -k --known-types -r -R --recurse -s --follow \
        --nofollow --nopager --noenv -l --files-with-matches -L --files-without-matches \
        -f",
    valued: "-A --after-context -B --before-context -C --context -m --max-count -g --type \
        --ignore-dir --noignore-dir --ignore-file --match",
    optional: "--color",
    lists: "-l --files-with-matches -L --files-without-matches -f -g",
};

pub(crate) const LS: Spec = Spec {
    flags: "-a -A -l -R -h -F -p -t -S -r -1 -U -c -u -G -d -n -L -s -i --all --almost-all \
        --recursive --human-readable --classify --reverse --full-time \
        --group-directories-first",
    valued: "--sort --time-style",
    optional: "--color",
    lists: "",
};

const TREE: Spec = Spec {
    flags: "-a -d -f -F -C -n -l -x -v -t -r -c -U -h -s --dirsfirst --filesfirst --noreport \
        --gitignore --prune --matchdirs --ignore-case",
    valued: "-L -I -P --filelimit --charset --sort",
    optional: "",
    lists: "",
};

const FD: Spec = Spec {
    flags: "-H --hidden -I --no-ignore --no-ignore-vcs -u --unrestricted -s --case-sensitive \
        -i --ignore-case -g --glob --regex -F --fixed-strings -a --absolute-path -L \
        --follow -p --full-path --strip-cwd-prefix --prune --one-file-system \
        --show-errors --no-require-git",
    valued: "-d --max-depth --min-depth --exact-depth -t --type -e --extension -E --exclude \
        -S --size --changed-within --changed-before -o --owner --max-results -j \
        --threads --base-directory --search-path -c --color --ignore-file",
    optional: "",
    lists: "",
};

const LS_FILES: Spec = Spec {
    flags: "-c --cached -o --others -i --ignored --exclude-standard -m --modified -d \
        --deleted -k --killed --directory --no-empty-directory --full-name \
        --recurse-submodules --error-unmatch --deduplicate",
    valued: "-x --exclude -X --exclude-from --exclude-per-directory",
    optional: "",
    lists: "",
};

#[derive(Default)]
pub(crate) struct Parsed<'a> {
    seen: Vec<(&'static str, Option<&'a str>)>,
    pub positional: Vec<&'a str>,
}

impl Parsed<'_> {
    pub fn has(&self, names: &[&str]) -> bool {
        self.seen.iter().any(|(flag, _)| names.contains(flag))
    }

    fn has_any(&self, list: &str) -> bool {
        list.split_whitespace().any(|name| self.has(&[name]))
    }

    fn value(&self, names: &[&str]) -> Option<&str> {
        self.seen
            .iter()
            .find(|(flag, _)| names.contains(flag))
            .and_then(|(_, value)| *value)
    }
}

/// Parse options against a spec, including short clusters such as `-rnI` and `-A3`.
pub(crate) fn parse<'a>(args: &[&'a str], spec: &Spec) -> Option<Parsed<'a>> {
    let known = |name: &str, list: &'static str| list.split_whitespace().find(|f| *f == name);
    let mut parsed = Parsed {
        seen: Vec::with_capacity(args.len()),
        positional: Vec::with_capacity(args.len().min(4)),
    };
    let mut args = args.iter().copied();
    while let Some(arg) = args.next() {
        if arg == "--" {
            parsed.positional.extend(args);
            break;
        }
        if arg.starts_with("--") {
            let (key, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(k, v)| (k, Some(v)));
            if let Some(flag) = known(key, spec.valued) {
                let value = match inline {
                    Some(value) => value,
                    None => args.next()?,
                };
                parsed.seen.push((flag, Some(value)));
            } else {
                let flag = known(key, spec.optional)
                    .or_else(|| inline.is_none().then(|| known(key, spec.flags)).flatten())?;
                parsed.seen.push((flag, inline));
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            let body = &arg[1..];
            let mut buf = [0u8; 5];
            buf[0] = b'-';
            for (i, c) in body.char_indices() {
                let n = c.encode_utf8(&mut buf[1..]).len();
                let flag = std::str::from_utf8(&buf[..1 + n]).ok()?;
                if let Some(flag) = known(flag, spec.valued) {
                    let rest = &body[i + c.len_utf8()..];
                    let value = if rest.is_empty() { args.next()? } else { rest };
                    parsed.seen.push((flag, Some(value)));
                    break;
                }
                parsed.seen.push((known(flag, spec.flags)?, None));
            }
        } else {
            parsed.positional.push(arg);
        }
    }
    Some(parsed)
}

fn basename(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program)
}

/// The option spec of a search or listing and its arguments after any
/// subcommand, `Some((None, args))` for `find`, which has its own allow list, or
/// `None` for any other program.
fn spec<'w, 'a>(words: &'w [&'a str]) -> Option<(Option<&'static Spec>, &'w [&'a str])> {
    let (&program, args) = words.split_first()?;
    Some(match (basename(program), args) {
        ("rg", _) => (Some(&RG), args),
        ("grep" | "egrep" | "fgrep", _) => (Some(&GREP), args),
        ("git", rest) => match crate::filters::git_display_subcommand(rest) {
            ["grep", tail @ ..] => (Some(&GIT_GREP), tail),
            ["ls-files", tail @ ..] => (Some(&LS_FILES), tail),
            _ => return None,
        },
        ("ag", _) => (Some(&AG), args),
        ("ack" | "ack-grep", _) => (Some(&ACK), args),
        ("ls", _) => (Some(&LS), args),
        ("tree", _) => (Some(&TREE), args),
        ("fd" | "fdfind", _) => (Some(&FD), args),
        ("find", _) => (None, args),
        _ => return None,
    })
}

/// Whether a search or listing passes only options its spec lists, or `None` for
/// any other program. Every listed option only selects, matches or formats what
/// the tool reads and prints, so any other option fails, such as ripgrep's
/// `--pre`, `--hostname-bin` and `-z`, which run other programs, `fd --exec` or
/// `find -exec`. Hooks route ripgrep with any option, so it is checked against
/// all of its options that only read (see `RG_READS`); the others are routed
/// only with the display options of their spec.
pub fn read_only(words: &[&str]) -> Option<bool> {
    let (spec, args) = spec(words)?;
    let rg = words
        .first()
        .is_some_and(|program| basename(program) == "rg");
    Some(match spec {
        Some(_) if rg => parse(args, &RG_READS).is_some(),
        Some(spec) => parse(args, spec).is_some(),
        None => find_display(args),
    })
}

/// Select the search or listing profile for display-oriented invocations only.
/// Machine formats, counts, NUL separators and side effects select nothing.
pub fn select(words: &[&str]) -> Option<Profile> {
    let (spec, args) = spec(words)?;
    let Some(spec) = spec else {
        return find_display(args).then_some(Profile::Listing);
    };
    let parsed = parse(args, spec)?;
    if spec.lists.is_empty() {
        return Some(Profile::Listing);
    }
    if std::ptr::eq(spec, &GREP) {
        // grep prints file names only when it recurses or reads several files.
        let recursive = parsed.has(&["-r", "-R", "--recursive", "--dereference-recursive"])
            || parsed.value(&["-d", "--directories"]) == Some("recurse");
        let explicit = parsed.has(&["-e", "--regexp", "-f", "--file"]);
        let files = parsed.positional.len() - usize::from(!explicit).min(parsed.positional.len());
        // One file's numbered lines with context still read as blocks.
        let blocks = parsed.has(CONTEXT) && parsed.has(&["-n", "--line-number"]);
        if !(recursive
            || files >= 2
            || files == 1 && (blocks || parsed.has(&["-H", "--with-filename"])))
        {
            return None;
        }
    }
    // rg prints file names as headings only on request, and only numbered blocks
    // read them.
    if parsed.has(&["--heading"]) && !(parsed.has(CONTEXT) && parsed.has(&["-n", "--line-number"]))
    {
        return None;
    }
    Some(if parsed.has_any(spec.lists) {
        Profile::Listing
    } else {
        Profile::Search
    })
}

/// Options that print lines around each match.
const CONTEXT: &[&str] = &[
    "-A",
    "-B",
    "-C",
    "--after-context",
    "--before-context",
    "--context",
    "--after",
    "--before",
];

/// A search tool's spec and options, unless it lists file names, such as
/// `rg --files .`, which prints no lines and may have no pattern.
fn searched<'a>(words: &[&'a str]) -> Option<(&'static Spec, Parsed<'a>)> {
    let (&program, args) = words.split_first()?;
    let (spec, args) = match (basename(program), args) {
        ("rg", _) => (&RG, args),
        ("grep" | "egrep" | "fgrep", _) => (&GREP, args),
        ("git", rest) => match crate::filters::git_display_subcommand(rest) {
            ["grep", tail @ ..] => (&GIT_GREP, tail),
            _ => return None,
        },
        ("ag", _) => (&AG, args),
        ("ack" | "ack-grep", _) => (&ACK, args),
        _ => return None,
    };
    let parsed = parse(args, spec).filter(|parsed| !parsed.has_any(spec.lists))?;
    Some((spec, parsed))
}

/// The patterns and the path operands of a search.
fn operands<'p, 'a>(parsed: &'p Parsed<'a>) -> (Vec<&'a str>, &'p [&'a str]) {
    let explicit: Vec<&str> = parsed
        .seen
        .iter()
        .filter(|(flag, _)| matches!(*flag, "-e" | "--regexp"))
        .filter_map(|(_, value)| *value)
        .collect();
    if explicit.is_empty() && !parsed.has(&["-f", "--file"]) {
        let pattern = usize::from(!parsed.positional.is_empty());
        let (pattern, paths) = parsed.positional.split_at(pattern);
        (pattern.to_vec(), paths)
    } else {
        (explicit, &parsed.positional)
    }
}

/// Whether a search prints lines around its matches, such as `grep -A5`. Its
/// report shows blocks rather than matching lines.
pub(crate) fn prints_context(words: &[&str]) -> bool {
    searched(words).is_some_and(|(_, parsed)| parsed.has(CONTEXT))
}

/// Whether a search pattern matches every line, such as `grep -rn "" file`. The
/// agent asked for those lines, and a report grouped by file would show only a few.
pub(crate) fn matches_every_line(words: &[&str]) -> bool {
    const EVERY_LINE: &[&str] = &["", "^", "$", ".", ".*", "^.*", ".*$", "^.*$"];
    searched(words).is_some_and(|(_, parsed)| {
        operands(&parsed)
            .0
            .iter()
            .any(|pattern| EVERY_LINE.contains(pattern))
    })
}

// `find` options are single-dash words, so it has its own allow list. Actions such
// as -exec and -delete and formats such as -print0, -printf and -ls select nothing.
fn find_display(args: &[&str]) -> bool {
    let mut args = args.iter().copied().peekable();
    while args.next_if(|a| matches!(*a, "-H" | "-L" | "-P")).is_some() {}
    while args
        .next_if(|a| !a.starts_with('-') && !matches!(*a, "(" | ")" | "!" | ","))
        .is_some()
    {}
    while let Some(arg) = args.next() {
        match arg {
            "-name" | "-iname" | "-path" | "-ipath" | "-wholename" | "-iwholename" | "-regex"
            | "-iregex" | "-regextype" | "-type" | "-xtype" | "-maxdepth" | "-mindepth"
            | "-mtime" | "-mmin" | "-atime" | "-amin" | "-ctime" | "-cmin" | "-newer"
            | "-newermt" | "-newerct" | "-size" | "-perm" | "-user" | "-group" | "-uid"
            | "-gid" | "-links" | "-inum" | "-samefile" => {
                if args.next().is_none() {
                    return false;
                }
            }
            "("
            | ")"
            | "!"
            | "-not"
            | "-a"
            | "-and"
            | "-o"
            | "-or"
            | "-prune"
            | "-print"
            | "-empty"
            | "-follow"
            | "-xdev"
            | "-mount"
            | "-depth"
            | "-d"
            | "-true"
            | "-false"
            | "-readable"
            | "-writable"
            | "-executable"
            | "-nouser"
            | "-nogroup"
            | "-daystart"
            | "-noleaf"
            | "-ignore_readdir_race" => {}
            _ => return false,
        }
    }
    true
}

/// Whether a command hook should route this command, or `None` when it is not a
/// search or listing. A plain `ls` is usually short, so only `ls -l` and `ls -R` go.
pub fn route(words: &[&str]) -> Option<bool> {
    let unwrapped = crate::wrappers::hook_effective(words)?;
    let refs: Vec<&str> = unwrapped.iter().map(String::as_str).collect();
    let words = crate::filters::unwrap_tool(&refs);
    select(words)?;
    if basename(words[0]) == "ls" {
        return Some(parse(&words[1..], &LS)?.has(&["-l", "--full-time", "-R", "--recursive"]));
    }
    Some(true)
}

/// An option that keeps a recursive search out of Justless's own state directory.
/// grep and ack read hidden directories, so they would match archived logs and,
/// without an input-is-output check, the stdout file of the run itself. ripgrep and
/// ag skip hidden directories unless asked to search them.
pub fn exclude_state(words: &[&str]) -> Option<&'static str> {
    let (&program, args) = words.split_first()?;
    let spec = match basename(program) {
        "grep" | "egrep" | "fgrep" => &GREP,
        "ack" | "ack-grep" => &ACK,
        "ag" => &AG,
        "rg" => &RG,
        _ => return None,
    };
    // Searching inside the state directory on purpose keeps it. A pattern or option
    // value that mentions it does not. Unknown options count every word as a path.
    let operands = match parse(args, spec) {
        Some(parsed) => {
            let explicit = parsed.has(&["-e", "--regexp", "-f", "--file"]);
            let pattern = usize::from(!explicit).min(parsed.positional.len());
            parsed.positional[pattern..].to_vec()
        }
        None => args.to_vec(),
    };
    if operands
        .iter()
        .any(|path| path.split('/').any(|part| part == ".justless"))
    {
        return None;
    }
    let hidden = || {
        args.iter().any(|a| {
            matches!(*a, "--hidden" | "--unrestricted")
                || a.starts_with('-') && !a.starts_with("--") && a.contains(['u', '.'])
        })
    };
    match basename(program) {
        "grep" | "egrep" | "fgrep" => Some("--exclude-dir=.justless"),
        "ack" | "ack-grep" => Some("--ignore-dir=.justless"),
        "ag" if hidden() => Some("--ignore=.justless"),
        "rg" if hidden() => Some("--glob=!.justless"),
        _ => None,
    }
}

#[derive(Clone, Copy, Default)]
struct LineMode {
    /// `Some(true)` when `-n`/`--line-number`/`--vimgrep` or a tool that defaults
    /// to line numbers is active; `Some(false)` for `-N`/`--no-line-number`; `None`
    /// when unknown and auto-detected from strictly increasing line numbers.
    explicit_numbered: Option<bool>,
    /// `--column` or `--vimgrep`: lines carry `line:col:text`.
    has_col: bool,
    /// `-o`/`--only-matching`: the same file line may produce several matches.
    only_matching: bool,
}

/// Strip ANSI CSI escape sequences (`\x1b\[[0-?]*[ -/]*[@-~]`) with a
/// zero-allocation fast path when no ESC byte is present.
fn strip_ansi_fast(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let Some(first_esc) = bytes.iter().position(|&b| b == 0x1b) else {
        return Cow::Borrowed(text);
    };
    let mut out = String::new();
    let mut copy_start = 0usize;
    let mut i = first_esc;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < bytes.len() && (0x30..=0x3f).contains(&bytes[j]) {
                j += 1;
            }
            while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && (0x40..=0x7e).contains(&bytes[j]) {
                if out.is_empty() {
                    out.reserve(text.len());
                }
                out.push_str(&text[copy_start..i]);
                i = j + 1;
                copy_start = i;
                continue;
            }
        }
        i += 1;
    }
    if copy_start == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copy_start..]);
    Cow::Owned(out)
}

/// Collapse runs of unquoted ASCII horizontal whitespace (`' '` / `'\t'`) into a single
/// space, keeping string literals verbatim and borrowing with zero allocation on the
/// fast path when no tabs or consecutive spaces occur.
fn normalize_ws(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut prev_space = false;
    let mut needs_norm = false;
    for &b in bytes {
        if b == b'\t' || (b == b' ' && prev_space) {
            needs_norm = true;
            break;
        }
        prev_space = b == b' ';
    }
    if !needs_norm {
        return Cow::Borrowed(text);
    }
    let mut out = String::new();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut copy_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                quote = None;
            }
            i += 1;
        } else if b == b' ' || b == b'\t' {
            let ws_start = i;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            if i - ws_start > 1 || b == b'\t' {
                if out.is_empty() {
                    out.reserve(text.len());
                }
                out.push_str(&text[copy_start..ws_start]);
                out.push(' ');
                copy_start = i;
            }
        } else {
            if b == b'"' || b == b'`' {
                quote = Some(b);
            } else if b == b'\'' {
                let apostrophe =
                    i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                let lifetime = i > 0
                    && matches!(bytes[i - 1], b'<' | b'&')
                    && bytes.get(i + 2) != Some(&b'\'');
                if !apostrophe && !lifetime {
                    quote = Some(b);
                }
            }
            i += 1;
        }
    }
    if copy_start == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copy_start..]);
    Cow::Owned(out)
}

/// Zero-allocation ASCII case-insensitive substring search (`needle_lower` must be
/// ASCII-lowercased).
fn find_ascii_ci(haystack: &str, needle_lower: &str) -> Option<usize> {
    let n = needle_lower.as_bytes();
    let h = haystack.as_bytes();
    if n.is_empty() {
        return Some(0);
    }
    if n.len() > h.len() {
        return None;
    }
    let first = n[0];
    let first_alt = if first.is_ascii_lowercase() {
        first.to_ascii_uppercase()
    } else {
        first
    };
    let tail = &n[1..];
    let limit = h.len() - n.len();
    let mut i = 0;
    while i <= limit {
        let b = h[i];
        if (b == first || b == first_alt)
            && h[i + 1..i + n.len()]
                .iter()
                .zip(tail)
                .all(|(hb, &nb)| hb.to_ascii_lowercase() == nb)
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

struct PatternSpec {
    pattern: String,
    fixed: bool,
    ignore_case: bool,
    smart_case: bool,
    word: bool,
}

enum CompiledPattern {
    Literal(String),
    AsciiLiteralCi(String),
    Regex(Regex),
}

impl CompiledPattern {
    fn find(&self, haystack: &str) -> Option<(usize, usize)> {
        match self {
            Self::Literal(lit) => haystack.find(lit.as_str()).map(|s| (s, s + lit.len())),
            Self::AsciiLiteralCi(lit) => {
                find_ascii_ci(haystack, lit.as_str()).map(|s| (s, s + lit.len()))
            }
            Self::Regex(re) => re.find(haystack).map(|m| (m.start(), m.end())),
        }
    }
}

#[derive(Default)]
struct PatternMatcher {
    pending: Vec<PatternSpec>,
    compiled: Vec<CompiledPattern>,
}

impl PatternMatcher {
    fn from_tool(tool: &[&str], parsed: &Parsed<'_>) -> Self {
        let mut matcher = Self::default();
        if parsed.has(&["-v", "--invert-match"]) {
            return matcher;
        }
        let program = tool.first().map_or("", |p| basename(p));
        let fixed = parsed.has(&["-F", "--fixed-strings", "-Q", "--literal"]) || program == "fgrep";
        let ignore_case = parsed.has(&["-i", "--ignore-case", "-y"]);
        let case_sensitive = parsed.has(&["-s", "--case-sensitive", "--no-ignore-case", "-I"]);
        let smart_case = !ignore_case
            && !case_sensitive
            && (parsed.has(&["-S", "--smart-case"]) || program == "ag");
        let word = parsed.has(&["-w", "--word-regexp"]);

        let (mut patterns, _) = operands(parsed);
        if patterns.is_empty() {
            patterns.extend(
                parsed
                    .seen
                    .iter()
                    .filter(|(flag, _)| *flag == "--match")
                    .filter_map(|(_, val)| *val),
            );
        }
        for pat in patterns.into_iter().take(16) {
            matcher.add(pat, fixed, ignore_case, smart_case, word);
        }
        matcher
    }

    fn add(&mut self, pattern: &str, fixed: bool, ignore_case: bool, smart_case: bool, word: bool) {
        if self.pending.len() + self.compiled.len() >= 16
            || pattern.is_empty()
            || pattern.len() > 2048
        {
            return;
        }
        self.pending.push(PatternSpec {
            pattern: pattern.to_owned(),
            fixed,
            ignore_case,
            smart_case,
            word,
        });
    }

    fn ensure_compiled(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        for spec in self.pending.drain(..) {
            if let Some(compiled) = compile_pattern(
                &spec.pattern,
                spec.fixed,
                spec.ignore_case,
                spec.smart_case,
                spec.word,
            ) {
                self.compiled.push(compiled);
            }
        }
    }

    /// The earliest match span `(start, end)` in `haystack`, optionally preferring
    /// a match at or after `from_byte`.
    fn find(&mut self, haystack: &str, from_byte: Option<usize>) -> Option<(usize, usize)> {
        self.ensure_compiled();
        if self.compiled.is_empty() {
            return None;
        }
        if let Some(at) = from_byte {
            let at = haystack.floor_char_boundary(at.min(haystack.len()));
            let after = self
                .compiled
                .iter()
                .filter_map(|pat| pat.find(&haystack[at..]).map(|(s, e)| (at + s, at + e)))
                .min_by_key(|&(s, _)| s);
            if after.is_some() {
                return after;
            }
        }
        self.compiled
            .iter()
            .filter_map(|pat| pat.find(haystack))
            .min_by_key(|&(s, _)| s)
    }
}

fn is_plain_literal(pattern: &str) -> bool {
    !pattern.bytes().any(|b| {
        matches!(
            b,
            b'\\'
                | b'.'
                | b'+'
                | b'*'
                | b'?'
                | b'('
                | b')'
                | b'|'
                | b'['
                | b']'
                | b'{'
                | b'}'
                | b'^'
                | b'$'
        )
    })
}

fn compile_pattern(
    pattern: &str,
    fixed: bool,
    ignore_case: bool,
    smart_case: bool,
    word: bool,
) -> Option<CompiledPattern> {
    if pattern.is_empty() || pattern.len() > 2048 {
        return None;
    }
    let ci = ignore_case || (smart_case && !pattern.chars().any(char::is_uppercase));
    if !word && (fixed || is_plain_literal(pattern)) {
        if !ci {
            return Some(CompiledPattern::Literal(pattern.to_owned()));
        }
        if pattern.is_ascii() {
            return Some(CompiledPattern::AsciiLiteralCi(
                pattern.to_ascii_lowercase(),
            ));
        }
    }
    let build = |expr: &str| {
        RegexBuilder::new(expr)
            .case_insensitive(ci)
            .size_limit(64 * 1024)
            .dfa_size_limit(64 * 1024)
            .build()
            .ok()
            .map(CompiledPattern::Regex)
    };
    let wrap = |expr: String| {
        if word {
            format!(r"\b(?:{expr})\b")
        } else {
            expr
        }
    };
    if fixed {
        return build(&wrap(regex::escape(pattern)));
    }
    if let Some(re) = build(&wrap(pattern.to_owned())) {
        return Some(re);
    }
    // BRE/PCRE2 fallback: match the longest literal token inside the pattern.
    let literal = longest_literal_token(pattern)?;
    build(&regex::escape(literal))
}

fn longest_literal_token(pattern: &str) -> Option<&str> {
    let bytes = pattern.as_bytes();
    let mut best: Option<&str> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            // Skip escape sequence (`\d`, `\w`, `\(`, etc.).
            i += 2;
            continue;
        }
        let start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'-' | b'/'))
        {
            i += 1;
        }
        if i > start {
            let token = &pattern[start..i];
            if token.len() >= 2 && best.is_none_or(|b| token.len() > b.len()) {
                best = Some(token);
            }
        } else {
            i += 1;
        }
    }
    best
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Loc {
    Range(usize, usize),
    LineCol(usize, usize),
}

struct MatchSlot {
    hash: u64,
    raw_len: usize,
    raw_exact: Option<String>,
    raw_sha: Option<[u8; 32]>,
    count: usize,
    /// Up to 2 exact original occurrences stored inline: `(seq, display_line, stdout_line, clipped)`.
    samples: [Option<(usize, String, usize, bool)>; 2],
    samples_len: u8,
    /// First file line location inline to avoid `Vec` allocation on single-hit / contiguous slots.
    first_loc: Option<Loc>,
    extra_locs: Vec<Loc>,
    last_line: Option<usize>,
    numbered: bool,
    /// Whether `raw_exact` was stored with a leading line-number prefix stripped.
    prefix_stripped: bool,
    /// Lazily populated windowed body for clipped (`> LINE_BYTES`) lines when folded.
    folded_body: Option<String>,
    first_col: Option<usize>,
    first_stdout: usize,
    first_seq: usize,
    clipped: bool,
}

impl MatchSlot {
    fn first_sample(&self) -> Option<&(usize, String, usize, bool)> {
        self.samples[0].as_ref()
    }

    fn samples(&self) -> impl Iterator<Item = &(usize, String, usize, bool)> {
        self.samples[..usize::from(self.samples_len)]
            .iter()
            .flatten()
    }

    fn push_sample(&mut self, sample: (usize, String, usize, bool)) {
        let idx = usize::from(self.samples_len);
        if idx < self.samples.len() {
            self.samples[idx] = Some(sample);
            self.samples_len += 1;
        }
    }

    fn clear_locs(&mut self) {
        self.first_loc = None;
        self.extra_locs.clear();
    }

    fn folded_body(&self) -> &str {
        self.folded_body
            .as_deref()
            .or(self.raw_exact.as_deref())
            .unwrap_or("")
    }

    fn matches_raw(&self, hash: u64, raw: &str) -> bool {
        if self.hash != hash || self.raw_len != raw.len() {
            return false;
        }
        if let Some(exact) = &self.raw_exact {
            return exact == raw;
        }
        self.raw_sha.is_some_and(|sha| sha256_bytes(raw) == sha)
    }

    fn same_key(&self, other: &Self) -> bool {
        if self.hash != other.hash || self.raw_len != other.raw_len {
            return false;
        }
        match (&self.raw_exact, &other.raw_exact) {
            (Some(a), Some(b)) => a == b,
            _ => self.raw_sha.is_some() && self.raw_sha == other.raw_sha,
        }
    }

    fn push_loc(&mut self, line: usize, col: Option<usize>, only_matching: bool) {
        if !self.numbered {
            return;
        }
        if let Some(prev) = self.last_line
            && (line < prev || (line == prev && !only_matching))
        {
            self.numbered = false;
            return;
        }
        self.last_line = Some(line);
        let loc_count = usize::from(self.first_loc.is_some()) + self.extra_locs.len();
        if let Some(c) = col {
            if loc_count < MAX_FOLDED_RANGES {
                if self.first_loc.is_none() {
                    self.first_loc = Some(Loc::LineCol(line, c));
                } else {
                    self.extra_locs.push(Loc::LineCol(line, c));
                }
            }
            return;
        }
        if let Some(Loc::Range(_, end)) = self.extra_locs.last_mut().or(self.first_loc.as_mut())
            && *end + 1 == line
        {
            *end = line;
            return;
        }
        if loc_count < MAX_FOLDED_RANGES {
            if self.first_loc.is_none() {
                self.first_loc = Some(Loc::Range(line, line));
            } else {
                self.extra_locs.push(Loc::Range(line, line));
            }
        }
    }

    fn write_folded(&self, out: &mut String, pointers: bool) {
        if self.numbered && self.first_loc.is_some() {
            out.push('L');
            write_locs(out, self.first_loc, &self.extra_locs, self.count);
            let body = self.folded_body();
            if body.is_empty() {
                out.push(':');
            } else {
                out.push_str(": ");
                out.push_str(body);
            }
        } else {
            // Unnumbered (or duplicate-line-number) fallback: if a leading number was
            // stripped tentatively and later rejected, keep the exact original text.
            let body = if self.prefix_stripped {
                self.first_sample()
                    .map_or(self.folded_body(), |(_, s, _, _)| s.trim())
            } else {
                self.folded_body()
            };
            if body.is_empty() {
                let _ = write!(out, "×{}:", self.count);
            } else {
                let _ = write!(out, "×{}: {body}", self.count);
            }
        }
        if pointers && self.clipped {
            let _ = write!(out, " [stdout:{}]", self.first_stdout);
        }
    }
}

fn hash64(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn sha256_bytes(text: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.finalize().into()
}

fn write_locs(out: &mut String, first_loc: Option<Loc>, extra_locs: &[Loc], total_count: usize) {
    let start_len = out.len();
    let mut covered = 0usize;
    for (i, &loc) in first_loc.iter().chain(extra_locs.iter()).enumerate() {
        let before_len = out.len();
        if i > 0 {
            out.push(',');
        }
        let span = match loc {
            Loc::Range(a, b) if a == b => {
                let _ = write!(out, "{a}");
                1
            }
            Loc::Range(a, b) if b == a + 1 => {
                let _ = write!(out, "{a},{b}");
                2
            }
            Loc::Range(a, b) => {
                let _ = write!(out, "{a}-{b}");
                b - a + 1
            }
            Loc::LineCol(l, c) => {
                let _ = write!(out, "{l}:{c}");
                1
            }
        };
        if i >= 3 && out.len() - start_len > MAX_LOC_STR_BYTES {
            out.truncate(before_len);
            break;
        }
        covered += span;
    }
    if covered < total_count {
        let _ = write!(out, " (+{} more)", total_count - covered);
    }
}

struct Group {
    path: Arc<str>,
    matches: usize,
    first: usize,
    last: usize,
    /// In block mode, the first block's lines.
    shown: Vec<String>,
    /// In normal search mode, distinct match slots supporting intra-file folding
    /// and match-centered long-line windows.
    slots: Vec<MatchSlot>,
    /// Next sequence number for preserving original occurrence order within `slots`.
    next_seq: usize,
    /// Last parsed file line number when auto-detecting numbered output.
    last_file_line: Option<usize>,
    /// Indexes into `Blocks::list`.
    blocks: Vec<usize>,
    /// The first match's place in `shown`, which keeps only the nearest lines
    /// before it.
    lead: Option<usize>,
}

impl Group {
    /// When every match in this file has the exact same normalized body (captured in slot 0),
    /// return that slot. Files with any other distinct match return `None`.
    fn single_body(&self) -> Option<&MatchSlot> {
        (self.slots.len() == 1 && self.slots[0].count == self.matches).then_some(&self.slots[0])
    }

    /// Write up to `k` indented display lines for this file directly into `out`,
    /// returning the total number of matches those lines represent.
    fn write_lines(&self, out: &mut String, k: usize, pointers: bool) -> usize {
        if k == 0 || self.slots.is_empty() {
            return 0;
        }
        if let [slot] = self.slots.as_slice() {
            if slot.count >= INTRA_FOLD_MIN {
                out.push_str("  ");
                slot.write_folded(out, pointers);
                out.push('\n');
                return slot.count;
            }
            let mut represented = 0usize;
            for &(_, ref display, stdout_line, clipped) in slot.samples().take(k) {
                out.push_str("  ");
                out.push_str(display);
                if pointers && clipped {
                    let _ = write!(out, " [stdout:{stdout_line}]");
                }
                out.push('\n');
                represented += 1;
            }
            return represented;
        }
        // `(seq, slot_idx, sample_idx)` where `sample_idx == u8::MAX` means folded slot.
        let mut items: Vec<(usize, u8, u8)> = Vec::with_capacity(self.slots.len() * 2);
        for (slot_idx, slot) in self.slots.iter().enumerate() {
            if slot.count >= INTRA_FOLD_MIN {
                items.push((slot.first_seq, slot_idx as u8, u8::MAX));
            } else {
                for (sample_idx, &(seq, _, _, _)) in slot.samples().enumerate() {
                    items.push((seq, slot_idx as u8, sample_idx as u8));
                }
            }
        }
        items.sort_unstable_by_key(|&(seq, _, _)| seq);
        let mut represented = 0usize;
        for &(_, slot_idx, sample_idx) in items.iter().take(k) {
            let slot = &self.slots[usize::from(slot_idx)];
            out.push_str("  ");
            if sample_idx == u8::MAX {
                slot.write_folded(out, pointers);
                represented += slot.count;
            } else if let Some((_, display, stdout_line, clipped)) =
                &slot.samples[usize::from(sample_idx)]
            {
                out.push_str(display);
                if pointers && *clipped {
                    let _ = write!(out, " [stdout:{stdout_line}]");
                }
                represented += 1;
            }
            out.push('\n');
        }
        represented
    }

    /// Write a compact inline citation for a pure-boilerplate follower file into `out`.
    fn write_follower_ref(&self, out: &mut String, hoist: &PrefixHoist, pointers: bool) {
        hoist.write_display_path(out, &self.path);
        let Some(slot) = self.single_body() else {
            let _ = write!(out, " ×{}", self.matches);
            write_pointer(out, pointers, self);
            return;
        };
        if self.matches == 1 {
            if slot.numbered
                && let Some(loc) = slot.first_loc
            {
                match loc {
                    Loc::Range(l, _) => {
                        let _ = write!(out, ":{l}");
                    }
                    Loc::LineCol(l, c) => {
                        let _ = write!(out, ":{l}:{c}");
                    }
                }
            }
            write_pointer(out, pointers, self);
            return;
        }
        if slot.numbered && slot.first_loc.is_some() {
            let _ = write!(out, " ×{} L", self.matches);
            write_locs(out, slot.first_loc, &slot.extra_locs, slot.count);
        } else {
            let _ = write!(out, " ×{}", self.matches);
        }
        write_pointer(out, pointers, self);
    }
}

/// Lines a block report shows of each file's first block.
const BLOCK_LINES: usize = 40;
/// Lines read before a block's file is known: its leading context.
const PENDING: usize = 4096;

/// One context block: contiguous lines of one file between `--` separators.
struct Block {
    group: usize,
    /// Its first and last stdout lines.
    start: usize,
    end: usize,
    /// Its first and last file lines, when lines carry numbers.
    from: Option<usize>,
    to: Option<usize>,
}

/// Block mode, for searches that print lines around their matches. Every line
/// must be a match, a context line, a `--` separator or a heading, or the report
/// falls back to the ordinary preview.
struct Blocks {
    /// Lines carry their numbers, as with `-n`.
    numbered: bool,
    /// The one file the command names, whose lines may carry no file name.
    bare: Option<String>,
    /// rg `--heading`: each file's name on its own line before its lines.
    heading: bool,
    /// The file that unprefixed lines belong to, from `bare` or a heading.
    section: Option<usize>,
    list: Vec<Block>,
    open: Option<usize>,
    /// Lines before a block's first match, whose file name is not known yet.
    pending: Vec<(usize, String)>,
    failed: bool,
}

/// Groups search output by file while the stream is scanned.
pub struct Search {
    base: PathBuf,
    probe: PathBuf,
    keep: Vec<String>,
    groups: Vec<Group>,
    index: HashMap<Arc<str>, usize>,
    current: Option<usize>,
    pending: Vec<(usize, String)>,
    other: Option<(usize, String)>,
    unparsed: usize,
    records: usize,
    matches: usize,
    context: usize,
    exists: HashMap<Arc<str>, bool>,
    checks: usize,
    overflow: bool,
    blocks: Option<Blocks>,
    line_mode: LineMode,
    matcher: PatternMatcher,
}

impl Search {
    /// `base` is the directory relative paths in the output resolve against.
    /// Dependency and build directories named in `words` are not collapsed.
    pub fn new(base: &Path, words: &[&str]) -> Self {
        let unwrapped = crate::wrappers::hook_effective(words);
        let eff_refs: Vec<&str> = match &unwrapped {
            Some(eff) => eff.iter().map(String::as_str).collect(),
            None => words.to_vec(),
        };
        let tool = crate::filters::unwrap_tool(&eff_refs);
        let parsed_search = searched(tool);
        let line_mode = parsed_search
            .as_ref()
            .map(|(_, parsed)| {
                let program = basename(tool[0]);
                let no_num = parsed.has(&["-N", "--no-line-number"]);
                let has_num = !no_num
                    && (parsed.has(&["-n", "--line-number", "--vimgrep", "--numbers"])
                        || matches!(program, "ag" | "ack" | "ack-grep"));
                // `grep` and `git grep` only print line numbers when `-n`/`--line-number`
                // is explicitly passed; without it, leading `8080:` is file content.
                let never_implicit_num = matches!(program, "grep" | "egrep" | "fgrep" | "git");
                LineMode {
                    explicit_numbered: if no_num || (!has_num && never_implicit_num) {
                        Some(false)
                    } else if has_num {
                        Some(true)
                    } else {
                        None
                    },
                    has_col: parsed.has(&["--column", "--vimgrep"]),
                    only_matching: parsed.has(&["-o", "--only-matching"]),
                }
            })
            .unwrap_or_default();
        let matcher = parsed_search
            .as_ref()
            .map(|(_, parsed)| PatternMatcher::from_tool(tool, parsed))
            .unwrap_or_default();
        let blocks = parsed_search
            .filter(|(_, parsed)| parsed.has(CONTEXT))
            .map(|(_, parsed)| {
                let program = basename(tool[0]);
                let numbered = parsed.has(&["-n", "--line-number"])
                    && !parsed.has(&["-N", "--no-line-number"]);
                let paths = operands(&parsed).1;
                let bare = (program != "git"
                    && paths.len() == 1
                    && !parsed.has(&["-H", "--with-filename"])
                    && base.join(paths[0]).is_file())
                .then(|| paths[0].to_owned());
                let heading = parsed.has(&["--heading"]) && !parsed.has(&["--no-heading"]);
                Blocks {
                    numbered,
                    bare,
                    heading,
                    section: None,
                    list: Vec::new(),
                    open: None,
                    pending: Vec::new(),
                    // ag and ack print other layouts, and lines without numbers
                    // under a heading cannot be told apart from file names.
                    failed: matches!(program, "ag" | "ack" | "ack-grep") || heading && !numbered,
                }
            });
        Self {
            base: base.to_path_buf(),
            probe: PathBuf::with_capacity(base.as_os_str().len() + 64),
            keep: crate::listing::named_noise(words),
            groups: Vec::new(),
            index: HashMap::new(),
            current: None,
            pending: Vec::new(),
            other: None,
            unparsed: 0,
            records: 0,
            matches: 0,
            context: 0,
            exists: HashMap::new(),
            checks: 0,
            overflow: false,
            blocks,
            line_mode,
            matcher,
        }
    }

    pub fn observe(&mut self, text: &str, n: usize) {
        if self.overflow {
            return;
        }
        let clean = strip_ansi_fast(text);
        let line = clean.trim_end_matches(['\n', '\r']);
        if self.blocks.is_some() {
            self.block_line(line, n);
            return;
        }
        if line.is_empty() {
            return;
        }
        if line == "--" {
            self.drop_pending();
            return;
        }
        self.records += 1;
        // BSD and older GNU grep report a matching binary file on stdout.
        if let Some(path) = line
            .strip_prefix("Binary file ")
            .and_then(|rest| rest.strip_suffix(" matches"))
        {
            if let Some(c) = self.group(path, n) {
                self.hit(c, "(binary file matches)", n);
            }
            return;
        }
        if let Some(c) = self.current
            && let Some(rest) = line.strip_prefix(&*self.groups[c].path)
        {
            if let Some(text) = rest.strip_prefix(':') {
                self.hit(c, text, n);
                return;
            }
            // `path-` starts a context line, unless the line is a match from another
            // file whose name extends this one, such as `settings-prod:3:…` after
            // `settings`. Prefer the longer path when it names a real file.
            if rest.starts_with('-') {
                let longer = self
                    .split(line)
                    .filter(|&end| end > self.groups[c].path.len());
                if longer.is_none() {
                    self.context_line(c, n);
                    return;
                }
            }
        }
        if let Some(end) = self.split(line) {
            let path = &line[..end];
            let Some(c) = self.group(path, n) else {
                return;
            };
            // Lines printed just before a file's first match are its leading context.
            if !self.pending.is_empty() {
                for (m, pending) in std::mem::take(&mut self.pending) {
                    if pending
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('-'))
                    {
                        self.context_line(c, m);
                    } else {
                        self.unparsed_line(m, pending);
                    }
                }
            }
            self.hit(c, &line[end + 1..], n);
            return;
        }
        if self.pending.len() >= 64 {
            self.drop_pending();
        }
        self.pending.push((n, line.to_owned()));
    }

    fn group(&mut self, path: &str, n: usize) -> Option<usize> {
        if let Some(&c) = self.index.get(path) {
            return Some(c);
        }
        if self.groups.len() >= MAX_FILES {
            self.overflow = true;
            return None;
        }
        let shared: Arc<str> = match self.exists.remove_entry(path) {
            Some((key, _)) => key,
            None => Arc::from(path),
        };
        let idx = self.groups.len();
        self.index.insert(Arc::clone(&shared), idx);
        self.groups.push(Group {
            path: shared,
            matches: 0,
            first: n,
            last: n,
            shown: Vec::new(),
            slots: Vec::new(),
            next_seq: 0,
            last_file_line: None,
            blocks: Vec::new(),
            lead: None,
        });
        Some(idx)
    }

    fn hit(&mut self, c: usize, text: &str, n: usize) {
        let line_mode = self.line_mode;
        let (parsed_line, col, prefix_str, content) = split_line_prefix(text, line_mode);
        let group = &mut self.groups[c];
        // When `-n` was not explicitly requested, accept `<digits>:` as a line number
        // only while numbers stay strictly increasing (or non-decreasing with `-o`),
        // so unnumbered key-value files like `8080: foo` never lose their key.
        let (line_no, col, prefix_str, content) = match (line_mode.explicit_numbered, parsed_line) {
            (Some(true), Some(l)) => (Some(l), col, prefix_str, content),
            (None, Some(l))
                if group
                    .last_file_line
                    .is_none_or(|prev| l > prev || (line_mode.only_matching && l >= prev)) =>
            {
                group.last_file_line = Some(l);
                (Some(l), col, prefix_str, content)
            }
            (None, _) => {
                // Non-increasing or unnumbered line in auto-detect mode: demote any
                // earlier tentative numbered slots in this group to full-line keys so
                // identical lines such as `8080: foo` still fold together accurately.
                for slot in &mut group.slots {
                    if slot.numbered
                        && let Some((_, sample, _, _)) = slot.first_sample()
                    {
                        let full = normalize_ws(sample.trim()).into_owned();
                        slot.hash = hash64(&full);
                        slot.raw_len = full.len();
                        if full.len() <= INLINE_KEY_BYTES {
                            slot.raw_exact = Some(full);
                            slot.raw_sha = None;
                            slot.folded_body = None;
                        } else {
                            slot.raw_sha = Some(sha256_bytes(&full));
                            slot.raw_exact = None;
                            slot.folded_body = Some(full);
                        }
                        slot.numbered = false;
                        slot.prefix_stripped = false;
                        slot.clear_locs();
                    }
                }
                (None, None, "", text)
            }
            _ => (None, None, "", text),
        };
        group.matches += 1;
        group.first = group.first.min(n);
        group.last = group.last.max(n);
        let seq = group.next_seq;
        group.next_seq += 1;

        let norm_key = normalize_ws(content.trim());
        let key_str = norm_key.as_ref();
        let hash = hash64(key_str);
        if let Some(slot) = group
            .slots
            .iter_mut()
            .find(|slot| slot.matches_raw(hash, key_str))
        {
            slot.count += 1;
            if let Some(l) = line_no {
                slot.push_loc(l, col, line_mode.only_matching);
            } else {
                slot.numbered = false;
            }
            if usize::from(slot.samples_len) < INTRA_FOLD_MIN - 1 {
                let (display, clipped) =
                    window_line(text, prefix_str, content, col, &mut self.matcher);
                slot.push_sample((seq, display, n, clipped));
            }
            if slot.clipped && slot.count == INTRA_FOLD_MIN && slot.folded_body.is_none() {
                slot.folded_body = Some(window_snippet(
                    key_str,
                    slot.first_col.or(col),
                    &mut self.matcher,
                    WINDOW_SNIPPET_BYTES,
                ));
            }
        } else if group.slots.len() < MAX_SLOTS {
            let (display, clipped) = window_line(text, prefix_str, content, col, &mut self.matcher);
            let key_len = key_str.len();
            let (raw_exact, raw_sha) = if key_len <= INLINE_KEY_BYTES {
                (Some(norm_key.into_owned()), None)
            } else {
                (None, Some(sha256_bytes(key_str)))
            };
            let mut slot = MatchSlot {
                hash,
                raw_len: key_len,
                raw_exact,
                raw_sha,
                count: 1,
                samples: [Some((seq, display, n, clipped)), None],
                samples_len: 1,
                first_loc: None,
                extra_locs: Vec::new(),
                last_line: None,
                numbered: line_no.is_some(),
                prefix_stripped: line_no.is_some(),
                folded_body: None,
                first_col: col,
                first_stdout: n,
                first_seq: seq,
                clipped,
            };
            if let Some(l) = line_no {
                slot.push_loc(l, col, line_mode.only_matching);
            }
            group.slots.push(slot);
        }
        self.matches += 1;
        self.current = Some(c);
    }

    fn context_line(&mut self, c: usize, n: usize) {
        let group = &mut self.groups[c];
        group.first = group.first.min(n);
        group.last = group.last.max(n);
        self.context += 1;
    }

    fn unparsed_line(&mut self, n: usize, line: String) {
        self.unparsed += 1;
        self.other.get_or_insert((n, line));
    }

    fn drop_pending(&mut self) {
        for (n, line) in std::mem::take(&mut self.pending) {
            self.unparsed_line(n, line);
        }
    }

    /// The end of the leading path, trying each of the first few colons in turn.
    fn split(&mut self, line: &str) -> Option<usize> {
        self.path_end(line, ':', false)
    }

    /// The end of a leading path that `sep` follows, trying each of the first few
    /// in turn. `numbered` requires a line number and the same `sep` after it.
    fn path_end(&mut self, line: &str, sep: char, numbered: bool) -> Option<usize> {
        let tries = if sep == ':' { 6 } else { 16 };
        for (i, _) in line.match_indices(sep).take(tries) {
            let candidate = &line[..i];
            if candidate.is_empty()
                || numbered && number(&line[i + 1..]).is_none_or(|(_, kind)| kind != sep)
            {
                continue;
            }
            if self.known(candidate) {
                return Some(i);
            }
            if self.overflow {
                return None;
            }
        }
        None
    }

    /// Whether `candidate` names a file already grouped or an existing file.
    fn known(&mut self, candidate: &str) -> bool {
        if self.index.contains_key(candidate) {
            return true;
        }
        if let Some(&found) = self.exists.get(candidate) {
            return found;
        }
        if self.checks >= MAX_CHECKS {
            self.overflow = true;
            return false;
        }
        self.checks += 1;
        self.probe.clear();
        self.probe.push(&self.base);
        self.probe.push(candidate);
        let found = self.probe.is_file();
        self.exists.insert(Arc::from(candidate), found);
        found
    }

    fn mode(&mut self) -> &mut Blocks {
        self.blocks.as_mut().expect("block mode")
    }

    /// Read one line of a search that prints context.
    fn block_line(&mut self, line: &str, n: usize) {
        if self.mode().failed {
            return;
        }
        if line == "--" {
            self.close_block();
            return;
        }
        // The first line shows whether the one named file's lines carry its name,
        // as ugrep prints them with `-r`.
        if let Some(path) = self.mode().bare.take()
            && !line
                .strip_prefix(path.as_str())
                .is_some_and(|rest| rest.starts_with([':', '-']))
        {
            let Some(c) = self.group(&path, n) else {
                return;
            };
            let mode = self.mode();
            mode.section = Some(c);
            // Without numbers, its matches, context lines and separators look alike.
            mode.failed |= !mode.numbered;
        }
        let mode = self.mode();
        if mode.heading {
            if line.is_empty() {
                self.close_block();
                self.mode().section = None;
                return;
            }
            if mode.section.is_none() {
                match self.known(line).then(|| self.group(line, n)).flatten() {
                    Some(c) => self.mode().section = Some(c),
                    None => self.mode().failed = true,
                }
                return;
            }
        }
        if let Some(c) = self.mode().section {
            match number(line) {
                Some((number, sep)) => self.add_line(c, n, Some(number), sep == ':', line),
                None => self.mode().failed = true,
            }
            return;
        }
        if let Some(path) = binary(line)
            && self.known(path)
        {
            self.close_block();
            if let Some(c) = self.group(path, n) {
                self.add_line(c, n, None, true, "(binary file matches)");
            }
            self.close_block();
            return;
        }
        self.prefixed_line(line, n);
    }

    /// Read a line that starts with its file name: `path:…` for a match and
    /// `path-…` for context.
    fn prefixed_line(&mut self, line: &str, n: usize) {
        let numbered = self.mode().numbered;
        if let Some(open) = self.mode().open {
            let c = self.mode().list[open].group;
            let len = self.groups[c].path.len();
            if line.starts_with(&*self.groups[c].path)
                && let Some((matched, number, text)) = parts(line, len, numbered)
            {
                // Without numbers, `settings-prod:3` or `settings-prod-3` after
                // `settings` may name the longer file. A real one wins.
                let longer = !numbered
                    && [':', '-'].into_iter().any(|sep| {
                        line[len + 1..]
                            .match_indices(sep)
                            .take(4)
                            .any(|(i, _)| self.known(&line[..len + 1 + i]))
                    });
                if !longer {
                    self.add_line(c, n, number, matched, text);
                    return;
                }
            }
            // Another file's line starts a new block: ugrep prints no `--` there.
            self.close_block();
        }
        if let Some(end) = self.path_end(line, ':', numbered) {
            let path = &line[..end];
            let Some(c) = self.group(path, n) else {
                return;
            };
            // Lines printed before a file's match are its leading context.
            if !self.mode().pending.is_empty() {
                for (m, before) in std::mem::take(&mut self.mode().pending) {
                    match parts(&before, end, numbered).filter(|(matched, ..)| !matched) {
                        Some((_, number, text)) if before.starts_with(path) => {
                            self.add_line(c, m, number, false, text)
                        }
                        _ => self.mode().failed = true,
                    }
                }
            }
            if let Some((_, number, text)) = parts(line, end, numbered) {
                self.add_line(c, n, number, true, text);
            }
            return;
        }
        let mode = self.mode();
        mode.failed |= mode.pending.len() >= PENDING;
        mode.pending.push((n, line.to_owned()));
    }

    /// End the open block. Context lines that no match followed belong to a block
    /// cut short, as by `head`, and name their file before a dash.
    fn close_block(&mut self) {
        let mode = self.mode();
        mode.open = None;
        let pending = std::mem::take(&mut mode.pending);
        let numbered = mode.numbered;
        let Some((first, line)) = pending.first() else {
            return;
        };
        // A name that a longer real name also fits, such as `a` and `a-1` before
        // `a-1-2-text`, leaves the file unknown.
        let end = self.path_end(line, '-', numbered).filter(|&end| {
            !line[end + 1..].match_indices('-').take(16).any(|(i, _)| {
                let longer = end + 1 + i;
                (!numbered || number(&line[longer + 1..]).is_some_and(|(_, sep)| sep == '-'))
                    && self.known(&line[..longer])
            })
        });
        let Some(end) = end else {
            self.mode().failed = true;
            return;
        };
        let path = &line[..end];
        let Some(c) = self.group(path, *first) else {
            return;
        };
        for (m, line) in &pending {
            match parts(line, end, numbered) {
                Some((false, number, text)) if line.starts_with(path) => {
                    self.add_line(c, *m, number, false, text)
                }
                _ => self.mode().failed = true,
            }
        }
        self.mode().open = None;
    }

    /// Add a line to the open block of file `c`, or start one. A block holds
    /// consecutive lines, so a gap in line numbers starts another, as `-A0`
    /// prints no `--`.
    fn add_line(&mut self, c: usize, n: usize, number: Option<usize>, matched: bool, text: &str) {
        let mode = self.blocks.as_mut().expect("block mode");
        if let Some(open) = mode.open {
            let block = &mode.list[open];
            if block.group != c || number.is_some() && block.to.map(|to| to + 1) != number {
                mode.open = None;
            }
        }
        let open = *mode.open.get_or_insert_with(|| {
            // A file's lines start at its first block, after any heading.
            if self.groups[c].blocks.is_empty() {
                self.groups[c].first = n;
            }
            mode.list.push(Block {
                group: c,
                start: n,
                end: n,
                from: number,
                to: number,
            });
            self.groups[c].blocks.push(mode.list.len() - 1);
            mode.list.len() - 1
        });
        let block = &mut mode.list[open];
        block.end = n;
        block.to = number;
        let group = &mut self.groups[c];
        group.first = group.first.min(n);
        group.last = group.last.max(n);
        if group.blocks[0] == open {
            match group.lead {
                None if matched => group.lead = Some(group.shown.len()),
                None if group.shown.len() >= BLOCK_LINES / 2 => {
                    group.shown.remove(0);
                }
                _ => {}
            }
            if group.shown.len() < BLOCK_LINES {
                let line = if matched && text.len() > LINE_BYTES {
                    let (_, col, prefix_str, content) = split_line_prefix(text, self.line_mode);
                    window_line(text, prefix_str, content, col, &mut self.matcher).0
                } else {
                    clip(text)
                };
                group.shown.push(line);
            }
        }
        group.matches += usize::from(matched);
        self.matches += usize::from(matched);
        self.context += usize::from(!matched);
        self.records += 1;
    }

    /// The grouped report within `budget` bytes and the comparison record, or
    /// `None` when the output was not recognizable search output. `pointers` adds
    /// the original stdout lines of each file.
    pub fn finish(
        mut self,
        label: &str,
        budget: usize,
        previous: Option<(&str, &Summary)>,
        pointers: bool,
    ) -> Option<(String, Summary)> {
        self.drop_pending();
        if self.blocks.is_some() {
            self.close_block();
            // A file named with no lines after it, such as a heading before a blank
            // line or at the end, is not a form a search prints.
            if self.mode().failed
                || self.matches == 0
                || self.groups.iter().any(|g| g.blocks.is_empty())
            {
                return None;
            }
        }
        if self.overflow
            || (self.matches == 0 && self.records > 0)
            || self.unparsed > (self.records / 10).max(16)
        {
            return None;
        }
        let mut order: Vec<&Group> = self.groups.iter().collect();
        order.sort_by(|a, b| b.matches.cmp(&a.matches).then(a.path.cmp(&b.path)));
        let files = order.len();
        let mut header = format!(
            "{label}: {} in {}",
            plural(self.matches, "match", "matches"),
            plural(files, "file", "files")
        );
        if let Some(blocks) = &self.blocks {
            let _ = write!(header, ", {}", plural(blocks.list.len(), "block", "blocks"));
        }
        if self.context > 0 {
            let _ = write!(header, ", {} context lines", self.context);
        }
        let summary = Summary {
            kind: "search".into(),
            outcomes: vec![header.clone()],
            items: order
                .iter()
                .take(STORED)
                .map(|g| {
                    let mut item = Item::new(&g.path, "", g.first);
                    item.digest = g.matches.to_string();
                    item.last = g.last;
                    item
                })
                .collect(),
            ..Summary::default()
        };
        let current = self.groups.iter().map(|g| (g.path.as_ref(), g.matches));
        let compared = compare(previous, "search", current, "no longer matched");
        // Matches inside dependency and build directories collapse into one line each.
        let mut collapsed: Vec<(&str, usize, usize, usize, usize)> = Vec::new();
        order.retain(|g| {
            let Some(prefix) = crate::listing::noise_prefix(&g.path, &self.keep) else {
                return true;
            };
            match collapsed.iter_mut().find(|c| c.0 == prefix) {
                Some(c) => {
                    c.1 += 1;
                    c.2 += g.matches;
                    c.3 = c.3.min(g.first);
                    c.4 = c.4.max(g.last);
                }
                None => collapsed.push((prefix, 1, g.matches, g.first, g.last)),
            }
            false
        });
        collapsed.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(b.0)));
        let mut other = String::new();
        for (prefix, files, matches, first, last) in &collapsed {
            let _ = write!(
                other,
                "{prefix}/ {} in {} collapsed",
                plural(*matches, "match", "matches"),
                plural(*files, "file", "files"),
            );
            if pointers {
                let _ = write!(other, " [stdout:{first}-{last}]");
            }
            other.push('\n');
        }
        if let Some((n, line)) = &self.other {
            let _ = write!(
                other,
                "{} other line(s), first: {}",
                self.unparsed,
                clip(line)
            );
            if pointers {
                let _ = write!(other, " [stdout:{n}]");
            }
            other.push('\n');
        }

        let hoist = choose_prefix_hoist(&order, !collapsed.is_empty());
        let build_report = |hoist: &PrefixHoist| -> (String, (usize, usize)) {
            let mut full_header =
                String::with_capacity(header.len() + compared.len() + hoist.header_extra_len() + 1);
            full_header.push_str(&header);
            hoist.write_header_suffix(&mut full_header);
            full_header.push('\n');
            full_header.push_str(&compared);
            let room = budget.saturating_sub(full_header.len() + other.len() + 96);
            let (body, tier) = if let Some(blocks) = &self.blocks {
                block_body(&order, &blocks.list, room, pointers, hoist)
            } else {
                search_body(&order, room, pointers, hoist)
            };
            let mut report = String::with_capacity(full_header.len() + body.len() + other.len());
            report.push_str(&full_header);
            report.push_str(&body);
            report.push_str(&other);
            (report, tier)
        };
        let (report, hoist_tier) = build_report(&hoist);
        // Full prefix hoisting that fits all files at a full-file detail tier is strictly
        // smaller than plain rendering because `(order.len() - 1) * prefix.len() > 8`.
        let guaranteed_smaller = matches!(hoist, PrefixHoist::Full(_))
            && hoist_tier.0 > 0
            && hoist_tier.1 == order.len();
        if !matches!(hoist, PrefixHoist::None) && !guaranteed_smaller {
            let (plain, plain_tier) = build_report(&PrefixHoist::None);
            if plain_tier > hoist_tier || (plain_tier == hoist_tier && plain.len() <= report.len())
            {
                return Some((plain, summary));
            }
        }
        Some((report, summary))
    }
}

/// Prefix hoisted into the search header when matching files share a deep directory.
enum PrefixHoist<'a> {
    None,
    /// Every displayed file starts with `prefix` (ending with `/`). Header appends
    /// `, under {prefix}` and paths render relative to it.
    Full(&'a str),
    /// A majority of displayed files share `prefix` (ending with `/`), while other
    /// files or collapsed trees sit outside it. Header appends ` (… = {prefix})`
    /// and matching paths render as `…/{rel}` so non-matching paths stay unambiguous.
    Partial(&'a str),
}

impl PrefixHoist<'_> {
    fn header_extra_len(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Full(prefix) => ", under ".len() + prefix.len(),
            Self::Partial(prefix) => " (… = )".len() + prefix.len(),
        }
    }

    fn write_header_suffix(&self, out: &mut String) {
        match self {
            Self::None => {}
            Self::Full(prefix) => {
                out.push_str(", under ");
                out.push_str(prefix);
            }
            Self::Partial(prefix) => {
                out.push_str(" (… = ");
                out.push_str(prefix);
                out.push(')');
            }
        }
    }

    fn write_display_path(&self, out: &mut String, path: &str) {
        match self {
            Self::None => out.push_str(path),
            Self::Full(prefix) => out.push_str(path.strip_prefix(prefix).unwrap_or(path)),
            Self::Partial(prefix) => match path.strip_prefix(prefix) {
                Some(rel) => {
                    out.push_str("…/");
                    out.push_str(rel);
                }
                None => out.push_str(path),
            },
        }
    }

    fn display_folder<'a>(&self, path: &'a str) -> Cow<'a, str> {
        match self {
            Self::None => Cow::Borrowed(path.rsplit_once('/').map_or(".", |(dir, _)| dir)),
            Self::Full(prefix) => {
                let rel = path.strip_prefix(prefix).unwrap_or(path);
                Cow::Borrowed(rel.rsplit_once('/').map_or(".", |(dir, _)| dir))
            }
            Self::Partial(prefix) => match path.strip_prefix(prefix) {
                Some(rel) => match rel.rsplit_once('/') {
                    Some((dir, _)) => Cow::Owned(format!("…/{dir}")),
                    None => Cow::Borrowed("…"),
                },
                None => Cow::Borrowed(path.rsplit_once('/').map_or(".", |(dir, _)| dir)),
            },
        }
    }
}

fn real_dir_depth(prefix: &str) -> usize {
    prefix
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .count()
}

fn common_dir_prefix<'a>(mut paths: impl Iterator<Item = &'a str>) -> &'a str {
    fn dir_of(p: &str) -> &str {
        p.rfind('/').map_or("", |i| &p[..=i])
    }
    let Some(first) = paths.next() else {
        return "";
    };
    let mut common = dir_of(first);
    for path in paths {
        let dir = dir_of(path);
        let shared = common
            .bytes()
            .zip(dir.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        let cut = common[..common.floor_char_boundary(shared)]
            .rfind('/')
            .map_or(0, |i| i + 1);
        common = &common[..cut];
        if common.is_empty() {
            break;
        }
    }
    common
}

fn choose_prefix_hoist<'a>(order: &[&'a Group], has_collapsed: bool) -> PrefixHoist<'a> {
    if order.len() < 2 {
        return PrefixHoist::None;
    }
    if !has_collapsed {
        let common = common_dir_prefix(order.iter().map(|g| g.path.as_ref()));
        // Require at least 2 real directory components (ignoring leading `./`) so shallow
        // roots like `src/` or `./src/` stay inline, and verify net byte savings.
        if real_dir_depth(common) >= 2
            && common.len() >= 10
            && (order.len() - 1) * common.len() > ", under ".len()
        {
            return PrefixHoist::Full(common);
        }
    }
    if order.len() < 3 {
        return PrefixHoist::None;
    }
    let mut counts: HashMap<&'a str, usize> = HashMap::with_capacity(order.len());
    for group in order {
        let path = group.path.as_ref();
        let mut depth = 0usize;
        let mut seg_start = 0usize;
        for (i, _) in path.match_indices('/') {
            let seg = &path[seg_start..i];
            if !seg.is_empty() && seg != "." {
                depth += 1;
            }
            seg_start = i + 1;
            if depth >= 2 && i + 1 >= 12 {
                *counts.entry(&path[..=i]).or_default() += 1;
            }
        }
    }
    let marker_len = "…/".len();
    let header_fixed = " (… = )".len();
    let mut best: Option<(&'a str, usize)> = None;
    for (prefix, count) in counts {
        if count < 3 || count * 2 < order.len() {
            continue;
        }
        let gross = count.saturating_mul(prefix.len().saturating_sub(marker_len));
        let cost = header_fixed + prefix.len();
        let savings = gross.saturating_sub(cost);
        if savings < 24 {
            continue;
        }
        let replace = match best {
            None => true,
            Some((best_prefix, best_savings)) => {
                savings > best_savings
                    || (savings == best_savings && prefix.len() > best_prefix.len())
                    || (savings == best_savings
                        && prefix.len() == best_prefix.len()
                        && prefix < best_prefix)
            }
        };
        if replace {
            best = Some((prefix, savings));
        }
    }
    best.map_or(PrefixHoist::None, |(prefix, _)| {
        PrefixHoist::Partial(prefix)
    })
}

enum DisplayUnit {
    Single(usize),
    Boilerplate {
        leader: usize,
        followers: Vec<usize>,
    },
}

impl DisplayUnit {
    fn push_group_indices(&self, out: &mut Vec<usize>) {
        match self {
            Self::Single(i) => out.push(*i),
            Self::Boilerplate { leader, followers } => {
                out.push(*leader);
                out.extend_from_slice(followers);
            }
        }
    }
}

fn build_display_units(
    order: &[&Group],
    pointers: bool,
    hoist: &PrefixHoist<'_>,
) -> Vec<DisplayUnit> {
    // Group pure single-body files sharing the exact same normalized line of sufficient length.
    let mut buckets: Vec<Vec<usize>> = Vec::new();
    let mut by_hash: HashMap<(u64, usize), Vec<usize>> = HashMap::new();
    for (idx, group) in order.iter().enumerate() {
        let Some(slot) = group.single_body() else {
            continue;
        };
        if slot.clipped || slot.raw_len < CROSS_DEDUP_MIN_LEN {
            continue;
        }
        let bucket_list = by_hash.entry((slot.hash, slot.raw_len)).or_default();
        let found = bucket_list.iter().copied().find(|&b| {
            let first_idx = buckets[b][0];
            order[first_idx]
                .single_body()
                .is_some_and(|first| first.same_key(slot))
        });
        match found {
            Some(b) => buckets[b].push(idx),
            None => {
                let b = buckets.len();
                buckets.push(vec![idx]);
                bucket_list.push(b);
            }
        }
    }

    let mut absorbed = vec![false; order.len()];
    let mut leader_followers: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut scratch = String::new();
    for indices in buckets {
        if indices.len() < CROSS_DEDUP_MIN_FILES {
            continue;
        }
        let leader = indices[0];
        let followers = &indices[1..];
        let mut plain_bytes = 0usize;
        for &i in &indices {
            scratch.clear();
            write_single(&mut scratch, order[i], 1, pointers, hoist);
            plain_bytes += scratch.len();
        }
        scratch.clear();
        write_boilerplate(
            &mut scratch,
            order,
            leader,
            followers,
            1,
            None,
            pointers,
            hoist,
        );
        if scratch.len() < plain_bytes {
            for &f in followers {
                absorbed[f] = true;
            }
            leader_followers.insert(leader, followers.to_vec());
        }
    }

    let mut units = Vec::with_capacity(order.len());
    for (i, &was_absorbed) in absorbed.iter().enumerate() {
        if was_absorbed {
            continue;
        }
        if let Some(followers) = leader_followers.remove(&i) {
            units.push(DisplayUnit::Boilerplate {
                leader: i,
                followers,
            });
        } else {
            units.push(DisplayUnit::Single(i));
        }
    }
    units
}

fn write_single(out: &mut String, g: &Group, k: usize, pointers: bool, hoist: &PrefixHoist<'_>) {
    hoist.write_display_path(out, &g.path);
    let _ = write!(out, " ×{}", g.matches);
    write_pointer(out, pointers, g);
    out.push('\n');
    if k == 0 {
        return;
    }
    let represented = g.write_lines(out, k, pointers);
    if g.matches > represented {
        let _ = writeln!(out, "  +{} more in file", g.matches - represented);
    }
}

#[allow(clippy::too_many_arguments)]
fn write_boilerplate(
    out: &mut String,
    order: &[&Group],
    leader: usize,
    followers: &[usize],
    k: usize,
    max_followers: Option<usize>,
    pointers: bool,
    hoist: &PrefixHoist<'_>,
) {
    write_single(out, order[leader], k, pointers, hoist);
    if followers.is_empty() {
        return;
    }
    let cap = max_followers
        .unwrap_or(followers.len())
        .min(followers.len());
    let _ = write!(
        out,
        "  same line in {}: ",
        plural(followers.len(), "more file", "more files"),
    );
    for (pos, &i) in followers[..cap].iter().enumerate() {
        if pos > 0 {
            out.push_str(", ");
        }
        order[i].write_follower_ref(out, hoist, pointers);
    }
    if cap < followers.len() {
        let rest = &followers[cap..];
        let mut rest_matches = 0usize;
        let mut min_first = usize::MAX;
        let mut max_last = 0usize;
        for &i in rest {
            let g = order[i];
            rest_matches += g.matches;
            min_first = min_first.min(g.first);
            max_last = max_last.max(g.last);
        }
        let _ = write!(
            out,
            ", +{} ({})",
            plural(rest.len(), "more file", "more files"),
            plural(rest_matches, "match", "matches"),
        );
        if pointers && min_first <= max_last {
            write_stdout_range(out, min_first, max_last);
        }
    }
    out.push('\n');
}

fn write_unit(
    out: &mut String,
    order: &[&Group],
    unit: &DisplayUnit,
    k: usize,
    max_followers: Option<usize>,
    pointers: bool,
    hoist: &PrefixHoist<'_>,
) {
    match unit {
        DisplayUnit::Single(i) => write_single(out, order[*i], k, pointers, hoist),
        DisplayUnit::Boilerplate { leader, followers } => write_boilerplate(
            out,
            order,
            *leader,
            followers,
            k,
            max_followers,
            pointers,
            hoist,
        ),
    }
}

fn search_body(
    order: &[&Group],
    room: usize,
    pointers: bool,
    hoist: &PrefixHoist<'_>,
) -> (String, (usize, usize)) {
    let units = build_display_units(order, pointers, hoist);
    let mut body = String::with_capacity(room.min(4096));
    for k in [SHOWN, 5, 3, 2, 1] {
        body.clear();
        for unit in &units {
            write_unit(&mut body, order, unit, k, None, pointers, hoist);
            if body.len() > room {
                break;
            }
        }
        if body.len() <= room {
            return (body, (k + 1, order.len()));
        }
    }

    // Budget-constrained mode: show the busiest units with match lines in up to 2/3 of
    // `room`, falling back from k=2 to k=1 when k=2 leaves too few files visible.
    let detail_budget = room * 2 / 3;
    let fill_detail = |k: usize, cap: usize| {
        let mut out = String::with_capacity(detail_budget.min(4096));
        let mut listed = 0;
        for unit in &units {
            let before = out.len();
            write_unit(&mut out, order, unit, k, Some(cap), pointers, hoist);
            if out.len() > detail_budget {
                out.truncate(before);
                break;
            }
            listed += 1;
        }
        (out, listed)
    };
    let (mut body, mut listed) = fill_detail(2, 8);
    if listed < 2 && units.len() >= 2 {
        let (alt_body, alt_listed) = fill_detail(1, 4);
        if alt_listed > listed {
            body = alt_body;
            listed = alt_listed;
        }
    }

    let listed_files: usize = units[..listed]
        .iter()
        .map(|u| match u {
            DisplayUnit::Single(_) => 1,
            DisplayUnit::Boilerplate { followers, .. } => 1 + followers.len(),
        })
        .sum();
    let mut remaining_idx = Vec::with_capacity(order.len().saturating_sub(listed_files));
    for u in &units[listed..] {
        u.push_group_indices(&mut remaining_idx);
    }
    let remaining: Vec<&Group> = remaining_idx.into_iter().map(|i| order[i]).collect();
    if remaining.is_empty() {
        return (body, (1, listed_files));
    }

    // If every remaining file fits as a 1-line header (`path ×N [stdout:A-B]`), list
    // them all so no filenames are lost.
    let rem_room = room.saturating_sub(body.len());
    let mut headers_only = String::with_capacity(rem_room.min(2048));
    let mut all_fit = true;
    for group in &remaining {
        write_single(&mut headers_only, group, 0, pointers, hoist);
        if headers_only.len() > rem_room {
            all_fit = false;
            break;
        }
    }
    if all_fit {
        body.push_str(&headers_only);
        return (body, (0, order.len()));
    }

    // Otherwise list as many 1-line file headers as fit while reserving room for the
    // directory fold summary with exact omitted counts and `[stdout:A-B]` span.
    let fold_reserve = rem_room.min(220);
    let mut rem_listed = 0;
    for group in &remaining {
        let before = body.len();
        write_single(&mut body, group, 0, pointers, hoist);
        if body.len() + fold_reserve > room {
            body.truncate(before);
            break;
        }
        rem_listed += 1;
    }
    let tail = &remaining[rem_listed..];
    if !tail.is_empty() {
        let tag = stdout_span(pointers, tail);
        let fold_room = room.saturating_sub(body.len());
        write_fold(&mut body, tail, fold_room, &tag, hoist);
    }
    (body, (0, listed_files + rem_listed))
}

fn write_stdout_range(out: &mut String, a: usize, b: usize) {
    if a == b {
        let _ = write!(out, " [stdout:{a}]");
    } else {
        let _ = write!(out, " [stdout:{a}-{b}]");
    }
}

fn stdout_span(pointers: bool, groups: &[&Group]) -> String {
    if !pointers {
        return String::new();
    }
    let (Some(a), Some(b)) = (
        groups.iter().map(|g| g.first).min(),
        groups.iter().map(|g| g.last).max(),
    ) else {
        return String::new();
    };
    let mut out = String::with_capacity(24);
    write_stdout_range(&mut out, a, b);
    out
}

/// The files that did not fit, summarized by directory, largest first, within
/// `room` bytes for the directory names, ending with `tag`.
fn write_fold(out: &mut String, rest: &[&Group], room: usize, tag: &str, hoist: &PrefixHoist<'_>) {
    let mut folders: HashMap<Cow<'_, str>, (usize, usize)> =
        HashMap::with_capacity(rest.len().min(32));
    let mut total_matches = 0usize;
    for group in rest {
        total_matches += group.matches;
        let folder = hoist.display_folder(&group.path);
        let entry = folders.entry(folder).or_default();
        entry.0 += 1;
        entry.1 += group.matches;
    }
    let mut folders: Vec<_> = folders.into_iter().collect();
    folders.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(&b.0)));
    let head = format!(
        "+{} with {}",
        plural(rest.len(), "more file", "more files"),
        plural(total_matches, "match", "matches"),
    );
    // Reserve space for `head`, `" in "`, `", …"`, `tag` and `'\n'` so `fold` fits `room`.
    let fixed = head.len() + " in ".len() + ", …".len() + tag.len() + 1;
    let names_room = room.saturating_sub(fixed);
    let mut names = String::with_capacity(names_room.min(256));
    let mut named = 0usize;
    for (folder, (files, matches)) in &folders {
        let before = names.len();
        if !names.is_empty() {
            names.push_str(", ");
        }
        let _ = write!(
            names,
            "{folder}/ {} ×{matches}",
            plural(*files, "file", "files")
        );
        if names.len() > names_room {
            names.truncate(before);
            break;
        }
        named += 1;
    }
    out.push_str(&head);
    if named > 0 {
        out.push_str(" in ");
        out.push_str(&names);
        if named < folders.len() {
            out.push_str(", …");
        }
    }
    out.push_str(tag);
    out.push('\n');
}

/// A block report: each file's match and block counts, its first block in full,
/// then where its other blocks are. Fewer lines, then files alone, then folders
/// make it fit `room`.
fn block_body(
    order: &[&Group],
    list: &[Block],
    room: usize,
    pointers: bool,
    hoist: &PrefixHoist<'_>,
) -> (String, (usize, usize)) {
    let write_tag = |out: &mut String, a: usize, b: usize| {
        if pointers {
            write_stdout_range(out, a, b);
        }
    };
    let write_entry = |out: &mut String, block: &Block| -> bool {
        let before = out.len();
        match (block.from, block.to) {
            (Some(from), Some(to)) if from == to => {
                let _ = write!(out, "L{from}");
                write_tag(out, block.start, block.end);
            }
            (Some(from), Some(to)) => {
                let _ = write!(out, "L{from}-{to}");
                write_tag(out, block.start, block.end);
            }
            _ if pointers => {
                if block.start == block.end {
                    let _ = write!(out, "[stdout:{}]", block.start);
                } else {
                    let _ = write!(out, "[stdout:{}-{}]", block.start, block.end);
                }
            }
            _ => {}
        }
        out.len() > before
    };
    let write_block_group = |out: &mut String, g: &Group, lines: usize, entries: usize| {
        hoist.write_display_path(out, &g.path);
        let _ = write!(
            out,
            " ×{} in {}",
            g.matches,
            plural(g.blocks.len(), "block", "blocks"),
        );
        write_pointer(out, pointers, g);
        out.push('\n');
        // A cut block starts near its first match, and its range covers it whole.
        let Some(first) = g.blocks.first().filter(|_| lines > 0).map(|&b| &list[b]) else {
            return;
        };
        let shown = lines.min(g.shown.len());
        let lead = g.lead.unwrap_or(0);
        let start = lead.saturating_sub(lines / 4).min(g.shown.len() - shown);
        for line in &g.shown[start..start + shown] {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        let size = first.end - first.start + 1;
        if size > shown {
            let marker = out.len();
            out.push_str("  whole block ");
            if !write_entry(out, first) {
                out.truncate(marker);
                let _ = writeln!(
                    out,
                    "  +{}",
                    plural(size - shown, "more line", "more lines")
                );
            } else {
                out.push('\n');
            }
        }
        if let [_, rest @ ..] = g.blocks.as_slice()
            && let (Some(&a), Some(&b)) = (rest.first(), rest.last())
        {
            let _ = write!(
                out,
                "  +{}",
                plural(rest.len(), "more block", "more blocks")
            );
            write_tag(out, list[a].start, list[b].end);
            let mut named = 0usize;
            for &i in rest.iter().take(entries) {
                let sep_pos = out.len();
                out.push_str(if named == 0 { ": " } else { ", " });
                if write_entry(out, &list[i]) {
                    named += 1;
                } else {
                    out.truncate(sep_pos);
                }
            }
            if named > 0 && named < rest.len() {
                out.push_str(", …");
            }
            out.push('\n');
        }
    };
    let mut body = String::with_capacity(room.min(4096));
    for (lines, entries) in [(BLOCK_LINES, 32), (12, 12), (6, 6), (3, 3), (1, 0)] {
        body.clear();
        for group in order {
            write_block_group(&mut body, group, lines, entries);
            if body.len() > room {
                break;
            }
        }
        if body.len() <= room {
            return (body, (lines + 1, order.len()));
        }
    }
    // Name files alone while they fit, then fold the rest by directory.
    body.clear();
    let mut listed = 0;
    let limit = room * 2 / 3;
    for group in order {
        let before = body.len();
        write_block_group(&mut body, group, 0, 0);
        if body.len() > limit {
            body.truncate(before);
            break;
        }
        listed += 1;
    }
    let rest = &order[listed..];
    if let (Some(a), Some(b)) = (
        rest.iter().map(|g| g.first).min(),
        rest.iter().map(|g| g.last).max(),
    ) {
        let mut tag = String::with_capacity(24);
        write_tag(&mut tag, a, b);
        let fold_room = room.saturating_sub(body.len());
        write_fold(&mut body, rest, fold_room, &tag, hoist);
    }
    (body, (0, listed))
}

/// Parse a positive base-10 integer prefix terminated by `:` (no leading zero),
/// returning `(value, digit_count)` in a single byte pass.
fn parse_pos_digits_colon(bytes: &[u8]) -> Option<(usize, usize)> {
    let first = *bytes.first()?;
    if !(b'1'..=b'9').contains(&first) {
        return None;
    }
    let mut val = usize::from(first - b'0');
    let mut i = 1usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() {
            val = val.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
            i += 1;
        } else if b == b':' {
            return Some((val, i));
        } else {
            return None;
        }
    }
    None
}

/// Parse an optional leading `line:` (or `line:col:` when `--column`/`--vimgrep` is
/// active) prefix from the text after `path:`.
fn split_line_prefix(text: &str, mode: LineMode) -> (Option<usize>, Option<usize>, &str, &str) {
    if mode.explicit_numbered == Some(false) {
        return (None, None, "", text);
    }
    let bytes = text.as_bytes();
    let Some((line_no, digits)) = parse_pos_digits_colon(bytes) else {
        return (None, None, "", text);
    };
    let after_line = digits + 1;
    if mode.has_col
        && let Some((col, cdigits)) = parse_pos_digits_colon(&bytes[after_line..])
    {
        let after_col = after_line + cdigits + 1;
        return (
            Some(line_no),
            Some(col),
            &text[..after_col],
            &text[after_col..],
        );
    }
    (
        Some(line_no),
        None,
        &text[..after_line],
        &text[after_line..],
    )
}

/// Keep a normal-length line verbatim; for lines over `LINE_BYTES`, extract a
/// match-centered window around the query pattern (or `--column` offset, or the
/// first non-boilerplate region) while preserving any leading `line:` prefix.
fn window_line(
    raw: &str,
    prefix: &str,
    content: &str,
    col: Option<usize>,
    matcher: &mut PatternMatcher,
) -> (String, bool) {
    if raw.len() <= LINE_BYTES {
        return (raw.to_owned(), false);
    }
    let budget = WINDOW_SNIPPET_BYTES.min(LINE_BYTES.saturating_sub(prefix.len()).max(80));
    let snippet = window_snippet(content, col, matcher, budget);
    let mut out = String::with_capacity(prefix.len() + snippet.len());
    out.push_str(prefix);
    out.push_str(&snippet);
    (out, true)
}

fn window_snippet(
    content: &str,
    col: Option<usize>,
    matcher: &mut PatternMatcher,
    budget: usize,
) -> String {
    if content.len() <= budget {
        return content.to_owned();
    }
    let col_byte = col.map(|c| c.saturating_sub(1).min(content.len()));
    let (m_start, m_end) = if let Some(span) = matcher.find(content, col_byte) {
        span
    } else if let Some(at) = col_byte {
        let at = content.floor_char_boundary(at);
        (at, at)
    } else {
        let nb = skip_boilerplate(content);
        (nb, nb)
    };

    // Anchor at byte 0 only when the match sits comfortably inside the leading window
    // with room after it; otherwise center around the match so it is always visible.
    let (mut win_start, mut win_end) = if m_start == 0 || m_end.saturating_add(24) <= budget {
        (0, content.floor_char_boundary(budget.min(content.len())))
    } else {
        let m_len = m_end.saturating_sub(m_start);
        if m_len >= budget {
            let start = content.floor_char_boundary(m_start);
            let end = content.floor_char_boundary((start + budget).min(content.len()));
            (start, end)
        } else {
            let slack = budget - m_len;
            let before = slack * 2 / 5;
            let after = slack - before;
            let (raw_start, raw_end) = if m_start < before {
                (0, (m_end + after + (before - m_start)).min(content.len()))
            } else if m_end + after > content.len() {
                let overflow = m_end + after - content.len();
                (m_start.saturating_sub(before + overflow), content.len())
            } else {
                (m_start - before, m_end + after)
            };
            let mut start = content.ceil_char_boundary(raw_start);
            if start > m_start {
                start = content.floor_char_boundary(m_start);
            }
            let min_end = content.ceil_char_boundary(m_end.min(content.len()));
            let end = content.floor_char_boundary(raw_end).max(min_end);
            (start, end.min(content.len()))
        }
    };

    // Avoid emitting a 3-byte `…` just to elide <= 4 bytes at either edge.
    if win_start <= 4 {
        win_start = 0;
    }
    if content.len().saturating_sub(win_end) <= 4 {
        win_end = content.len();
    }

    let mut out = String::with_capacity((win_end - win_start) + 6);
    if win_start > 0 {
        out.push('…');
    }
    out.push_str(&content[win_start..win_end]);
    if win_end < content.len() {
        out.push('…');
    }
    out
}

/// Skip leading whitespace, block/HTML comments (`/* ... */`, `<!-- ... -->`) and
/// repeated single-character padding runs at the start of an ultra-long line when
/// no search pattern or column offset is available.
fn skip_boilerplate(content: &str) -> usize {
    let mut i = 0;
    let bytes = content.as_bytes();
    while i < bytes.len() {
        let before = i;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if content[i..].starts_with("/*")
            && let Some(end) = content[i + 2..].find("*/")
        {
            i += 2 + end + 2;
            continue;
        }
        if content[i..].starts_with("<!--")
            && let Some(end) = content[i + 4..].find("-->")
        {
            i += 4 + end + 3;
            continue;
        }
        if i < bytes.len() {
            let ch = bytes[i];
            let run = bytes[i..].iter().take_while(|&&b| b == ch).count();
            if run >= 24 {
                i += run;
                continue;
            }
        }
        if i == before {
            break;
        }
    }
    if i < content.len() { i } else { 0 }
}

/// The line number at the start of `text` and the separator after it: `12:`
/// before a match and `12-` before a context line.
fn number(text: &str) -> Option<(usize, char)> {
    let bytes = text.as_bytes();
    let first = *bytes.first()?;
    if !first.is_ascii_digit() {
        return None;
    }
    let mut val = usize::from(first - b'0');
    let mut i = 1usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() {
            val = val.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
            i += 1;
        } else if matches!(b, b':' | b'-') {
            return Some((val, b as char));
        } else {
            return None;
        }
    }
    None
}

/// Whether a line whose file name ends at `len` is a match, its line number, and
/// the text a report shows: from the number on, or from the separator on when
/// lines carry no numbers.
fn parts(line: &str, len: usize, numbered: bool) -> Option<(bool, Option<usize>, &str)> {
    let rest = line.get(len..)?;
    let sep = match *rest.as_bytes().first()? {
        b':' => ':',
        b'-' => '-',
        _ => return None,
    };
    if !numbered {
        return Some((sep == ':', None, rest));
    }
    let text = &rest[1..];
    let (number, kind) = number(text)?;
    (kind == sep).then_some((sep == ':', Some(number), text))
}

/// The file a "binary file matches" notice names: `Binary file X matches` from BSD
/// and older GNU grep, `grep: X: binary file matches` from newer GNU grep on
/// stderr, and `X: binary file matches (…)` from rg.
fn binary(line: &str) -> Option<&str> {
    if let Some(path) = line
        .strip_prefix("Binary file ")
        .and_then(|rest| rest.strip_suffix(" matches"))
    {
        return Some(path);
    }
    let (path, _) = line.split_once(": binary file matches")?;
    Some(path.strip_prefix("grep: ").unwrap_or(path))
}

fn write_pointer(out: &mut String, pointers: bool, group: &Group) {
    if pointers {
        write_stdout_range(out, group.first, group.last);
    }
}

pub(crate) fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Keep the exact text of a line, cut at a character boundary with a marker.
pub(crate) fn clip(text: &str) -> String {
    if text.len() <= LINE_BYTES {
        return text.to_owned();
    }
    format!("{}…", short(text, LINE_BYTES))
}

/// Rerun changes in per-group counts against the previous record of the same kind.
pub(crate) fn compare<'a>(
    previous: Option<(&str, &Summary)>,
    kind: &str,
    current: impl Iterator<Item = (&'a str, usize)>,
    gone: &str,
) -> String {
    let Some((id, old)) = previous.filter(|(_, s)| s.kind == kind) else {
        return String::new();
    };
    let current: HashMap<_, _> = current.collect();
    let before: HashMap<_, _> = old
        .items
        .iter()
        .map(|i| (i.name.as_str(), i.digest.parse::<usize>().unwrap_or(0)))
        .collect();
    let mut changes = Vec::new();
    for item in &old.items {
        let count = before[item.name.as_str()];
        match current.get(item.name.as_str()) {
            None => changes.push(format!("- {} ×{count} ({gone})", item.name)),
            Some(now) if *now != count => {
                changes.push(format!("~ {} ×{count} → ×{now}", item.name))
            }
            _ => {}
        }
    }
    // A record keeps the largest groups only, so new groups are reliable only when
    // the previous record was complete.
    if old.items.len() < STORED {
        let mut added: Vec<_> = current
            .iter()
            .filter(|(name, _)| !before.contains_key(*name))
            .collect();
        added.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (name, count) in added {
            changes.push(format!("+ {name} ×{count} (new)"));
        }
    }
    if changes.is_empty() {
        return format!("Same groups and counts as {id}.\n");
    }
    let mut text = format!("Compared with {id}:\n");
    for change in changes.iter().take(8) {
        text.push_str(change);
        text.push('\n');
    }
    if changes.len() > 8 {
        text.push_str(&format!("+{} more changes\n", changes.len() - 8));
    }
    text
}

/// Claude Code PostToolUse handler for the built-in Grep and Glob tools.
///
/// The hooks reference (PostToolUse decision control) documents `updatedToolOutput`:
/// "Replaces the tool's output with the provided value before it is sent to Claude.
/// The value must match the tool's output shape." It also says "`updatedMCPToolOutput`
/// Replaces the output for MCP tools only. Prefer `updatedToolOutput`, which also works
/// for built-in tools" and "For built-in tools, a value that doesn't match the tool's
/// output schema is ignored and the original output is used."
/// https://code.claude.com/docs/en/hooks#posttooluse-decision-control
///
/// The reference does not list per-tool output fields. These come from the output
/// schemas in Claude Code 2.1. Grep returns `mode`, `numFiles`, `filenames`, `content`
/// and `numLines`, among optional pagination fields. In content mode the model reads
/// `content`. Otherwise it reads "Found N files" from `numFiles` followed by
/// `filenames`, one per line. Glob returns `durationMs`, `numFiles`, `filenames` and
/// `truncated`, and the model reads `filenames`, one per line. This handler rewrites
/// only those text fields, keeps every count, and archives the original text so
/// `justless recall ID` returns it exactly.
pub fn claude_tool_output(ws: &Workspace, input: &Value) -> Option<Value> {
    if input["hook_event_name"] != "PostToolUse" {
        return None;
    }
    let tool = input["tool_name"].as_str()?;
    let mut response = input["tool_response"].clone();
    let content = match (tool, response.get("mode").map(|m| m.as_str())) {
        ("Grep", Some(Some("content"))) => true,
        ("Grep", None | Some(Some("files_with_matches"))) | ("Glob", None) => false,
        _ => return None,
    };
    response["numFiles"].as_u64()?;
    let budget = ws.settings().ok()?.context_budget;
    // Grep prints paths relative to the session directory, and Glob absolute paths.
    let base = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| ws.cwd.clone());
    // A dependency directory the agent searched on purpose is not collapsed.
    let args = &input["tool_input"];
    let named: Vec<&str> = ["path", "glob", "pattern"]
        .iter()
        .map(|key| args[*key].as_str().unwrap_or(""))
        .collect();
    let words = [&[tool][..], &named].concat();
    let (original, lines) = if content {
        let original = response["content"].as_str()?.to_owned();
        if original.len() <= budget || !response["numLines"].is_u64() {
            return None;
        }
        let mut search = Search::new(&base, &words);
        if let Some(pattern) = args["pattern"].as_str() {
            let ignore_case = args["-i"].as_bool().unwrap_or(false);
            search
                .matcher
                .add(pattern, false, ignore_case, false, false);
        }
        if args["-n"].as_bool() == Some(true) {
            search.line_mode.explicit_numbered = Some(true);
        }
        for (i, line) in original.lines().enumerate() {
            search.observe(line, i + 1);
        }
        let (report, _) = search.finish(tool, budget, None, false)?;
        (original, report)
    } else {
        let names = response["filenames"]
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?;
        let original = names.join("\n");
        if original.len() <= budget {
            return None;
        }
        let report = crate::listing::paths(tool, &names, &base, &words, budget)?;
        (original, report)
    };
    let id = crate::observations::archive(ws, &Value::String(original.clone())).ok()?;
    let mut text = format!("[justless recall {id}] exact original result\n{lines}");
    if tool == "Glob" && response["truncated"] == true {
        // Glob appends its truncation note from the number of names it returns.
        // Keep that note exact by writing it here from the original count.
        let shown = response["filenames"].as_array()?.len();
        text.push_str(&truncation_note(&response, shown));
        response["truncated"] = json!(false);
    }
    // The hint grows by any `--root`, so the report must save that too.
    let justless = ws.justless();
    if text.len() + 160 + (justless.len() - "justless".len()) >= original.len() {
        return None;
    }
    let text = text.trim_end_matches('\n');
    if content {
        response["content"] = json!(text);
        response["numLines"] = json!(text.lines().count());
    } else {
        response["filenames"] = json!(text.lines().collect::<Vec<_>>());
    }
    Some(json!({"hookSpecificOutput":{
        "hookEventName":"PostToolUse",
        "updatedToolOutput":response,
        "additionalContext":format!("Justless grouped this {tool} result. Recover the exact original with {justless} recall {id}, or narrow the path or pattern.")
    }}))
}

fn truncation_note(response: &Value, shown: usize) -> String {
    match (
        response["totalMatches"].as_u64(),
        response["countIsComplete"].as_bool(),
    ) {
        (Some(total), Some(true)) => format!(
            "(Showing {shown} of {total} matching files; {} more are not listed. Narrow the pattern or path to see the rest.)\n",
            (total as usize).saturating_sub(shown)
        ),
        (Some(total), _) => format!(
            "(Showing the first {shown} files; there are more than {total} matches. Narrow the pattern or path to see the rest.)\n"
        ),
        (None, _) => {
            "(Results are truncated. Consider using a more specific path or pattern.)\n".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Search;

    #[test]
    fn blocks_account_for_every_line_or_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "a-b", "a-1", "x-1.rs"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let grep = ["grep", "-rn", "-C1", "x", "."];
        let cases: [(&[&str], &[&str], Option<&str>); 10] = [
            // ugrep prints no `--` between files.
            (
                &grep,
                &["a-1-p", "a:2:x", "x-1.rs-1-q", "x-1.rs:2:x", "x-1.rs-3-"],
                Some("2 matches in 2 files, 2 blocks, 3 context lines\na ×1 in 1 block"),
            ),
            // Without numbers, `a-b` lines after `a` belong to the longer name.
            (
                &["grep", "-r", "-C1", "x", "."],
                &["a:x", "a-p", "--", "a-b-q", "a-b:x"],
                Some("a-b ×1 in 1 block [stdout:4-5]\n  -q\n  :x\n"),
            ),
            // A block that `head` cut before its match keeps its file.
            (
                &grep,
                &["a:2:x", "a-3-p", "--", "x-1.rs-1-q"],
                Some("1 match in 2 files, 2 blocks"),
            ),
            // A gap in line numbers starts a block, as `-A0` prints no `--`.
            (
                &["rg", "-n", "-A0", "x", "a"],
                &["2:x", "3:x", "5:x"],
                Some(
                    "a ×3 in 2 blocks [stdout:1-3]\n  2:x\n  3:x\n  +1 more block [stdout:3]: L5 [stdout:3]\n",
                ),
            ),
            // rg --heading names each file once, before a blank line ends it.
            (
                &["rg", "-n", "-C1", "--heading", "x", "."],
                &["a", "1-p", "2:x", "", "x-1.rs", "4:x", "--", "9:x"],
                Some("x-1.rs ×2 in 2 blocks [stdout:6-8]\n  4:x\n  +1 more block [stdout:8]: L9"),
            ),
            (
                &grep,
                &["a:2:x", "--", "Binary file x-1.rs matches"],
                Some("x-1.rs ×1 in 1 block [stdout:3]\n  (binary file matches)\n"),
            ),
            // Both `a` and `a-1` fit a cut block's `a-1-2-q`.
            (&grep, &["a:2:x", "--", "a-1-2-q"], None),
            (&grep, &["a:2:x", "stray"], None),
            // One file's lines without numbers cannot tell matches from context.
            (&["grep", "-C1", "x", "a"], &["p", "x"], None),
            (&["ag", "-C1", "x", "."], &["a:2:x"], None),
        ];
        for (words, lines, expected) in cases {
            let mut search = Search::new(dir.path(), words);
            for (i, line) in lines.iter().enumerate() {
                search.observe(line, i + 1);
            }
            let report = search
                .finish("grep", 4096, None, true)
                .map(|(text, _)| text);
            match expected {
                Some(part) => assert!(
                    report.as_deref().is_some_and(|text| text.contains(part)),
                    "{lines:?}: {report:?}"
                ),
                None => assert_eq!(report, None, "{lines:?}"),
            }
        }
    }

    #[test]
    fn a_heading_without_lines_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "a-1"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let words = ["rg", "-n", "-C1", "--heading", "x", "."];
        // A heading before a blank line, and one at the end of the output.
        for lines in [
            &["a", "", "a-1", "1:x", "2-y"][..],
            &["a-1", "1:x", "", "a"],
        ] {
            let mut search = Search::new(dir.path(), &words);
            for (i, line) in lines.iter().enumerate() {
                search.observe(line, i + 1);
            }
            let report = search.finish("rg", 4096, None, true);
            assert_eq!(report.map(|(text, _)| text), None, "{lines:?}");
        }
    }

    #[test]
    fn git_display_flags_keep_grep_context_and_every_line_checks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "p\nx\nq\n").unwrap();
        for words in [
            &["git", "--no-pager", "grep", "-n", "-C", "1", "x"][..],
            &["git", "-P", "--no-optional-locks", "grep", "-n", "-A1", "x"][..],
        ] {
            assert!(super::prints_context(words), "{words:?}");
            let mut search = Search::new(dir.path(), words);
            for (i, line) in ["a.rs-1-p", "a.rs:2:x", "a.rs-3-q"].iter().enumerate() {
                search.observe(line, i + 1);
            }
            let report = search
                .finish("git grep", 4096, None, true)
                .map(|(text, _)| text)
                .unwrap();
            assert!(
                report.contains("1 match in 1 file, 1 block, 2 context lines"),
                "{words:?}: {report}"
            );
        }
        assert!(super::matches_every_line(&[
            "git",
            "--no-pager",
            "grep",
            "-n",
            "",
            "a.rs"
        ]));
        for words in [
            &[
                "rg",
                "--glob-case-insensitive",
                "--no-require-git",
                "--no-ignore-messages",
                "foo",
            ][..],
            &["rg", "--no-pcre2", "--no-unicode", "foo"][..],
        ] {
            assert_eq!(
                super::select(words),
                Some(crate::filters::Profile::Search),
                "{words:?}"
            );
            assert_eq!(super::read_only(words), Some(true), "{words:?}");
        }
        assert_eq!(
            super::route(&["ls", "--full-time", "--reverse", "src"]),
            Some(true)
        );
        assert_eq!(super::route(&["ls", "--reverse", "src"]), Some(false));
        for words in [
            &["ls", "-l", "--time-style=long-iso", "src"][..],
            &["ls", "-la", "--time-style", "long-iso", "src"][..],
            &["ls", "-lLsi", "src"][..],
            &["tree", "-h", "-s", "--sort=size", "src"][..],
            &["tree", "-hs", "--sort", "mtime", "-L", "2", "src"][..],
        ] {
            assert_eq!(
                super::select(words),
                Some(crate::filters::Profile::Listing),
                "{words:?}"
            );
            assert_eq!(super::route(words), Some(true), "{words:?}");
            assert_eq!(super::read_only(words), Some(true), "{words:?}");
        }
        // Plain `ls` with display flags but no `-l`/`-R`/`--full-time` stays unrouted yet read-only.
        for words in [
            &["ls", "--time-style=long-iso", "src"][..],
            &["ls", "-L", "-s", "-i", "src"][..],
        ] {
            assert_eq!(
                super::select(words),
                Some(crate::filters::Profile::Listing),
                "{words:?}"
            );
            assert_eq!(super::route(words), Some(false), "{words:?}");
            assert_eq!(super::read_only(words), Some(true), "{words:?}");
        }
        // Unsafe or unknown flags on `ls` and `tree` remain rejected.
        for words in [
            &["ls", "--time-style"][..],
            &["tree", "-o", "out.txt"][..],
            &["tree", "--sort"][..],
            &["tree", "-J", "src"][..],
        ] {
            assert_eq!(super::select(words), None, "{words:?}");
            assert_eq!(super::read_only(words), Some(false), "{words:?}");
        }
    }

    /// Verify every `[stdout:A]` / `[stdout:A-B]` tag in `report` refers to valid
    /// 1-based lines within `1..=total_lines`.
    fn assert_valid_stdout_pointers(report: &str, total_lines: usize) {
        let mut count = 0usize;
        for range in report.split("[stdout:").skip(1) {
            let range = range.split(']').next().expect("closing ] on stdout tag");
            let (a, b) = range.split_once('-').unwrap_or((range, range));
            let (a, b): (usize, usize) = (
                a.parse().expect("numeric start"),
                b.parse().expect("numeric end"),
            );
            assert!(
                1 <= a && a <= b && b <= total_lines,
                "invalid pointer [stdout:{range}] for {total_lines} lines in:\n{report}"
            );
            count += 1;
        }
        assert!(
            count > 0,
            "expected at least one [stdout:] pointer:\n{report}"
        );
    }

    fn run_search(
        dir: &std::path::Path,
        words: &[&str],
        label: &str,
        lines: &[String],
        budget: usize,
        pointers: bool,
    ) -> (String, crate::summary::Summary) {
        let mut search = Search::new(dir, words);
        for (i, line) in lines.iter().enumerate() {
            search.observe(line, i + 1);
        }
        search
            .finish(label, budget, None, pointers)
            .expect("recognized search report")
    }

    #[test]
    fn deep_monorepo_common_prefix_hoisting_saves_bytes_and_stays_unambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = "src/services/billing/v2/";
        let rel_files = [
            "mod.rs",
            "handlers/charge.rs",
            "handlers/refund.rs",
            "handlers/invoice.rs",
            "handlers/subscription.rs",
            "handlers/webhook.rs",
            "models/ledger.rs",
            "models/line_item.rs",
            "models/currency.rs",
            "models/tax_rate.rs",
            "validators/card.rs",
            "validators/address.rs",
            "validators/limits.rs",
            "clients/stripe.rs",
            "clients/adyen.rs",
            "clients/paypal.rs",
            "jobs/reconcile.rs",
            "jobs/dunning.rs",
            "jobs/settlement.rs",
            "jobs/export.rs",
        ];
        let mut raw_lines = Vec::new();
        for (idx, rel) in rel_files.iter().enumerate() {
            let full = format!("{prefix}{rel}");
            let path = dir.path().join(&full);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
            raw_lines.push(format!(
                "{full}:{}:    let billing_id = resolve_account_{idx}(ctx);",
                10 + idx
            ));
            raw_lines.push(format!(
                "{full}:{}:    audit_billing_event_{idx}(billing_id);",
                30 + idx
            ));
        }
        let raw_bytes: usize = raw_lines.iter().map(|l| l.len() + 1).sum();
        let words = ["rg", "-n", "billing", "src/services/billing/v2"];
        let (report, summary) = run_search(dir.path(), &words, "rg", &raw_lines, 8192, true);

        // Header hoists the deep common prefix once, and body uses clean relative paths.
        let first_line = report.lines().next().unwrap();
        assert_eq!(
            first_line,
            "rg: 40 matches in 20 files, under src/services/billing/v2/"
        );
        let body = &report[first_line.len() + 1..];
        assert!(
            !body.contains(prefix),
            "prefix should not repeat in body:\n{report}"
        );
        for rel in &rel_files {
            assert!(
                body.contains(&format!("{rel} ×2 [stdout:")),
                "missing relative path {rel} in:\n{report}"
            );
        }
        assert_valid_stdout_pointers(&report, raw_lines.len());

        // Compare real byte savings against unhoisted path rendering and raw output.
        let unhoisted_extra = rel_files.len() * prefix.len() - (", under ".len() + prefix.len());
        let unhoisted_len = report.len() + unhoisted_extra;
        let hoist_pct = (unhoisted_extra * 100) / unhoisted_len;
        assert!(
            hoist_pct >= 14,
            "expected >=14% savings on 2-match files from prefix hoisting, got {hoist_pct}% (saved {unhoisted_extra} of {unhoisted_len} B)"
        );
        assert!(
            report.len() < raw_bytes,
            "report {} B should be smaller than raw {} B",
            report.len(),
            raw_bytes
        );

        // Single-match files under a deep monorepo prefix save 20-30% of the entire report.
        let one_per_file: Vec<String> = rel_files
            .iter()
            .enumerate()
            .map(|(i, rel)| format!("{prefix}{rel}:{}:    billing_call_{i}(ctx);", 10 + i))
            .collect();
        let (hoisted_one, _) = run_search(dir.path(), &words, "rg", &one_per_file, 8192, true);
        let unhoisted_one_len = hoisted_one.len() + unhoisted_extra;
        let one_pct = (unhoisted_extra * 100) / unhoisted_one_len;
        assert!(
            one_pct >= 20,
            "expected >=20% savings on 1-match deep monorepo search, got {one_pct}% (saved {unhoisted_extra} of {unhoisted_one_len} B)"
        );

        // Summary items keep exact full paths so rerun diffs stay accurate across prefix shifts.
        assert!(
            summary
                .items
                .iter()
                .all(|item| item.name.starts_with(prefix)),
            "summary items must retain full paths"
        );

        // Partial prefix hoisting: 12 deep monorepo files + 2 top-level outliers stay unambiguous.
        let dir2 = tempfile::tempdir().unwrap();
        let deep = "packages/commerce/services/billing/v2/";
        let mut mixed_lines = Vec::new();
        for i in 0..12 {
            let sub = if i < 6 { "handlers" } else { "models" };
            let full = format!("{deep}{sub}/route_{i:02}.ts");
            let p = dir2.path().join(&full);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            mixed_lines.push(format!("{full}:14:export function handleBilling{i}() {{}}"));
        }
        for outlier in ["docs/billing.md", "tests/billing_e2e.ts"] {
            let p = dir2.path().join(outlier);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            mixed_lines.push(format!(
                "{outlier}:3:verify handleBilling flow in {outlier}"
            ));
        }
        let (partial, _) = run_search(
            dir2.path(),
            &["rg", "-n", "handleBilling", "."],
            "rg",
            &mixed_lines,
            8192,
            true,
        );
        let partial_header = partial.lines().next().unwrap();
        assert!(
            partial_header.contains("(… = packages/commerce/services/billing/v2/)"),
            "{partial}"
        );
        assert!(
            partial.contains("…/handlers/route_00.ts ×1 [stdout:1]"),
            "{partial}"
        );
        assert!(
            partial.contains("\ndocs/billing.md ×1 [stdout:13]"),
            "{partial}"
        );
        assert!(
            partial.contains("\ntests/billing_e2e.ts ×1 [stdout:14]"),
            "{partial}"
        );
        assert_valid_stdout_pointers(&partial, mixed_lines.len());

        // Shallow prefixes (`src/` and `./src/`) are never hoisted.
        let dir3 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir3.path().join("src")).unwrap();
        for f in ["src/a.rs", "src/b.rs", "src/c.rs"] {
            std::fs::write(dir3.path().join(f), "").unwrap();
        }
        let shallow_lines: Vec<String> = ["src/a.rs", "src/b.rs", "src/c.rs"]
            .iter()
            .enumerate()
            .map(|(i, f)| format!("{f}:{}:let x_{i} = needle();", i + 1))
            .collect();
        let (shallow, _) = run_search(
            dir3.path(),
            &["rg", "-n", "needle", "src"],
            "rg",
            &shallow_lines,
            4096,
            true,
        );
        assert!(
            shallow.starts_with("rg: 3 matches in 3 files\nsrc/a.rs ×1"),
            "{shallow}"
        );

        // Block/context mode also hoists deep common prefixes cleanly.
        let ctx_lines: Vec<String> = rel_files[..4]
            .iter()
            .enumerate()
            .flat_map(|(i, rel)| {
                let full = format!("{prefix}{rel}");
                vec![
                    format!("{full}-9-fn before_{i}() {{}}"),
                    format!("{full}:10:    billing_call_{i}();"),
                    format!("{full}-11-fn after_{i}() {{}}"),
                    "--".to_owned(),
                ]
            })
            .collect();
        let (ctx_report, _) = run_search(
            dir.path(),
            &["rg", "-n", "-C1", "billing_call", "src/services/billing/v2"],
            "rg",
            &ctx_lines,
            8192,
            true,
        );
        assert!(
            ctx_report
                .lines()
                .next()
                .unwrap()
                .contains(", under src/services/billing/v2/"),
            "{ctx_report}"
        );
        assert!(
            ctx_report.contains("\nmod.rs ×1 in 1 block [stdout:"),
            "{ctx_report}"
        );
    }

    #[test]
    fn minified_long_line_centers_window_around_match_at_offset_2000() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("dist/bundle.min.js"), "").unwrap();
        std::fs::write(dir.path().join("dist/data.json"), "").unwrap();

        // Build a 4.2 KB minified single line with the target match at byte offset ~2,000.
        let before = format!(
            "/*! vendor bundle v4.2.0 MIT */{}",
            "var a=function(e,t){return e+t};".repeat(62)
        );
        assert!(before.len() >= 2000 && before.len() <= 2100);
        let needle =
            "function calculateInvoiceTotal(items,tax){return items.reduce((s,x)=>s+x,0)*(1+tax)}";
        let after = "var b=function(r,n){return r*n};".repeat(65);
        let minified = format!("{before}{needle}{after}");
        assert!(minified.len() > 4000);
        let raw_line = format!("dist/bundle.min.js:1:{minified}");

        // Prove naive `clip()` completely hides the match at offset 2,000.
        let naive = super::clip(&format!("1:{minified}"));
        assert!(
            !naive.contains("calculateInvoiceTotal"),
            "naive clip should have missed offset 2000"
        );

        // 1. Literal pattern (`rg -n calculateInvoiceTotal dist`).
        let lines = vec![raw_line.clone()];
        let (report, _) = run_search(
            dir.path(),
            &["rg", "-n", "calculateInvoiceTotal", "dist"],
            "rg",
            &lines,
            4096,
            true,
        );
        assert!(
            report.contains("calculateInvoiceTotal"),
            "match at offset 2000 must be visible:\n{report}"
        );
        assert!(
            report.contains("\n  1:…") && report.contains("… [stdout:1]\n"),
            "windowed line must preserve line number, ellipses, and [stdout:1]:\n{report}"
        );
        assert!(
            report.len() * 15 < raw_line.len(),
            "expected >93% reduction on 4 KB minified line: {} B vs {} B",
            report.len(),
            raw_line.len()
        );
        assert_valid_stdout_pointers(&report, 1);

        // 2. Regex pattern, case-insensitive `-i`, and fixed-string `-F`.
        for words in [
            &["rg", "-n", r"calculate\w+Total", "dist"][..],
            &["rg", "-ni", "CALCULATEINVOICETOTAL", "dist"][..],
            &["rg", "-nF", "items.reduce((s,x)", "dist"][..],
        ] {
            let (r, _) = run_search(dir.path(), words, "rg", &lines, 4096, true);
            assert!(
                r.contains("calculateInvoiceTotal") || r.contains("items.reduce((s,x)"),
                "{words:?} missed match:\n{r}"
            );
            assert!(r.contains("[stdout:1]"), "{words:?}:\n{r}");
        }

        // 3. `--column` centers on the reported column even when pattern came from `-f`.
        let col = before.len() + 1;
        let col_lines = vec![format!("dist/bundle.min.js:1:{col}:{minified}")];
        let (col_report, _) = run_search(
            dir.path(),
            &["rg", "-n", "--column", "-f", "patterns.txt", "dist"],
            "rg",
            &col_lines,
            4096,
            true,
        );
        assert!(
            col_report.contains("calculateInvoiceTotal")
                && col_report.contains(&format!("1:{col}:…")),
            "{col_report}"
        );

        // 4. Unknown pattern skips leading license comment boilerplate.
        let banner = format!(
            "/*! {} */const ACTIVE_ENTRY=42;{}",
            "license ".repeat(40),
            "x".repeat(400)
        );
        let banner_lines = vec![format!("dist/bundle.min.js:1:{banner}")];
        let (fallback, _) = run_search(
            dir.path(),
            &["rg", "-n", "-f", "patterns.txt", "dist"],
            "rg",
            &banner_lines,
            4096,
            true,
        );
        assert!(
            fallback.contains("const ACTIVE_ENTRY=42;") && fallback.contains("[stdout:1]"),
            "{fallback}"
        );

        // 5. Multi-byte UTF-8 JSON line (> 4 KB) with match at offset ~2,400 never panics.
        let utf8_before = "{\"note\":\"日本語—émoji🚀—padding\",".repeat(65);
        let utf8_needle = "\"targetKey\":\"invoice_₹_9900\"";
        let utf8_after = ",\"tail\":\"αβγδεζηθ\"".repeat(90);
        let json_line = format!("dist/data.json:1:{utf8_before}{utf8_needle}{utf8_after}");
        let (json_report, _) = run_search(
            dir.path(),
            &["rg", "-n", "invoice_₹_9900", "dist"],
            "rg",
            std::slice::from_ref(&json_line),
            4096,
            true,
        );
        assert!(
            json_report.contains("invoice_₹_9900") && json_report.len() * 10 < json_line.len(),
            "{json_report}"
        );

        // 6. Block/context mode (`-C1`) also centers long match lines without extra `[stdout:n]`.
        let ctx_lines = vec![
            format!("dist/bundle.min.js:1:{minified}"),
            "dist/bundle.min.js-2-//# sourceMappingURL=bundle.min.js.map".to_owned(),
        ];
        let (ctx_report, _) = run_search(
            dir.path(),
            &["rg", "-n", "-C1", "calculateInvoiceTotal", "dist"],
            "rg",
            &ctx_lines,
            4096,
            true,
        );
        assert!(
            ctx_report.contains("calculateInvoiceTotal") && !ctx_report.contains("… [stdout:1]"),
            "{ctx_report}"
        );
    }

    #[test]
    fn intra_file_repeated_identical_match_lines_fold_with_exact_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        for f in [
            "src/billing.ts",
            "src/assertions.test.ts",
            "src/two_only.ts",
            "src/contiguous.txt",
            "src/ports.yaml",
        ] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }

        // 1. 4 non-contiguous identical trimmed lines (varying indentation) + 2 distinct lines.
        let lines = vec![
            "src/billing.ts:5:const header = initBilling();".to_owned(),
            "src/billing.ts:12:  import { InvoiceClient } from '@acme/billing';".to_owned(),
            "src/billing.ts:18:import { InvoiceClient } from '@acme/billing';".to_owned(),
            "src/billing.ts:25:    import { InvoiceClient } from '@acme/billing';".to_owned(),
            "src/billing.ts:44:  import { InvoiceClient } from '@acme/billing';".to_owned(),
            "src/billing.ts:60:return finalizeBilling(header);".to_owned(),
        ];
        let (report, _) = run_search(
            dir.path(),
            &["rg", "-n", "billing", "src"],
            "rg",
            &lines,
            4096,
            true,
        );
        assert!(
            report.contains("  5:const header = initBilling();\n")
                && report
                    .contains("  L12,18,25,44: import { InvoiceClient } from '@acme/billing';\n")
                && report.contains("  60:return finalizeBilling(header);\n")
                && !report.contains("more in file"),
            "{report}"
        );

        // 2. 10 repeated identical assertions collapse to 1 line AND reveal a distinct match at L120.
        let mut test_lines: Vec<String> = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]
            .into_iter()
            .map(|ln| {
                format!(
                    "src/assertions.test.ts:{ln}:    expect(calculateTotal(fixtureInvoice)).toEqual(10800);"
                )
            })
            .collect();
        test_lines.push(
            "src/assertions.test.ts:120:    expect(calculateTotal(discountInvoice)).toEqual(9720);"
                .to_owned(),
        );
        let raw_bytes: usize = test_lines.iter().map(|l| l.len() + 1).sum();
        let (folded, _) = run_search(
            dir.path(),
            &["rg", "-n", "calculateTotal", "src"],
            "rg",
            &test_lines,
            4096,
            true,
        );
        assert!(
            folded.contains(
                "  L10,20,30,40,50,60,70,80,90,100: expect(calculateTotal(fixtureInvoice)).toEqual(10800);\n"
            ) && folded.contains("120:    expect(calculateTotal(discountInvoice)).toEqual(9720);")
                && !folded.contains("more in file"),
            "{folded}"
        );
        assert!(
            folded.len() * 3 < raw_bytes,
            "expected >66% savings on 10x repeated assertions: {} B vs {} B",
            folded.len(),
            raw_bytes
        );

        // 3. Only 2 identical occurrences stay unfolded in exact original order.
        let two_lines = vec![
            "src/two_only.ts:12:  callHelper(needle);".to_owned(),
            "src/two_only.ts:15:  otherCall(needle);".to_owned(),
            "src/two_only.ts:18:  callHelper(needle);".to_owned(),
        ];
        let (two_report, _) = run_search(
            dir.path(),
            &["rg", "-n", "needle", "src"],
            "rg",
            &two_lines,
            4096,
            true,
        );
        assert!(
            two_report.contains(
                "  12:  callHelper(needle);\n  15:  otherCall(needle);\n  18:  callHelper(needle);\n"
            ),
            "{two_report}"
        );

        // 4. 50 contiguous identical lines collapse to `L1-50: <line>`.
        let contig: Vec<String> = (1..=50)
            .map(|ln| format!("src/contiguous.txt:{ln}:repeated needle entry in config"))
            .collect();
        let (contig_report, _) = run_search(
            dir.path(),
            &["rg", "-n", "needle", "src"],
            "rg",
            &contig,
            4096,
            true,
        );
        assert!(
            contig_report.contains("  L1-50: repeated needle entry in config\n")
                && !contig_report.contains("more in file"),
            "{contig_report}"
        );

        // 5. Unnumbered search collapses 5 identical lines to `×5: <line>`, and repeated
        //    YAML `8080: needle` without `-n` never loses `8080:`.
        let unnum: Vec<String> = (0..5)
            .map(|_| "src/ports.yaml:8080: needle_service".to_owned())
            .collect();
        let (unnum_report, _) = run_search(
            dir.path(),
            &["grep", "-r", "needle_service", "src"],
            "grep",
            &unnum,
            4096,
            true,
        );
        assert!(
            unnum_report.contains("  ×5: 8080: needle_service\n")
                && !unnum_report.contains("L8080"),
            "{unnum_report}"
        );
    }

    #[test]
    fn cross_file_boilerplate_dedup_and_tight_budget_summaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        let import_line = "import { InvoiceClient, TaxEngine } from '@acme/billing/v2/client';";

        // 4 files with real call sites (multi-match: import + real call) stay full blocks.
        for i in 0..4 {
            let rel = format!("src/real_{i}.ts");
            let p = dir.path().join(&rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            lines.push(format!("{rel}:1:{import_line}"));
            lines.push(format!(
                "{rel}:42:export const res{i} = InvoiceClient.chargeOrder(order_{i});"
            ));
        }
        // 16 pure-boilerplate files matching ONLY the identical import line.
        for i in 0..16 {
            let rel = format!("src/mod_{i:02}.ts");
            let p = dir.path().join(&rel);
            std::fs::write(&p, "").unwrap();
            lines.push(format!("{rel}:{}:{import_line}", (i % 5) + 1));
        }
        let raw_bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
        let (report, _) = run_search(
            dir.path(),
            &["rg", "-n", "InvoiceClient", "src"],
            "rg",
            &lines,
            8192,
            true,
        );

        // All 4 real-call files appear at the top with their actual call sites.
        for i in 0..4 {
            assert!(
                report.contains(&format!("src/real_{i}.ts ×2"))
                    && report.contains(&format!("InvoiceClient.chargeOrder(order_{i})")),
                "missing real call site {i}:\n{report}"
            );
        }
        // The 16 pure-boilerplate files collapse onto 1 leader + 15 compact inline refs.
        assert!(
            report.contains("  same line in 15 more files: "),
            "{report}"
        );
        for i in 1..16 {
            let expected_line = (i % 5) + 1;
            let expected_stdout = 8 + i + 1;
            assert!(
                report.contains(&format!(
                    "src/mod_{i:02}.ts:{expected_line} [stdout:{expected_stdout}]"
                )),
                "missing follower ref for mod_{i:02}:\n{report}"
            );
        }
        // Only 5 occurrences of the 67-byte import string (4 multi-match + 1 leader) instead of 20.
        assert_eq!(report.matches(import_line).count(), 5, "{report}");
        let un_deduped_extra = 15 * (import_line.len() + 6);
        assert!(
            report.len() * 3 < raw_bytes * 2
                && report.len() * 100 < (report.len() + un_deduped_extra) * 58,
            "expected substantial reduction via cross-file dedup: {} B vs {} B raw (~{} B un-deduped)",
            report.len(),
            raw_bytes,
            report.len() + un_deduped_extra
        );
        assert_valid_stdout_pointers(&report, lines.len());

        // Tight budget on 60 files across 6 subdirectories preserves exact omitted file/match
        // counts and `[stdout:A-B]` span while fitting strictly within budget.
        let dir2 = tempfile::tempdir().unwrap();
        let mut many_lines = Vec::new();
        for d in 0..6 {
            for f in 0..10 {
                let rel = format!("src/pkg_{d}/file_{f:02}.rs");
                let p = dir2.path().join(&rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, "").unwrap();
                for m in 1..=4 {
                    many_lines.push(format!(
                        "{rel}:{}:    process_request_{d}_{f}_{m}(payload);",
                        m * 10
                    ));
                }
            }
        }
        for budget in [700, 1400] {
            let (tight, _) = run_search(
                dir2.path(),
                &["rg", "-n", "process_request", "src"],
                "rg",
                &many_lines,
                budget,
                true,
            );
            assert!(
                tight.len() <= budget,
                "budget {budget} exceeded ({} B):\n{tight}",
                tight.len()
            );
            assert!(
                tight.contains("rg: 240 matches in 60 files\n")
                    && tight.contains("more files with ")
                    && tight.contains(" matches"),
                "budget {budget} missing exact omitted counts:\n{tight}"
            );
            let last = tight.lines().last().unwrap();
            assert!(
                last.starts_with('+') && last.contains("[stdout:") && last.ends_with(']'),
                "budget {budget} fold line must include [stdout:A-B]: {last}"
            );
            assert_valid_stdout_pointers(&tight, many_lines.len());
        }
    }

    #[test]
    fn large_multifile_rg_and_git_grep_fixtures_measure_end_to_end_savings() {
        // 1. Large realistic monorepo `rg -n` combining ALL four optimizations:
        //    deep common prefix + minified bundle + intra-file repeats + cross-file boilerplate.
        let dir = tempfile::tempdir().unwrap();
        let prefix = "packages/platform/services/orders/v3/";
        let mut rg_lines = Vec::new();
        let boilerplate = "import { OrderLedger, OrderStatus } from '@platform/orders/v3/ledger';";

        // 18 boilerplate-only files under the deep monorepo prefix.
        for i in 0..18 {
            let full = format!("{prefix}routes/endpoint_{i:02}.ts");
            let p = dir.path().join(&full);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            rg_lines.push(format!("{full}:2:{boilerplate}"));
        }
        // 10 active service files with both repeated assertions and unique calls.
        for i in 0..10 {
            let full = format!("{prefix}domain/service_{i:02}.ts");
            let p = dir.path().join(&full);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            rg_lines.push(format!("{full}:1:{boilerplate}"));
            for ln in [15, 29, 43, 57, 71] {
                rg_lines.push(format!(
                    "{full}:{ln}:    OrderLedger.assertInvariant(state, OrderStatus.Open);"
                ));
            }
            rg_lines.push(format!(
                "{full}:95:    return OrderLedger.commitOrder_{i:02}(tx);"
            ));
        }
        // 1 minified 4.5 KB bundle inside the same tree (in `assets/`, not collapsed `dist/`)
        // with match at offset ~2,200.
        let bundle = format!("{prefix}assets/orders.min.js");
        let bp = dir.path().join(&bundle);
        std::fs::create_dir_all(bp.parent().unwrap()).unwrap();
        std::fs::write(&bp, "").unwrap();
        let min_line = format!(
            "{bundle}:1:{}{}{}",
            "function z(e){return e+1};".repeat(85),
            "exports.OrderLedger={commit:function(x){return x.id}}",
            "function y(t){return t-1};".repeat(85),
        );
        rg_lines.push(min_line);

        let rg_raw_bytes: usize = rg_lines.iter().map(|l| l.len() + 1).sum();
        let (rg_report, _) = run_search(
            dir.path(),
            &[
                "rg",
                "-n",
                "OrderLedger",
                "packages/platform/services/orders/v3",
            ],
            "rg",
            &rg_lines,
            4096,
            true,
        );
        assert!(rg_report.len() <= 4096, "rg report exceeded 4096 B");
        assert!(
            rg_report.len() * 4 < rg_raw_bytes,
            "expected >75% end-to-end byte reduction on monorepo rg: {} B vs {} B",
            rg_report.len(),
            rg_raw_bytes
        );
        assert!(
            rg_report
                .lines()
                .next()
                .unwrap()
                .contains(", under packages/platform/services/orders/v3/")
                && rg_report.contains("L15,29,43,57,71: OrderLedger.assertInvariant(")
                && rg_report.contains("same line in 17 more files: ")
                && rg_report.contains("exports.OrderLedger={commit:function(x){return x.id}}"),
            "{rg_report}"
        );
        assert_valid_stdout_pointers(&rg_report, rg_lines.len());

        // 2. Large multi-file `git grep -n` fixture under a deep crate tree.
        let dir2 = tempfile::tempdir().unwrap();
        let crate_prefix = "crates/engine/src/runtime/scheduler/";
        let mut gg_lines = Vec::new();
        let use_line = "use crate::runtime::scheduler::queue::TaskQueue;";
        for i in 0..20 {
            let full = format!("{crate_prefix}workers/worker_{i:02}.rs");
            let p = dir2.path().join(&full);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            gg_lines.push(format!("{full}:3:{use_line}"));
        }
        for i in 0..8 {
            let full = format!("{crate_prefix}queues/shard_{i:02}.rs");
            let p = dir2.path().join(&full);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            for ln in [12, 24, 36, 48] {
                gg_lines.push(format!(
                    "{full}:{ln}:    TaskQueue::wake_parked_workers(&self.pool);"
                ));
            }
            gg_lines.push(format!(
                "{full}:80:    TaskQueue::drain_shard_{i:02}(&mut self.local);"
            ));
        }
        let gg_raw_bytes: usize = gg_lines.iter().map(|l| l.len() + 1).sum();
        let (gg_report, _) = run_search(
            dir2.path(),
            &[
                "git",
                "grep",
                "-n",
                "TaskQueue",
                "--",
                "crates/engine/src/runtime/scheduler",
            ],
            "git grep",
            &gg_lines,
            4096,
            true,
        );
        assert!(gg_report.len() <= 4096);
        assert!(
            gg_report.len() * 2 < gg_raw_bytes,
            "expected >50% reduction on git grep: {} B vs {} B",
            gg_report.len(),
            gg_raw_bytes
        );
        assert!(
            gg_report
                .lines()
                .next()
                .unwrap()
                .contains(", under crates/engine/src/runtime/scheduler/")
                && gg_report.contains("L12,24,36,48: TaskQueue::wake_parked_workers(&self.pool);")
                && gg_report.contains("same line in 19 more files: "),
            "{gg_report}"
        );
        assert_valid_stdout_pointers(&gg_report, gg_lines.len());
    }

    #[test]
    fn ansi_fast_path_skips_alloc_and_colored_search_parses_identically() {
        // Plain lines borrow with zero allocation (`Cow::Borrowed`).
        let plain_line = "src/billing.rs:42:let total = compute_invoice(order);";
        assert!(matches!(
            super::strip_ansi_fast(plain_line),
            std::borrow::Cow::Borrowed(s) if s == plain_line
        ));
        // Stray non-CSI ESC bytes also borrow without allocating.
        let stray = "raw\x1btext";
        assert!(matches!(
            super::strip_ansi_fast(stray),
            std::borrow::Cow::Borrowed(s) if s == stray
        ));
        // Colored `rg --color=always` lines strip cleanly and produce the exact same
        // grouped output (including `--column` and `--heading -C1`) as plain input.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "").unwrap();

        let plain_lines = vec![
            "src/a.rs:10:5:let invoice = compute(1);".to_owned(),
            "src/a.rs:20:5:let invoice = compute(2);".to_owned(),
            "src/b.rs:7:9:return invoice;".to_owned(),
        ];
        let colored_lines = vec![
            "\x1b[0m\x1b[35msrc/a.rs\x1b[0m:\x1b[0m\x1b[32m10\x1b[0m:5:let \x1b[0m\x1b[1m\x1b[31minvoice\x1b[0m = compute(1);".to_owned(),
            "\x1b[0m\x1b[35msrc/a.rs\x1b[0m:\x1b[0m\x1b[32m20\x1b[0m:5:let \x1b[0m\x1b[1m\x1b[31minvoice\x1b[0m = compute(2);".to_owned(),
            "\x1b[0m\x1b[35msrc/b.rs\x1b[0m:\x1b[0m\x1b[32m7\x1b[0m:9:return \x1b[0m\x1b[1m\x1b[31minvoice\x1b[0m;".to_owned(),
        ];
        let words = ["rg", "-n", "--column", "invoice", "src"];
        let (plain_rep, _) = run_search(dir.path(), &words, "rg", &plain_lines, 4096, true);
        let (color_rep, _) = run_search(dir.path(), &words, "rg", &colored_lines, 4096, true);
        assert_eq!(color_rep, plain_rep);
        assert_valid_stdout_pointers(&color_rep, colored_lines.len());

        // Colored `rg -n -C1 --heading` also parses identically to uncolored input.
        let h_words = ["rg", "-n", "-C1", "--heading", "invoice", "src"];
        let h_plain = vec![
            "src/a.rs".to_owned(),
            "9-fn run() {".to_owned(),
            "10:let invoice = compute(1);".to_owned(),
            "".to_owned(),
            "src/b.rs".to_owned(),
            "7:return invoice;".to_owned(),
        ];
        let h_colored = vec![
            "\x1b[35msrc/a.rs\x1b[0m".to_owned(),
            "\x1b[32m9\x1b[0m-fn run() {".to_owned(),
            "\x1b[32m10\x1b[0m:let \x1b[1;31minvoice\x1b[0m = compute(1);".to_owned(),
            "".to_owned(),
            "\x1b[35msrc/b.rs\x1b[0m".to_owned(),
            "\x1b[32m7\x1b[0m:return \x1b[1;31minvoice\x1b[0m;".to_owned(),
        ];
        let (hp, _) = run_search(dir.path(), &h_words, "rg", &h_plain, 4096, true);
        let (hc, _) = run_search(dir.path(), &h_words, "rg", &h_colored, 4096, true);
        assert_eq!(hc, hp);
        assert_valid_stdout_pointers(&hc, h_colored.len());
    }

    #[test]
    fn whitespace_normalized_collapse_merges_aligned_boilerplate_without_merging_quoted_spaces() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        for f in [
            "src/table.rs",
            "src/strings.rs",
            "src/u0.rs",
            "src/u1.rs",
            "src/u2.rs",
            "src/u3.rs",
        ] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }

        // 1. Intra-file aligned boilerplate with mixed tabs/spaces collapses cleanly
        //    while keeping exact line numbers and distinct lines intact.
        let aligned = vec![
            "src/table.rs:4:    status:    OrderStatus::Pending,".to_owned(),
            "src/table.rs:12:\tstatus:\t\tOrderStatus::Pending,".to_owned(),
            "src/table.rs:27:  status:  OrderStatus::Pending,".to_owned(),
            "src/table.rs:35:    status: OrderStatus::Pending,".to_owned(),
            "src/table.rs:40:    status: OrderStatus::Committed,".to_owned(),
        ];
        let (report, _) = run_search(
            dir.path(),
            &["rg", "-n", "OrderStatus", "src"],
            "rg",
            &aligned,
            4096,
            true,
        );
        assert!(
            report.contains("  L4,12,27,35: status: OrderStatus::Pending,\n")
                && report.contains("  40:    status: OrderStatus::Committed,\n")
                && !report.contains("more in file"),
            "{report}"
        );
        assert_valid_stdout_pointers(&report, aligned.len());

        // 2. Quoted whitespace differences (`"a  b"` vs `"a b"`) NEVER collapse together.
        let quoted = vec![
            "src/strings.rs:1:    assert_eq!(out, \"needle  spaced\");".to_owned(),
            "src/strings.rs:2:    assert_eq!(out, \"needle  spaced\");".to_owned(),
            "src/strings.rs:3:    assert_eq!(out, \"needle  spaced\");".to_owned(),
            "src/strings.rs:4:    assert_eq!(out, \"needle spaced\");".to_owned(),
        ];
        let (q_report, _) = run_search(
            dir.path(),
            &["rg", "-n", "needle", "src"],
            "rg",
            &quoted,
            4096,
            true,
        );
        assert!(
            q_report.contains("  L1-3: assert_eq!(out, \"needle  spaced\");\n")
                && q_report.contains("  4:    assert_eq!(out, \"needle spaced\");\n"),
            "{q_report}"
        );

        // 3. Multi-file identical boilerplate differing only in unquoted spacing (including
        //    short `use crate::foo;` imports >= 10 B) collapses with exact line & stdout refs.
        let multi = vec![
            "src/u0.rs:1:use   crate::billing;".to_owned(),
            "src/u1.rs:3:use crate::billing;".to_owned(),
            "src/u2.rs:5:use\tcrate::billing;".to_owned(),
            "src/u3.rs:8:use  crate::billing;".to_owned(),
        ];
        let (m_report, _) = run_search(
            dir.path(),
            &["rg", "-n", "billing", "src"],
            "rg",
            &multi,
            4096,
            true,
        );
        assert!(
            m_report.contains("  same line in 3 more files: src/u1.rs:3 [stdout:2], src/u2.rs:5 [stdout:3], src/u3.rs:8 [stdout:4]\n"),
            "{m_report}"
        );
        assert_valid_stdout_pointers(&m_report, multi.len());
    }
}
