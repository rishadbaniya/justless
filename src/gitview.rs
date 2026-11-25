//! Compact, lossless-link views for display-oriented Git commands:
//! `git log`, `git reflog`, `git status`, `git branch`, `git tag`, and
//! `git stash list`.
//!
//! - `git log` in default multi-line format compacts commits into dense single
//!   lines (`<short-hash> YYYY-MM-DD Author: subject [stdout:A-B]`), keeping
//!   branch/tag decorations and `Merge:` notes verbatim while folding bodies and
//!   older commits behind exact `[stdout:A-B]` links. `--oneline`, `--reverse`,
//!   `--stat`, custom one-line formats and `git reflog` keep their newest entries
//!   and count the rest.
//! - `git status` strips Git's `(use "git ...")` usage hints, keeps branch and
//!   operation headers plus exact per-state counts, surfaces 100% of conflicted
//!   paths (with conflict kinds), renames (`old -> new`), and small sections
//!   verbatim, and groups dense directories cleanly with `[stdout:A-B]` links.
//! - `git branch` prioritizes the current (`*`) branch, linked worktrees (`+`),
//!   and `[gone]` / `[ahead N]` / `[behind N]` tracking branches first, then
//!   folds large pools of plain local branches and groups repetitive remote
//!   prefixes (`remotes/origin/ (N branches)`) with exact `[stdout:A-B]` links.
//! - `git tag` and `git stash list` keep the most relevant entries and fold the
//!   rest behind exact counts and `[stdout:A-B]` links.
//!
//! Every view reads stdout as the preview scan passes it (after redaction),
//! declines unrecognized or machine-formatted output (`--porcelain`, `-z`, `-p`,
//! `--graph`, `--format` on branch/tag/stash), and replaces the output only when
//! strictly smaller and within `budget`.
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::listing::Listing;
use crate::search::plural;

/// A Git command view, fed one stdout line at a time.
pub enum View {
    Log(Box<Log>),
    Status(Box<Status>),
    Branch(Box<Branch>),
    Tag(Box<Tag>),
    Stash(Box<Stash>),
}

impl View {
    /// The view for `words`, when they run a display-oriented Git command this
    /// module understands.
    pub fn select(words: &[&str], budget: usize) -> Option<Self> {
        let owned = crate::wrappers::effective(words);
        let words: Vec<&str> = match &owned {
            Some(words) => words.iter().map(String::as_str).collect(),
            None => words.to_vec(),
        };
        let (subcommand, args) = subcommand(crate::filters::unwrap_tool(&words))?;
        match subcommand {
            "log" => Log::new(args, budget).map(Box::new).map(Self::Log),
            "reflog" => Log::reflog(args, budget).map(Box::new).map(Self::Log),
            "status" => status_display(args).then(|| Self::Status(Box::new(Status::new()))),
            "branch" => Branch::new(args).map(Box::new).map(Self::Branch),
            "tag" => Tag::new(args).map(Box::new).map(Self::Tag),
            "stash" => Stash::new(args, budget).map(Box::new).map(Self::Stash),
            _ => None,
        }
    }

    pub fn observe(&mut self, text: &str, n: usize) {
        match self {
            Self::Log(log) => log.observe(text, n),
            Self::Status(status) => status.observe(text, n),
            Self::Branch(branch) => branch.observe(text, n),
            Self::Tag(tag) => tag.observe(text, n),
            Self::Stash(stash) => stash.observe(text, n),
        }
    }

    /// The report within `budget` bytes, or `None` when the output does not
    /// benefit from compaction or was not recognized.
    pub fn finish(self, budget: usize) -> Option<String> {
        match self {
            Self::Log(log) => (*log).finish(budget),
            Self::Status(status) => (*status).finish(budget),
            Self::Branch(branch) => (*branch).finish(budget),
            Self::Tag(tag) => (*tag).finish(budget),
            Self::Stash(stash) => (*stash).finish(budget),
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

fn line_range_tag(first: usize, last: usize) -> String {
    if first == last {
        format!("[stdout:{first}]")
    } else {
        format!("[stdout:{first}-{last}]")
    }
}

// ---------------------------------------------------------------------------
// git log / git reflog
// ---------------------------------------------------------------------------

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

/// One kept commit (either verbatim or compacted to one line) along with the
/// exact 1-based line span it covers in raw stdout.
struct KeptCommit {
    text: String,
    first_line: usize,
    raw_end_line: usize,
}

/// Incremental parser for a single multi-line `git log` commit header + body.
struct ParsedCommit {
    hash: String,
    decorations: String,
    merge: Option<String>,
    author: Option<String>,
    committer: Option<String>,
    date: Option<String>,
    subject: Option<String>,
    in_body: bool,
    valid: bool,
    first_line: usize,
    body_end_line: usize,
    raw_end_line: usize,
}

impl ParsedCommit {
    fn start(line: &str, n: usize) -> Self {
        let rest = line.strip_prefix("commit ").unwrap_or("");
        let (hash, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let short_hash = &hash[..hash.len().min(7)];
        Self {
            hash: short_hash.to_owned(),
            decorations: tail.trim().to_owned(),
            merge: None,
            author: None,
            committer: None,
            date: None,
            subject: None,
            in_body: false,
            valid: !short_hash.is_empty(),
            first_line: n,
            body_end_line: n,
            raw_end_line: n,
        }
    }

    fn observe_line(&mut self, line: &str, n: usize) {
        self.raw_end_line = n;
        if !line.trim().is_empty() {
            self.body_end_line = n;
        }
        if !self.valid {
            return;
        }
        if !self.in_body {
            if line.is_empty() {
                self.in_body = true;
                return;
            }
            if let Some(val) = line.strip_prefix("Merge:") {
                self.merge = Some(val.trim().to_owned());
            } else if let Some(val) = line.strip_prefix("Author:") {
                self.author = Some(compact_author(val.trim()).to_owned());
            } else if let Some(val) = line.strip_prefix("Commit:") {
                self.committer = Some(compact_author(val.trim()).to_owned());
            } else if let Some(val) = line
                .strip_prefix("Date:")
                .or_else(|| line.strip_prefix("AuthorDate:"))
            {
                if self.date.is_none() {
                    self.date = Some(compact_date(val.trim()));
                }
            } else if line.starts_with("CommitDate:")
                || line.starts_with("Reflog:")
                || line.starts_with("Reflog message:")
                || line.starts_with("Encoding:")
            {
                // Standard metadata headers in `--pretty=fuller` or reflog-style logs.
            } else {
                // Unexpected header (e.g. GPG signature or `--pretty=raw` tree/parent).
                self.valid = false;
            }
            return;
        }
        if line.is_empty() {
            return;
        }
        if let Some(msg) = line
            .strip_prefix("    ")
            .or_else(|| line.strip_prefix('\t'))
        {
            if self.subject.is_none() {
                let trimmed = msg.trim();
                if !trimmed.is_empty() {
                    self.subject = Some(trimmed.to_owned());
                }
            }
        } else {
            // Non-indented line after the message (e.g. diff, stat, or notes
            // injected by git config) cannot be folded without losing data.
            self.valid = false;
        }
    }

    fn format_compact(&self) -> Option<String> {
        if !self.valid {
            return None;
        }
        let author = self.author.as_deref()?;
        let mut out = String::with_capacity(96);
        out.push_str(&self.hash);
        if !self.decorations.is_empty() {
            out.push(' ');
            out.push_str(&self.decorations);
        }
        if let Some(date) = &self.date {
            out.push(' ');
            out.push_str(date);
        }
        out.push(' ');
        out.push_str(author);
        if let Some(committer) = &self.committer
            && committer != author
        {
            let _ = write!(out, " (committer: {committer})");
        }
        if let Some(subject) = &self.subject {
            out.push_str(": ");
            out.push_str(subject);
        }
        if let Some(merge) = &self.merge {
            let _ = write!(out, " (Merge: {merge})");
        }
        out.push(' ');
        out.push_str(&line_range_tag(self.first_line, self.body_end_line));
        out.push('\n');
        Some(out)
    }
}

fn compact_author(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some((name, _email)) = trimmed.rsplit_once(" <")
        && trimmed.ends_with('>')
    {
        let name = name.trim();
        if !name.is_empty() {
            return name;
        }
    }
    trimmed.trim_matches(['<', '>']).trim()
}

fn month_number(name: &str) -> Option<u8> {
    match name {
        "Jan" => Some(1),
        "Feb" => Some(2),
        "Mar" => Some(3),
        "Apr" => Some(4),
        "May" => Some(5),
        "Jun" => Some(6),
        "Jul" => Some(7),
        "Aug" => Some(8),
        "Sep" => Some(9),
        "Oct" => Some(10),
        "Nov" => Some(11),
        "Dec" => Some(12),
        _ => None,
    }
}

fn is_digits(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit())
}

/// Normalize Git's default / ISO / RFC date strings to `YYYY-MM-DD`, preserving
/// relative or custom date strings verbatim.
fn compact_date(raw: &str) -> String {
    let s = raw.trim();
    let bytes = s.as_bytes();
    // ISO / short: `2026-11-25...`
    if bytes.len() >= 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && is_digits(&s[..4], 4)
        && is_digits(&s[5..7], 2)
        && is_digits(&s[8..10], 2)
    {
        return s[..10].to_owned();
    }
    let parts: Vec<&str> = s.split_whitespace().collect();
    // Default / local ctime: `Wed Nov 25 14:52:23 2026 +0000`
    if parts.len() >= 5
        && let Some(month) = month_number(parts[1])
        && let Ok(day) = parts[2].parse::<u8>()
        && (1..=31).contains(&day)
        && parts[3].contains(':')
        && is_digits(parts[4], 4)
    {
        return format!("{}-{month:02}-{day:02}", parts[4]);
    }
    // RFC 2822: `Wed, 25 Nov 2026 14:52:23 +0000`
    if parts.len() >= 5
        && let Ok(day) = parts[1].parse::<u8>()
        && (1..=31).contains(&day)
        && let Some(month) = month_number(parts[2])
        && is_digits(parts[3], 4)
        && parts[4].contains(':')
    {
        return format!("{}-{month:02}-{day:02}", parts[3]);
    }
    s.to_owned()
}

/// A log's newest commits within the budget, and the count of the rest.
pub struct Log {
    /// `--reverse`: the newest commits come last.
    reverse: bool,
    shape: Shape,
    /// Whether multi-line `Shape::Headers` commits should be compacted to 1-line
    /// entries when valid.
    compact_headers: bool,
    /// Remains true only while every observed commit parses cleanly as a
    /// standard multi-line commit without config-injected stats/diffs/sigs.
    can_compact: bool,
    budget: usize,
    bytes: usize,
    lines: usize,
    commits: usize,
    /// A line did not start a commit where one had to.
    unknown: bool,
    /// Current raw commit being accumulated for verbatim mode or fallback.
    current_raw: Option<(String, usize, usize)>,
    /// Current structured commit being parsed for compact multi-line mode.
    current_parsed: Option<ParsedCommit>,
    /// Verbatim commits kept in order (fallback when `!compact_headers` or `!can_compact`).
    kept_raw: VecDeque<KeptCommit>,
    kept_raw_bytes: usize,
    raw_full: bool,
    /// Compact 1-line commits kept in order when `compact_headers && can_compact`.
    kept_compact: VecDeque<KeptCommit>,
    kept_compact_bytes: usize,
    compact_full: bool,
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
        // can then separate (and which must not be collapsed into 1-line summaries).
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
        let mut compactable_pretty = true;
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
                compactable_pretty = false;
            } else if arg == "--pretty" {
                shape = Some(Shape::Headers);
                compactable_pretty = true;
            } else if let Some(format) = arg
                .strip_prefix("--pretty=")
                .or_else(|| arg.strip_prefix("--format="))
            {
                shape = pretty(format);
                compactable_pretty = matches!(format, "" | "short" | "medium" | "full" | "fuller");
            } else if extra(arg) {
                more = true;
            }
        }
        let shape = shape?;
        // A line per commit holds no room for the lines these options add.
        if shape != Shape::Headers && more {
            return None;
        }
        let compact_headers = shape == Shape::Headers && compactable_pretty && !more;
        Some(Self {
            reverse,
            shape,
            compact_headers,
            can_compact: compact_headers,
            budget,
            bytes: 0,
            lines: 0,
            commits: 0,
            unknown: false,
            current_raw: None,
            current_parsed: None,
            kept_raw: VecDeque::new(),
            kept_raw_bytes: 0,
            raw_full: false,
            kept_compact: VecDeque::new(),
            kept_compact_bytes: 0,
            compact_full: false,
        })
    }

