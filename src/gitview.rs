//! What a large `git log` or `git status` shows in place of the pipes agents write
//! after it, such as `git log --oneline | head -20` or `git status -sb | head -2`.
//!
//! A log longer than the budget shows its newest commits, whole, and one line that
//! counts the older ones and names their exact lines. With `--reverse` the newest
//! are at the end, so the count comes first. A status longer than the budget leads
//! with its branch lines and a count per state, then lists each state's paths
//! grouped by directory, as the listing report does, with each group pointing at
//! its original lines. Both read the stream as the preview scan passes it, after
//! redaction, and decline output they do not recognize, which keeps the ordinary
//! preview. Machine formats, such as `--porcelain`, `-z` or a patch, never qualify.
use std::collections::VecDeque;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::listing::Listing;
use crate::search::plural;

/// A `git log` or `git status` view, fed one stdout line at a time.
pub enum View {
    Log(Log),
    Status(Status),
}

impl View {
    /// The view for `words`, when they run a display-oriented `git log` or
    /// `git status` this module understands.
    pub fn select(words: &[&str], budget: usize) -> Option<Self> {
        let owned = crate::wrappers::effective(words);
        let words: Vec<&str> = match &owned {
            Some(words) => words.iter().map(String::as_str).collect(),
            None => words.to_vec(),
        };
        let (subcommand, args) = subcommand(crate::filters::unwrap_tool(&words))?;
        match subcommand {
            "log" => Log::new(args, budget).map(Self::Log),
            "status" => status_display(args).then(|| Self::Status(Status::new())),
            _ => None,
        }
    }

    pub fn observe(&mut self, text: &str, n: usize) {
        match self {
            Self::Log(log) => log.observe(text, n),
            Self::Status(status) => status.observe(text, n),
        }
    }

    /// The report within `budget` bytes, or `None` when the output fit the budget
    /// or was not recognized.
    pub fn finish(self, budget: usize) -> Option<String> {
        match self {
            Self::Log(log) => log.finish(budget),
            Self::Status(status) => status.finish(budget),
        }
    }
}

/// Git's subcommand and its arguments, after the global options that change only
/// where or how Git runs.
fn subcommand<'a>(words: &'a [&'a str]) -> Option<(&'a str, &'a [&'a str])> {
    let (program, mut rest) = words.split_first()?;
    if Path::new(program).file_name().and_then(|n| n.to_str()) != Some("git") {
        return None;
    }
    while let Some((first, tail)) = rest.split_first() {
        match *first {
            "--no-pager"
            | "-P"
            | "--paginate"
            | "--no-optional-locks"
            | "--no-replace-objects"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs" => rest = tail,
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" if !tail.is_empty() => {
                rest = &tail[1..];
            }
            word if ["--git-dir=", "--work-tree=", "--namespace="]
                .iter()
                .any(|prefix| word.starts_with(prefix)) =>
            {
                rest = tail
            }
            word if word.starts_with('-') => return None,
            word => return Some((word, tail)),
        }
    }
    None
}

static HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^commit [0-9a-f]{7,64}(?: |$)").unwrap());
static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{4,64}(?: |$)").unwrap());

/// How a log's commits are separated.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// Each commit starts at a `commit SHA` line, as the default format prints.
    Headers,
    /// Each line is a commit that starts with its id, as `--oneline` prints.
    Ids,
    /// Each line is a commit, as a one-line `--format` template prints.
    Lines,
}

/// A log's newest commits within the budget, and the count of the rest.
pub struct Log {
    /// `--reverse`: the newest commits come last.
    reverse: bool,
    shape: Shape,
    budget: usize,
    bytes: usize,
    lines: usize,
    commits: usize,
    /// A line did not start a commit where one had to.
    unknown: bool,
    /// The commit being read: its text, while it can still fit, and first line.
    current: Option<(String, usize)>,
    /// Commits kept in order, with their first lines. Forward runs keep the first
    /// ones that fit, reverse runs the last ones.
    kept: VecDeque<(String, usize)>,
    kept_bytes: usize,
    /// A forward run filled its budget and keeps no more.
    full: bool,
}

