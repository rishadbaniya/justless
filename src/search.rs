//! Group search output by file and select the search and listing profiles.
//!
//! `rg`, `grep -r`, `git grep`, `ag` and `ack` print one record per line as
//! `path:line:text` or `path:text`. A path is accepted only when it names an existing
//! file, so output without paths, such as a single-file grep, keeps the ordinary
//! preview. Shown lines keep the exact text after the path.
//!
//! With context options, matches use `path:line:text`, context lines
//! `path-line-text`, and blocks of them are separated by `--`. Block mode reads
//! those, the numbered lines of one file without its name and `rg --heading`, and
//! reports blocks instead of matching lines.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::filters::Profile;
use crate::summary::{Item, Summary, short};
use crate::workspace::Workspace;

const SHOWN: usize = 8;
const LINE_BYTES: usize = 200;
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
        --line-regexp -F --fixed-strings -n --line-number -N --no-line-number -H \
        --with-filename --hidden -. -u --unrestricted --no-ignore --no-ignore-vcs \
        --no-ignore-parent --no-ignore-dot --no-ignore-global --no-ignore-exclude \
        --no-ignore-files -L --follow -U --multiline --multiline-dotall -P --pcre2 -o \
        --only-matching -v --invert-match --column -a --text --no-heading --heading --no-messages \
        --trim --crlf --sort-files --binary --no-config --one-file-system --vimgrep \
        --max-columns-preview -l --files-with-matches --files-without-match --files",
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
    flags: "-a -A -l -R -h -F -p -t -S -r -1 -U -c -u -G -d -n --all --almost-all \
        --recursive --human-readable --classify --group-directories-first",
    valued: "--sort",
    optional: "--color",
    lists: "",
};

const TREE: Spec = Spec {
    flags: "-a -d -f -F -C -n -l -x -v -t -r -c -U --dirsfirst --filesfirst --noreport \
        --gitignore --prune --matchdirs --ignore-case",
    valued: "-L -I -P --filelimit --charset",
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
            } else if let Some(flag) = known(key, spec.optional)
                .or_else(|| inline.is_none().then(|| known(key, spec.flags)).flatten())
            {
                parsed.seen.push((flag, inline));
            } else {
                return None;
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
        ("git", ["grep", rest @ ..]) => (Some(&GIT_GREP), rest),
        ("git", ["ls-files", rest @ ..]) => (Some(&LS_FILES), rest),
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
        ("git", ["grep", rest @ ..]) => (&GIT_GREP, rest),
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
    let words = crate::filters::unwrap_tool(words);
    select(words)?;
    if basename(words[0]) == "ls" {
        return Some(parse(&words[1..], &LS)?.has(&["-l", "-R", "--recursive"]));
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

struct Group {
    path: String,
    matches: usize,
    first: usize,
    last: usize,
    /// The first matching lines, or in block mode the first block's lines.
    shown: Vec<String>,
    /// Indexes into `Blocks::list`.
    blocks: Vec<usize>,
    /// The first match's place in `shown`, which keeps only the nearest lines
    /// before it.
    lead: Option<usize>,
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
}

impl Search {
    /// `base` is the directory relative paths in the output resolve against.
    /// Dependency and build directories named in `words` are not collapsed.
    pub fn new(base: &Path, words: &[&str]) -> Self {
        let tool = crate::filters::unwrap_tool(words);
        let blocks = searched(tool)
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
            blocks: Vec::new(),
            lead: None,
        });
        Some(self.groups.len() - 1)
    }

    fn hit(&mut self, c: usize, text: &str, n: usize) {
        let group = &mut self.groups[c];
        group.matches += 1;
        group.first = group.first.min(n);
        group.last = group.last.max(n);
        if group.shown.len() < SHOWN {
            group.shown.push(clip(text));
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
                group.shown.push(clip(text));
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
        };
        header.push('\n');
        let current = self.groups.iter().map(|g| (g.path.as_str(), g.matches));
        header.push_str(&compare(previous, "search", current, "no longer matched"));
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
        let room = budget.saturating_sub(header.len() + other.len() + 96);
        if let Some(blocks) = &self.blocks {
            let body = block_body(&order, &blocks.list, room, pointers);
            return Some((format!("{header}{body}{other}"), summary));
        }
        let block = |g: &Group, k: usize| {
            let mut text = format!("{} ×{}{}\n", g.path, g.matches, pointer(pointers, g));
            for line in g.shown.iter().take(k) {
                text.push_str(&format!("  {line}\n"));
            }
            let shown = k.min(g.shown.len());
            if g.matches > shown {
                text.push_str(&format!("  +{} more in file\n", g.matches - shown));
            }
            text
        };
        for k in [SHOWN, 5, 3, 2, 1] {
            let mut body = String::new();
            for group in &order {
                body.push_str(&block(group, k));
                if body.len() > room {
                    break;
                }
            }
            if body.len() <= room {
                return Some((format!("{header}{body}{other}"), summary));
            }
        }
        let mut body = String::new();
        let mut listed = 0;
        for group in &order {
            let text = block(group, 2);
            if body.len() + text.len() > room * 2 / 3 {
                break;
            }
            body.push_str(&text);
            listed += 1;
        }
        body.push_str(&fold(&order[listed..], room.saturating_sub(body.len()), ""));
        Some((format!("{header}{body}{other}"), summary))
    }
}

/// The files that did not fit, summarized by directory, largest first, within
/// `room` bytes for the directory names, ending with `tag`.
fn fold(rest: &[&Group], room: usize, tag: &str) -> String {
    let mut folders: HashMap<&str, (usize, usize)> = HashMap::new();
    for group in rest {
        let folder = group.path.rsplit_once('/').map_or(".", |(dir, _)| dir);
        let entry = folders.entry(folder).or_default();
        entry.0 += 1;
        entry.1 += group.matches;
    }
    let mut folders: Vec<_> = folders.into_iter().collect();
    folders.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(b.0)));
    let mut names = String::new();
    let mut named = 0;
    for (folder, (files, matches)) in &folders {
        let item = format!("{folder}/ {} ×{matches}", plural(*files, "file", "files"));
        if names.len() + item.len() + 2 > room {
            break;
        }
        if !names.is_empty() {
            names.push_str(", ");
        }
        names.push_str(&item);
        named += 1;
    }
    let matches: usize = rest.iter().map(|g| g.matches).sum();
    format!(
        "+{} with {}{}{}{}{tag}\n",
        plural(rest.len(), "more file", "more files"),
        plural(matches, "match", "matches"),
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
fn block_body(order: &[&Group], list: &[Block], room: usize, pointers: bool) -> String {
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
        let mut text = format!(
            "{} ×{} in {}{}\n",
            g.path,
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
            return body;
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
        body.push_str(&fold(rest, room.saturating_sub(body.len()), &tag(a, b)));
    }
    body
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
}