    fn reflog(args: &[&str], budget: usize) -> Option<Self> {
        let mut iter = args.iter().copied().take_while(|arg| *arg != "--");
        if let Some(first) = iter.next() {
            if matches!(first, "expire" | "delete" | "drop" | "exists" | "write") {
                return None;
            }
            // Reject custom formats, patches, graphs, and NUL-delimited reflog output.
            for arg in std::iter::once(first).chain(iter) {
                if arg == "show" || arg == "--oneline" || arg == "--date=relative" {
                    continue;
                }
                if arg.starts_with("--pretty")
                    || arg.starts_with("--format")
                    || matches!(arg, "-p" | "-u" | "--patch" | "--graph" | "-z" | "--stat")
                {
                    return None;
                }
            }
        }
        Some(Self {
            reverse: false,
            shape: Shape::Ids,
            compact_headers: false,
            can_compact: false,
            budget,
            bytes: 0,
            lines: 0,
            commits: 0,
            unknown: false,
            current_raw: None,
            current_parsed: None,
            kept_raw: VecDeque::new(),
            kept_raw_bytes: 0,
            raw_full: false,
            kept_compact: VecDeque::new(),
            kept_compact_bytes: 0,
            compact_full: false,
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
            self.current_raw = Some((String::new(), n, n));
            if self.can_compact {
                self.current_parsed = Some(ParsedCommit::start(line, n));
            }
        } else if let Some(parsed) = self.current_parsed.as_mut() {
            parsed.observe_line(line, n);
            if !parsed.valid {
                self.can_compact = false;
                self.kept_compact.clear();
                self.kept_compact_bytes = 0;
            }
        }
        let want_raw = !self.raw_full || self.reverse;
        if let Some((current, _, end)) = self.current_raw.as_mut() {
            *end = n;
            if want_raw && current.len() <= self.budget {
                current.push_str(text);
            }
        }
    }

    /// Keep the commit just read, if it fits.
    fn close(&mut self) {
        if let Some((text, first_line, raw_end_line)) = self.current_raw.take() {
            push_kept(
                &mut self.kept_raw,
                &mut self.kept_raw_bytes,
                &mut self.raw_full,
                KeptCommit {
                    text,
                    first_line,
                    raw_end_line,
                },
                self.budget,
                self.reverse,
            );
        }
        if let Some(parsed) = self.current_parsed.take()
            && self.can_compact
        {
            if let Some(line) = parsed.format_compact() {
                let want = !self.compact_full || self.reverse;
                if want {
                    push_kept(
                        &mut self.kept_compact,
                        &mut self.kept_compact_bytes,
                        &mut self.compact_full,
                        KeptCommit {
                            text: line,
                            first_line: parsed.first_line,
                            raw_end_line: parsed.raw_end_line,
                        },
                        self.budget,
                        self.reverse,
                    );
                }
            } else {
                self.can_compact = false;
                self.kept_compact.clear();
                self.kept_compact_bytes = 0;
            }
        }
    }

    fn finish(mut self, budget: usize) -> Option<String> {
        self.close();
        if self.unknown {
            return None;
        }
        if self.compact_headers && self.can_compact && !self.kept_compact.is_empty() {
            let over_budget = self.bytes > budget;
            // Even when a multi-line log fits the budget, compact 3+ verbose commits
            // if doing so cuts at least 25% (and stays strictly smaller).
            let compact_all = self.kept_compact.len() == self.commits
                && self.commits >= 3
                && self.kept_compact_bytes <= budget
                && self.kept_compact_bytes * 4 <= self.bytes * 3;
            if over_budget || compact_all {
                if let Some(view) = finish_kept(
                    self.kept_compact,
                    self.kept_compact_bytes,
                    self.commits,
                    self.lines,
                    self.reverse,
                    budget,
                ) && view.len() < self.bytes
                {
                    return Some(view);
                }
            } else {
                return None;
            }
        }
        if self.bytes <= budget {
            return None;
        }
        finish_kept(
            self.kept_raw,
            self.kept_raw_bytes,
            self.commits,
            self.lines,
            self.reverse,
            budget,
        )
        .filter(|view| view.len() < self.bytes)
    }
}

fn push_kept(
    kept: &mut VecDeque<KeptCommit>,
    kept_bytes: &mut usize,
    full: &mut bool,
    commit: KeptCommit,
    budget: usize,
    reverse: bool,
) {
    if reverse {
        if commit.text.len() > budget {
            // Nothing before a commit too long to show can be kept with the
            // newer ones after it.
            kept.clear();
            *kept_bytes = 0;
            return;
        }
        *kept_bytes += commit.text.len();
        kept.push_back(commit);
        while *kept_bytes > budget {
            let old = kept.pop_front().expect("kept commits");
            *kept_bytes -= old.text.len();
        }
    } else if !*full {
        if *kept_bytes + commit.text.len() <= budget {
            *kept_bytes += commit.text.len();
            kept.push_back(commit);
        } else {
            *full = true;
        }
    }
}

fn finish_kept(
    mut kept: VecDeque<KeptCommit>,
    mut kept_bytes: usize,
    commits: usize,
    lines: usize,
    reverse: bool,
    budget: usize,
) -> Option<String> {
    loop {
        if kept.is_empty() {
            return None;
        }
        let rest = commits - kept.len();
        if rest == 0 {
            if kept_bytes <= budget {
                let mut view = String::with_capacity(kept_bytes);
                for commit in &kept {
                    view.push_str(&commit.text);
                }
                return Some(view);
            }
        } else {
            let range = if reverse {
                (1, kept.front()?.first_line.saturating_sub(1))
            } else {
                (kept.back()?.raw_end_line + 1, lines)
            };
            let count = format!(
                "+{} {}\n",
                plural(rest, "older commit", "older commits"),
                line_range_tag(range.0, range.1)
            );
            let ends_line = kept.back().is_none_or(|c| c.text.ends_with('\n'));
            let gap = usize::from(!reverse && !ends_line);
            if kept_bytes + gap + count.len() <= budget {
                let mut view = String::with_capacity(kept_bytes + gap + count.len());
                if reverse {
                    view.push_str(&count);
                }
                for commit in &kept {
                    view.push_str(&commit.text);
                }
                if !reverse {
                    if gap > 0 {
                        view.push('\n');
                    }
                    view.push_str(&count);
                }
                return Some(view);
            }
        }
        let dropped = if reverse {
            kept.pop_front()
        } else {
            kept.pop_back()
        };
        kept_bytes -= dropped?.text.len();
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

// ---------------------------------------------------------------------------
// git status
// ---------------------------------------------------------------------------

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

/// Sections with at most this many entries stay verbatim (with change kinds,
/// rename sources, and submodule notes preserved).
const SMALL_SECTION: usize = 8;

/// A single directory with at least this many entries triggers directory grouping
/// even when the overall status fits inside a generous budget.
const DENSE_DIR_THRESHOLD: usize = 8;

/// One parsed file entry in `git status`.
struct StatusEntry {
    line: usize,
    path: String,
    kind: &'static str,
    from: Option<String>,
    note: Option<String>,
}

/// A status grouped by state and directory, preserving all conflicts, renames,
/// change kinds, and branch headers while stripping usage hints.
pub struct Status {
    /// Short format, known from the first line.
    short: Option<bool>,
    lead: Vec<String>,
    section: Option<State>,
    entries: [Vec<StatusEntry>; 5],
    listings: Vec<Listing>,
    /// Separate path listings excluding renames/copies for `Staged` (0) and
    /// `Modified` (1), so large sections with renames count each entry once.
    non_rename_listings: [Listing; 2],
    counts: [usize; 5],
    dir_counts: [BTreeMap<String, usize>; 5],
    hints: usize,
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
            entries: Default::default(),
            // Tracked paths under build or dependency directories are changes like
            // any other; untracked and ignored ones collapse as in a listing.
            listings: STATES
                .iter()
                .map(|state| Listing::of_paths(!matches!(state, State::Untracked | State::Ignored)))
                .collect(),
            non_rename_listings: [Listing::of_paths(true), Listing::of_paths(true)],
            counts: [0; 5],
            dir_counts: Default::default(),
            hints: 0,
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

    fn add(
        &mut self,
        state: State,
        raw_path: &str,
        kind: &'static str,
        note: Option<&str>,
        n: usize,
    ) {
        // A rename/copy names the old and the new path; keep `from` for zero loss.
        let (from, path) = match raw_path.rsplit_once(" -> ") {
            Some((old, new)) if matches!(kind, "renamed" | "copied") => (Some(old.to_owned()), new),
            Some((_, new)) => (None, new),
            None => (None, raw_path),
        };
        let idx = state.index();
        self.counts[idx] += 1;
        self.listings[idx].observe(path, n);
        if from.is_none() {
            match state {
                State::Staged => self.non_rename_listings[0].observe(path, n),
                State::Modified => self.non_rename_listings[1].observe(path, n),
                _ => {}
            }
        }
        let (parent, _) = split_status_path(path);
        *self.dir_counts[idx].entry(parent.to_owned()).or_insert(0) += 1;
        // Keep full entry metadata for conflicts (always), renames (always), and
        // sections that may render verbatim (cap non-conflict/non-rename entries
        // only when huge so memory stays bounded on 100k-file repos).
        if state == State::Conflicted
            || from.is_some()
            || self.entries[idx].len() <= SMALL_SECTION + 1
        {
            self.entries[idx].push(StatusEntry {
                line: n,
                path: path.to_owned(),
                kind,
                from,
                note: note.map(str::to_owned),
            });
        }
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
            (b'?', b'?') => self.add(State::Untracked, path, "", None, n),
            (b'!', b'!') => self.add(State::Ignored, path, "", None, n),
            (b'?' | b'!', _) | (_, b'?' | b'!') => return false,
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => {
                let kind = short_conflict_kind(x, y);
                self.add(State::Conflicted, path, kind, None, n);
            }
            _ => {
                if x != b' ' {
                    self.add(State::Staged, path, short_change_kind(x), None, n);
                }
                if y != b' ' {
                    // If `x` was a rename (`RM old -> new`), the working-tree edit is on `new`.
                    let dest = path.rsplit_once(" -> ").map_or(path, |(_, new)| new);
                    self.add(State::Modified, dest, short_change_kind(y), None, n);
                }
            }
        }
        true
    }

    fn long_line(&mut self, line: &str, n: usize) -> bool {
        // Usage hints such as `  (use "git add <file>..." to update …)`.
        if line.starts_with("  (") {
            self.hints += 1;
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
            } else if line.starts_with("no changes added to commit")
                || line.starts_with("nothing added to commit")
                || line.starts_with("nothing to commit")
            {
                self.hints += 1;
            } else if !(line.is_empty() || line.starts_with('\t')) {
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
            self.add(state, entry, "", None, n);
            return true;
        }
        let Some((label, path)) = entry.split_once(':') else {
            return false;
        };
        let kind: &'static str = match state {
            State::Conflicted => match label {
                "both modified" => "both modified",
                "both added" => "both added",
                "both deleted" => "both deleted",
                "added by us" => "added by us",
                "added by them" => "added by them",
                "deleted by us" => "deleted by us",
                "deleted by them" => "deleted by them",
                _ => return false,
            },
            _ => match label {
                "modified" => "modified",
                "new file" => "new file",
                "deleted" => "deleted",
                "renamed" => "renamed",
                "copied" => "copied",
                "typechange" => "typechange",
                _ => return false,
            },
        };
        let (clean_path, note) = split_submodule_path(path.trim_start());
        self.add(state, clean_path, kind, note, n);
        true
    }

    fn has_dense_dir(&self) -> bool {
        self.dir_counts
            .iter()
            .any(|dirs| dirs.values().any(|&count| count >= DENSE_DIR_THRESHOLD))
    }

    fn finish(self, budget: usize) -> Option<String> {
        let total: usize = self.counts.iter().sum();
        let should_compact = self.bytes > budget
            || (self.short == Some(false) && self.hints > 0)
            || self.has_dense_dir();
        if self.unknown || total == 0 || !should_compact {
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

        // Always allocate conflicted paths first so they are never squeezed out,
        // then smallest sections first so small sections stay verbatim and large
        // untracked/modified pools fold cleanly into dense directory groups.
        let mut order: Vec<State> = STATES
            .into_iter()
            .filter(|state| self.counts[state.index()] > 0)
            .collect();
        order.sort_by_key(|state| (*state != State::Conflicted, self.counts[state.index()]));

        let mut room = budget.checked_sub(head.len())?;
        let mut reports: [Option<String>; 5] = Default::default();
        let mut listings: Vec<Option<Listing>> = self.listings.into_iter().map(Some).collect();
        let [staged_nr, modified_nr] = self.non_rename_listings;
        let mut non_rename: [Option<Listing>; 2] = [Some(staged_nr), Some(modified_nr)];
        let mut entries_by_state = self.entries;

        for (done, &state) in order.iter().enumerate() {
            let remaining = order.len() - done;
            let equal_share = room / remaining;
            let reserved_for_others = (remaining - 1) * 140;
            let priority_share = room.saturating_sub(reserved_for_others).max(equal_share);
            let idx = state.index();
            let count = self.counts[idx];
            let entries = std::mem::take(&mut entries_by_state[idx]);
            let listing = listings[idx].take()?;
            let nr_listing = match state {
                State::Staged => non_rename[0].take(),
                State::Modified => non_rename[1].take(),
                _ => None,
            };

            let report = render_status_section(
                state,
                count,
                &entries,
                listing,
                nr_listing,
                equal_share,
                priority_share,
            )?;
            room = room.checked_sub(report.len())?;
            reports[idx] = Some(report);
        }
        for report in reports.into_iter().flatten() {
            head.push_str(&report);
        }
        (head.len() <= budget && head.len() < self.bytes).then_some(head)
    }
}

fn short_conflict_kind(x: u8, y: u8) -> &'static str {
    match (x, y) {
        (b'U', b'U') => "both modified",
        (b'A', b'A') => "both added",
        (b'D', b'D') => "both deleted",
        (b'A', b'U') => "added by us",
        (b'U', b'A') => "added by them",
        (b'D', b'U') => "deleted by us",
        (b'U', b'D') => "deleted by them",
        _ => "unmerged",
    }
}

fn short_change_kind(code: u8) -> &'static str {
    match code {
        b'M' => "modified",
        b'A' => "new file",
        b'D' => "deleted",
        b'R' => "renamed",
        b'C' => "copied",
        b'T' => "typechange",
        _ => "",
    }
}

