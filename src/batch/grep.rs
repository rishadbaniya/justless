//! `grep` pieces of a read list, rerun with the grep the shell ran.
//!
//! Claude Code's shell defines `grep` as a function that runs the ugrep built
//! into Claude Code, with options of its own. That ugrep searches several files
//! at once and prints each file's lines together in the order the files finish,
//! so its output is checked file by file against Claude Code's stdout. A grep a
//! wrapper such as `timeout` starts, or one the shell has no function for, is the
//! system grep, which prints files in its own fixed order.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use super::parse::{Filter, local, number};
use super::reads;
use crate::postroute::MAX_BYTES;
use crate::workspace::Workspace;

/// The options Claude Code's `grep` function passes its ugrep before the agent's.
const UGREP: &[&str] = &[
    "-G",
    "--ignore-files",
    "--hidden",
    "-I",
    "--encoding=UTF-8",
    "--exclude-dir=.git",
    "--exclude-dir=.svn",
    "--exclude-dir=.hg",
    "--exclude-dir=.bzr",
    "--exclude-dir=.jj",
    "--exclude-dir=.sl",
];

/// At most this many files are searched one at a time to check an output.
const FILES: usize = 64;

pub(super) struct Grep {
    /// The words grep runs with, its name first.
    pub words: Vec<String>,
    /// The file and directory operands.
    paths: Vec<String>,
    /// Pattern files of `-f`.
    patterns: Vec<String>,
    recursive: bool,
    /// `-l` or `-c`: one line for each file.
    per_file: bool,
    /// `-h`: lines without file names.
    bare: bool,
    /// Whether the shell runs the system grep rather than its function.
    system: bool,
}

/// Arguments for which Claude Code's `grep` function runs the system grep, as its
/// `case` patterns match them, such as `--null` or anything that may start a pager.
fn bypasses(word: &str) -> bool {
    let Some(rest) = word.strip_prefix('-') else {
        return false;
    };
    let mut chars = rest.chars();
    [
        "-filter",
        "-pager",
        "-view",
        "-format-open",
        "-config",
        "-save-config",
    ]
    .iter()
    .any(|s| rest.contains(s))
        || rest.starts_with("--")
        || rest.starts_with(['@', 'Z', 'z'])
        || chars.next().is_some_and(|c| c != '-') && chars.any(|c| matches!(c, 'Z' | 'z'))
        || matches!(word, "--null" | "--null-data")
}

/// A `grep` of files whose options only select and format lines, with the words
/// before it, such as a wrapper.
pub(super) fn parse(original: &[String], words: Vec<String>) -> Option<Grep> {
    // An assignment such as `LC_ALL=C` before grep changes what it prints. A
    // wrapper such as `timeout` or `bash -c` runs the system grep.
    let wrapper = match original.len().checked_sub(words.len()) {
        Some(n) if original[n..] == words[..] => n,
        _ => original.len(),
    };
    if original[..wrapper]
        .iter()
        .any(|w| crate::wrappers::assignment(w))
    {
        return None;
    }
    let mut grep = Grep {
        words,
        paths: Vec::new(),
        patterns: Vec::new(),
        recursive: false,
        per_file: false,
        bare: false,
        system: wrapper > 0,
    };
    let (mut named, mut explicit) = (false, false);
    let mut operands = Vec::new();
    let mut args = grep.words[1..].iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            operands.extend(args.by_ref().cloned());
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (key, inline) = long
                .split_once('=')
                .map_or((long, None), |(k, v)| (k, Some(v.to_owned())));
            let mut value = || inline.clone().or_else(|| args.next().cloned());
            match key {
                "line-number" | "ignore-case" | "word-regexp" | "line-regexp" | "invert-match"
                | "fixed-strings" | "extended-regexp" | "basic-regexp" | "only-matching"
                | "no-messages"
                    if inline.is_none() => {}
                "with-filename" if inline.is_none() => named = true,
                "no-filename" if inline.is_none() => grep.bare = true,
                "recursive" if inline.is_none() => grep.recursive = true,
                "files-with-matches" | "count" if inline.is_none() => grep.per_file = true,
                "after-context" | "before-context" | "context" | "max-count" => {
                    number(&value()?)?;
                }
                "regexp" => {
                    value()?;
                    explicit = true;
                }
                "file" => {
                    grep.patterns.push(value()?);
                    explicit = true;
                }
                _ => return None,
            }
            continue;
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) else {
            operands.push(arg.clone());
            continue;
        };
        for (i, c) in cluster.char_indices() {
            match c {
                'n' | 'i' | 'w' | 'x' | 'v' | 'F' | 'E' | 'G' | 'o' | 's' | 'I' => {}
                'H' => named = true,
                'h' => grep.bare = true,
                'r' => grep.recursive = true,
                'l' | 'c' => grep.per_file = true,
                'A' | 'B' | 'C' | 'm' | 'e' | 'f' => {
                    let rest = &cluster[i + 1..];
                    let value = if rest.is_empty() {
                        args.next()?.clone()
                    } else {
                        rest.to_owned()
                    };
                    match c {
                        'e' => explicit = true,
                        'f' => {
                            grep.patterns.push(value);
                            explicit = true;
                        }
                        _ => {
                            number(&value)?;
                        }
                    }
                    break;
                }
                _ => return None,
            }
        }
    }
    if !explicit {
        // The first operand is the pattern.
        if operands.is_empty() {
            return None;
        }
        operands.remove(0);
    }
    grep.system |= grep.words[1..].iter().any(|w| bypasses(w));
    (!(named && grep.bare)
        && (1..=32).contains(&operands.len())
        && grep.patterns.len() <= 4
        && operands.iter().chain(&grep.patterns).all(|p| local(p)))
    .then(|| {
        grep.paths = operands;
        grep
    })
}