impl Log {
    fn new(args: &[&str], budget: usize) -> Option<Self> {
        // `-p` and the other patch options have the patch summary.
        let patch = |arg: &str| {
            matches!(
                arg,
                "-p" | "-u"
                    | "--patch"
                    | "--patch-with-stat"
                    | "--patch-with-raw"
                    | "-W"
                    | "--function-context"
                    | "-m"
                    | "-c"
                    | "--cc"
                    | "--dd"
                    | "--remerge-diff"
                    | "--binary"
            ) || [
                "-U",
                "--unified",
                "--diff-merges",
                "-L",
                "--output",
                "--word-diff",
                "--color-words",
            ]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
        };
        // Options that print more lines for each commit, which only commit headers
        // can then separate.
        let extra = |arg: &str| {
            [
                "--stat",
                "--numstat",
                "--shortstat",
                "--name-only",
                "--name-status",
                "--raw",
                "--summary",
                "--dirstat",
                "--compact-summary",
                "--notes",
                "--show-notes",
                "--show-signature",
                "--show-linear-break",
                "--check",
            ]
            .iter()
            .any(|option| arg.starts_with(option))
        };
        let mut shape = Some(Shape::Headers);
        let mut reverse = false;
        let mut more = false;
        for &arg in args.iter().take_while(|arg| **arg != "--") {
            if patch(arg) || matches!(arg, "--graph" | "-z" | "--left-right" | "--boundary") {
                return None;
            }
            if arg == "--reverse" {
                reverse = true;
            } else if arg == "--oneline" {
                shape = Some(Shape::Ids);
            } else if arg == "--pretty" {
                shape = Some(Shape::Headers);
            } else if let Some(format) = arg
                .strip_prefix("--pretty=")
                .or_else(|| arg.strip_prefix("--format="))
            {
                shape = pretty(format);
            } else if extra(arg) {
                more = true;
            }
        }
        let shape = shape?;
        // A line per commit holds no room for the lines these options add.
        if shape != Shape::Headers && more {
            return None;
        }
        Some(Self {
            reverse,
            shape,
            budget,
            bytes: 0,
            lines: 0,
            commits: 0,
            unknown: false,
            current: None,
            kept: VecDeque::new(),
            kept_bytes: 0,
            full: false,
        })
    }

    fn observe(&mut self, text: &str, n: usize) {
        self.bytes += text.len();
        self.lines = n;
        if self.unknown {
            return;
        }
        let line = text.trim_end_matches(['\n', '\r']);
        let starts = match self.shape {
            Shape::Headers => HEADER.is_match(line),
            Shape::Ids => ID.is_match(line),
            Shape::Lines => true,
        };
        // A header format's first line, and every line of `--oneline`, starts
        // a commit. Other lines, such as a patch or a signature check that
        // the configuration adds, are no commit to count.
        if !starts && (n == 1 || self.shape == Shape::Ids) {
            self.unknown = true;
            return;
        }
        if starts {
            self.close();
            self.commits += 1;
            self.current = Some((String::new(), n));
        }
        let wanted = !self.full || self.reverse;
        if let Some((current, _)) = self.current.as_mut().filter(|_| wanted) {
            // A commit longer than the budget is never shown, so stop copying it.
            if current.len() <= self.budget {
                current.push_str(text);
            }
        }
    }

    /// Keep the commit just read, if it fits.
    fn close(&mut self) {
        let Some((text, first)) = self.current.take() else {
            return;
        };
        if self.reverse {
            if text.len() > self.budget {
                // Nothing before a commit too long to show can be kept with the
                // newer ones after it.
                self.kept.clear();
                self.kept_bytes = 0;
                return;
            }
            self.kept_bytes += text.len();
            self.kept.push_back((text, first));
            while self.kept_bytes > self.budget {
                let (old, _) = self.kept.pop_front().expect("kept commits");
                self.kept_bytes -= old.len();
            }
        } else if !self.full {
            if self.kept_bytes + text.len() <= self.budget {
                self.kept_bytes += text.len();
                self.kept.push_back((text, first));
            } else {
                self.full = true;
            }
        }
    }