fn split_status_path(path: &str) -> (&str, String) {
    let is_dir = path.len() > 1 && path.ends_with('/');
    let trimmed = path.trim_start_matches("./").trim_end_matches('/');
    let (parent, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
    let display_name = if is_dir {
        format!("{name}/")
    } else {
        name.to_owned()
    };
    (parent, display_name)
}

fn format_verbatim_status_entry(entry: &StatusEntry) -> String {
    let (parent, name) = split_status_path(&entry.path);
    let dir = if parent.is_empty() {
        "./".to_owned()
    } else {
        format!("{parent}/")
    };
    let mut detail = String::new();
    if let Some(from) = &entry.from {
        let _ = write!(detail, " ({} from {from})", entry.kind);
    } else if !entry.kind.is_empty() {
        if let Some(note) = &entry.note {
            let _ = write!(detail, " ({}, {note})", entry.kind);
        } else {
            let _ = write!(detail, " ({})", entry.kind);
        }
    } else if let Some(note) = &entry.note {
        let _ = write!(detail, " ({note})");
    }
    format!("{dir} 1: {name}{detail} [stdout:{}]\n", entry.line)
}

fn render_status_section(
    state: State,
    count: usize,
    entries: &[StatusEntry],
    listing: Listing,
    non_rename_listing: Option<Listing>,
    equal_share: usize,
    priority_share: usize,
) -> Option<String> {
    // Keep 100% of conflicted paths and small sections verbatim whenever they fit.
    let want_verbatim = state == State::Conflicted || count <= SMALL_SECTION;
    if want_verbatim && entries.len() == count {
        let mut out = format!("{}: {}\n", state.name(), plural(count, "entry", "entries"));
        for entry in entries {
            out.push_str(&format_verbatim_status_entry(entry));
        }
        if out.len() <= priority_share {
            return Some(out);
        }
    }

    // In large staged/modified sections, keep any renames verbatim alongside
    // directory grouping for the remaining non-rename entries so every entry is
    // counted once and no rename source is lost.
    let renames: Vec<&StatusEntry> = entries.iter().filter(|e| e.from.is_some()).collect();
    if !renames.is_empty()
        && count > SMALL_SECTION
        && let Some(nr_listing) = non_rename_listing
    {
        let mut rename_lines = String::new();
        for entry in &renames {
            rename_lines.push_str(&format_verbatim_status_entry(entry));
        }
        let non_rename_count = count - renames.len();
        if non_rename_count == 0 {
            let combined = format!(
                "{}: {}\n{rename_lines}",
                state.name(),
                plural(count, "entry", "entries")
            );
            if combined.len() <= priority_share {
                return Some(combined);
            }
        } else if rename_lines.len() + 64 < priority_share {
            let grouped_budget = priority_share.saturating_sub(rename_lines.len() + 48);
            if let Some((report, _)) = nr_listing.finish(state.name(), grouped_budget, None, true)
                && let Some((group_header, body)) = report.split_once('\n')
            {
                let suffix = group_header
                    .strip_prefix(&format!("{}: ", state.name()))
                    .unwrap_or(group_header);
                let header = format!(
                    "{}: {} ({} renamed, {suffix})\n",
                    state.name(),
                    plural(count, "entry", "entries"),
                    renames.len()
                );
                let combined = format!("{header}{rename_lines}{body}");
                if combined.len() <= priority_share {
                    return Some(combined);
                }
            }
        }
    }

    let (report, _) = listing.finish(state.name(), equal_share, None, true)?;
    Some(report)
}

/// Split a submodule's path and its trailing `git status` note, such as
/// `sub (new commits, modified content)`.
fn split_submodule_path(path: &str) -> (&str, Option<&str>) {
    if let Some((name, note)) = path.rsplit_once(" (")
        && let Some(note) = note.strip_suffix(')')
        && note.split(", ").all(|part| {
            matches!(
                part,
                "new commits" | "modified content" | "untracked content"
            )
        })
    {
        return (name, Some(note));
    }
    (path, None)
}

// ---------------------------------------------------------------------------
// git branch
// ---------------------------------------------------------------------------

static BRANCH_HASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{7,64}(?: |$)").unwrap());
static TRACKING_BRACKET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[[^\]\s]+: (?:gone|ahead \d+(?:, behind \d+)?|behind \d+)\]").unwrap()
});