impl Grep {
    /// Whether the recheck reaches every operand and pattern file when grep runs
    /// in `dir`, judged from the words alone, for a list recorded in a
    /// transcript: each is in `root`, or is an operand of a search that walks
    /// nothing and names a file outside it that a read list may reread.
    pub(super) fn reaches(&self, root: &Path, dir: &Path) -> bool {
        let place = |file: &String| crate::find::lexical(&dir.join(file));
        self.patterns
            .iter()
            .all(|file| place(file).starts_with(root))
            && self.paths.iter().all(|file| {
                let path = place(file);
                path.starts_with(root)
                    || !self.recursive && crate::agents::outside_file(root, &path)
            })
    }

    /// The words without the file operands, to search one file at a time.
    fn options(&self) -> Vec<String> {
        let mut paths = self.paths.iter().rev().peekable();
        let mut kept = Vec::new();
        for word in self.words[1..].iter().rev() {
            if paths.next_if(|p| *p == word).is_some() {
                continue;
            }
            kept.push(word.clone());
        }
        kept.reverse();
        if !kept.iter().any(|w| w == "--") {
            kept.push("--".into());
        }
        kept
    }
}

/// The ugrep built into Claude Code, as its `grep` function finds it: the path
/// Claude Code exports to its shell, or the launcher the native installer links.
/// Only a binary the native installer keeps under `versions/`, and that says
/// it is ugrep when asked, is run with grep arguments.
fn ugrep(deadline: Instant) -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("CLAUDE_CODE_EXECPATH").map(PathBuf::from),
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin/claude")),
    ];
    candidates.into_iter().flatten().find_map(|path| {
        let path = path.canonicalize().ok()?;
        (path.is_file() && path.parent()?.file_name()? == "versions" && answers(&path, deadline))
            .then_some(path)
    })
}

/// Whether `binary`, named `ugrep`, prints ugrep's version and exits. A
/// version that ignored the name would start Claude Code itself, which goes
/// online, so it is never given grep arguments. The verdict is kept for the
/// binary as last modified, for the rest of the hook.
fn answers(binary: &Path, deadline: Instant) -> bool {
    static VERDICTS: Mutex<Vec<(PathBuf, SystemTime, bool)>> = Mutex::new(Vec::new());
    let Ok(modified) = std::fs::metadata(binary).and_then(|m| m.modified()) else {
        return false;
    };
    let Ok(mut verdicts) = VERDICTS.lock() else {
        return false;
    };
    if let Some((_, _, verdict)) = verdicts
        .iter()
        .find(|(path, time, _)| path == binary && *time == modified)
    {
        return *verdict;
    }
    let mut cmd = Command::new(binary);
    cmd.current_dir("/")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .arg("--version");
    #[cfg(unix)]
    std::os::unix::process::CommandExt::arg0(&mut cmd, "ugrep");
    let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
    let verdict = cfg!(unix)
        && super::bounded(&mut cmd, 4096, deadline)
            .is_some_and(|(code, text)| code == 0 && text.starts_with("ugrep "));
    verdicts.push((binary.to_owned(), modified, verdict));
    verdict
}