    fn finish(mut self, budget: usize) -> Option<String> {
        self.close();
        if self.unknown || self.bytes <= budget {
            return None;
        }
        // Drop commits until the count of the others fits beside them.
        loop {
            if self.kept.is_empty() {
                return None;
            }
            let rest = self.commits - self.kept.len();
            let range = if self.reverse {
                (1, self.kept.front()?.1 - 1)
            } else {
                (self.kept.back()?.1 + self.kept_tail_lines(), self.lines)
            };
            let count = format!(
                "+{} [stdout:{}-{}]\n",
                plural(rest, "older commit", "older commits"),
                range.0,
                range.1
            );
            let gap = usize::from(!self.reverse && !self.kept_ends_line());
            if rest > 0 && self.kept_bytes + gap + count.len() <= budget {
                let mut view = String::with_capacity(budget);
                if self.reverse {
                    view.push_str(&count);
                }
                for (text, _) in &self.kept {
                    view.push_str(text);
                }
                if !self.reverse {
                    if gap > 0 {
                        view.push('\n');
                    }
                    view.push_str(&count);
                }
                return Some(view);
            }
            let dropped = if self.reverse {
                self.kept.pop_front()
            } else {
                self.kept.pop_back()
            };
            self.kept_bytes -= dropped?.0.len();
        }
    }

    /// The lines the last kept commit spans, so the line after it starts the rest.
    fn kept_tail_lines(&self) -> usize {
        self.kept
            .back()
            .map_or(0, |(text, _)| text.split_inclusive('\n').count())
    }

    fn kept_ends_line(&self) -> bool {
        self.kept
            .back()
            .is_none_or(|(text, _)| text.ends_with('\n'))
    }
}

/// How a `--pretty` or `--format` value separates commits, if it is known.
fn pretty(format: &str) -> Option<Shape> {
    match format {
        "" | "short" | "medium" | "full" | "fuller" | "raw" => Some(Shape::Headers),
        "oneline" | "reference" => Some(Shape::Ids),
        "email" | "mboxrd" => None,
        _ => {
            let template = format
                .strip_prefix("format:")
                .or_else(|| format.strip_prefix("tformat:"));
            // A named format from the configuration is unknown here.
            let template = template.or_else(|| format.contains('%').then_some(format))?;
            // Line breaks, bodies, notes, trailers, wrapping and signature checks
            // can print more than one line.
            let multiline = template.contains('\n')
                || ["%n", "%b", "%B", "%N", "%(trailers", "%w(", "%G", "%x0"]
                    .iter()
                    .any(|code| template.contains(code));
            // A JSON template keeps the JSON view.
            let json = template.trim_start().starts_with(['{', '[']);
            (!multiline && !json).then_some(Shape::Lines)
        }
    }
}

/// Whether `git status` runs with `args` in its long or short human format.
fn status_display(args: &[&str]) -> bool {
    for &arg in args.iter().take_while(|arg| **arg != "--") {
        if arg.starts_with("--porcelain")
            || arg.starts_with("--column")
            || matches!(arg, "-z" | "--null" | "-v" | "-vv" | "--verbose")
        {
            return false;
        }
        // Clustered short options such as `-sb` or `-suno`.
        if let Some(cluster) = arg.strip_prefix('-').filter(|rest| !rest.starts_with('-')) {
            for flag in cluster.chars() {
                match flag {
                    'z' | 'v' => return false,
                    // `-u` takes the rest of the cluster as its value.
                    'u' => break,
                    _ => {}
                }
            }
        }
    }
    true
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Conflicted,
    Staged,
    Modified,
    Untracked,
    Ignored,
}

const STATES: [State; 5] = [
    State::Conflicted,
    State::Staged,
    State::Modified,
    State::Untracked,
    State::Ignored,
];