#[derive(Clone, Copy, PartialEq)]
enum TrackState {
    None,
    Diverged,
    Gone,
}

struct BranchEntry {
    line: usize,
    marker: char,
    name: String,
    detail: String,
    track: TrackState,
    remote_prefix: Option<String>,
    short_name: String,
}

/// Prioritized, prefix-tightened `git branch` view.
pub struct Branch {
    remotes_only: bool,
    entries: Vec<BranchEntry>,
    bytes: usize,
    unknown: bool,
}

/// Minimum number of branches before under-budget tightening kicks in.
const BRANCH_TIGHTEN_MIN: usize = 15;

/// Maximum plain local branches printed individually before folding into a
/// compact count + sample list.
const BRANCH_PLAIN_INLINE_MAX: usize = 6;

impl Branch {
    fn new(args: &[&str]) -> Option<Self> {
        let remotes_only = branch_display(args)?;
        Some(Self {
            remotes_only,
            entries: Vec::new(),
            bytes: 0,
            unknown: false,
        })
    }

    fn observe(&mut self, text: &str, n: usize) {
        self.bytes += text.len();
        if self.unknown {
            return;
        }
        let line = text.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            return;
        }
        let Some((marker, rest)) = line
            .strip_prefix("* ")
            .map(|r| ('*', r))
            .or_else(|| line.strip_prefix("+ ").map(|r| ('+', r)))
            .or_else(|| line.strip_prefix("  ").map(|r| (' ', r)))
        else {
            self.unknown = true;
            return;
        };
        let (name, raw_detail) = parse_branch_name(rest);
        if name.is_empty() {
            self.unknown = true;
            return;
        }
        let detail = tighten_branch_detail(raw_detail, marker);
        let track = detect_tracking_state(&detail, marker);
        let (remote_prefix, short_name) = if let Some(after) = name.strip_prefix("remotes/") {
            if let Some((remote, branch)) = after.split_once('/') {
                (Some(format!("remotes/{remote}/")), branch.to_owned())
            } else {
                (None, name.to_owned())
            }
        } else if self.remotes_only {
            if let Some((remote, branch)) = name.split_once('/') {
                (Some(format!("{remote}/")), branch.to_owned())
            } else {
                (None, name.to_owned())
            }
        } else {
            (None, name.to_owned())
        };
        let short_name = if let Some(target) = detail.strip_prefix("-> ") {
            format!("{short_name} -> {target}")
        } else {
            short_name
        };
        self.entries.push(BranchEntry {
            line: n,
            marker,
            name: name.to_owned(),
            detail,
            track,
            remote_prefix,
            short_name,
        });
    }

    fn finish(self, budget: usize) -> Option<String> {
        if self.unknown || self.entries.is_empty() {
            return None;
        }
        let over_budget = self.bytes > budget;
        if !over_budget && self.entries.len() < BRANCH_TIGHTEN_MIN {
            return None;
        }

        let mut priority: Vec<&BranchEntry> = Vec::new();
        let mut plain_local: Vec<&BranchEntry> = Vec::new();
        let mut remotes: BTreeMap<&str, Vec<&BranchEntry>> = BTreeMap::new();

        let mut local_count = 0usize;
        let mut remote_count = 0usize;
        let mut current_count = 0usize;
        let mut diverged_count = 0usize;
        let mut gone_count = 0usize;

        for entry in &self.entries {
            if let Some(prefix) = entry.remote_prefix.as_deref() {
                remote_count += 1;
                remotes.entry(prefix).or_default().push(entry);
            } else {
                local_count += 1;
                if entry.marker == '*' {
                    current_count += 1;
                }
                match entry.track {
                    TrackState::Diverged => diverged_count += 1,
                    TrackState::Gone => gone_count += 1,
                    TrackState::None => {}
                }
                if entry.marker != ' ' || entry.track != TrackState::None {
                    priority.push(entry);
                } else {
                    plain_local.push(entry);
                }
            }
        }

        // Order priority: `*` current first, then `+` worktrees, then `[ahead]/[behind]`, then `[gone]`.
        priority.sort_by_key(|e| {
            let tier = match (e.marker, e.track) {
                ('*', _) => 0u8,
                ('+', _) => 1,
                (_, TrackState::Diverged) => 2,
                (_, TrackState::Gone) => 3,
                _ => 4,
            };
            (tier, e.line)
        });

        let mut out = format_branch_header(
            self.entries.len(),
            local_count,
            remote_count,
            current_count,
            diverged_count,
            gone_count,
        );
        if out.len() >= budget {
            return None;
        }

        // Format remote summary lines first so we know how much room to reserve for them.
        let remote_lines: Vec<(String, usize, usize, usize)> = remotes
            .iter()
            .map(|(prefix, list)| {
                let first = list.iter().map(|e| e.line).min().unwrap_or(1);
                let last = list.iter().map(|e| e.line).max().unwrap_or(first);
                (
                    format_remote_group(prefix, list, 6),
                    list.len(),
                    first,
                    last,
                )
            })
            .collect();

        let total_remote_bytes: usize = remote_lines.iter().map(|(l, ..)| l.len()).sum();
        let remote_reserve = total_remote_bytes.min(budget / 3);
        let mut room = budget.saturating_sub(out.len());

        // Tier 1: Always emit current (`*`), worktree (`+`), `[ahead]`/`[behind]`, and `[gone]` first.
        let mut shown_priority = 0usize;
        let priority_cap = room.saturating_sub(remote_reserve);
        let mut used_priority = 0usize;
        for entry in &priority {
            let line = format_branch_entry(entry);
            if used_priority + line.len() > priority_cap && shown_priority > 0 {
                break;
            }
            if out.len() + line.len() > budget {
                break;
            }
            out.push_str(&line);
            used_priority += line.len();
            shown_priority += 1;
        }
        if shown_priority < priority.len() {
            let rest = &priority[shown_priority..];
            let first = rest.iter().map(|e| e.line).min().unwrap_or(1);
            let last = rest.iter().map(|e| e.line).max().unwrap_or(first);
            let summary = format!(
                "+{} {}\n",
                plural(rest.len(), "more tracked branch", "more tracked branches"),
                line_range_tag(first, last)
            );
            if out.len() + summary.len() <= budget {
                out.push_str(&summary);
            }
        }
        room = budget.saturating_sub(out.len());

        // Tier 2: Plain local branches.
        if !plain_local.is_empty() {
            let local_room = room.saturating_sub(remote_reserve);
            let inline: Option<String> = (plain_local.len() <= BRANCH_PLAIN_INLINE_MAX)
                .then(|| {
                    let mut buf = String::new();
                    for entry in &plain_local {
                        buf.push_str(&format_branch_entry(entry));
                    }
                    buf
                })
                .filter(|buf| buf.len() <= local_room);

            if let Some(buf) = inline {
                out.push_str(&buf);
            } else {
                let folded = format_plain_local_fold(&plain_local, local_room);
                if out.len() + folded.len() <= budget {
                    out.push_str(&folded);
                }
            }
        }

        // Tier 3: Grouped remote prefixes (`remotes/origin/ (N branches): ... [stdout:A-B]`).
        let mut shown_remotes = 0usize;
        for (idx, (line, _, _, _)) in remote_lines.iter().enumerate() {
            let rest_count = remote_lines.len() - idx - 1;
            let reserve_tail = if rest_count > 0 { 64 } else { 0 };
            if out.len() + line.len() + reserve_tail <= budget {
                out.push_str(line);
                shown_remotes += 1;
            } else {
                // Try a tighter sample count (2 names) for this remote group.
                if let Some((prefix, list)) = remotes.iter().nth(idx) {
                    let tighter = format_remote_group(prefix, list, 2);
                    if out.len() + tighter.len() + reserve_tail <= budget {
                        out.push_str(&tighter);
                        shown_remotes += 1;
                        continue;
                    }
                }
                break;
            }
        }
        if shown_remotes < remote_lines.len() {
            let omitted = &remote_lines[shown_remotes..];
            let branches: usize = omitted.iter().map(|(_, count, _, _)| *count).sum();
            let first = omitted.iter().map(|(_, _, f, _)| *f).min().unwrap_or(1);
            let last = omitted.iter().map(|(_, _, _, l)| *l).max().unwrap_or(first);
            let tail = format!(
                "+{} ({}) {}\n",
                plural(omitted.len(), "more remote", "more remotes"),
                plural(branches, "branch", "branches"),
                line_range_tag(first, last)
            );
            if out.len() + tail.len() <= budget {
                out.push_str(&tail);
            }
        }

        let saves_enough = over_budget || out.len() * 4 <= self.bytes * 3;
        (saves_enough && out.len() <= budget && out.len() < self.bytes).then_some(out)
    }
}

/// Returns `Some(remotes_only)` when `git branch` runs in a read-only display
/// mode this view understands, or `None` for mutating or machine-formatted runs.
fn branch_display(args: &[&str]) -> Option<bool> {
    let mut list = false;
    let mut remotes_only = false;
    let mut all = false;
    let mut iter = args.iter().copied().peekable();
    while let Some(arg) = iter.next() {
        if let Some(short) = arg
            .strip_prefix('-')
            .filter(|s| !s.is_empty() && !s.starts_with('-'))
        {
            for c in short.chars() {
                match c {
                    'a' => all = true,
                    'r' => remotes_only = true,
                    'v' | 'i' => {}
                    _ => return None,
                }
            }
            continue;
        }
        if arg.starts_with("--") {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(f, v)| (f, Some(v)));
            match flag {
                "--list" if inline.is_none() => list = true,
                "--all" if inline.is_none() => all = true,
                "--remotes" if inline.is_none() => remotes_only = true,
                "--verbose" | "--ignore-case" | "--no-abbrev" | "--no-color" | "--no-column"
                    if inline.is_none() => {}
                "--color" | "--abbrev" => {
                    if inline.is_some_and(str::is_empty) {
                        return None;
                    }
                }
                "--merged" | "--no-merged" => {
                    if inline.is_some_and(str::is_empty) {
                        return None;
                    }
                    if inline.is_none() && iter.peek().is_some_and(|next| !next.starts_with('-')) {
                        iter.next();
                    }
                }
                "--contains" | "--no-contains" | "--points-at" => {
                    let value = match inline {
                        Some(v) => Some(v),
                        None => iter.next(),
                    };
                    if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                        return None;
                    }
                }
                "--sort" => {
                    let value = match inline {
                        Some(v) => Some(v),
                        None => iter.next(),
                    };
                    if value.is_none_or(|v| v.is_empty() || v.starts_with("--")) {
                        return None;
                    }
                }
                _ => return None,
            }
            continue;
        }
        if !list {
            return None;
        }
    }
    Some(remotes_only && !all)
}

