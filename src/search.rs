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
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

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
const CROSS_DEDUP_MIN_LEN: usize = 20;
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
    let mut parsed = Parsed::default();
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
            for (i, c) in body.char_indices() {
                let flag = format!("-{c}");
                if let Some(flag) = known(&flag, spec.valued) {
                    let rest = &body[i + c.len_utf8()..];
                    let value = if rest.is_empty() { args.next()? } else { rest };
                    parsed.seen.push((flag, Some(value)));
                    break;
                }
                parsed.seen.push((known(&flag, spec.flags)?, None));
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

#[derive(Default)]
struct PatternMatcher {
    regexes: Vec<Regex>,
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
        if self.regexes.len() >= 16 {
            return;
        }
        if let Some(re) = compile_pattern(pattern, fixed, ignore_case, smart_case, word) {
            self.regexes.push(re);
        }
    }

    /// The earliest match span `(start, end)` in `haystack`, optionally preferring
    /// a match at or after `from_byte`.
    fn find(&self, haystack: &str, from_byte: Option<usize>) -> Option<(usize, usize)> {
        if self.regexes.is_empty() {
            return None;
        }
        if let Some(at) = from_byte {
            let at = haystack.floor_char_boundary(at.min(haystack.len()));
            let after = self
                .regexes
                .iter()
                .filter_map(|re| {
                    re.find(&haystack[at..])
                        .map(|m| (at + m.start(), at + m.end()))
                })
                .min_by_key(|&(s, _)| s);
            if after.is_some() {
                return after;
            }
        }
        self.regexes
            .iter()
            .filter_map(|re| re.find(haystack).map(|m| (m.start(), m.end())))
            .min_by_key(|&(s, _)| s)
    }
}