struct Run<'a> {
    ws: &'a Workspace,
    /// The resolved directory grep runs in, and the shell's name for it.
    dir: &'a Path,
    named: &'a Path,
    deadline: Instant,
}

impl Run<'_> {
    fn ugrep(&self, binary: &Path, args: &[String]) -> Option<(i32, String)> {
        let mut cmd = super::quiet(binary, self.dir);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::arg0(&mut cmd, "ugrep");
        cmd.args(UGREP).args(["-D", "skip"]).args(args);
        self.finish(cmd)
    }

    fn system(&self, args: &[String]) -> Option<(i32, String)> {
        let binary = ["/usr/bin/grep", "/bin/grep"]
            .into_iter()
            .find(|p| Path::new(p).is_file())?;
        let mut cmd = super::quiet(Path::new(binary), self.dir);
        cmd.args(["-D", "skip"]).args(args);
        self.finish(cmd)
    }

    fn finish(&self, mut cmd: Command) -> Option<(i32, String)> {
        let (code, text) = super::bounded(&mut cmd, MAX_BYTES, self.deadline)?;
        // A matching binary file is reported, not shown.
        let binary = text
            .lines()
            .any(|l| l.starts_with("Binary file ") && l.ends_with(" matches"));
        (matches!(code, 0 | 1) && !binary).then_some((code, text))
    }

    /// Whether `path` names a file, or a directory a recursive search walks,
    /// inside the workspace. With `leaves`, a file outside the workspace counts
    /// when a read list may reread it there; no directory out there is walked.
    fn inside(&self, path: &str, directory: bool, leaves: bool) -> Option<bool> {
        let spelled = self.dir.join(path);
        let named = self.named.join(path);
        let path = spelled.canonicalize().ok()?;
        if !path.starts_with(&self.ws.root) {
            return (leaves && reads::outside(self.ws, &spelled, &named).is_some())
                .then_some(false);
        }
        if path.is_file() {
            Some(false)
        } else {
            (directory && path.is_dir()).then_some(true)
        }
    }
}

/// Whether `text` is what `rest` of Claude Code's stdout starts with. An inline
/// result may have lost its final newlines.
fn fits(text: &str, rest: &str, trimmed: bool) -> bool {
    rest.starts_with(text)
        || trimmed
            && text.len() > rest.len()
            && text.starts_with(rest)
            && text[rest.len()..].bytes().all(|b| b == b'\n')
}

/// Order the blocks a search printed for each file as `rest` shows them, after
/// the line filters. Each block is used once, and all of them unless a `head`
/// stops the output first.
pub(super) fn arrange(
    blocks: Vec<String>,
    filters: &[Filter],
    rest: &str,
    trimmed: bool,
) -> Option<String> {
    let mut limit = usize::MAX;
    let mut columns = Vec::new();
    for filter in filters {
        match filter {
            Filter::All => {}
            Filter::First(n) => limit = limit.min(*n),
            Filter::Columns(_) => columns.push(filter.clone()),
            _ => return None,
        }
    }
    let mut left = blocks
        .into_iter()
        .filter(|b| !b.is_empty())
        .map(|b| reads::filter(b, &columns))
        .collect::<Option<Vec<_>>>()?;
    let (mut out, mut lines) = (String::new(), 0);
    while lines < limit && !left.is_empty() {
        let at = &rest[out.len().min(rest.len())..];
        let (index, shown) = left
            .iter()
            .enumerate()
            .map(|(i, block)| {
                let end = block
                    .match_indices('\n')
                    .nth(limit - lines - 1)
                    .map_or(block.len(), |(n, _)| n + 1);
                (i, &block[..end])
            })
            .filter(|(_, shown)| fits(shown, at, trimmed))
            .max_by_key(|(_, shown)| shown.len())?;
        let shown = shown.to_owned();
        lines += shown.bytes().filter(|&b| b == b'\n').count();
        out.push_str(&shown);
        left.swap_remove(index);
    }
    Some(out)
}