fn parse_branch_name(rest: &str) -> (&str, &str) {
    if rest.starts_with('(')
        && let Some(close) = rest.find(')')
    {
        return (&rest[..=close], rest[close + 1..].trim_start());
    }
    match rest.split_once(char::is_whitespace) {
        Some((name, tail)) => (name, tail.trim_start()),
        None => (rest, ""),
    }
}

fn tighten_branch_detail(raw: &str, marker: char) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Preserve symbolic ref targets cleanly (`-> origin/main`).
    if let Some(target) = trimmed.strip_prefix("->") {
        return format!("-> {}", target.trim());
    }
    // Collapse column padding between optional `(/path/to/wt)`, `<hash>`, and subject.
    let mut parts = Vec::new();
    let mut rest = trimmed;
    if marker == '+'
        && rest.starts_with('(')
        && let Some(close) = rest.find(')')
    {
        parts.push(&rest[..=close]);
        rest = rest[close + 1..].trim_start();
    }
    if let Some((first, tail)) = rest.split_once(char::is_whitespace) {
        parts.push(first);
        let tail = tail.trim_start();
        if !tail.is_empty() {
            parts.push(tail);
        }
    } else if !rest.is_empty() {
        parts.push(rest);
    }
    parts.join(" ")
}

fn detect_tracking_state(detail: &str, marker: char) -> TrackState {
    let mut rest = detail;
    if marker == '+'
        && rest.starts_with('(')
        && let Some(close) = rest.find(')')
    {
        rest = rest[close + 1..].trim_start();
    }
    if !BRANCH_HASH.is_match(rest) {
        return TrackState::None;
    }
    let Some((_, after_hash)) = rest.split_once(' ') else {
        return TrackState::None;
    };
    let after_hash = after_hash.trim_start();
    let Some(m) = TRACKING_BRACKET.find(after_hash) else {
        return TrackState::None;
    };
    let bracket = m.as_str();
    if bracket.contains(": gone]") {
        TrackState::Gone
    } else {
        TrackState::Diverged
    }
}

fn format_branch_header(
    total: usize,
    local: usize,
    remote: usize,
    current: usize,
    diverged: usize,
    gone: usize,
) -> String {
    let mut parts = Vec::new();
    if local > 0 && remote > 0 {
        parts.push(format!("{local} local, {remote} remote"));
    } else if local > 0 {
        parts.push(format!("{local} local"));
    } else if remote > 0 {
        parts.push(format!("{remote} remote"));
    }
    if current > 0 {
        parts.push(format!("{current} current"));
    }
    if diverged > 0 {
        parts.push(format!("{diverged} ahead/behind"));
    }
    if gone > 0 {
        parts.push(format!("{gone} gone"));
    }
    format!(
        "git branch: {} ({})\n",
        plural(total, "branch", "branches"),
        parts.join(" · ")
    )
}

fn format_branch_entry(entry: &BranchEntry) -> String {
    if entry.detail.is_empty() {
        format!("{} {} [stdout:{}]\n", entry.marker, entry.name, entry.line)
    } else {
        format!(
            "{} {} {} [stdout:{}]\n",
            entry.marker, entry.name, entry.detail, entry.line
        )
    }
}

fn format_plain_local_fold(entries: &[&BranchEntry], room: usize) -> String {
    let first = entries.iter().map(|e| e.line).min().unwrap_or(1);
    let last = entries.iter().map(|e| e.line).max().unwrap_or(first);
    let range = line_range_tag(first, last);
    for max_names in [8, 6, 4, 2, 0] {
        let take = entries.len().min(max_names);
        let line = if take == 0 {
            format!("local ({} other) {range}\n", entries.len())
        } else {
            let names: Vec<&str> = entries[..take].iter().map(|e| e.name.as_str()).collect();
            let more = entries.len() - take;
            if more > 0 {
                format!(
                    "local ({} other): {} +{more} more {range}\n",
                    entries.len(),
                    names.join(", ")
                )
            } else {
                format!(
                    "local ({} other): {} {range}\n",
                    entries.len(),
                    names.join(", ")
                )
            }
        };
        if line.len() <= room || max_names == 0 {
            return line;
        }
    }
    unreachable!()
}

fn format_remote_group(prefix: &str, entries: &[&BranchEntry], max_names: usize) -> String {
    let first = entries.iter().map(|e| e.line).min().unwrap_or(1);
    let last = entries.iter().map(|e| e.line).max().unwrap_or(first);
    let range = line_range_tag(first, last);
    // Prioritize `HEAD -> ...`, `main`, `master`, `develop` in the preview names.
    let mut ranked: Vec<usize> = (0..entries.len()).collect();
    ranked.sort_by_key(|&i| {
        let s = entries[i].short_name.as_str();
        let rank = if s.starts_with("HEAD -> ") || s == "HEAD" {
            0u8
        } else if matches!(s, "main" | "master" | "develop") {
            1
        } else {
            2
        };
        (rank, i)
    });
    let take = entries.len().min(max_names);
    let names: Vec<&str> = ranked[..take]
        .iter()
        .map(|&i| entries[i].short_name.as_str())
        .collect();
    let more = entries.len() - take;
    if more > 0 {
        format!(
            "{prefix} ({}): {} +{more} more {range}\n",
            plural(entries.len(), "branch", "branches"),
            names.join(", ")
        )
    } else {
        format!(
            "{prefix} ({}): {} {range}\n",
            plural(entries.len(), "branch", "branches"),
            names.join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// git tag
// ---------------------------------------------------------------------------

struct TagEntry {
    name: String,
    annotation: Option<String>,
    first_line: usize,
    last_line: usize,
}

/// Compact `git tag` view preserving both newest and oldest tags (or leading
/// tags when sorted descending) with exact counts and `[stdout:A-B]` links.
pub struct Tag {
    descending: bool,
    entries: Vec<TagEntry>,
    bytes: usize,
    unknown: bool,
}

const TAG_TIGHTEN_MIN: usize = 30;

impl Tag {
    fn new(args: &[&str]) -> Option<Self> {
        let descending = tag_display(args)?;
        Some(Self {
            descending,
            entries: Vec::new(),
            bytes: 0,
            unknown: false,
        })
    }

    fn observe(&mut self, text: &str, n: usize) {
        self.bytes += text.len();
        if self.unknown {
            return;
        }
        let line = text.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            return;
        }
        // Continuation lines of `git tag -n2+` start with indentation.
        if line.starts_with([' ', '\t']) {
            if let Some(last) = self.entries.last_mut() {
                last.last_line = n;
            } else {
                self.unknown = true;
            }
            return;
        }
        let (name, note) = match line.split_once(char::is_whitespace) {
            Some((name, rest)) => {
                let note = rest.trim();
                (name, (!note.is_empty()).then(|| note.to_owned()))
            }
            None => (line, None),
        };
        self.entries.push(TagEntry {
            name: name.to_owned(),
            annotation: note,
            first_line: n,
            last_line: n,
        });
    }

    fn finish(self, budget: usize) -> Option<String> {
        if self.unknown || self.entries.is_empty() {
            return None;
        }
        let over_budget = self.bytes > budget;
        if !over_budget && self.entries.len() < TAG_TIGHTEN_MIN {
            return None;
        }
        let header = format!("git tag: {}\n", plural(self.entries.len(), "tag", "tags"));
        if header.len() >= budget {
            return None;
        }
        let formatted: Vec<String> = self.entries.iter().map(format_tag_entry).collect();
        let all_len: usize = header.len() + formatted.iter().map(String::len).sum::<usize>();
        if all_len <= budget {
            let mut out = String::with_capacity(all_len);
            out.push_str(&header);
            for line in &formatted {
                out.push_str(line);
            }
            let saves_enough = over_budget || out.len() * 4 <= self.bytes * 3;
            return (saves_enough && out.len() < self.bytes).then_some(out);
        }

        // Fit newest entries (head when `--sort=-...`, otherwise small head + tail).
        let total = self.entries.len();
        if self.descending {
            let mut body = String::new();
            let mut shown = 0usize;
            for (i, line) in formatted.iter().enumerate() {
                let rest = total - i - 1;
                let footer_len = if rest > 0 { 48 } else { 0 };
                if header.len() + body.len() + line.len() + footer_len > budget {
                    break;
                }
                body.push_str(line);
                shown += 1;
            }
            if shown == 0 {
                return None;
            }
            let rest = total - shown;
            let from = self.entries[shown].first_line;
            let to = self.entries[total - 1].last_line;
            let footer = format!(
                "+{} {}\n",
                plural(rest, "more tag", "more tags"),
                line_range_tag(from, to)
            );
            let out = format!("{header}{body}{footer}");
            return (out.len() <= budget && out.len() < self.bytes).then_some(out);
        }

        // Ascending order (Git default): keep first few tags + newest tail tags.
        let head_target = 3.min(total);
        let mut head_count = 0usize;
        let mut head_bytes = 0usize;
        for line in formatted.iter().take(head_target) {
            if header.len() + head_bytes + line.len() + 96 > budget {
                break;
            }
            head_bytes += line.len();
            head_count += 1;
        }
        let mut tail_indices = Vec::new();
        let mut tail_bytes = 0usize;
        for i in (head_count..total).rev() {
            let line = &formatted[i];
            let mid_count = i - head_count;
            let mid_est = if mid_count > 0 { 48 } else { 0 };
            if header.len() + head_bytes + tail_bytes + line.len() + mid_est > budget {
                break;
            }
            tail_bytes += line.len();
            tail_indices.push(i);
        }
        tail_indices.reverse();
        if head_count == 0 && tail_indices.is_empty() {
            return None;
        }
        let tail_start = tail_indices.first().copied().unwrap_or(total);
        let mid_count = tail_start.saturating_sub(head_count);
        let mut out = String::with_capacity(budget.min(self.bytes));
        out.push_str(&header);
        for line in formatted.iter().take(head_count) {
            out.push_str(line);
        }
        if mid_count > 0 {
            let from = self.entries[head_count].first_line;
            let to = self.entries[tail_start - 1].last_line;
            let _ = writeln!(
                out,
                "+{} {}",
                plural(mid_count, "more tag", "more tags"),
                line_range_tag(from, to)
            );
        }
        for i in tail_indices {
            out.push_str(&formatted[i]);
        }
        (out.len() <= budget && out.len() < self.bytes).then_some(out)
    }
}

fn format_tag_entry(entry: &TagEntry) -> String {
    match &entry.annotation {
        Some(note) if entry.last_line > entry.first_line => {
            format!(
                "{} {} {}\n",
                entry.name,
                note,
                line_range_tag(entry.first_line, entry.last_line)
            )
        }
        Some(note) => format!("{} {}\n", entry.name, note),
        None => format!("{}\n", entry.name),
    }
}

/// Returns `Some(descending)` when `git tag` runs in read-only listing mode.
fn tag_display(args: &[&str]) -> Option<bool> {
    let mut list = false;
    let mut descending = false;
    let mut iter = args.iter().copied().peekable();
    while let Some(arg) = iter.next() {
        if let Some(short) = arg
            .strip_prefix('-')
            .filter(|s| !s.is_empty() && !s.starts_with('-'))
        {
            let mut chars = short.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    'l' => list = true,
                    'i' => {}
                    'n' => {
                        let rest: String = chars.collect();
                        if !rest.is_empty() {
                            if !rest.bytes().all(|b| b.is_ascii_digit()) {
                                return None;
                            }
                        } else if iter.peek().is_some_and(|next| {
                            !next.is_empty() && next.bytes().all(|b| b.is_ascii_digit())
                        }) {
                            iter.next();
                        }
                        break;
                    }
                    _ => return None,
                }
            }
            continue;
        }
        if arg.starts_with("--") {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(f, v)| (f, Some(v)));
            match flag {
                "--list" if inline.is_none() => list = true,
                "--ignore-case" | "--no-color" | "--no-column" if inline.is_none() => {}
                "--color" => {
                    if inline.is_some_and(str::is_empty) {
                        return None;
                    }
                }
                "--merged" | "--no-merged" => {
                    if inline.is_some_and(str::is_empty) {
                        return None;
                    }
                    if inline.is_none() && iter.peek().is_some_and(|next| !next.starts_with('-')) {
                        iter.next();
                    }
                }
                "--contains" | "--no-contains" | "--points-at" => {
                    let value = match inline {
                        Some(v) => Some(v),
                        None => iter.next(),
                    };
                    if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                        return None;
                    }
                }
                "--sort" => {
                    let value = match inline {
                        Some(v) => Some(v),
                        None => iter.next(),
                    };
                    let val = value.filter(|v| !v.is_empty() && !v.starts_with("--"))?;
                    if val.starts_with('-') {
                        descending = true;
                    }
                }
                _ => return None,
            }
            continue;
        }
        if !list {
            return None;
        }
    }
    Some(descending)
}