fn compile_pattern(
    pattern: &str,
    fixed: bool,
    ignore_case: bool,
    smart_case: bool,
    word: bool,
) -> Option<Regex> {
    if pattern.is_empty() || pattern.len() > 2048 {
        return None;
    }
    let ci = ignore_case || (smart_case && !pattern.chars().any(char::is_uppercase));
    let build = |expr: &str| {
        RegexBuilder::new(expr)
            .case_insensitive(ci)
            .size_limit(64 * 1024)
            .dfa_size_limit(64 * 1024)
            .build()
            .ok()
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
    /// Up to 2 exact original occurrences: `(seq, display_line, stdout_line, clipped)`.
    samples: Vec<(usize, String, usize, bool)>,
    /// File line locations for collapsed `L12,18,25: <body>` rendering (`count >= 3`).
    locs: Vec<Loc>,
    last_line: Option<usize>,
    numbered: bool,
    /// Trimmed (and windowed if long) body text used when `count >= INTRA_FOLD_MIN`.
    folded_body: String,
    first_stdout: usize,
    first_seq: usize,
    clipped: bool,
}

impl MatchSlot {
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
        if let Some(c) = col {
            if self.locs.len() < MAX_FOLDED_RANGES {
                self.locs.push(Loc::LineCol(line, c));
            }
            return;
        }
        if let Some(Loc::Range(_, end)) = self.locs.last_mut()
            && *end + 1 == line
        {
            *end = line;
            return;
        }
        if self.locs.len() < MAX_FOLDED_RANGES {
            self.locs.push(Loc::Range(line, line));
        }
    }

    fn format_folded(&self, pointers: bool) -> String {
        let mut line = if self.numbered && !self.locs.is_empty() {
            let locs = format_locs(&self.locs, self.count);
            if self.folded_body.is_empty() {
                format!("L{locs}:")
            } else {
                format!("L{locs}: {}", self.folded_body)
            }
        } else {
            // Unnumbered (or duplicate-line-number) fallback: keep exact original text.
            let body = if self.numbered {
                self.folded_body.as_str()
            } else {
                self.samples
                    .first()
                    .map_or(self.folded_body.as_str(), |(_, s, _, _)| s.trim())
            };
            if body.is_empty() {
                format!("×{}:", self.count)
            } else {
                format!("×{}: {body}", self.count)
            }
        };
        if pointers && self.clipped {
            line.push_str(&format!(" [stdout:{}]", self.first_stdout));
        }
        line
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

fn format_locs(locs: &[Loc], total_count: usize) -> String {
    let mut out = String::new();
    let mut covered = 0usize;
    for (i, loc) in locs.iter().enumerate() {
        let (piece, span) = match *loc {
            Loc::Range(a, b) if a == b => (format!("{a}"), 1),
            Loc::Range(a, b) if b == a + 1 => (format!("{a},{b}"), 2),
            Loc::Range(a, b) => (format!("{a}-{b}"), b - a + 1),
            Loc::LineCol(l, c) => (format!("{l}:{c}"), 1),
        };
        if i >= 3 && out.len() + piece.len() + 1 > MAX_LOC_STR_BYTES {
            break;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&piece);
        covered += span;
    }
    if covered < total_count {
        out.push_str(&format!(" (+{} more)", total_count - covered));
    }
    out
}

struct Group {
    path: String,
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
    /// When every match in this file has the exact same trimmed body (captured in slot 0),
    /// return that slot. Files with any other distinct match return `None`.
    fn single_body(&self) -> Option<&MatchSlot> {
        (self.slots.len() == 1 && self.slots[0].count == self.matches).then_some(&self.slots[0])
    }

    /// Render up to `k` display lines for this file, returning the rendered lines
    /// and the total number of matches those lines represent.
    fn render_lines(&self, k: usize, pointers: bool) -> (Vec<String>, usize) {
        if k == 0 {
            return (Vec::new(), 0);
        }
        let mut items: Vec<(usize, usize, String)> = Vec::new();
        for slot in &self.slots {
            if slot.count >= INTRA_FOLD_MIN {
                items.push((slot.first_seq, slot.count, slot.format_folded(pointers)));
            } else {
                for &(seq, ref display, stdout_line, clipped) in &slot.samples {
                    let line = if pointers && clipped {
                        format!("{display} [stdout:{stdout_line}]")
                    } else {
                        display.clone()
                    };
                    items.push((seq, 1, line));
                }
            }
        }
        items.sort_by_key(|(seq, _, _)| *seq);
        let mut out = Vec::with_capacity(k.min(items.len()));
        let mut represented = 0usize;
        for (_, count, line) in items.into_iter().take(k) {
            out.push(line);
            represented += count;
        }
        (out, represented)
    }

    /// Compact inline citation for a pure-boilerplate follower file.
    fn follower_ref(&self, display_path: &str, pointers: bool) -> String {
        let ptr = pointer(pointers, self);
        let Some(slot) = self.single_body() else {
            return format!("{display_path} ×{}{ptr}", self.matches);
        };
        if self.matches == 1 {
            if slot.numbered
                && let Some(loc) = slot.locs.first()
            {
                return match *loc {
                    Loc::Range(l, _) => format!("{display_path}:{l}{ptr}"),
                    Loc::LineCol(l, c) => format!("{display_path}:{l}:{c}{ptr}"),
                };
            }
            return format!("{display_path}{ptr}");
        }
        if slot.numbered && !slot.locs.is_empty() {
            let locs = format_locs(&slot.locs, slot.count);
            format!("{display_path} ×{} L{locs}{ptr}", self.matches)
        } else {
            format!("{display_path} ×{}{ptr}", self.matches)
        }
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
    keep: Vec<String>,
    groups: Vec<Group>,
    index: HashMap<String, usize>,
    current: Option<usize>,
    pending: Vec<(usize, String)>,
    other: Option<(usize, String)>,
    unparsed: usize,
    records: usize,
    matches: usize,
    context: usize,
    exists: HashMap<String, bool>,
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
        let line = text.trim_end_matches(['\n', '\r']);
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
            && let Some(rest) = line.strip_prefix(self.groups[c].path.as_str())
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
            let prefix = format!("{path}-");
            for (m, pending) in std::mem::take(&mut self.pending) {
                if pending.starts_with(&prefix) {
                    self.context_line(c, m);
                } else {
                    self.unparsed_line(m, pending);
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
        self.index.insert(path.to_owned(), self.groups.len());
        self.groups.push(Group {
            path: path.to_owned(),
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
        Some(self.groups.len() - 1)
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
                        && let Some((_, sample, _, _)) = slot.samples.first()
                    {
                        let full = sample.trim().to_owned();
                        slot.hash = hash64(&full);
                        slot.raw_len = full.len();
                        if full.len() <= INLINE_KEY_BYTES {
                            slot.raw_exact = Some(full.clone());
                            slot.raw_sha = None;
                        } else {
                            slot.raw_sha = Some(sha256_bytes(&full));
                            slot.raw_exact = None;
                        }
                        slot.folded_body = full;
                        slot.numbered = false;
                        slot.locs.clear();
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

        let raw_trimmed = content.trim();
        let hash = hash64(raw_trimmed);
        if let Some(slot) = group
            .slots
            .iter_mut()
            .find(|slot| slot.matches_raw(hash, raw_trimmed))
        {
            slot.count += 1;
            if let Some(l) = line_no {
                slot.push_loc(l, col, line_mode.only_matching);
            } else {
                slot.numbered = false;
            }
            if slot.samples.len() < INTRA_FOLD_MIN - 1 {
                let (display, clipped) = window_line(text, prefix_str, content, col, &self.matcher);
                slot.samples.push((seq, display, n, clipped));
            }
        } else if group.slots.len() < MAX_SLOTS {
            let (display, clipped) = window_line(text, prefix_str, content, col, &self.matcher);
            let folded_body = if clipped {
                window_snippet(raw_trimmed, col, &self.matcher, WINDOW_SNIPPET_BYTES)
            } else {
                raw_trimmed.to_owned()
            };
            let (raw_exact, raw_sha) = if raw_trimmed.len() <= INLINE_KEY_BYTES {
                (Some(raw_trimmed.to_owned()), None)
            } else {
                (None, Some(sha256_bytes(raw_trimmed)))
            };
            let mut slot = MatchSlot {
                hash,
                raw_len: raw_trimmed.len(),
                raw_exact,
                raw_sha,
                count: 1,
                samples: vec![(seq, display, n, clipped)],
                locs: Vec::new(),
                last_line: None,
                numbered: line_no.is_some(),
                folded_body,
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
        let found = self.base.join(candidate).is_file();
        self.exists.insert(candidate.to_owned(), found);
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
            if line.starts_with(self.groups[c].path.as_str())
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
            let path = line[..end].to_owned();
            let Some(c) = self.group(&path, n) else {
                return;
            };
            // Lines printed before a file's match are its leading context.
            for (m, before) in std::mem::take(&mut self.mode().pending) {
                match parts(&before, end, numbered).filter(|(matched, ..)| !matched) {
                    Some((_, number, text)) if before.starts_with(&path) => {
                        self.add_line(c, m, number, false, text)
                    }
                    _ => self.mode().failed = true,
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
        let path = line[..end].to_owned();
        let Some(c) = self.group(&path, *first) else {
            return;
        };
        for (m, line) in &pending {
            match parts(line, end, numbered) {
                Some((false, number, text)) if line.starts_with(&path) => {
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
                    window_line(text, prefix_str, content, col, &self.matcher).0
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
            header.push_str(&format!(
                ", {}",
                plural(blocks.list.len(), "block", "blocks")
            ));
        }
        if self.context > 0 {
            header.push_str(&format!(", {} context lines", self.context));
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
        let current = self.groups.iter().map(|g| (g.path.as_str(), g.matches));
        let compared = compare(previous, "search", current, "no longer matched");
        let other = self.other.as_ref().map_or_else(String::new, |(n, line)| {
            format!(
                "{} other line(s), first: {}{}\n",
                self.unparsed,
                clip(line),
                if pointers {
                    format!(" [stdout:{n}]")
                } else {
                    String::new()
                }
            )
        });
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
        let other: String = collapsed
            .iter()
            .map(|(prefix, files, matches, first, last)| {
                format!(
                    "{prefix}/ {} in {} collapsed{}\n",
                    plural(*matches, "match", "matches"),
                    plural(*files, "file", "files"),
                    if pointers {
                        format!(" [stdout:{first}-{last}]")
                    } else {
                        String::new()
                    }
                )
            })
            .chain(std::iter::once(other))
            .collect();

        let hoist = choose_prefix_hoist(&order, !collapsed.is_empty());
        let build_report = |hoist: &PrefixHoist| -> (String, (usize, usize)) {
            let mut full_header = format!("{header}{}\n", hoist.header_suffix());
            full_header.push_str(&compared);
            let room = budget.saturating_sub(full_header.len() + other.len() + 96);
            if let Some(blocks) = &self.blocks {
                let (body, tier) = block_body(&order, &blocks.list, room, pointers, hoist);
                return (format!("{full_header}{body}{other}"), tier);
            }
            let (body, tier) = search_body(&order, room, pointers, hoist);
            (format!("{full_header}{body}{other}"), tier)
        };
        let (report, hoist_tier) = build_report(&hoist);
        // Only keep prefix hoisting when it achieves a richer detail tier within budget
        // or is strictly smaller at the same detail tier.
        if !matches!(hoist, PrefixHoist::None) {
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
enum PrefixHoist {
    None,
    /// Every displayed file starts with `prefix` (ending with `/`). Header appends
    /// `, under {prefix}` and paths render relative to it.
    Full(String),
    /// A majority of displayed files share `prefix` (ending with `/`), while other
    /// files or collapsed trees sit outside it. Header appends ` (… = {prefix})`
    /// and matching paths render as `…/{rel}` so non-matching paths stay unambiguous.
    Partial(String),
}

impl PrefixHoist {
    fn header_suffix(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::Full(prefix) => format!(", under {prefix}"),
            Self::Partial(prefix) => format!(" (… = {prefix})"),
        }
    }

    fn display_path<'a>(&self, path: &'a str) -> Cow<'a, str> {
        match self {
            Self::None => Cow::Borrowed(path),
            Self::Full(prefix) => Cow::Borrowed(path.strip_prefix(prefix.as_str()).unwrap_or(path)),
            Self::Partial(prefix) => match path.strip_prefix(prefix.as_str()) {
                Some(rel) => Cow::Owned(format!("…/{rel}")),
                None => Cow::Borrowed(path),
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

fn common_dir_prefix(paths: &[&str]) -> String {
    fn dir_of(p: &str) -> &str {
        p.rfind('/').map_or("", |i| &p[..=i])
    }
    let Some((first, rest)) = paths.split_first() else {
        return String::new();
    };
    let mut common = dir_of(first);
    for path in rest {
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
    common.to_owned()
}

fn choose_prefix_hoist(order: &[&Group], has_collapsed: bool) -> PrefixHoist {
    if order.len() < 2 {
        return PrefixHoist::None;
    }
    if !has_collapsed {
        let paths: Vec<&str> = order.iter().map(|g| g.path.as_str()).collect();
        let common = common_dir_prefix(&paths);
        // Require at least 2 real directory components (ignoring leading `./`) so shallow
        // roots like `src/` or `./src/` stay inline, and verify net byte savings.
        if real_dir_depth(&common) >= 2
            && common.len() >= 10
            && (order.len() - 1) * common.len() > ", under ".len()
        {
            return PrefixHoist::Full(common);
        }
    }
    if order.len() < 3 {
        return PrefixHoist::None;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for group in order {
        for (i, _) in group.path.match_indices('/') {
            let prefix = &group.path[..=i];
            if prefix.len() >= 12 && real_dir_depth(prefix) >= 2 {
                *counts.entry(prefix).or_default() += 1;
            }
        }
    }
    let marker_len = "…/".len();
    let header_fixed = " (… = )".len();
    let mut best: Option<(&str, usize)> = None;
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
        PrefixHoist::Partial(prefix.to_owned())
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
    fn group_indices(&self) -> Vec<usize> {
        match self {
            Self::Single(i) => vec![*i],
            Self::Boilerplate { leader, followers } => std::iter::once(*leader)
                .chain(followers.iter().copied())
                .collect(),
        }
    }
}

fn build_display_units(order: &[&Group], pointers: bool, hoist: &PrefixHoist) -> Vec<DisplayUnit> {
    // Group pure single-body files sharing the exact same trimmed line of sufficient length.
    let mut buckets: Vec<Vec<usize>> = Vec::new();
    let mut by_hash: HashMap<(u64, usize), Vec<usize>> = HashMap::new();
    for (idx, group) in order.iter().enumerate() {
        let Some(slot) = group.single_body() else {
            continue;
        };
        if slot.clipped || slot.folded_body.len() < CROSS_DEDUP_MIN_LEN {
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
    for indices in buckets {
        if indices.len() < CROSS_DEDUP_MIN_FILES {
            continue;
        }
        let leader = indices[0];
        let followers = indices[1..].to_vec();
        let plain_bytes: usize = indices
            .iter()
            .map(|&i| render_single(order[i], 1, pointers, hoist).len())
            .sum();
        let dedup_bytes =
            render_boilerplate(order, leader, &followers, 1, None, pointers, hoist).len();
        if dedup_bytes < plain_bytes {
            for &f in &followers {
                absorbed[f] = true;
            }
            leader_followers.insert(leader, followers);
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

fn render_single(g: &Group, k: usize, pointers: bool, hoist: &PrefixHoist) -> String {
    let path = hoist.display_path(&g.path);
    let mut text = format!("{path} ×{}{}\n", g.matches, pointer(pointers, g));
    if k == 0 {
        return text;
    }
    let (lines, represented) = g.render_lines(k, pointers);
    for line in lines {
        text.push_str(&format!("  {line}\n"));
    }
    if g.matches > represented {
        text.push_str(&format!("  +{} more in file\n", g.matches - represented));
    }
    text
}

fn render_boilerplate(
    order: &[&Group],
    leader: usize,
    followers: &[usize],
    k: usize,
    max_followers: Option<usize>,
    pointers: bool,
    hoist: &PrefixHoist,
) -> String {
    let mut text = render_single(order[leader], k, pointers, hoist);
    if followers.is_empty() {
        return text;
    }
    let cap = max_followers
        .unwrap_or(followers.len())
        .min(followers.len());
    let shown: Vec<String> = followers[..cap]
        .iter()
        .map(|&i| {
            let g = order[i];
            let display = hoist.display_path(&g.path);
            g.follower_ref(&display, pointers)
        })
        .collect();
    text.push_str(&format!(
        "  same line in {}: {}",
        plural(followers.len(), "more file", "more files"),
        shown.join(", ")
    ));
    if cap < followers.len() {
        let rest: Vec<&Group> = followers[cap..].iter().map(|&i| order[i]).collect();
        let rest_matches: usize = rest.iter().map(|g| g.matches).sum();
        text.push_str(&format!(
            ", +{} ({}){}",
            plural(rest.len(), "more file", "more files"),
            plural(rest_matches, "match", "matches"),
            stdout_span(pointers, &rest),
        ));
    }
    text.push('\n');
    text
}

fn render_unit(
    order: &[&Group],
    unit: &DisplayUnit,
    k: usize,
    max_followers: Option<usize>,
    pointers: bool,
    hoist: &PrefixHoist,
) -> String {
    match unit {
        DisplayUnit::Single(i) => render_single(order[*i], k, pointers, hoist),
        DisplayUnit::Boilerplate { leader, followers } => {
            render_boilerplate(order, *leader, followers, k, max_followers, pointers, hoist)
        }
    }
}

fn search_body(
    order: &[&Group],
    room: usize,
    pointers: bool,
    hoist: &PrefixHoist,
) -> (String, (usize, usize)) {
    let units = build_display_units(order, pointers, hoist);
    for k in [SHOWN, 5, 3, 2, 1] {
        let mut body = String::new();
        for unit in &units {
            body.push_str(&render_unit(order, unit, k, None, pointers, hoist));
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
        let mut body = String::new();
        let mut listed = 0;
        for unit in &units {
            let text = render_unit(order, unit, k, Some(cap), pointers, hoist);
            if body.len() + text.len() > detail_budget {
                break;
            }
            body.push_str(&text);
            listed += 1;
        }
        (body, listed)
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
    let remaining: Vec<&Group> = units[listed..]
        .iter()
        .flat_map(DisplayUnit::group_indices)
        .map(|i| order[i])
        .collect();
    if remaining.is_empty() {
        return (body, (1, listed_files));
    }

    // If every remaining file fits as a 1-line header (`path ×N [stdout:A-B]`), list
    // them all so no filenames are lost.
    let rem_room = room.saturating_sub(body.len());
    let mut headers_only = String::new();
    let mut all_fit = true;
    for group in &remaining {
        headers_only.push_str(&render_single(group, 0, pointers, hoist));
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
        let line = render_single(group, 0, pointers, hoist);
        if body.len() + line.len() + fold_reserve > room {
            break;
        }
        body.push_str(&line);
        rem_listed += 1;
    }
    let tail = &remaining[rem_listed..];
    if !tail.is_empty() {
        let tag = stdout_span(pointers, tail);
        body.push_str(&fold(tail, room.saturating_sub(body.len()), &tag, hoist));
    }
    (body, (0, listed_files + rem_listed))
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
    if a == b {
        format!(" [stdout:{a}]")
    } else {
        format!(" [stdout:{a}-{b}]")
    }
}

/// The files that did not fit, summarized by directory, largest first, within
/// `room` bytes for the directory names, ending with `tag`.
fn fold(rest: &[&Group], room: usize, tag: &str, hoist: &PrefixHoist) -> String {
    let mut folders: HashMap<String, (usize, usize)> = HashMap::new();
    for group in rest {
        let shown = hoist.display_path(&group.path);
        let folder = shown.rsplit_once('/').map_or(".", |(dir, _)| dir);
        let entry = folders.entry(folder.to_owned()).or_default();
        entry.0 += 1;
        entry.1 += group.matches;
    }
    let mut folders: Vec<_> = folders.into_iter().collect();
    folders.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(&b.0)));
    let matches: usize = rest.iter().map(|g| g.matches).sum();
    let head = format!(
        "+{} with {}",
        plural(rest.len(), "more file", "more files"),
        plural(matches, "match", "matches"),
    );
    // Reserve space for `head`, `" in "`, `", …"`, `tag` and `'\n'` so `fold` fits `room`.
    let fixed = head.len() + " in ".len() + ", …".len() + tag.len() + 1;
    let names_room = room.saturating_sub(fixed);
    let mut names = String::new();
    let mut named = 0;
    for (folder, (files, matches)) in &folders {
        let item = format!("{folder}/ {} ×{matches}", plural(*files, "file", "files"));
        let sep = if names.is_empty() { 0 } else { 2 };
        if names.len() + sep + item.len() > names_room {
            break;
        }
        if !names.is_empty() {
            names.push_str(", ");
        }
        names.push_str(&item);
        named += 1;
    }
    format!(
        "{head}{}{}{}{tag}\n",
        if named > 0 { " in " } else { "" },
        names,
        if named < folders.len() && named > 0 {
            ", …"
        } else {
            ""
        }
    )
}

/// A block report: each file's match and block counts, its first block in full,
/// then where its other blocks are. Fewer lines, then files alone, then folders
/// make it fit `room`.
fn block_body(
    order: &[&Group],
    list: &[Block],
    room: usize,
    pointers: bool,
    hoist: &PrefixHoist,
) -> (String, (usize, usize)) {
    let tag = |a: usize, b: usize| match (pointers, a == b) {
        (false, _) => String::new(),
        (true, true) => format!(" [stdout:{a}]"),
        (true, false) => format!(" [stdout:{a}-{b}]"),
    };
    let entry = |block: &Block| match (block.from, block.to) {
        (Some(from), Some(to)) if from == to => format!("L{from}{}", tag(block.start, block.end)),
        (Some(from), Some(to)) => format!("L{from}-{to}{}", tag(block.start, block.end)),
        _ => tag(block.start, block.end).trim_start().to_owned(),
    };
    let render = |g: &Group, lines: usize, entries: usize| {
        let path = hoist.display_path(&g.path);
        let mut text = format!(
            "{path} ×{} in {}{}\n",
            g.matches,
            plural(g.blocks.len(), "block", "blocks"),
            pointer(pointers, g)
        );
        // A cut block starts near its first match, and its range covers it whole.
        let Some(first) = g.blocks.first().filter(|_| lines > 0).map(|&b| &list[b]) else {
            return text;
        };
        let shown = lines.min(g.shown.len());
        let lead = g.lead.unwrap_or(0);
        let start = lead.saturating_sub(lines / 4).min(g.shown.len() - shown);
        for line in &g.shown[start..start + shown] {
            text.push_str(&format!("  {line}\n"));
        }
        let size = first.end - first.start + 1;
        if size > shown {
            let whole = entry(first);
            text.push_str(&if whole.is_empty() {
                format!("  +{}\n", plural(size - shown, "more line", "more lines"))
            } else {
                format!("  whole block {whole}\n")
            });
        }
        if let [_, rest @ ..] = g.blocks.as_slice()
            && let (Some(&a), Some(&b)) = (rest.first(), rest.last())
        {
            text.push_str(&format!(
                "  +{}{}",
                plural(rest.len(), "more block", "more blocks"),
                tag(list[a].start, list[b].end)
            ));
            let named: Vec<String> = rest
                .iter()
                .take(entries)
                .map(|&i| entry(&list[i]))
                .filter(|e| !e.is_empty())
                .collect();
            if !named.is_empty() {
                text.push_str(&format!(": {}", named.join(", ")));
                if named.len() < rest.len() {
                    text.push_str(", …");
                }
            }
            text.push('\n');
        }
        text
    };
    for (lines, entries) in [(BLOCK_LINES, 32), (12, 12), (6, 6), (3, 3), (1, 0)] {
        let mut body = String::new();
        for group in order {
            body.push_str(&render(group, lines, entries));
            if body.len() > room {
                break;
            }
        }
        if body.len() <= room {
            return (body, (lines + 1, order.len()));
        }
    }
    // Name files alone while they fit, then fold the rest by directory.
    let mut body = String::new();
    let mut listed = 0;
    for group in order {
        let text = render(group, 0, 0);
        if body.len() + text.len() > room * 2 / 3 {
            break;
        }
        body.push_str(&text);
        listed += 1;
    }
    let rest = &order[listed..];
    if let (Some(a), Some(b)) = (
        rest.iter().map(|g| g.first).min(),
        rest.iter().map(|g| g.last).max(),
    ) {
        body.push_str(&fold(
            rest,
            room.saturating_sub(body.len()),
            &tag(a, b),
            hoist,
        ));
    }
    (body, (0, listed))
}

/// Parse an optional leading `line:` (or `line:col:` when `--column`/`--vimgrep` is
/// active) prefix from the text after `path:`.
fn split_line_prefix(text: &str, mode: LineMode) -> (Option<usize>, Option<usize>, &str, &str) {
    if mode.explicit_numbered == Some(false) {
        return (None, None, "", text);
    }
    let bytes = text.as_bytes();
    if bytes
        .first()
        .is_none_or(|b| !b.is_ascii_digit() || *b == b'0')
    {
        return (None, None, "", text);
    }
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if bytes.get(digits) != Some(&b':') {
        return (None, None, "", text);
    }
    let Ok(line_no) = text[..digits].parse::<usize>() else {
        return (None, None, "", text);
    };
    let after_line = digits + 1;
    if mode.has_col {
        let rest = &text[after_line..];
        let rbytes = rest.as_bytes();
        if rbytes
            .first()
            .is_some_and(|b| b.is_ascii_digit() && *b != b'0')
        {
            let cdigits = rbytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if rbytes.get(cdigits) == Some(&b':')
                && let Ok(col) = rest[..cdigits].parse::<usize>()
            {
                let after_col = after_line + cdigits + 1;
                return (
                    Some(line_no),
                    Some(col),
                    &text[..after_col],
                    &text[after_col..],
                );
            }
        }
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
    matcher: &PatternMatcher,
) -> (String, bool) {
    if raw.len() <= LINE_BYTES {
        return (raw.to_owned(), false);
    }
    let budget = WINDOW_SNIPPET_BYTES.min(LINE_BYTES.saturating_sub(prefix.len()).max(80));
    let snippet = window_snippet(content, col, matcher, budget);
    (format!("{prefix}{snippet}"), true)
}

fn window_snippet(
    content: &str,
    col: Option<usize>,
    matcher: &PatternMatcher,
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
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let number = text[..digits].parse().ok()?;
    let sep = text[digits..]
        .chars()
        .next()
        .filter(|c| matches!(c, ':' | '-'))?;
    Some((number, sep))
}

/// Whether a line whose file name ends at `len` is a match, its line number, and
/// the text a report shows: from the number on, or from the separator on when
/// lines carry no numbers.
fn parts(line: &str, len: usize, numbered: bool) -> Option<(bool, Option<usize>, &str)> {
    let rest = line.get(len..)?;
    let sep = rest.chars().next().filter(|c| matches!(c, ':' | '-'))?;
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

fn pointer(pointers: bool, group: &Group) -> String {
    match (pointers, group.first == group.last) {
        (false, _) => String::new(),
        (true, true) => format!(" [stdout:{}]", group.first),
        (true, false) => format!(" [stdout:{}-{}]", group.first, group.last),
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
}