impl State {
    fn name(self) -> &'static str {
        match self {
            Self::Conflicted => "conflicted",
            Self::Staged => "staged",
            Self::Modified => "modified",
            Self::Untracked => "untracked",
            Self::Ignored => "ignored",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Lines before the sections, such as the branch and an operation in progress,
/// that a status report keeps at most.
const LEAD_LINES: usize = 12;

/// A status grouped by state and directory.
pub struct Status {
    /// Short format, known from the first line.
    short: Option<bool>,
    lead: Vec<String>,
    section: Option<State>,
    listings: Vec<Listing>,
    counts: [usize; 5],
    bytes: usize,
    unknown: bool,
}

static SHORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[ MTADRCU?!]{2} \S").unwrap());

impl Status {
    fn new() -> Self {
        Self {
            short: None,
            lead: Vec::new(),
            section: None,
            // Tracked paths under build or dependency directories are changes like
            // any other; untracked and ignored ones collapse as in a listing.
            listings: STATES
                .iter()
                .map(|state| Listing::of_paths(!matches!(state, State::Untracked | State::Ignored)))
                .collect(),
            counts: [0; 5],
            bytes: 0,
            unknown: false,
        }
    }

    fn observe(&mut self, text: &str, n: usize) {
        self.bytes += text.len();
        if self.unknown {
            return;
        }
        let line = text.trim_end_matches(['\n', '\r']);
        let short = *self
            .short
            .get_or_insert_with(|| line.starts_with("## ") || SHORT.is_match(line));
        let known = if short {
            self.short_line(line, n)
        } else {
            self.long_line(line, n)
        };
        if !known || self.lead.len() > LEAD_LINES {
            self.unknown = true;
        }
    }

    fn add(&mut self, state: State, path: &str, n: usize) {
        // A rename names the old and the new path; the new one is in the tree.
        let path = path.rsplit_once(" -> ").map_or(path, |(_, new)| new);
        self.counts[state.index()] += 1;
        self.listings[state.index()].observe(path, n);
    }

    fn short_line(&mut self, line: &str, n: usize) -> bool {
        if self.counts.iter().all(|&count| count == 0) && line.starts_with("## ") {
            self.lead.push(line.to_owned());
            return true;
        }
        if !SHORT.is_match(line) {
            return false;
        }
        let (code, path) = line.split_at(3);
        let (x, y) = (code.as_bytes()[0], code.as_bytes()[1]);
        match (x, y) {
            (b'?', b'?') => self.add(State::Untracked, path, n),
            (b'!', b'!') => self.add(State::Ignored, path, n),
            (b'?' | b'!', _) | (_, b'?' | b'!') => return false,
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => {
                self.add(State::Conflicted, path, n);
            }
            _ => {
                if x != b' ' {
                    self.add(State::Staged, path, n);
                }
                if y != b' ' {
                    self.add(State::Modified, path, n);
                }
            }
        }
        true
    }

    fn long_line(&mut self, line: &str, n: usize) -> bool {
        // Hints such as `  (use "git add <file>..." to update …)`.
        if line.starts_with("  (") {
            return true;
        }
        let Some(state) = self.section else {
            let section = match line {
                "Changes to be committed:" => Some(State::Staged),
                "Changes not staged for commit:" => Some(State::Modified),
                "Untracked files:" => Some(State::Untracked),
                "Unmerged paths:" => Some(State::Conflicted),
                "Ignored files:" => Some(State::Ignored),
                _ => None,
            };
            if section.is_some() {
                self.section = section;
            } else if !(line.is_empty()
                || line.starts_with('\t')
                || line.starts_with("no changes added to commit")
                || line.starts_with("nothing added to commit")
                || line.starts_with("nothing to commit"))
            {
                // The branch, its upstream and an operation in progress.
                self.lead.push(line.to_owned());
            }
            return !line.starts_with('\t');
        };
        if line.is_empty() {
            self.section = None;
            return true;
        }
        let Some(entry) = line.strip_prefix('\t') else {
            return false;
        };
        if matches!(state, State::Untracked | State::Ignored) {
            self.add(state, entry, n);
            return true;
        }
        let Some((label, path)) = entry.split_once(':') else {
            return false;
        };
        let known = match state {
            State::Conflicted => matches!(
                label,
                "both modified"
                    | "both added"
                    | "both deleted"
                    | "added by us"
                    | "added by them"
                    | "deleted by us"
                    | "deleted by them"
            ),
            _ => matches!(
                label,
                "modified" | "new file" | "deleted" | "renamed" | "copied" | "typechange"
            ),
        };
        if !known {
            return false;
        }
        self.add(state, submodule_path(path.trim_start()), n);
        true
    }

    fn finish(self, budget: usize) -> Option<String> {
        let entries: usize = self.counts.iter().sum();
        if self.unknown || entries == 0 || self.bytes <= budget {
            return None;
        }
        let mut head = String::new();
        for line in &self.lead {
            head.push_str(line);
            head.push('\n');
        }
        let counts: Vec<String> = STATES
            .iter()
            .filter(|state| **state != State::Ignored || self.counts[state.index()] > 0)
            .map(|state| format!("{} {}", self.counts[state.index()], state.name()))
            .collect();
        head.push_str(&counts.join(" · "));
        head.push('\n');
        // Smaller states take what they need first and leave the rest to larger ones.
        let mut order: Vec<State> = STATES
            .into_iter()
            .filter(|state| self.counts[state.index()] > 0)
            .collect();
        order.sort_by_key(|state| self.counts[state.index()]);
        let mut room = budget.checked_sub(head.len())?;
        let mut reports: [Option<String>; 5] = Default::default();
        let mut listings: Vec<Option<Listing>> = self.listings.into_iter().map(Some).collect();
        for (done, state) in order.iter().enumerate() {
            let share = room / (order.len() - done);
            let listing = listings[state.index()].take()?;
            let (report, _) = listing.finish(state.name(), share, None, true)?;
            room = room.checked_sub(report.len())?;
            reports[state.index()] = Some(report);
        }
        for report in reports.into_iter().flatten() {
            head.push_str(&report);
        }
        (head.len() <= budget).then_some(head)
    }
}

/// A submodule's path without the note `git status` adds after it, such as
/// `sub (new commits, modified content)`.
fn submodule_path(path: &str) -> &str {
    if let Some((name, note)) = path.rsplit_once(" (")
        && let Some(note) = note.strip_suffix(')')
        && note.split(", ").all(|part| {
            matches!(
                part,
                "new commits" | "modified content" | "untracked content"
            )
        })
    {
        return name;
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(command: &str, output: &str, budget: usize) -> Option<String> {
        let words: Vec<&str> = command.split(' ').collect();
        let mut view = View::select(&words, budget)?;
        for (index, line) in output.split_inclusive('\n').enumerate() {
            view.observe(line, index + 1);
        }
        view.finish(budget)
    }

    fn oneline(count: usize) -> String {
        (0..count)
            .rev()
            .map(|i| format!("{:07x} commit number {i}\n", 0xabc0000 + i))
            .collect()
    }

    #[test]
    fn selects_display_logs_and_statuses_only() {
        let selected = |command: &str| {
            let words: Vec<&str> = command.split(' ').collect();
            View::select(&words, 2048).is_some()
        };
        for command in [
            "git log",
            "git log --oneline",
            "git --no-pager log -n 50 main..HEAD -- src",
            "git -C repo --no-optional-locks log --stat",
            "git log --format=%h·%s --reverse",
            "git status",
            "git status -sb",
            "git status --short --branch -uall",
            "git --no-optional-locks status",
        ] {
            assert!(selected(command), "{command}");
        }
        for command in [
            "git log -p",
            "git log --patch",
            "git log -U1",
            "git log --graph --oneline",
            "git log -z",
            "git log --oneline --stat",
            "git log --format=%h%n%b",
            "git log --pretty=email",
            "git log --pretty=myalias",
            "git log -L1,2:src/main.rs",
            "git log --oneline --word-diff",
            "git log --color-words",
            "git log --binary",
            "git log --format={\"h\":\"%h\"}",
            "git status --porcelain",
            "git status --porcelain=v2",
            "git status -z",
            "git status -sz",
            "git status -v",
            "git status --column",
            "git show",
            "git -x log",
            "cat log",
        ] {
            assert!(!selected(command), "{command}");
        }
    }

    #[test]
    fn a_log_that_fits_stays_as_it_is() {
        assert_eq!(view("git log --oneline", &oneline(10), 2048), None);
    }

    #[test]
    fn a_long_log_keeps_its_newest_commits_and_counts_the_rest() {
        let log = oneline(200);
        let report = view("git log --oneline", &log, 1024).unwrap();
        assert!(report.len() <= 1024, "{report}");
        let lines: Vec<&str> = report.lines().collect();
        let kept = lines.len() - 1;
        assert_eq!(lines[..kept], log.lines().collect::<Vec<_>>()[..kept]);
        assert_eq!(
            lines[kept],
            format!("+{} older commits [stdout:{}-200]", 200 - kept, kept + 1)
        );
    }

    #[test]
    fn a_reversed_log_keeps_its_newest_commits_at_the_end() {
        let log: String = oneline(200)
            .lines()
            .rev()
            .map(|l| format!("{l}\n"))
            .collect();
        let report = view("git log --oneline --reverse", &log, 1024).unwrap();
        let lines: Vec<&str> = report.lines().collect();
        let kept = lines.len() - 1;
        let first = 200 - kept + 1;
        assert_eq!(
            lines[0],
            format!("+{} older commits [stdout:1-{}]", 200 - kept, first - 1)
        );
        assert_eq!(lines[1..], log.lines().collect::<Vec<_>>()[first - 1..]);
    }

    #[test]
    fn a_full_log_cuts_between_commits() {
        let log: String = (0..40)
            .rev()
            .map(|i| {
                format!(
                    "commit {:040x}\nAuthor: A <a@example.com>\nDate:   Mon Nov 2 10:00:00 2026 +0000\n\n    change {i}\n\n    body line\n",
                    i + 1
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let report = view("git log", &log, 1000).unwrap();
        let commits = report.matches("\ncommit ").count() + 1;
        let shown = report.lines().count() - 1;
        let total = log.lines().count();
        assert!(report.starts_with("commit "));
        assert!(
            report.ends_with(&format!(
                "+{} older commits [stdout:{}-{total}]\n",
                40 - commits,
                shown + 1
            )),
            "{report}"
        );
        let next = log.lines().nth(shown).unwrap();
        assert!(next.starts_with("commit "), "{next}");
    }

    #[test]
    fn a_log_in_an_unknown_format_keeps_the_preview() {
        let log = "abc\n".repeat(1000);
        assert_eq!(view("git log", &log, 512), None);
        // Every `--oneline` line starts with a commit id; a patch line does not.
        let mut log = oneline(200);
        log.insert_str(log.find('\n').unwrap() + 1, "+added line\n");
        assert_eq!(view("git log --oneline", &log, 512), None);
    }

    #[test]
    fn a_short_status_counts_and_groups_each_state() {
        let mut status = String::from("## main...origin/main [ahead 2]\n");
        status.push_str("UU src/conflict.rs\n");
        status.push_str("MM src/both.rs\n");
        status.push_str("A  src/new.rs\n");
        status.push_str("R  old.rs -> src/renamed.rs\n");
        for i in 0..80 {
            status.push_str(&format!("?? scratch/notes/file{i:03}.txt\n"));
        }
        let report = view("git status -sb", &status, 1024).unwrap();
        assert!(report.len() <= 1024);
        assert!(
            report.starts_with(
                "## main...origin/main [ahead 2]\n1 conflicted · 3 staged · 1 modified · 80 untracked\n"
            ),
            "{report}"
        );
        assert!(report.contains("staged: 3 entries"), "{report}");
        assert!(report.contains("[stdout:6-85]"), "{report}");

        // `git status -sb --show-stash` (or `status.showStash = true`) prints a
        // second `## stash: N` header line before the file entries.
        let mut with_stash = String::from("## main...origin/main\n## stash: 3\n");
        for i in 0..80 {
            with_stash.push_str(&format!(" M src/file{i:03}.rs\n"));
        }
        let report = view("git status -sb --show-stash", &with_stash, 1024).unwrap();
        assert!(
            report.starts_with(
                "## main...origin/main\n## stash: 3\n0 conflicted · 0 staged · 80 modified · 0 untracked\n"
            ),
            "{report}"
        );
    }

    #[test]
    fn a_long_status_keeps_its_lead_and_drops_hints() {
        let mut status = String::from(
            "On branch main\nYour branch is ahead of 'origin/main' by 1 commit.\n  (use \"git push\" to publish your local commits)\n\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n\tmodified:   src/lib.rs\n\tmodified:   vendor (new commits)\n\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n",
        );
        for i in 0..80 {
            status.push_str(&format!("\tlogs/run{i:03}.txt\n"));
        }
        status
            .push_str("\nno changes added to commit (use \"git add\" and/or \"git commit -a\")\n");
        let report = view("git status", &status, 1024).unwrap();
        assert!(
            report.starts_with(
                "On branch main\nYour branch is ahead of 'origin/main' by 1 commit.\n0 conflicted · 0 staged · 2 modified · 80 untracked\n"
            ),
            "{report}"
        );
        assert!(report.contains("vendor"), "{report}");
        assert!(!report.contains("(use"), "{report}");
        assert!(
            report.contains("80 entries in 1 directory under logs/"),
            "{report}"
        );
    }

    #[test]
    fn a_status_with_unknown_lines_keeps_the_preview() {
        let mut status = String::from("On branch main\n\nSubmodule changes to be committed:\n\n");
        status.push_str(&"* sub abc..def (1):\n  > change\n".repeat(100));
        assert_eq!(view("git status", &status, 512), None);
    }
}