// ---------------------------------------------------------------------------
// git stash list
// ---------------------------------------------------------------------------

static STASH_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^stash@\{[^}]+\}: ").unwrap());

/// A `git stash list` view that keeps the newest stashes and folds older ones
/// with exact count and `[stdout:A-B]` links.
pub struct Stash {
    budget: usize,
    bytes: usize,
    lines: usize,
    count: usize,
    kept: VecDeque<(String, usize)>,
    kept_bytes: usize,
    full: bool,
    unknown: bool,
}

impl Stash {
    fn new(args: &[&str], budget: usize) -> Option<Self> {
        let ["list", rest @ ..] = args else {
            return None;
        };
        if !stash_list_display(rest) {
            return None;
        }
        Some(Self {
            budget,
            bytes: 0,
            lines: 0,
            count: 0,
            kept: VecDeque::new(),
            kept_bytes: 0,
            full: false,
            unknown: false,
        })
    }

    fn observe(&mut self, text: &str, n: usize) {
        self.bytes += text.len();
        self.lines = n;
        if self.unknown {
            return;
        }
        let line = text.trim_end_matches(['\n', '\r']);
        if !STASH_LINE.is_match(line) {
            self.unknown = true;
            return;
        }
        self.count += 1;
        if !self.full {
            if self.kept_bytes + text.len() <= self.budget {
                self.kept_bytes += text.len();
                self.kept.push_back((text.to_owned(), n));
            } else {
                self.full = true;
            }
        }
    }

    fn finish(mut self, budget: usize) -> Option<String> {
        if self.unknown || self.bytes <= budget || self.count == 0 {
            return None;
        }
        loop {
            if self.kept.is_empty() {
                return None;
            }
            let rest = self.count - self.kept.len();
            if rest > 0 {
                let from = self.kept.back()?.1 + 1;
                let footer = format!(
                    "+{} {}\n",
                    plural(rest, "older stash", "older stashes"),
                    line_range_tag(from, self.lines)
                );
                let gap = usize::from(self.kept.back().is_some_and(|(t, _)| !t.ends_with('\n')));
                if self.kept_bytes + gap + footer.len() <= budget {
                    let mut out = String::with_capacity(self.kept_bytes + gap + footer.len());
                    for (text, _) in &self.kept {
                        out.push_str(text);
                    }
                    if gap > 0 {
                        out.push('\n');
                    }
                    out.push_str(&footer);
                    return (out.len() < self.bytes).then_some(out);
                }
            }
            let (dropped, _) = self.kept.pop_back()?;
            self.kept_bytes -= dropped.len();
        }
    }
}