/// Rerun a grep piece and return its status and output, as `rest` of Claude
/// Code's stdout shows it, after the piece's filters.
pub(super) fn reproduce(
    ws: &Workspace,
    (dir, named): (&Path, &Path),
    grep: &Grep,
    filters: &[Filter],
    rest: &str,
    trimmed: bool,
    deadline: Instant,
) -> Option<(i32, String)> {
    let run = Run {
        ws,
        dir,
        named,
        deadline,
    };
    let mut walks = false;
    for path in &grep.paths {
        walks |= run.inside(path, grep.recursive, true)?;
    }
    for path in &grep.patterns {
        run.inside(path, false, false)?;
    }
    let args = &grep.words[1..];
    let several = walks || grep.paths.len() > 1;
    // Only a Unix process can name the program it runs `ugrep`.
    if let Some(binary) = (cfg!(unix) && !grep.system)
        .then(|| ugrep(deadline))
        .flatten()
    {
        let found = if !several {
            run.ugrep(&binary, args).and_then(|(code, text)| {
                let text = reads::filter(text, filters)?;
                fits(&text, rest, trimmed).then_some((code, text))
            })
        } else if grep.per_file {
            run.ugrep(&binary, args).and_then(|(code, text)| {
                let lines = text.split_inclusive('\n').map(str::to_owned).collect();
                Some((code, arrange(lines, filters, rest, trimmed)?))
            })
        } else {
            several_files(&run, &binary, grep, filters, rest, trimmed, walks)
        };
        if found.is_some() {
            return found;
        }
    }
    let (code, text) = run.system(args)?;
    let text = reads::filter(text, filters)?;
    fits(&text, rest, trimmed).then_some((code, text))
}

/// Search each file on its own, those named or those a walk lists, and order
/// their blocks as Claude Code printed them.
fn several_files(
    run: &Run<'_>,
    binary: &Path,
    grep: &Grep,
    filters: &[Filter],
    rest: &str,
    trimmed: bool,
    walks: bool,
) -> Option<(i32, String)> {
    let options = grep.options();
    let (mut code, files) = if walks {
        let mut listing = vec!["-l".to_owned()];
        listing.extend(grep.words[1..].iter().cloned());
        let (code, text) = run.ugrep(binary, &listing)?;
        let files: Vec<String> = text.lines().map(str::to_owned).collect();
        (code, files)
    } else {
        (1, grep.paths.clone())
    };
    if files.len() > FILES || !files.iter().all(|f| local(f)) {
        return None;
    }
    let mut blocks = Vec::new();
    for file in files {
        let mut args = Vec::new();
        if !grep.bare {
            args.push("-H".to_owned());
        }
        args.extend(options.iter().cloned());
        args.push(file);
        let (found, text) = run.ugrep(binary, &args)?;
        if !walks && found == 0 {
            code = 0;
        }
        blocks.push(text);
    }
    Some((code, arrange(blocks, filters, rest, trimmed)?))
}

#[cfg(test)]
mod tests {
    use super::{Filter, arrange, bypasses, parse};

    fn words(command: &str) -> Vec<String> {
        shlex::split(command).unwrap()
    }

    fn grep(command: &str) -> Option<String> {
        let original = words(command);
        let refs: Vec<&str> = original.iter().map(String::as_str).collect();
        let effective = crate::wrappers::effective(&refs)?;
        let grep = parse(&original, effective)?;
        let flags = [
            (grep.recursive, "recursive"),
            (grep.per_file, "per-file"),
            (grep.bare, "bare"),
            (grep.system, "system"),
        ];
        let flags: Vec<&str> = flags.iter().filter(|f| f.0).map(|f| f.1).collect();
        Some(format!("{} [{}]", grep.paths.join(" "), flags.join(" ")))
    }