fn stash_list_display(args: &[&str]) -> bool {
    for &arg in args.iter().take_while(|a| **a != "--") {
        let flag = arg.split_once('=').map_or(arg, |(k, _)| k);
        if matches!(
            flag,
            "-p" | "-u"
                | "--patch"
                | "--patch-with-stat"
                | "--patch-with-raw"
                | "--stat"
                | "--numstat"
                | "--shortstat"
                | "--name-only"
                | "--name-status"
                | "--raw"
                | "--summary"
                | "--dirstat"
                | "--compact-summary"
                | "--pretty"
                | "--format"
                | "--graph"
                | "-z"
                | "--null"
                | "--output"
                | "--ext-diff"
                | "--textconv"
                | "--porcelain"
                | "--word-diff"
                | "--color-words"
                | "--binary"
        ) || flag.starts_with("-U")
            || flag.starts_with("--unified")
        {
            return false;
        }
    }
    true
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
            "git branch",
            "git branch -a",
            "git branch -vv",
            "git branch -avv",
            "git branch -r --sort=-committerdate",
            "git branch --merged main",
            "git branch --list feat/*",
            "git tag",
            "git tag -l v1.*",
            "git tag -n",
            "git tag -n3",
            "git tag --sort=-v:refname",
            "git stash list",
            "git stash list -n 20",
            "git reflog",
            "git reflog show main",
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
            "git branch new-branch",
            "git branch -d old",
            "git branch -D old",
            "git branch -m old new",
            "git branch --format=%(refname:short)",
            "git branch --column",
            "git branch --show-current",
            "git tag v1.0.0",
            "git tag -a v1.0.0 -m release",
            "git tag -d v1.0.0",
            "git tag --format=%(refname:short)",
            "git tag --column",
            "git stash",
            "git stash push",
            "git stash pop",
            "git stash list -p",
            "git stash list --stat",
            "git stash list --format=%gd",
            "git reflog expire --all",
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
    fn a_full_log_compacts_commits_and_cuts_between_them() {
        let log: String = (0..40)
            .rev()
            .map(|i| {
                format!(
                    "commit {:040x}\nAuthor: Alice Smith <alice@example.com>\nDate:   Mon Nov 2 10:00:00 2026 +0000\n\n    change {i}\n\n    body line\n",
                    i + 1
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let report = view("git log", &log, 1000).unwrap();
        assert!(report.len() <= 1000, "{report}");
        let lines: Vec<&str> = report.lines().collect();
        let kept = lines.len() - 1;
        assert!(
            kept >= 10,
            "expected >=10 compact commits in 1000B, got {kept}:\n{report}"
        );
        assert!(
            lines[0].starts_with("0000000 2026-11-02 Alice Smith: change 39 [stdout:1-7]"),
            "{}",
            lines[0]
        );
        let footer = lines[kept];
        let total = log.lines().count();
        let expected_from = kept * 8 + 1;
        assert_eq!(
            footer,
            format!(
                "+{} older commits [stdout:{expected_from}-{total}]",
                40 - kept
            )
        );
        let next = log.lines().nth(expected_from - 1).unwrap();
        assert!(next.starts_with("commit "), "{next}");
    }

    #[test]
    fn a_stat_log_keeps_verbatim_commits_and_cuts_between_them() {
        let log: String = (0..40)
            .rev()
            .map(|i| {
                format!(
                    "commit {:040x}\nAuthor: A <a@example.com>\nDate:   Mon Nov 2 10:00:00 2026 +0000\n\n    change {i}\n\n src/lib.rs | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n",
                    i + 1
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let report = view("git log --stat", &log, 1000).unwrap();
        let commits = report.matches("\ncommit ").count() + 1;
        let shown = report.lines().count() - 1;
        let total = log.lines().count();
        assert!(report.starts_with("commit "), "{report}");
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
    fn verbose_multiline_git_log_30_commits_achieves_high_reduction_with_decorations_and_merges() {
        let mut log = String::new();
        for i in (1..=30).rev() {
            let hash = format!("{:07x}{:033x}", 0x1a2b300 + i, i);
            let dec = if i == 30 {
                " (HEAD -> main, tag: v1.0, origin/main)"
            } else if i == 25 {
                " (tag: v0.9)"
            } else {
                ""
            };
            let _ = writeln!(log, "commit {hash}{dec}");
            if i == 29 {
                let _ = writeln!(log, "Merge: a1b2c3d e4f5a6b");
            }
            let _ = writeln!(log, "Author: Alice Developer <alice.developer@example.com>");
            let _ = writeln!(log, "Date:   Wed Nov 25 14:52:{:02} 2026 +0000", i % 60);
            log.push('\n');
            let _ = writeln!(log, "    feat(gitview): improve output handling step {i}");
            log.push('\n');
            let _ = writeln!(
                log,
                "    Detailed commit body explaining why step {i} is needed across the"
            );
            let _ = writeln!(
                log,
                "    repository, including migration notes and verification details."
            );
            if i > 1 {
                log.push('\n');
            }
        }
        // Even with a generous budget where all 30 commits fit after compaction,
        // verbose multi-line logs compact cleanly and cut >= 65% of bytes.
        let report = view("git log", &log, 16_384).unwrap();
        let raw_bytes = log.len();
        let compact_bytes = report.len();
        let reduction_pct = (raw_bytes - compact_bytes) * 100 / raw_bytes;
        assert!(
            reduction_pct >= 65,
            "expected >=65% reduction, got {reduction_pct}% ({raw_bytes} -> {compact_bytes})\n{report}"
        );
        assert_eq!(report.lines().count(), 30, "{report}");
        assert!(
            report.contains("(HEAD -> main, tag: v1.0, origin/main) 2026-11-25 Alice Developer: feat(gitview): improve output handling step 30 [stdout:1-8]"),
            "{report}"
        );
        assert!(
            report.contains("(Merge: a1b2c3d e4f5a6b) [stdout:10-18]"),
            "{report}"
        );
        assert!(report.contains("(tag: v0.9)"), "{report}");
        assert!(!report.contains("Detailed commit body"), "{report}");

        // `--reverse` preserves oldest-folded-first and newest-at-bottom.
        let report_rev = view("git log --reverse", &log, 1024).unwrap();
        assert!(report_rev.len() <= 1024, "{report_rev}");
        assert!(report_rev.starts_with("+"), "{report_rev}");
        assert!(
            report_rev.contains("older commits [stdout:1-"),
            "{report_rev}"
        );
    }

    #[test]
    fn compact_date_handles_iso_rfc_ctime_and_relative() {
        assert_eq!(compact_date("Wed Nov 25 14:52:23 2026 +0545"), "2026-11-25");
        assert_eq!(compact_date("Mon Nov 2 10:00:00 2026 +0000"), "2026-11-02");
        assert_eq!(compact_date("2026-11-25 14:52:23 +0000"), "2026-11-25");
        assert_eq!(compact_date("2026-11-25T14:52:23+05:45"), "2026-11-25");
        assert_eq!(compact_date("2026-11-25"), "2026-11-25");
        assert_eq!(
            compact_date("Wed, 25 Nov 2026 14:52:23 +0545"),
            "2026-11-25"
        );
        assert_eq!(compact_date("2 hours ago"), "2 hours ago");
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
        assert!(
            report.contains("conflict.rs (both modified) [stdout:2]"),
            "{report}"
        );
        assert!(
            report.contains("renamed.rs (renamed from old.rs) [stdout:5]"),
            "{report}"
        );
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
        assert!(
            report.contains("vendor (modified, new commits)"),
            "{report}"
        );
        assert!(!report.contains("(use"), "{report}");
        assert!(
            report.contains("80 entries in 1 directory under logs/"),
            "{report}"
        );
    }

    #[test]
    fn large_git_status_with_conflicts_renames_hints_and_40_untracked_files() {
        let mut status = String::from(concat!(
            "On branch feat/merge-work\n",
            "Your branch and 'origin/feat/merge-work' have diverged,\n",
            "and have 2 and 3 different commits each, respectively.\n",
            "  (use \"git pull\" if you want to integrate the remote branch with yours)\n",
            "You have unmerged paths.\n",
            "  (fix conflicts and run \"git commit\")\n",
            "  (use \"git merge --abort\" to abort the merge)\n",
            "\n",
            "Changes to be committed:\n",
            "  (use \"git restore --staged <file>...\" to unstage)\n",
            "\trenamed:    src/old_auth.rs -> src/auth/session.rs\n",
            "\tnew file:   src/auth/token.rs\n",
            "\n",
            "Unmerged paths:\n",
            "  (use \"git add/rm <file>...\" as appropriate to mark resolution)\n",
            "\tboth modified:   src/api/handler.rs\n",
            "\tboth added:      src/api/routes.rs\n",
            "\tdeleted by us:   src/legacy/v1.rs\n",
            "\tdeleted by them: src/legacy/v2.rs\n",
            "\tboth modified:   src/api/auth.rs\n",
            "\tboth modified:   src/api/middleware.rs\n",
            "\tboth modified:   src/api/errors.rs\n",
            "\tboth modified:   src/api/metrics.rs\n",
            "\n",
            "Changes not staged for commit:\n",
            "  (use \"git add/rm <file>...\" to update what will be committed)\n",
            "  (use \"git restore <file>...\" to discard changes in working directory)\n",
            "\tmodified:   Cargo.toml\n",
            "\tdeleted:    docs/deprecated.md\n",
            "\n",
            "Untracked files:\n",
            "  (use \"git add <file>...\" to include in what will be committed)\n",
        ));
        for i in 0..24 {
            let _ = writeln!(status, "\tgenerated/proto/msg_{i:02}.rs");
        }
        for i in 0..16 {
            let _ = writeln!(status, "\ttmp/snapshots/snap_{i:02}.json");
        }
        status
            .push_str("\nno changes added to commit (use \"git add\" and/or \"git commit -a\")\n");

        let report = view("git status", &status, 4096).unwrap();
        let reduction_pct = (status.len() - report.len()) * 100 / status.len();
        assert!(
            reduction_pct >= 40,
            "expected >=40% reduction, got {reduction_pct}% ({} -> {})\n{report}",
            status.len(),
            report.len()
        );
        // Zero usage hints remain.
        assert!(!report.contains("(use "), "{report}");
        assert!(!report.contains("(fix conflicts"), "{report}");
        assert!(!report.contains("no changes added to commit"), "{report}");
        // Branch header + diverged status + exact counts preserved.
        assert!(report.contains("On branch feat/merge-work\n"), "{report}");
        assert!(report.contains("have diverged,"), "{report}");
        assert!(
            report.contains("8 conflicted · 2 staged · 2 modified · 40 untracked\n"),
            "{report}"
        );
        // 100% of all 8 conflicted files preserved verbatim with conflict kinds (never capped at 6!).
        for needle in [
            "handler.rs (both modified)",
            "routes.rs (both added)",
            "v1.rs (deleted by us)",
            "v2.rs (deleted by them)",
            "auth.rs (both modified)",
            "middleware.rs (both modified)",
            "errors.rs (both modified)",
            "metrics.rs (both modified)",
        ] {
            assert!(
                report.contains(needle),
                "missing conflict {needle}:\n{report}"
            );
        }
        // Renames and small sections preserved verbatim.
        assert!(
            report.contains("session.rs (renamed from src/old_auth.rs)"),
            "{report}"
        );
        assert!(report.contains("token.rs (new file)"), "{report}");
        assert!(report.contains("Cargo.toml (modified)"), "{report}");
        assert!(report.contains("deprecated.md (deleted)"), "{report}");
        // Dense untracked directories grouped cleanly with `[stdout:A-B]`.
        assert!(
            report.contains("generated/proto/ 24: msg_00.rs"),
            "{report}"
        );
        assert!(
            report.contains("tmp/snapshots/ 16: snap_00.json"),
            "{report}"
        );
    }

    #[test]
    fn large_staged_section_preserves_renames_without_double_counting() {
        let mut status = String::from(concat!(
            "On branch main\n",
            "\n",
            "Changes to be committed:\n",
            "  (use \"git restore --staged <file>...\" to unstage)\n",
            "\trenamed:    src/legacy_a.rs -> src/core/a.rs\n",
            "\trenamed:    src/legacy_b.rs -> src/core/b.rs\n",
        ));
        for i in 0..16 {
            let _ = writeln!(status, "\tmodified:   src/handlers/h{i:02}.rs");
        }
        let report = view("git status", &status, 2048).unwrap();
        assert!(
            report.contains("0 conflicted · 18 staged · 0 modified · 0 untracked\n"),
            "{report}"
        );
        assert!(
            report.contains(
                "staged: 18 entries (2 renamed, 16 entries in 1 directory under src/handlers/)"
            ),
            "{report}"
        );
        assert!(
            report.contains("a.rs (renamed from src/legacy_a.rs) [stdout:5]"),
            "{report}"
        );
        assert!(
            report.contains("b.rs (renamed from src/legacy_b.rs) [stdout:6]"),
            "{report}"
        );
        assert!(report.contains("./ 16: h00.rs"), "{report}");
        assert!(!report.contains("(use "), "{report}");
    }

    #[test]
    fn git_log_single_commit_stays_verbatim_and_config_injected_lines_fall_back() {
        // Single verbose commit under budget stays verbatim so `git log -1` shows full body.
        let single = concat!(
            "commit 0000000000000000000000000000000000000001 (HEAD -> main)\n",
            "Author: Alice <a@example.com>\n",
            "Date:   Wed Nov 25 14:52:23 2026 +0000\n",
            "\n",
            "    feat: single commit\n",
            "\n",
            "    full body that agent asked to read\n",
        );
        assert_eq!(view("git log", single, 4096), None);

        // `--pretty=fuller` preserves distinct committer.
        let mut fuller = String::new();
        for i in (1..=6).rev() {
            let _ = write!(
                fuller,
                "commit {:040x}\nAuthor:     Alice <a@example.com>\nAuthorDate: Wed Nov 25 14:52:23 2026 +0000\nCommit:     Bob <b@example.com>\nCommitDate: Wed Nov 25 15:00:00 2026 +0000\n\n    subject {i}\n\n    body line {i}\n\n",
                i
            );
        }
        let report = view("git log --pretty=fuller", &fuller, 4096).unwrap();
        assert!(
            report.contains("2026-11-25 Alice (committer: Bob): subject 6 [stdout:1-9]"),
            "{report}"
        );

        // Config-injected GPG signature lines cause clean fallback to verbatim commit-boundary cut.
        let gpg_log: String = (0..30)
            .rev()
            .map(|i| {
                format!(
                    "commit {:040x}\ngpg: Signature made Wed Nov 25 14:52:23 2026 UTC\nAuthor: Alice <a@example.com>\nDate:   Wed Nov 25 14:52:23 2026 +0000\n\n    signed {i}\n",
                    i + 1
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let fallback = view("git log", &gpg_log, 1000).unwrap();
        assert!(fallback.starts_with("commit "), "{fallback}");
        assert!(fallback.contains("gpg: Signature made"), "{fallback}");
        assert!(fallback.contains("older commits [stdout:"), "{fallback}");
    }

    #[test]
    fn git_branch_local_only_vv_and_remote_only_r_compact_cleanly() {
        let mut local_vv = String::new();
        local_vv
            .push_str("  feat/synced-00                1000000 [origin/feat/synced-00] synced 0\n");
        local_vv.push_str(
            "  feat/gone-branch              1000001 [origin/feat/gone-branch: gone] merged\n",
        );
        local_vv.push_str(
            "* develop                       1000002 [origin/develop: ahead 3] active work\n",
        );
        for i in 1..25 {
            let _ = writeln!(
                local_vv,
                "  feat/synced-{i:02}                {:07x} [origin/feat/synced-{i:02}] synced {i}",
                0x2000000 + i
            );
        }
        let report = view("git branch -vv", &local_vv, 1024).unwrap();
        let pos_dev = report
            .find("* develop 1000002 [origin/develop: ahead 3]")
            .unwrap();
        let pos_gone = report
            .find("feat/gone-branch 1000001 [origin/feat/gone-branch: gone]")
            .unwrap();
        let pos_fold = report.find("local (25 other):").unwrap();
        assert!(pos_dev < pos_gone && pos_gone < pos_fold, "{report}");

        let mut remote_r = String::from("  origin/HEAD -> origin/main\n  origin/main\n");
        for i in 0..40 {
            let _ = writeln!(remote_r, "  origin/feat/branch-{i:02}");
        }
        for i in 0..8 {
            let _ = writeln!(remote_r, "  upstream/release-{i}");
        }
        let r_report = view("git branch -r", &remote_r, 512).unwrap();
        assert!(
            r_report.contains("origin/ (42 branches): HEAD -> origin/main, main,"),
            "{r_report}"
        );
        assert!(r_report.contains("upstream/ (8 branches):"), "{r_report}");
        assert!(r_report.contains("[stdout:1-42]"), "{r_report}");
        assert!(r_report.contains("[stdout:43-50]"), "{r_report}");
    }

    #[test]
    fn git_tag_multiline_annotations_fold_behind_ranges() {
        let mut tags = String::new();
        for i in 0..40 {
            let _ = writeln!(tags, "v2.{i}.0          Release 2.{i}.0 summary");
            let _ = writeln!(tags, "    Detailed changelog line 1 for v2.{i}.0");
            let _ = writeln!(tags, "    Detailed changelog line 2 for v2.{i}.0");
        }
        let report = view("git tag -n3", &tags, 1024).unwrap();
        assert!(report.len() <= 1024, "{report}");
        assert!(
            report.contains("v2.0.0 Release 2.0.0 summary [stdout:1-3]"),
            "{report}"
        );
        assert!(
            report.contains("v2.39.0 Release 2.39.0 summary [stdout:118-120]"),
            "{report}"
        );
        assert!(!report.contains("Detailed changelog line"), "{report}");
    }

    #[test]
    fn a_status_with_unknown_lines_keeps_the_preview() {
        let mut status = String::from("On branch main\n\nSubmodule changes to be committed:\n\n");
        status.push_str(&"* sub abc..def (1):\n  > change\n".repeat(100));
        assert_eq!(view("git status", &status, 512), None);
    }

    #[test]
    fn large_git_branch_vv_and_all_prioritizes_current_gone_diverged_and_groups_remotes() {
        let mut out = String::new();
        // 30 local branches with wide padding: 1 current, 1 worktree, 2 ahead/behind, 3 gone, 23 plain.
        out.push_str(
            "  feat/plain-00                      1000000 [origin/feat/plain-00] plain branch 0\n",
        );
        out.push_str(
            "  feat/stale-old-work                1000001 [origin/feat/stale-old-work: gone] old wip\n",
        );
        out.push_str(
            "  feat/plain-01                      1000002 [origin/feat/plain-01] plain branch 1\n",
        );
        out.push_str(
            "  feat/needs-pull                    1000003 [origin/feat/needs-pull: behind 4] lagging\n",
        );
        out.push_str(
            "* main                               1eee17a [origin/main: ahead 2] expand outlining\n",
        );
        out.push_str(
            "+ wt/hotfix (/tmp/wt)                2222222 [origin/wt/hotfix: ahead 1] urgent fix\n",
        );
        out.push_str(
            "  feat/abandoned-api                 1000004 [origin/feat/abandoned-api: gone] dropped pr\n",
        );
        out.push_str(
            "  feat/diverged                      1000005 [origin/feat/diverged: ahead 1, behind 3] rebase me\n",
        );
        out.push_str(
            "  feat/removed-upstream              1000006 [origin/feat/removed-upstream: gone] merged pr\n",
        );
        for i in 2..23 {
            let _ = writeln!(
                out,
                "  feat/plain-{i:02}                      {:07x} [origin/feat/plain-{i:02}] plain branch {i}",
                0x2000000 + i
            );
        }
        // 50 remote branches across origin and upstream.
        out.push_str("  remotes/origin/HEAD                -> origin/main\n");
        out.push_str("  remotes/origin/main                1eee17a expand outlining\n");
        for i in 0..43 {
            let _ = writeln!(
                out,
                "  remotes/origin/feature/topic-{i:02}    {:07x} remote topic commit {i}",
                0x3000000 + i
            );
        }
        for i in 0..5 {
            let _ = writeln!(
                out,
                "  remotes/upstream/release-{i}         {:07x} upstream release {i}",
                0x4000000 + i
            );
        }

        let report = view("git branch -avv", &out, 2048).unwrap();
        let reduction_pct = (out.len() - report.len()) * 100 / out.len();
        assert!(
            reduction_pct >= 70,
            "expected >=70% reduction, got {reduction_pct}% ({} -> {})\n{report}",
            out.len(),
            report.len()
        );
        assert!(report.len() <= 2048, "{report}");
        assert!(
            report.starts_with(
                "git branch: 80 branches (30 local, 50 remote · 1 current · 4 ahead/behind · 3 gone)\n"
            ),
            "{report}"
        );
        // Priority ordering: `* main` first, then `+ wt/hotfix`, then ahead/behind, then gone, before plain/remotes.
        let pos_main = report
            .find("* main 1eee17a [origin/main: ahead 2]")
            .unwrap();
        let pos_wt = report.find("+ wt/hotfix (/tmp/wt) 2222222").unwrap();
        let pos_behind = report
            .find("feat/needs-pull 1000003 [origin/feat/needs-pull: behind 4]")
            .unwrap();
        let pos_diverged = report
            .find("feat/diverged 1000005 [origin/feat/diverged: ahead 1, behind 3]")
            .unwrap();
        let pos_gone = report
            .find("feat/stale-old-work 1000001 [origin/feat/stale-old-work: gone]")
            .unwrap();
        let pos_local_fold = report.find("local (23 other):").unwrap();
        let pos_origin = report.find("remotes/origin/ (45 branches):").unwrap();
        let pos_upstream = report.find("remotes/upstream/ (5 branches):").unwrap();
        assert!(pos_main < pos_wt && pos_wt < pos_behind && pos_behind < pos_diverged);
        assert!(pos_diverged < pos_gone && pos_gone < pos_local_fold);
        assert!(pos_local_fold < pos_origin && pos_origin < pos_upstream);
        assert!(report.contains("HEAD -> origin/main"), "{report}");
        assert!(report.contains("[stdout:31-75]"), "{report}");
        assert!(report.contains("[stdout:76-80]"), "{report}");
    }

    #[test]
    fn large_git_tag_and_stash_list_compact_with_exact_ranges() {
        let mut tags = String::new();
        for i in 0..150 {
            let _ = writeln!(tags, "v1.{i}.0");
        }
        let report = view("git tag", &tags, 300).unwrap();
        assert!(report.len() <= 300, "{report}");
        assert!(
            report.starts_with("git tag: 150 tags\nv1.0.0\n"),
            "{report}"
        );
        assert!(report.contains("more tags [stdout:"), "{report}");
        assert!(report.ends_with("v1.149.0\n"), "{report}");

        let desc: String = (0..150).rev().map(|i| format!("v1.{i}.0\n")).collect();
        let desc_report = view("git tag --sort=-v:refname", &desc, 300).unwrap();
        assert!(desc_report.len() <= 300, "{desc_report}");
        assert!(
            desc_report.starts_with("git tag: 150 tags\nv1.149.0\n"),
            "{desc_report}"
        );
        assert!(desc_report.contains("more tags [stdout:"), "{desc_report}");

        let mut stashes = String::new();
        for i in 0..40 {
            let _ = writeln!(
                stashes,
                "stash@{{{i}}}: WIP on main: {:07x} temporary experiment {i}",
                0xabc0000 + i
            );
        }
        let stash_report = view("git stash list", &stashes, 512).unwrap();
        assert!(stash_report.len() <= 512, "{stash_report}");
        assert!(
            stash_report.starts_with("stash@{0}: WIP on main:"),
            "{stash_report}"
        );
        assert!(
            stash_report.contains("older stashes [stdout:"),
            "{stash_report}"
        );
        assert!(stash_report.ends_with("-40]\n"), "{stash_report}");
    }

    #[test]
    fn large_git_reflog_keeps_newest_entries_with_line_range_for_older_ones() {
        let mut reflog = String::new();
        for i in 0..120 {
            reflog.push_str(&format!(
                "{i:07x} HEAD@{{{i}}}: commit: update feature step {i}\n"
            ));
        }
        let report = view("git reflog", &reflog, 512).unwrap();
        assert!(report.len() <= 512, "len={}: {report}", report.len());
        assert!(
            report.contains("HEAD@{0}: commit: update feature step 0"),
            "{report}"
        );
        assert!(report.contains("older commits [stdout:"), "{report}");
        assert_eq!(view("git reflog expire --all", &reflog, 512), None);
    }
}