    #[test]
    fn options_patterns_and_paths_parse_or_refuse() {
        for (command, expected) in [
            ("grep -n x a.rs", Some("a.rs []")),
            ("grep -n 'fn x' -A30 a.rs", Some("a.rs []")),
            ("grep -rnA3 -B 2 x src", Some("src [recursive]")),
            ("grep -e a -e b a b", Some("a b []")),
            ("grep -nE 'a|b' --context=2 -m5 a", Some("a []")),
            ("grep -rl x src docs", Some("src docs [recursive per-file]")),
            ("grep -c x a b", Some("a b [per-file]")),
            ("grep -h -o x a b", Some("a b [bare]")),
            ("grep -f pats a", Some("a []")),
            ("grep -n -- -x a", Some("a []")),
            ("timeout 5 grep -n x a", Some("a [system]")),
            ("grep -n -e -zebra a", Some("a [system]")),
            // Options that change what grep reads or prints, or paths that leave.
            ("grep -R x src", None),
            ("grep -P x a", None),
            ("grep --color=always x a", None),
            ("grep --include='*.rs' -r x src", None),
            ("grep -Z x a", None),
            ("grep -L x a", None),
            ("grep -a x a", None),
            ("grep -A x a", None),
            ("grep -n x", None),
            ("grep -rn x", None),
            ("grep -n x ../a", None),
            ("grep -n x .git/config", None),
            ("grep -f ../patterns a", None),
            ("grep -H -h x a b", None),
            ("LC_ALL=C grep -n x a", None),
            ("env LC_ALL=C grep -n x a", None),
            ("bash -c 'grep -n x a'", Some("a [system]")),
            ("bash -c 'LC_ALL=C grep -n x a'", None),
        ] {
            assert_eq!(grep(command).as_deref(), expected, "{command}");
        }
    }

    #[test]
    fn the_function_runs_the_system_grep_for_its_patterns() {
        for (word, expected) in [
            ("--null", true),
            ("--null-data", true),
            ("-Z", true),
            ("-z", true),
            ("-nZ", true),
            ("-@x", true),
            ("---x", true),
            ("--pager=less", true),
            ("--filter=x", true),
            ("-a-config", true),
            ("-n", false),
            ("--count", false),
            ("--zz", false),
            ("zebra", false),
            ("-", false),
        ] {
            assert_eq!(bypasses(word), expected, "{word}");
        }
    }

    #[test]
    fn blocks_follow_the_order_and_cut_of_the_output() {
        let blocks = || vec!["a:1\na:2\n".to_owned(), "b:1\n".to_owned(), String::new()];
        let order =
            |rest: &str, filters: &[Filter], trimmed| arrange(blocks(), filters, rest, trimmed);
        for (rest, filters, trimmed, expected) in [
            ("b:1\na:1\na:2\n", vec![], false, Some("b:1\na:1\na:2\n")),
            (
                "a:1\na:2\nb:1\nmore",
                vec![],
                false,
                Some("a:1\na:2\nb:1\n"),
            ),
            ("b:1\na:1\na:2", vec![], true, Some("b:1\na:1\na:2\n")),
            ("b:1\na:1\na:2", vec![], false, None),
            (
                "b:1\na:1\n",
                vec![Filter::First(2)],
                false,
                Some("b:1\na:1\n"),
            ),
            (
                "a:1\na:2\n",
                vec![Filter::First(2)],
                false,
                Some("a:1\na:2\n"),
            ),
            (
                "b:1\na:1\na:2\n",
                vec![Filter::First(9)],
                false,
                Some("b:1\na:1\na:2\n"),
            ),
            // A block missing, shown twice, or out of its own order.
            ("b:1\n", vec![], false, None),
            ("b:1\nb:1\na:1\na:2\n", vec![], false, None),
            ("a:2\na:1\nb:1\n", vec![], false, None),
            ("b:1\na:1\na:2\n", vec![Filter::Last(1)], false, None),
        ] {
            assert_eq!(
                order(rest, &filters, trimmed).as_deref(),
                expected,
                "{rest:?}"
            );
        }
    }
}
