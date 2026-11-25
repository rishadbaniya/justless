//! Compact records point back to inclusive line ranges in the original stdout.
use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default budget for the middle "Compact Diff" tier when `Summary::render` does not
/// receive an explicit budget from `capture.rs`.
pub(crate) const COMPACT_DIFF_BUDGET: usize = 4096;
/// Hard cap on buffered compact diff lines across non-lockfile files so huge multi-MB
/// diffs stay strictly bounded in memory.
const COMPACT_BUFFER_CAP: usize = 32 * 1024;
/// Per-file buffer cap for lockfile hunks before dropping them (lockfiles are replaced
/// by `lockdiff::summarize` anyway, so an oversized lockfile must not push code files
/// out of the compact diff tier).
const LOCKFILE_BUFFER_CAP: usize = 8 * 1024;

/// Text in a summary comes from captured output. Records written before redaction can
/// hold raw secrets, so code that prints a stored summary must pass it through
/// `redact::Redactor`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub kind: String,
    pub items: Vec<Item>,
    pub outcomes: Vec<String>,
    /// `cargo test` totals per test binary, when they account for every line of
    /// both streams. The report then stands in for both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo: Option<crate::cargo_tests::Totals>,
    /// Structured JS/TS or Playwright test report, when recognized output
    /// accounts for both streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub js: Option<crate::js_tests::Report>,
    /// Total raw bytes observed for a diff/patch run (in-memory only).
    #[serde(skip)]
    pub(crate) diff_raw_bytes: usize,
    /// Whether non-lockfile compact diff lines exceeded `COMPACT_BUFFER_CAP`.
    #[serde(skip)]
    pub(crate) compact_overflowed: bool,
    /// Structured commit/file entries for tiered diff rendering (in-memory only).
    #[serde(skip)]
    pub(crate) diff_entries: Vec<DiffEntry>,
}

#[derive(Clone, Debug)]
pub(crate) enum DiffEntry {
    Commit { item_index: usize },
    File(DiffFile),
}

#[derive(Clone, Debug)]
pub(crate) struct DiffFile {
    path: String,
    first: usize,
    last: usize,
    added: usize,
    deleted: usize,
    meta_lines: Vec<String>,
    /// Compact hunks (1-line trimmed context), or `None` if dropped due to cap.
    hunks: Option<Vec<CompactHunk>>,
    items_start: usize,
    items_end: usize,
    generated_folded: bool,
}

#[derive(Clone, Debug)]
struct CompactHunk {
    header: String,
    lines: Vec<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    pub detail: String,
    pub digest: String,
    pub first: usize,
    pub last: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stderr: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub added: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub deleted: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub file_first: usize,
}

pub fn family(args: &[OsString]) -> String {
    let all: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
    let owned = crate::wrappers::effective(&all);
    let refs: Vec<&str> = match &owned {
        Some(words) => words.iter().map(String::as_str).collect(),
        None => all.clone(),
    };
    let tool = crate::filters::unwrap_tool(&refs);
    let mut words: Vec<_> = tool.iter().take(3).copied().collect();
    if let Some(command) = words.first_mut() {
        *command = std::path::Path::new(*command)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
    }
    // `git log` prints patches only when asked; other logs stay in `git`.
    let patch = || {
        tool.iter().skip(2).take_while(|w| **w != "--").any(|w| {
            matches!(*w, "-p" | "-u" | "--patch")
                || w.starts_with("-U")
                || w.starts_with("--unified=")
        })
    };
    let playwright_display = || {
        let mut iter = tool.iter().skip(2);
        while let Some(&arg) = iter.next() {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(f, v)| (f, Some(v)));
            match flag {
                "--list" | "--ui" | "--debug" | "--headed" | "--help" | "--version" => {
                    return false;
                }
                "--reporter" => {
                    let val = inline.or_else(|| iter.next().copied()).unwrap_or("");
                    if !matches!(val, "list" | "line" | "dot") {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    };
    let rspec_display = || {
        let mut iter = tool.iter().skip(1);
        while let Some(&arg) = iter.next() {
            if arg == "--" {
                break;
            }
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(f, v)| (f, Some(v)));
            match flag {
                "-h" | "--help" | "-v" | "--version" | "--dry-run" | "--init" => {
                    return false;
                }
                "-f" | "--format" => {
                    let val = inline.or_else(|| iter.next().copied()).unwrap_or("");
                    if !matches!(
                        val,
                        "p" | "progress" | "d" | "doc" | "documentation" | "f" | "failures"
                    ) {
                        return false;
                    }
                }
                _ if arg.starts_with("-f") && arg.len() > 2 => {
                    let val = &arg[2..];
                    if !matches!(
                        val,
                        "p" | "progress" | "d" | "doc" | "documentation" | "f" | "failures"
                    ) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    };
    let phpunit_display = |skip: usize| {
        for &arg in tool.iter().skip(skip) {
            if arg == "--" {
                break;
            }
            let flag = arg.split_once('=').map_or(arg, |(f, _)| f);
            if matches!(
                flag,
                "-h" | "--help"
                    | "--version"
                    | "--list-tests"
                    | "--list-suites"
                    | "--list-groups"
                    | "--teamcity"
                    | "--testdox-html"
                    | "--testdox-xml"
                    | "--log-junit"
                    | "--log-teamcity"
                    | "--log-events-text"
                    | "--log-events-verbose-text"
            ) {
                return false;
            }
        }
        true
    };
    match words.as_slice() {
        ["git", "diff", ..] | ["gh", "pr", "diff", ..] | ["glab", "mr", "diff", ..] => "git diff",
        ["git", "show", ..] => "git show",
        ["git", "log", ..] if patch() => "git log",
        ["cargo", "test", ..] => "cargo test",
        ["cargo", "nextest", "run", ..] => "cargo nextest",
        ["go", "test", ..] => "go test",
        ["npm", "test", ..] => "npm test",
        ["playwright", "test", ..] if playwright_display() => "playwright test",
        ["pytest", ..] | ["python" | "python3", "-m", "pytest"] => "pytest",
        ["rspec", ..] if rspec_display() => "rspec",
        ["phpunit" | "paratest" | "pest", ..] if phpunit_display(1) => "phpunit",
        ["php", "artisan", "test", ..] if phpunit_display(3) => "phpunit",
        ["terraform" | "tofu", "plan", ..] => "terraform plan",
        [cmd @ ("cargo" | "git" | "go" | "npm" | "rg"), ..] => cmd,
        _ => "other",
    }
    .into()
}

const LIMIT: usize = 2048;

pub struct Parser {
    inventory: Option<crate::inventory::Inventory>,
    cargo: Option<crate::cargo_tests::Scan>,
    js_tests: Option<crate::js_tests::Scan>,
    rspec: Option<RspecScan>,
    phpunit: Option<PhpunitScan>,
    summary: Summary,
    hash: Sha256,
    file: String,
    pending: Option<Item>,
    failures: BTreeMap<String, Item>,
    current_test: String,
    /// Message lines still expected after `panicked at path:line:col:`.
    panic_lines: usize,
    /// `go test` package and per-test diagnostic state.
    go_tests: bool,
    go_ok: usize,
    go_no_tests: usize,
    go_ok_first: usize,
    go_ok_last: usize,
    go_ok_samples: Vec<String>,
    go_active: Option<(String, Vec<(usize, String)>)>,
    go_after_fail: Option<(String, usize)>,
    /// `git show` and `git log -p` output: a commit header before each patch.
    commits: bool,
    place: Place,
    /// Whether that output reached a file's patch, which a plain log never does.
    patched: bool,
    discarded: bool,
    /// The output can stop in the middle of an item, as `| head` leaves it.
    ends_early: bool,
    /// The old and new lines the current hunk's header announced and that have not
    /// appeared yet.
    hunk: Option<(usize, usize)>,
    /// Whether every hunk so far held the lines its header announced, as a patch
    /// whose lines no filter removed does.
    whole_hunks: bool,
    /// Parent count for combined merge diffs (`0` for standard `@@`, `>= 2` for `@@@`).
    combined_parents: usize,
    /// Raw bytes observed across a diff/patch stream.
    diff_raw_bytes: usize,
    /// Buffered commit and file entries for tiered diff rendering.
    diff_entries: Vec<DiffEntry>,
    /// Currently open file section while scanning a diff/patch.
    current_file: Option<ActiveDiffFile>,
    /// Total buffered compact diff bytes across non-lockfile files.
    compact_bytes: usize,
    /// Whether non-lockfile compact diff lines exceeded `COMPACT_BUFFER_CAP`.
    compact_overflowed: bool,
}

struct ActiveDiffFile {
    path: String,
    first: usize,
    last: usize,
    added: usize,
    deleted: usize,
    meta_lines: Vec<String>,
    saw_mode_header: bool,
    hunks: Option<Vec<CompactHunk>>,
    current_hunk: Option<ActiveCompactHunk>,
    buffered_bytes: usize,
    items_start: usize,
    is_generated: bool,
    is_lockfile: bool,
    generated_folded: bool,
}

struct ActiveCompactHunk {
    header: String,
    combined_parents: usize,
    raw_lines: Vec<String>,
    raw_bytes: usize,
}

/// Where a line of `git show` or `git log -p` output falls.
#[derive(Clone, Copy, PartialEq)]
enum Place {
    /// Before the first commit, or after the blank line that ends one.
    Between,
    /// `Author:`, `Date:` and the other fields after `commit SHA`.
    Fields,
    /// The indented message, after the blank line that ends the fields.
    Message,
    /// A `--stat` block between the commit message and its patch.
    Stat,
    /// Blank line after a `--stat` block, before `diff --git` / `diff --cc` or the next commit.
    AfterStat,
    /// A file's extended header lines, from `diff --git` / `diff --cc` to its first hunk.
    File,
    Hunk,
}

impl Parser {
    pub fn new(family: &str, profile: Option<crate::filters::Profile>) -> Self {
        let is_js_tests = profile == Some(crate::filters::Profile::JsTests)
            || (profile.is_none() && family == "playwright test");
        let commits = profile.is_none() && matches!(family, "git show" | "git log");
        let kind = match family {
            _ if is_js_tests => "tests",
            _ if profile.is_some() => "",
            "git diff" => "diff",
            _ if commits => "diff",
            "terraform plan" => "plan",
            "pytest" | "cargo test" | "cargo nextest" | "go test" | "npm test" | "rspec"
            | "phpunit" => "tests",
            _ => "",
        };
        Self {
            inventory: profile
                .filter(|p| p.inventory())
                .map(crate::inventory::Inventory::new),
            cargo: match family {
                "cargo test" if profile.is_none() => Some(crate::cargo_tests::Scan::default()),
                "cargo nextest" if profile.is_none() => Some(crate::cargo_tests::Scan::nextest()),
                _ => None,
            },
            js_tests: is_js_tests.then(crate::js_tests::Scan::default),
            rspec: (family == "rspec" && profile.is_none()).then(RspecScan::default),
            phpunit: (family == "phpunit" && profile.is_none()).then(PhpunitScan::default),
            summary: Summary {
                kind: kind.into(),
                ..Default::default()
            },
            hash: Sha256::new(),
            file: String::new(),
            pending: None,
            failures: BTreeMap::new(),
            current_test: String::new(),
            panic_lines: 0,
            go_tests: family == "go test" && profile.is_none(),
            go_ok: 0,
            go_no_tests: 0,
            go_ok_first: 0,
            go_ok_last: 0,
            go_ok_samples: Vec::new(),
            go_active: None,
            go_after_fail: None,
            commits,
            place: Place::Between,
            patched: false,
            discarded: false,
            ends_early: false,
            hunk: None,
            whole_hunks: true,
            combined_parents: 0,
            diff_raw_bytes: 0,
            diff_entries: Vec::new(),
            current_file: None,
            compact_bytes: 0,
            compact_overflowed: false,
        }
    }

    /// Read output that can stop in the middle of an item: a last hunk that holds
    /// fewer lines than its header announced is marked as cut off.
    pub fn ends_early(&mut self) {
        self.ends_early = true;
    }

    /// Count `line` against the current hunk's announced lines. A header or any
    /// line outside a hunk ends it, and a hunk header starts the next one.
    fn count_hunk(&mut self, line: &str) {
        if self.combined_parents >= 2 {
            if let Some(parents) = combined_hunk_parents(line) {
                self.combined_parents = parents;
                self.hunk = None;
                return;
            }
            if hunk_counts(line).is_some() {
                self.combined_parents = 0;
                self.hunk = hunk_counts(line);
                return;
            }
            if !line.starts_with([' ', '+', '-', '\\']) {
                self.combined_parents = 0;
            }
            return;
        }
        let remaining = match (self.hunk, line.as_bytes().first()) {
            (Some((old, new)), Some(b' ')) if old > 0 && new > 0 => Some((old - 1, new - 1)),
            (Some((old, new)), Some(b'-')) if old > 0 => Some((old - 1, new)),
            (Some((old, new)), Some(b'+')) if new > 0 => Some((old, new - 1)),
            (Some(counts), Some(b'\\')) => Some(counts),
            // A hunk line past the announced count, or a short hunk, means a filter
            // changed the patch.
            (Some(counts), first) => {
                self.whole_hunks &= counts == (0, 0) && !matches!(first, Some(b' ' | b'-' | b'+'));
                None
            }
            (None, _) => None,
        };
        if remaining.is_some() {
            self.hunk = remaining;
        } else if let Some(parents) = combined_hunk_parents(line) {
            self.combined_parents = parents;
            self.hunk = None;
        } else {
            self.hunk = hunk_counts(line);
        }
    }

    pub fn observe(&mut self, text: &str, number: usize) {
        if (self.summary.kind.is_empty() && self.inventory.is_none()) || self.discarded {
            return;
        }
        if text.len() >= 256 * 1024
            || text.contains('\0')
            || self.summary.items.len() + self.failures.len() >= LIMIT
            || self.summary.outcomes.len() >= 128
        {
            self.discarded = true;
            return;
        }
        if let Some(inventory) = &mut self.inventory {
            self.discarded = !inventory.observe(text, number);
            return;
        }
        if self.summary.kind == "diff" {
            self.diff_raw_bytes += text.len();
        }
        let line = text.trim_end_matches('\n');
        if self.commits {
            self.discarded = !self.commit(line, number);
            return;
        }
        if self.summary.kind == "diff"
            && self.file.is_empty()
            && !line.is_empty()
            && diff_header_path(line).is_none()
        {
            if is_stat_line(line) || is_stat_summary(line) || is_stat_mode_line(line) {
                return;
            }
            self.discarded = true;
            return;
        }
        if self.summary.kind == "diff" {
            self.diff(line, number);
        } else if self.summary.kind == "plan" {
            self.plan(line, number);
        } else if let Some(js) = &mut self.js_tests {
            js.observe(line, number, crate::capture::Stream::Stdout);
        } else if let Some(rspec) = &mut self.rspec {
            rspec.observe(line, number);
        } else if let Some(phpunit) = &mut self.phpunit {
            phpunit.observe(line, number);
        } else {
            if let Some(cargo) = &mut self.cargo {
                cargo.observe(line, number, crate::capture::Stream::Stdout);
            }
            self.test(line, number);
        }
    }

    /// Read a line of stderr, where `cargo test` and JS runners such as Jest,
    /// Vitest and Bun print test output.
    pub fn observe_stderr(&mut self, text: &str, number: usize) {
        if let Some(cargo) = &mut self.cargo {
            cargo.observe(text, number, crate::capture::Stream::Stderr);
        }
        if let Some(js) = &mut self.js_tests
            && !self.discarded
        {
            if text.len() >= 256 * 1024 || text.contains('\0') {
                self.discarded = true;
                return;
            }
            js.observe(
                text.trim_end_matches('\n'),
                number,
                crate::capture::Stream::Stderr,
            );
        }
    }

    fn flush(&mut self) {
        if let Some(mut item) = self.pending.take() {
            item.digest = format!("{:x}", self.hash.finalize_reset());
            self.summary.items.push(item);
        }
    }

    fn flush_compact_hunk(
        file: &mut ActiveDiffFile,
        compact_bytes: &mut usize,
        compact_overflowed: &mut bool,
    ) {
        let Some(hunk) = file.current_hunk.take() else {
            return;
        };
        if file.generated_folded {
            return;
        }
        let Some(hunks) = file.hunks.as_mut() else {
            return;
        };
        let trimmed = trim_hunk_context(&hunk.raw_lines, hunk.combined_parents);
        let added_bytes: usize =
            hunk.header.len() + 1 + trimmed.iter().map(|l| l.len() + 1).sum::<usize>();
        if file.is_lockfile {
            if file.buffered_bytes + added_bytes > LOCKFILE_BUFFER_CAP {
                file.hunks = None;
                return;
            }
            file.buffered_bytes += added_bytes;
        } else {
            if *compact_bytes + added_bytes > COMPACT_BUFFER_CAP {
                *compact_overflowed = true;
                file.hunks = None;
                return;
            }
            *compact_bytes += added_bytes;
            file.buffered_bytes += added_bytes;
        }
        hunks.push(CompactHunk {
            header: hunk.header,
            lines: trimmed,
        });
    }

    fn finish_diff_file(&mut self) {
        self.flush();
        let Some(mut file) = self.current_file.take() else {
            return;
        };
        Self::flush_compact_hunk(
            &mut file,
            &mut self.compact_bytes,
            &mut self.compact_overflowed,
        );
        let items_end = self.summary.items.len();
        self.diff_entries.push(DiffEntry::File(DiffFile {
            path: file.path,
            first: file.first,
            last: file.last,
            added: file.added,
            deleted: file.deleted,
            meta_lines: file.meta_lines,
            hunks: if file.generated_folded {
                None
            } else {
                file.hunks
            },
            items_start: file.items_start,
            items_end,
            generated_folded: file.generated_folded,
        }));
    }

    /// Read one line of `git show` or `git log -p` output. Each commit header, from
    /// `commit SHA` through its message, is an item before the commit's file and
    /// hunk items. Supports optional `--stat` blocks before the patch and merge
    /// commits (`diff --cc` / `diff --combined`). A line out of place, such as a
    /// `--raw` tree diff or a custom `--format`, returns false.
    fn commit(&mut self, line: &str, n: usize) -> bool {
        const FIELDS: [&str; 6] = [
            "Merge: ",
            "Author: ",
            "AuthorDate: ",
            "Commit: ",
            "CommitDate: ",
            "Date: ",
        ];
        const HEADERS: [&str; 16] = [
            "index ",
            "mode ",
            "--- ",
            "+++ ",
            "old mode ",
            "new mode ",
            "deleted file mode ",
            "new file mode ",
            "similarity index ",
            "dissimilarity index ",
            "rename from ",
            "rename to ",
            "copy from ",
            "copy to ",
            "Binary files ",
            "GIT binary patch",
        ];
        let place = self.place;
        if let Some(id) = commit_id(line)
            && place != Place::Fields
        {
            self.finish_diff_file();
            self.file.clear();
            let item_index = self.summary.items.len();
            self.diff_entries.push(DiffEntry::Commit { item_index });
            let name = format!("commit {}", short(id, 12));
            self.pending = Some(Item::new(&name, NO_MESSAGE, n));
            self.place = Place::Fields;
        } else if line.is_empty() {
            // A blank line ends the fields, a `--stat` block, or the patch before
            // the next commit. It belongs to no item.
            self.place = match place {
                Place::Fields | Place::Message => Place::Message,
                Place::Stat | Place::AfterStat => Place::AfterStat,
                _ => Place::Between,
            };
            return true;
        } else if diff_header_path(line).is_some() && place != Place::Between {
            self.patched = true;
            self.place = Place::File;
        } else if is_hunk_header(line) && matches!(place, Place::File | Place::Hunk) {
            self.place = Place::Hunk;
        } else if place == Place::Message && line == "---" {
            self.flush();
            self.place = Place::Stat;
            return true;
        } else if matches!(place, Place::Message | Place::Stat)
            && (is_stat_line(line) || is_stat_summary(line) || is_stat_mode_line(line))
        {
            if place == Place::Message {
                // Keep the commit header's `[stdout:first-last]` scoped to its
                // message, excluding the `--stat` block.
                self.flush();
            }
            self.place = Place::Stat;
            return true;
        } else {
            let fits = match place {
                Place::Between | Place::AfterStat | Place::Stat => false,
                Place::Fields => FIELDS.iter().any(|field| line.starts_with(field)),
                Place::Message => line.starts_with("    "),
                Place::File => HEADERS.iter().any(|header| line.starts_with(header)),
                Place::Hunk => line.starts_with([' ', '+', '-', '\\']),
            };
            if !fits {
                return false;
            }
            if place == Place::Message
                && let Some(item) = &mut self.pending
                && item.detail == NO_MESSAGE
                && !line.trim().is_empty()
            {
                item.detail = short(line.trim(), 240);
            }
        }
        self.diff(line, n);
        true
    }

    fn diff(&mut self, line: &str, n: usize) {
        self.count_hunk(line);
        if let Some(path) = diff_header_path(line) {
            self.finish_diff_file();
            self.file = path.to_owned();
            let items_start = self.summary.items.len();
            let is_generated = crate::lockdiff::is_generated_path(&self.file);
            let is_lockfile = crate::lockdiff::is_lockfile_path(&self.file);
            let mut item = Item::new(&self.file, "file metadata", n);
            item.file_first = n;
            self.pending = Some(item);
            self.current_file = Some(ActiveDiffFile {
                path: self.file.clone(),
                first: n,
                last: n,
                added: 0,
                deleted: 0,
                meta_lines: Vec::new(),
                saw_mode_header: false,
                hunks: if !is_lockfile && self.compact_overflowed {
                    None
                } else {
                    Some(Vec::new())
                },
                current_hunk: None,
                buffered_bytes: 0,
                items_start,
                is_generated,
                is_lockfile,
                generated_folded: false,
            });
        } else if is_hunk_header(line) && !self.file.is_empty() {
            if let Some(file) = self.current_file.as_mut() {
                Self::flush_compact_hunk(
                    file,
                    &mut self.compact_bytes,
                    &mut self.compact_overflowed,
                );
                file.last = n;
                if file.hunks.is_some() && !file.generated_folded {
                    file.current_hunk = Some(ActiveCompactHunk {
                        header: line.to_owned(),
                        combined_parents: combined_hunk_parents(line).unwrap_or(0),
                        raw_lines: Vec::new(),
                        raw_bytes: 0,
                    });
                }
            }
            // Ordinary index/---/+++ headers contain blob hashes for the entire file.
            // Avoid invalidating independent hunks when just one of them changes.
            if self
                .pending
                .as_ref()
                .is_some_and(|i| i.detail == "file metadata")
            {
                self.pending = None;
                self.hash = Sha256::new();
            }
            self.flush();
            let mut item = Item::new(&self.file, line, n);
            if let Some(file) = self.current_file.as_ref() {
                item.file_first = file.first;
            }
            self.pending = Some(item);
        } else {
            if (line.starts_with("old mode ")
                || line.starts_with("rename from ")
                || line.starts_with("new file mode ")
                || line.starts_with("deleted file mode ")
                || line.starts_with("copy from "))
                && let Some(item) = &mut self.pending
            {
                item.detail = short(line, 360);
            }
            if let Some(file) = self.current_file.as_mut() {
                file.last = n;
                if file.is_generated
                    && !file.generated_folded
                    && file.last.saturating_sub(file.first) + 1
                        > crate::lockdiff::GENERATED_FOLD_LINES
                {
                    file.generated_folded = true;
                    file.current_hunk = None;
                    if !file.is_lockfile {
                        self.compact_bytes = self.compact_bytes.saturating_sub(file.buffered_bytes);
                    }
                    file.buffered_bytes = 0;
                    file.hunks = None;
                }
                let in_hunk = self
                    .pending
                    .as_ref()
                    .is_some_and(|i| is_hunk_header(&i.detail));
                if in_hunk {
                    let (add, del) = hunk_line_delta(line, self.combined_parents);
                    if add {
                        file.added += 1;
                        if let Some(item) = &mut self.pending {
                            item.added += 1;
                        }
                    } else if del {
                        file.deleted += 1;
                        if let Some(item) = &mut self.pending {
                            item.deleted += 1;
                        }
                    }
                    if !file.generated_folded
                        && let Some(active_hunk) = file.current_hunk.as_mut()
                    {
                        let cap = if file.is_lockfile {
                            LOCKFILE_BUFFER_CAP
                        } else {
                            COMPACT_BUFFER_CAP
                        };
                        if active_hunk.raw_bytes + line.len() < cap * 2 {
                            active_hunk.raw_bytes += line.len() + 1;
                            active_hunk.raw_lines.push(line.to_owned());
                        } else {
                            file.current_hunk = None;
                            file.hunks = None;
                            if !file.is_lockfile {
                                self.compact_overflowed = true;
                            }
                        }
                    }
                } else if is_preserved_meta_header(line, file.saw_mode_header) {
                    if line.starts_with("new file mode ") || line.starts_with("deleted file mode ")
                    {
                        file.saw_mode_header = true;
                    }
                    if file.meta_lines.len() < 16 {
                        file.meta_lines.push(line.to_owned());
                    }
                }
            }
        }
        if let Some(item) = &mut self.pending {
            // A hunk is compared by its lines, not by where it starts, so hunks
            // keep their identity when an earlier change shifts them.
            if !is_hunk_header(line) {
                self.hash.update(line.as_bytes());
                self.hash.update(b"\n");
            }
            item.last = n;
        }
    }

    fn plan(&mut self, line: &str, n: usize) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ")
            && let Some((glyph, address, action)) = terraform_action(rest)
        {
            self.flush();
            self.pending = Some(Item::new(&format!("{glyph} {address}"), action, n));
            return;
        }
        if trimmed.starts_with("Plan: ") || trimmed.starts_with("No changes.") {
            self.flush();
            self.summary.outcomes.push(short(trimmed, 220));
            return;
        }
        if trimmed.starts_with("Changes to Outputs:")
            || trimmed.starts_with("Note:")
            || trimmed.starts_with("Saved the plan to:")
            || trimmed.starts_with("Error:")
            || trimmed.starts_with("────")
        {
            self.flush();
            return;
        }
        if let Some(item) = &mut self.pending
            && !trimmed.is_empty()
        {
            self.hash.update(trimmed.as_bytes());
            self.hash.update(b"\n");
            item.last = n;
        }
    }

    fn test(&mut self, raw: &str, n: usize) {
        let indented = raw.starts_with([' ', '\t']);
        let line = raw.trim();
        if self.go_tests {
            if line.starts_with("ok  \t") || line.starts_with("ok\t") {
                self.go_active = None;
                self.go_after_fail = None;
                if self.go_ok_first == 0 {
                    self.go_ok_first = n;
                }
                self.go_ok_last = n;
                if line.contains("[no tests to run]") {
                    self.go_no_tests += 1;
                } else {
                    self.go_ok += 1;
                }
                if self.go_ok_samples.len() < 4 {
                    self.go_ok_samples.push(short(line, 220));
                }
                return;
            }
            if line.starts_with("?\t") && line.contains("[no test files]") {
                self.go_active = None;
                self.go_after_fail = None;
                if self.go_ok_first == 0 {
                    self.go_ok_first = n;
                }
                self.go_ok_last = n;
                self.go_no_tests += 1;
                if self.go_ok_samples.len() < 4 {
                    self.go_ok_samples.push(short(line, 220));
                }
                return;
            }
            if line.starts_with("FAIL\t") {
                self.go_active = None;
                self.go_after_fail = None;
                self.summary.outcomes.push(short(line, 220));
                return;
            }
            if let Some(rest) = line
                .strip_prefix("=== RUN")
                .or_else(|| line.strip_prefix("=== CONT"))
                .map(str::trim_start)
                && !rest.is_empty()
            {
                self.go_after_fail = None;
                let name = rest.split_whitespace().next().unwrap_or(rest);
                self.go_active = Some((short(name, 240), Vec::new()));
                return;
            }
            if line.starts_with("=== PAUSE")
                || line.starts_with("--- PASS:")
                || line.starts_with("--- SKIP:")
                || line == "PASS"
                || line == "FAIL"
            {
                self.go_active = None;
                self.go_after_fail = None;
                return;
            }
            if indented && !line.is_empty() && !line.starts_with("--- FAIL:") {
                if let Some((name, left)) = self.go_after_fail.as_mut() {
                    if let Some(item) = self.failures.get_mut(name.as_str()) {
                        let snippet = short(line, 200);
                        item.digest = crate::capture::source_digest(
                            format!("{}{}", item.digest, snippet).as_bytes(),
                        );
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {}", item.detail, snippet), 440)
                        };
                        item.last = n;
                    }
                    *left -= 1;
                    if *left == 0 {
                        self.go_after_fail = None;
                    }
                    return;
                }
                if let Some((_, lines)) = self.go_active.as_mut() {
                    if lines.len() < 3 {
                        lines.push((n, short(line, 200)));
                    }
                    return;
                }
            } else if !line.starts_with("--- FAIL:") {
                self.go_after_fail = None;
            }
        }
        let failed = if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|s| s.strip_suffix(" ... FAILED"))
        {
            Some((name, ""))
        } else if let Some(rest) = line
            .strip_prefix("FAIL [")
            .or_else(|| line.strip_prefix("TIMEOUT ["))
            && let Some((_, tail)) = rest.split_once("] ")
        {
            let name = tail.trim_start();
            // Nextest retry lines prefix the test name with `(1/2) `.
            let name = name
                .strip_prefix('(')
                .and_then(|s| s.split_once(") "))
                .filter(|(tries, _)| {
                    tries.split_once('/').is_some_and(|(a, b)| {
                        !a.is_empty()
                            && !b.is_empty()
                            && a.bytes().all(|c| c.is_ascii_digit())
                            && b.bytes().all(|c| c.is_ascii_digit())
                    })
                })
                .map_or(name, |(_, rest)| rest.trim_start());
            (!name.is_empty()).then_some((name, ""))
        } else if let Some(entry) = line
            .strip_prefix("FAILED ")
            .or_else(|| line.strip_prefix("ERROR "))
        {
            Some(entry.split_once(" - ").unwrap_or((entry, "")))
        } else if let Some(nodeid) = pytest_verbose_failure(line) {
            Some((nodeid, ""))
        } else {
            line.strip_prefix("--- FAIL: ")
                .map(|entry| (entry.split_whitespace().next().unwrap_or(entry), ""))
        };
        if let Some((name, detail)) = failed {
            let item = self
                .failures
                .entry(short(name, 240))
                .or_insert_with(|| Item::new(name, "", n));
            if !detail.is_empty() {
                item.digest = crate::capture::source_digest(detail.as_bytes());
                item.detail = short(detail, 320);
                item.last = n;
            }
        }
        if self.go_tests
            && let Some(entry) = line.strip_prefix("--- FAIL: ")
        {
            let name = short(entry.split_whitespace().next().unwrap_or(entry), 240);
            if let Some((active, lines)) = self.go_active.take()
                && active == name
                && !lines.is_empty()
                && let Some(item) = self.failures.get_mut(&name)
            {
                item.first = lines[0].0;
                item.last = n;
                let joined = lines
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                item.digest = crate::capture::source_digest(joined.as_bytes());
                item.detail = short(&joined, 440);
            } else if self
                .failures
                .get(&name)
                .is_some_and(|item| item.detail.is_empty())
            {
                self.go_after_fail = Some((name, 3));
            }
        }
        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
            .or_else(|| {
                line.strip_prefix("--- STDOUT:")
                    .or_else(|| line.strip_prefix("--- STDERR:"))
                    .and_then(|s| s.strip_suffix(" ---"))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
        {
            self.current_test = short(name, 240);
        }
        // Since Rust 1.73 the panic message follows the location on its own lines,
        // and it is what tells failures at the same location apart.
        let mut message = false;
        if self.panic_lines > 0 {
            if line.is_empty()
                || line.starts_with("note:")
                || line.starts_with("stack backtrace:")
                || line.starts_with("---- ")
                || line.starts_with("--- ")
            {
                self.panic_lines = 0;
            } else if let Some(item) = self.failures.get_mut(&self.current_test) {
                item.digest =
                    crate::capture::source_digest(format!("{}{}", item.digest, line).as_bytes());
                item.detail = short(&format!("{} {}", item.detail, short(line, 200)), 440);
                item.last = n;
                self.panic_lines -= 1;
                message = true;
            }
        }
        if let Some((_, location)) = line.split_once(" panicked at ")
            && let Some(item) = self.failures.get_mut(&self.current_test)
        {
            item.first = n;
            item.last = n;
            item.digest = crate::capture::source_digest(location.as_bytes());
            item.detail = short(location, 200);
            self.panic_lines = if location.ends_with(':') { 3 } else { 0 };
        }
        if !message
            && (line.starts_with("assertion ")
                || line.starts_with("left:")
                || line.starts_with("right:"))
            && let Some(item) = self.failures.get_mut(&self.current_test)
            && n.saturating_sub(item.last) <= 2
        {
            item.digest =
                crate::capture::source_digest(format!("{}{}", item.digest, line).as_bytes());
            item.detail = short(&format!("{} {}", item.detail, line), 440);
            item.last = n;
        }
        if line.starts_with("test result:")
            || line.starts_with("Tests:")
            || line.starts_with("Test Suites:")
            || line.starts_with("ok  \t")
            || (line.starts_with("Summary [")
                && (line.contains(" tests run:") || line.contains(" test run:")))
            || ((line.contains(" passed")
                || line.contains(" failed")
                || line.contains(" error")
                || line.contains(" skipped")
                || line.contains(" xfailed"))
                && line.contains(" in ")
                && line
                    .trim_start_matches(['=', ' '])
                    .starts_with(|c: char| c.is_ascii_digit()))
        {
            self.summary
                .outcomes
                .push(short(line.trim_matches(['=', ' ']), 220));
        }
    }

    pub fn finish(mut self) -> Summary {
        // Commit headers with no patch, as a plain log or `git show -s` prints,
        // keep the ordinary preview.
        if self.discarded || (self.commits && !self.patched) {
            return Summary::default();
        }
        if let Some(inventory) = self.inventory {
            return inventory.finish();
        }
        if let Some(js) = self.js_tests {
            let Some(report) = js.finish() else {
                return Summary::default();
            };
            self.summary.kind = "tests".into();
            self.summary.items = report.summary_items();
            self.summary.outcomes = report.totals.clone();
            self.summary.js = Some(report);
            return self.summary;
        }
        if let Some(rspec) = self.rspec {
            let (items, outcomes) = rspec.finish();
            self.summary.items = items;
            self.summary.outcomes = outcomes;
            return self.summary;
        }
        if let Some(phpunit) = self.phpunit {
            let (items, outcomes) = phpunit.finish();
            self.summary.items = items;
            self.summary.outcomes = outcomes;
            return self.summary;
        }
        // A hunk that a filter such as `grep` thinned out tells nothing about where
        // the output stopped.
        if self.ends_early
            && self.whole_hunks
            && self.hunk.is_some_and(|counts| counts != (0, 0))
            && let Some(item) = &mut self.pending
        {
            item.detail.push_str(" (cut off)");
            if let Some(hunk) = self
                .current_file
                .as_mut()
                .and_then(|file| file.current_hunk.as_mut())
            {
                hunk.header.push_str(" (cut off)");
            }
        }
        self.finish_diff_file();
        self.summary.diff_raw_bytes = self.diff_raw_bytes;
        self.summary.compact_overflowed = self.compact_overflowed;
        self.summary.diff_entries = self.diff_entries;
        self.summary.items.extend(self.failures.into_values());
        let go_passed = self.go_ok + self.go_no_tests;
        if go_passed > 4 {
            let mut note = format!("go test: {} passed package(s)", self.go_ok);
            if self.go_no_tests > 0 {
                note.push_str(&format!(", {} without tests", self.go_no_tests));
            }
            note.push_str(&format!(
                " [stdout:{}-{}]",
                self.go_ok_first, self.go_ok_last
            ));
            self.summary.outcomes.push(note);
        } else {
            self.summary.outcomes.extend(self.go_ok_samples);
        }
        self.summary.cargo = self.cargo.and_then(crate::cargo_tests::Scan::finish);
        self.summary
    }
}

// ---------------------------------------------------------------------------
// RSpec and PHPUnit / ParaTest / Pest structured test parsers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum RspecSection {
    #[default]
    None,
    Failures,
    FailedExamples,
    LoadError,
}

#[derive(Default)]
struct RspecScan {
    section: RspecSection,
    failures: Vec<Item>,
    by_desc: BTreeMap<String, VecDeque<usize>>,
    current: Option<usize>,
    capture_after_error: usize,
    detail_lines: usize,
    outcomes: Vec<String>,
}

impl RspecScan {
    fn observe(&mut self, raw: &str, n: usize) {
        let cleaned = strip_ansi_cow(raw);
        let line = cleaned.as_ref();
        let trimmed = line.trim();

        if trimmed == "Failures:" {
            self.section = RspecSection::Failures;
            self.current = None;
            return;
        }
        if trimmed == "Failed examples:" {
            self.section = RspecSection::FailedExamples;
            self.current = None;
            return;
        }
        if trimmed == "Pending:"
            || trimmed.starts_with("Pending: (Failures listed here")
            || trimmed.starts_with("Finished in ")
        {
            self.section = RspecSection::None;
            self.current = None;
            return;
        }
        if trimmed.starts_with("An error occurred while loading ") {
            let name = trimmed.trim_end_matches('.');
            let idx = self.failures.len();
            self.failures.push(Item::new(name, "", n));
            self.section = RspecSection::LoadError;
            self.current = Some(idx);
            self.detail_lines = 0;
            return;
        }
        if is_rspec_totals(trimmed) {
            self.section = RspecSection::None;
            self.current = None;
            self.outcomes.push(short(trimmed, 220));
            return;
        }

        match self.section {
            RspecSection::None => {}
            RspecSection::Failures => {
                if let Some(desc) = parse_numbered_heading(trimmed) {
                    let idx = self.failures.len();
                    self.failures.push(Item::new(desc, "", n));
                    self.by_desc
                        .entry(desc.to_owned())
                        .or_default()
                        .push_back(idx);
                    self.current = Some(idx);
                    self.capture_after_error = 0;
                    return;
                }
                if let Some(idx) = self.current {
                    let item = &mut self.failures[idx];
                    if trimmed.is_empty() {
                        return;
                    }
                    item.last = n;
                    if trimmed.starts_with("Failure/Error:") {
                        if item.detail.is_empty() {
                            item.detail = short(trimmed, 360);
                        } else {
                            item.detail = short(&format!("{} | {trimmed}", item.detail), 360);
                        }
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.capture_after_error = 3;
                    } else if trimmed.starts_with("# ") || trimmed.starts_with("Diff:") {
                        self.capture_after_error = 0;
                    } else if self.capture_after_error > 0
                        && !trimmed.starts_with("(compared using")
                        && !trimmed.starts_with("Shared Example Group:")
                    {
                        let snippet = short(trimmed, 180);
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {snippet}", item.detail), 360)
                        };
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.capture_after_error -= 1;
                    }
                }
            }
            RspecSection::FailedExamples => {
                if let Some(rest) = trimmed.strip_prefix("rspec ")
                    && let Some((loc, desc)) = rest.split_once(" # ")
                {
                    let loc = loc.trim();
                    let desc = desc.trim();
                    if let Some(queue) = self.by_desc.get_mut(desc)
                        && let Some(idx) = queue.pop_front()
                    {
                        self.failures[idx].name = short(&format!("{loc} {desc}"), 320);
                    } else {
                        self.failures
                            .push(Item::new(&format!("{loc} {desc}"), "", n));
                    }
                }
            }
            RspecSection::LoadError => {
                if let Some(idx) = self.current {
                    let item = &mut self.failures[idx];
                    if trimmed.is_empty() {
                        return;
                    }
                    item.last = n;
                    if trimmed.starts_with("# ") {
                        self.detail_lines = 3;
                    } else if self.detail_lines < 3 {
                        let snippet = short(trimmed, 180);
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {snippet}", item.detail), 360)
                        };
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.detail_lines += 1;
                    }
                }
            }
        }
    }

    fn finish(self) -> (Vec<Item>, Vec<String>) {
        (self.failures, self.outcomes)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum PhpunitSection {
    #[default]
    None,
    Failure,
    PestFail,
}

#[derive(Default)]
struct PhpunitScan {
    section: PhpunitSection,
    current: Option<Item>,
    detail_lines: usize,
    saw_blank_after_detail: bool,
    failures: Vec<Item>,
    outcomes: Vec<String>,
    pending_banner: Option<String>,
}

impl PhpunitScan {
    fn flush(&mut self) {
        if let Some(item) = self.current.take() {
            self.failures.push(item);
        }
        self.detail_lines = 0;
        self.saw_blank_after_detail = false;
    }

    fn observe(&mut self, raw: &str, n: usize) {
        let cleaned = strip_ansi_cow(raw);
        let line = cleaned.as_ref();
        let trimmed = line.trim();

        if is_phpunit_failure_section(trimmed) {
            self.flush();
            self.section = PhpunitSection::Failure;
            return;
        }
        if is_phpunit_other_section(trimmed) {
            self.flush();
            self.section = PhpunitSection::None;
            return;
        }
        if trimmed == "--" || trimmed.starts_with("───") {
            self.flush();
            self.section = PhpunitSection::None;
            return;
        }
        if trimmed == "FAILURES!" || trimmed == "ERRORS!" {
            self.flush();
            self.section = PhpunitSection::None;
            self.pending_banner = Some(trimmed.to_owned());
            return;
        }
        if trimmed.starts_with("OK (") || trimmed == "No tests executed!" {
            self.flush();
            self.section = PhpunitSection::None;
            self.outcomes.push(short(trimmed, 220));
            return;
        }
        if trimmed.starts_with("Tests:") {
            self.flush();
            self.section = PhpunitSection::None;
            let norm = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
            let line = match self.pending_banner.take() {
                Some(banner) => format!("{banner} {norm}"),
                None => norm,
            };
            self.outcomes.push(short(&line, 220));
            return;
        }
        if trimmed.starts_with("Duration:") {
            self.flush();
            self.section = PhpunitSection::None;
            return;
        }
        if let Some(rest) = trimmed.strip_prefix("FAILED  ").or_else(|| {
            trimmed
                .strip_prefix("FAILED ")
                .filter(|r| r.contains(" > "))
        }) {
            let name = rest.trim();
            if !name.is_empty() {
                self.flush();
                self.current = Some(Item::new(name, "", n));
                self.section = PhpunitSection::PestFail;
                return;
            }
        }

        match self.section {
            PhpunitSection::None => {}
            PhpunitSection::Failure => {
                if !line.starts_with([' ', '\t'])
                    && let Some(name) = parse_numbered_heading(trimmed)
                {
                    self.flush();
                    self.current = Some(Item::new(name, "", n));
                    return;
                }
                if let Some(item) = self.current.as_mut() {
                    if trimmed.is_empty() {
                        if self.detail_lines > 0 {
                            self.saw_blank_after_detail = true;
                        }
                        return;
                    }
                    item.last = n;
                    if !self.saw_blank_after_detail && self.detail_lines < 3 {
                        let snippet = short(trimmed, 180);
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {snippet}", item.detail), 360)
                        };
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.detail_lines += 1;
                    }
                }
            }
            PhpunitSection::PestFail => {
                if let Some(item) = self.current.as_mut() {
                    if trimmed.is_empty() {
                        if self.detail_lines > 0 {
                            self.saw_blank_after_detail = true;
                        }
                        return;
                    }
                    item.last = n;
                    if !self.saw_blank_after_detail
                        && self.detail_lines < 3
                        && !trimmed.starts_with("at ")
                        && !trimmed.contains('▕')
                    {
                        let snippet = short(trimmed, 180);
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {snippet}", item.detail), 360)
                        };
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.detail_lines += 1;
                    }
                }
            }
        }
    }

    fn finish(mut self) -> (Vec<Item>, Vec<String>) {
        self.flush();
        if let Some(banner) = self.pending_banner.take() {
            self.outcomes.push(banner);
        }
        (self.failures, self.outcomes)
    }
}

fn strip_ansi_cow(line: &str) -> Cow<'_, str> {
    if line.contains('\x1b') {
        crate::capture::ANSI.replace_all(line, "")
    } else {
        Cow::Borrowed(line)
    }
}

/// Parse a top-level numbered failure heading such as `1) Foo#bar` (requiring
/// pure digits before `)` so RSpec aggregate sub-items like `1.1)` stay attached
/// to their parent failure).
fn parse_numbered_heading(trimmed: &str) -> Option<&str> {
    let (num, rest) = trimmed.split_once(") ")?;
    if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
        let desc = rest.trim();
        (!desc.is_empty()).then_some(desc)
    } else {
        None
    }
}

fn is_rspec_totals(trimmed: &str) -> bool {
    static RSPEC_TOTALS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\d+ examples?, \d+ failures?(?:, \d+ pending)?(?:, \d+ errors? occurred outside of examples)?$")
            .unwrap()
    });
    RSPEC_TOTALS.is_match(trimmed)
}

fn is_phpunit_failure_section(trimmed: &str) -> bool {
    static PHPUNIT_FAIL_SEC: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^There (?:was 1|were \d+) (?:error|errors|failure|failures):$").unwrap()
    });
    PHPUNIT_FAIL_SEC.is_match(trimmed)
}

fn is_phpunit_other_section(trimmed: &str) -> bool {
    trimmed.starts_with("There was ") || trimmed.starts_with("There were ")
}

/// Parse a Terraform/OpenTofu resource plan header such as
/// `aws_instance.web[0] will be created` or `module.net.aws_subnet.a must be replaced`.
fn terraform_action(rest: &str) -> Option<(&'static str, &str, &str)> {
    const ACTIONS: &[(&str, &str)] = &[
        (" must be replaced", "-/+"),
        (" is tainted, so it must be replaced", "-/+"),
        (" will be created", "+"),
        (" will be updated in-place", "~"),
        (" will be destroyed", "-"),
        (" will be read during apply", "<="),
        (" has moved to ", "->"),
        (" will be forgotten", "x"),
        (" will no longer be managed", "x"),
    ];
    for &(phrase, glyph) in ACTIONS {
        if let Some(pos) = rest.find(phrase) {
            let address = rest[..pos].trim();
            let action = rest[pos..].trim();
            if !address.is_empty() && !address.contains(' ') {
                return Some((glyph, address, action));
            }
        }
    }
    None
}

/// Verbose pytest failure line (`tests/test_a.py::test_x FAILED [ 50%]`) when
/// `--no-summary` / `-rN` omits the short test summary block.
fn pytest_verbose_failure(line: &str) -> Option<&str> {
    let rest = line.strip_suffix(" FAILED").or_else(|| {
        let (before, pct) = line.rsplit_once(" [")?;
        pct.ends_with("%]")
            .then(|| before.strip_suffix(" FAILED"))?
    })?;
    let nodeid = rest.trim();
    (nodeid.contains("::") && !nodeid.contains(' ')).then_some(nodeid)
}

/// The detail of a commit header whose message is empty.
const NO_MESSAGE: &str = "(no message)";

/// The old and new line counts a `@@ -a,b +c,d @@` hunk header announces. An
/// omitted count is 1.
fn hunk_counts(line: &str) -> Option<(usize, usize)> {
    let ranges = line.strip_prefix("@@ -")?.split_once(" @@")?.0;
    let (old, new) = ranges.split_once(" +")?;
    let count = |range: &str| match range.split_once(',') {
        Some((_, count)) => count.parse().ok(),
        None => range.parse::<usize>().ok().map(|_| 1),
    };
    Some((count(old)?, count(new)?))
}

/// Parent count announced by a combined diff hunk header (`@@@ ... @@@` -> 2,
/// `@@@@ ... @@@@` -> 3, etc.).
fn combined_hunk_parents(line: &str) -> Option<usize> {
    let at = line.bytes().take_while(|&b| b == b'@').count();
    if at < 3 {
        return None;
    }
    let rest = line.get(at..)?;
    if !rest.starts_with(" -") {
        return None;
    }
    let closing = format!(" {}", "@".repeat(at));
    rest.contains(&closing).then_some(at - 1)
}

/// Whether `line` is a standard (`@@ ... @@`) or combined (`@@@ ... @@@`) hunk header.
fn is_hunk_header(line: &str) -> bool {
    line.starts_with("@@ ") || combined_hunk_parents(line).is_some()
}

/// The path named by a `diff --git a/... b/...`, `diff --cc ...` or
/// `diff --combined ...` header line.
fn diff_header_path(line: &str) -> Option<&str> {
    if let Some(names) = line.strip_prefix("diff --git ") {
        Some(names.rsplit_once(" b/").map_or(names, |(_, new)| new))
    } else if let Some(path) = line
        .strip_prefix("diff --cc ")
        .or_else(|| line.strip_prefix("diff --combined "))
    {
        let path = path.trim();
        (!path.is_empty()).then_some(path)
    } else {
        None
    }
}

/// A per-file line of Git's `--stat` block, such as ` src/lib.rs | 12 ++++----`
/// or ` assets/logo.png | Bin 0 -> 1234 bytes`.
fn is_stat_line(line: &str) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let Some(rest) = line.strip_prefix(' ') else {
        return false;
    };
    let Some((path, stats)) = rest.rsplit_once(" | ") else {
        return false;
    };
    if path.trim().is_empty() {
        return false;
    }
    let stats = stats.trim();
    if stats == "Unmerged" || (stats.starts_with("Bin ") && stats.ends_with(" bytes")) {
        return true;
    }
    let mut parts = stats.split_whitespace();
    let Some(count) = parts.next() else {
        return false;
    };
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(graph) => parts.next().is_none() && graph.bytes().all(|b| matches!(b, b'+' | b'-')),
    }
}

/// The trailing summary line of Git's `--stat` block, such as
/// ` 2 files changed, 8 insertions(+), 3 deletions(-)`.
fn is_stat_summary(line: &str) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let Some(rest) = line.strip_prefix(' ') else {
        return false;
    };
    let trimmed = rest.trim();
    let Some((num, tail)) = trimmed.split_once(' ') else {
        return false;
    };
    !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
        && (tail.starts_with("file changed") || tail.starts_with("files changed"))
}

/// Mode/rename summary lines emitted by `git --stat --summary`.
fn is_stat_mode_line(line: &str) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let Some(rest) = line.strip_prefix(' ') else {
        return false;
    };
    [
        "create mode ",
        "delete mode ",
        "mode change ",
        "rename ",
        "copy ",
        "rewrite ",
    ]
    .iter()
    .any(|prefix| rest.starts_with(prefix))
}

/// Whether a file-header line carries semantic metadata (mode/rename/copy/binary)
/// that must be preserved verbatim in the compact diff tier.
fn is_preserved_meta_header(line: &str, saw_mode_header: bool) -> bool {
    if [
        "old mode ",
        "new mode ",
        "deleted file mode ",
        "new file mode ",
        "mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "Binary files ",
        "GIT binary patch",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
    {
        return true;
    }
    !saw_mode_header && (line == "--- /dev/null" || line == "+++ /dev/null")
}

/// Whether a hunk body line is an addition or deletion (handling both standard
/// 2-way hunks and `N`-parent combined merge hunks).
fn hunk_line_delta(line: &str, combined_parents: usize) -> (bool, bool) {
    if combined_parents >= 2 {
        let prefix = line.as_bytes().get(..combined_parents).unwrap_or(&[]);
        if prefix.contains(&b'+') {
            (true, false)
        } else if prefix.contains(&b'-') {
            (false, true)
        } else {
            (false, false)
        }
    } else {
        match line.as_bytes().first() {
            Some(b'+') => (true, false),
            Some(b'-') => (false, true),
            _ => (false, false),
        }
    }
}

/// Keep 100% of `+`/`-` change lines and trim surrounding context from 3 lines down
/// to 1 line immediately adjacent to any change.
fn trim_hunk_context(lines: &[String], combined_parents: usize) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    let changed: Vec<bool> = lines
        .iter()
        .map(|line| {
            let (add, del) = hunk_line_delta(line, combined_parents);
            add || del
        })
        .collect();
    if !changed.iter().any(|&c| c) {
        return lines.to_vec();
    }
    let len = lines.len();
    let mut keep = vec![false; len];
    for i in 0..len {
        if changed[i]
            || (i > 0 && changed[i - 1])
            || (i + 1 < len && changed[i + 1])
            || (lines[i].starts_with('\\') && i > 0 && (changed[i - 1] || keep[i - 1]))
        {
            keep[i] = true;
        }
    }
    let mut out = Vec::with_capacity(len);
    let mut emitted_any = false;
    let mut skipped_since_last = false;
    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            if emitted_any && skipped_since_last {
                out.push(" ...".to_owned());
            }
            out.push(line.clone());
            emitted_any = true;
            skipped_since_last = false;
        } else if emitted_any {
            skipped_since_last = true;
        }
    }
    out
}

/// The commit id on a `commit SHA` or `commit SHA (decorations)` line.
fn commit_id(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("commit ")?;
    let (id, decorations) = rest.split_once(' ').unwrap_or((rest, ""));
    (id.len() >= 7
        && id.bytes().all(|b| b.is_ascii_hexdigit())
        && (decorations.is_empty() || decorations.starts_with('(')))
    .then_some(id)
}

impl Item {
    /// Whether this is a commit header from `git show` or `git log -p`.
    pub(crate) fn is_commit(&self) -> bool {
        self.name
            .strip_prefix("commit ")
            .is_some_and(|id| id.len() >= 7 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    }

    pub(crate) fn new(name: &str, detail: &str, n: usize) -> Self {
        Self::new_in_stream(name, detail, n, crate::capture::Stream::Stdout)
    }

    pub(crate) fn new_in_stream(
        name: &str,
        detail: &str,
        n: usize,
        stream: crate::capture::Stream,
    ) -> Self {
        Self {
            name: short(name, 320),
            detail: short(detail, 360),
            digest: String::new(),
            first: n,
            last: n,
            stderr: matches!(stream, crate::capture::Stream::Stderr),
            added: 0,
            deleted: 0,
            file_first: 0,
        }
    }

    pub(crate) fn stream_name(&self) -> &'static str {
        if self.stderr { "stderr" } else { "stdout" }
    }
    /// The identity used to compare runs. A patch hunk drops its line ranges,
    /// which shift whenever an earlier hunk grows or shrinks.
    fn key(&self) -> (&str, &str, &str) {
        let detail = if is_hunk_header(&self.detail) {
            let at = self.detail.bytes().take_while(|&b| b == b'@').count();
            let marker = format!(" {}", "@".repeat(at));
            let after_open = &self.detail[at + 1..];
            after_open
                .split_once(&marker)
                .map_or(after_open, |(_, context)| context)
        } else {
            &self.detail
        };
        (&self.name, detail, &self.digest)
    }
    fn counts(items: &[Item]) -> BTreeMap<(&str, &str, &str), usize> {
        let mut counts = BTreeMap::new();
        for item in items {
            *counts.entry(item.key()).or_default() += 1;
        }
        counts
    }

    pub(crate) fn display(&self) -> String {
        format!(
            "{} {} [{}:{}-{}]",
            self.name,
            self.detail,
            self.stream_name(),
            self.first,
            self.last
        )
    }
}

pub fn short(s: &str, len: usize) -> String {
    s[..s.floor_char_boundary(s.len().min(len))].to_owned()
}

impl Summary {
    /// A test outcome line that reports failures, such as `test result: FAILED.` or
    /// `1 failed, 2 passed in 0.10s`. Output cut by `| tail` can keep this line and
    /// lose the names of the failing tests.
    fn failing_outcome(&self) -> Option<&str> {
        static FAILED: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"\bFAILED\b|\bFAIL\b|\b(?:FAILURES|ERRORS)!|\b(?:Failures|Errors):\s*[1-9][0-9]*\b|(?:^|[^0-9])[1-9][0-9]* (?:failed|failing|failures?|errors?|timed out)\b",
            )
            .expect("failing outcome pattern")
        });
        (self.kind == "tests")
            .then(|| self.outcomes.iter().find(|line| FAILED.is_match(line)))
            .flatten()
            .map(String::as_str)
    }

    /// Whether this and `prior` are patches of different commits, as `git show HEAD`
    /// prints before and after a commit. Compared, every item of both would show.
    pub fn other_commits(&self, prior: &Summary) -> bool {
        let ids = |summary: &Summary| -> std::collections::BTreeSet<String> {
            summary
                .items
                .iter()
                .filter(|item| item.is_commit())
                .map(|item| item.name.clone())
                .collect()
        };
        let (now, then) = (ids(self), ids(prior));
        !now.is_empty() && !then.is_empty() && now.is_disjoint(&then)
    }

    /// The first line of a patch summary, such as `1 commit(s), 3 diff section(s)`.
    pub(crate) fn patch_heading(&self) -> String {
        let commits = self.items.iter().filter(|item| item.is_commit()).count();
        let sections = format!("{} diff section(s)", self.items.len() - commits);
        if commits == 0 {
            sections
        } else {
            format!("{commits} commit(s), {sections}")
        }
    }

    /// A one-line outcome for handoffs, such as a failing test count or top lint rule.
    pub fn headline(&self) -> Option<String> {
        match self.kind.as_str() {
            "tests" if !self.items.is_empty() => Some(format!(
                "{} failing test(s), first {}",
                self.items.len(),
                self.items[0].name
            )),
            "tests" => self
                .cargo
                .as_ref()
                .filter(|totals| !totals.nextest_failures.is_empty())
                .map(|totals| {
                    let first = totals.nextest_failures[0]
                        .lines()
                        .next()
                        .unwrap_or(&totals.nextest_failures[0]);
                    let first = first.rsplit_once(" [").map_or(first, |(head, _)| head);
                    format!(
                        "{} failing test(s), first {first}",
                        totals.nextest_failures.len()
                    )
                })
                .or_else(|| self.failing_outcome().map(str::to_owned)),
            "lint" => {
                let total: usize = self
                    .items
                    .iter()
                    .filter_map(|i| i.digest.parse::<usize>().ok())
                    .sum();
                let top = self.items.first()?;
                Some(format!(
                    "{total} finding(s) in {} rule(s), first {} ×{}",
                    self.items.len(),
                    top.name,
                    top.digest
                ))
            }
            "diff" if !self.items.is_empty() => Some(self.patch_heading()),
            "plan" if !self.items.is_empty() => self
                .outcomes
                .last()
                .cloned()
                .or_else(|| Some(format!("{} resource change(s)", self.items.len()))),
            "inventory" | "search" | "listing" => self.outcomes.first().cloned(),
            _ => None,
        }
    }

    pub fn render(&self, previous: Option<(&str, &Summary)>, status: i32) -> Option<String> {
        if self.kind.is_empty()
            || (self.items.is_empty()
                && self.outcomes.is_empty()
                && self.cargo.is_none()
                && self.js.is_none()
                && !(self.kind == "diff" && previous.is_some_and(|(_, s)| !s.items.is_empty())))
        {
            return None;
        }
        let mut report = String::new();
        if self.kind == "inventory" {
            if status != 0 {
                return None;
            }
            report.push_str(self.outcomes.first()?);
            report.push('\n');
        }
        if self.kind == "plan" && status != 0 && self.items.is_empty() {
            return None;
        }
        if self.kind == "tests" {
            if let Some(js) = self.js.as_ref() {
                let has_diff = previous.is_some_and(|(_, old)| {
                    old.kind == self.kind && (!self.items.is_empty() || !old.items.is_empty())
                });
                if !has_diff {
                    if status != 0 && self.items.is_empty() {
                        return None;
                    }
                    return Some(js.render());
                }
            }
            if let Some(totals) = self
                .cargo
                .as_ref()
                .filter(|totals| totals.nextest_summary.is_some())
            {
                let has_stdout_diff = previous.is_some_and(|(_, old)| {
                    old.kind == self.kind && (!self.items.is_empty() || !old.items.is_empty())
                });
                if !has_stdout_diff {
                    if status != 0 && totals.nextest_failures.is_empty() && self.items.is_empty() {
                        return None;
                    }
                    return Some(totals.render());
                }
            }
            if status != 0 && self.items.is_empty() {
                return None;
            }
            // With no failing test, cargo's totals line comes first.
            if !self.items.is_empty() || (self.cargo.is_none() && self.js.is_none()) {
                if self.items.is_empty() && self.failing_outcome().is_some() {
                    report.push_str("failing test names not in the captured output\n");
                } else {
                    report.push_str(&format!("{} reported failing test(s)\n", self.items.len()));
                }
            }
        }
        let previous = previous.filter(|(_, s)| s.kind == self.kind);
        let mut remaining = Vec::new();
        if let Some((id, old)) = previous {
            // Compare as multisets so identical hunks are matched one for one.
            let known = Item::counts(&old.items);
            let current = Item::counts(&self.items);
            report.push_str(&format!("Compared with {id}:\n"));
            let mut unmatched = known.clone();
            for item in &self.items {
                match unmatched.get_mut(&item.key()) {
                    Some(left) if *left > 0 => {
                        *left -= 1;
                        remaining.push(item);
                    }
                    _ => report.push_str(&format!("+ {}\n", item.display())),
                }
            }
            let mut unmatched = current.clone();
            for item in &old.items {
                match unmatched.get_mut(&item.key()) {
                    Some(left) if *left > 0 => *left -= 1,
                    _ => report.push_str(&format!(
                        "- {} {} [prior {}:{}-{}]\n",
                        item.name,
                        item.detail,
                        item.stream_name(),
                        item.first,
                        item.last
                    )),
                }
            }
            if known == current {
                report.push_str(if self.kind == "inventory" {
                    "Attention list unchanged.\n"
                } else {
                    "Indexed items unchanged; raw output may differ.\n"
                });
            }
        } else if self.kind == "tests" {
            let mut groups: BTreeMap<(&str, &str), Vec<&Item>> = BTreeMap::new();
            for item in &self.items {
                groups
                    .entry((&item.detail, &item.digest))
                    .or_default()
                    .push(item);
            }
            for ((detail, _), items) in groups {
                if detail.is_empty() {
                    for item in items {
                        report.push_str(&format!("{}\n", item.display()));
                    }
                } else {
                    report.push_str(&format!(
                        "{} test(s): {}\n",
                        items.len(),
                        items[0].display()
                    ));
                    for item in &items[1..] {
                        report.push_str(&format!(
                            "  {} [{}:{}]\n",
                            item.name,
                            item.stream_name(),
                            item.first
                        ));
                    }
                }
            }
        } else if self.kind == "diff" {
            return Some(self.render_diff_with(COMPACT_DIFF_BUDGET, "", |_, _, _, _| None));
        } else {
            if self.kind == "plan" && !self.items.is_empty() {
                report.push_str(&format!("{} resource change(s)\n", self.items.len()));
            }
            for item in &self.items {
                report.push_str(&format!("{}\n", item.display()));
            }
        }
        if let Some(js) = self.js.as_ref().filter(|_| self.kind == "tests") {
            report.push_str(&js.render_after_diff(&remaining));
            return Some(report);
        }
        if let Some(totals) = self.cargo.as_ref().filter(|_| self.kind == "tests") {
            if totals.nextest_summary.is_some() {
                report.push_str(&totals.render_nextest_summary());
            } else {
                report.push_str(&totals.render());
            }
            return Some(report);
        }
        for line in self
            .outcomes
            .iter()
            .skip(usize::from(self.kind == "inventory"))
        {
            report.push_str(line);
            report.push('\n');
        }
        Some(report)
    }

    /// Render a diff/patch summary in two tiers:
    /// 1. **Compact diff** (`<= budget` and strictly smaller than raw): replaces
    ///    redundant 4-line git headers with `=== path (+A -D) [stdout:first-last]`,
    ///    preserves mode/rename/binary lines verbatim, folds large generated files,
    ///    and keeps 100% of `+`/`-` lines with 1 line of context.
    /// 2. **File-grouped hunk index**: falls back when even the compact diff exceeds
    ///    `budget`, grouping multi-hunk files with `(+A -D, N hunks)` stats and
    ///    folding large generated files to 1 line.
    pub(crate) fn render_diff_with<F>(&self, budget: usize, flags: &str, replace: F) -> String
    where
        F: Fn(&str, usize, usize, Option<usize>) -> Option<String>,
    {
        let heading = format!("{}\n", self.patch_heading());
        if !self.diff_entries.is_empty()
            && !self.compact_overflowed
            && let Some(compact) = self.try_render_compact(&heading, flags, budget, &replace)
        {
            return compact;
        }
        self.render_hunk_index(&heading, flags, &replace)
    }

    fn try_render_compact<F>(
        &self,
        heading: &str,
        flags: &str,
        budget: usize,
        replace: &F,
    ) -> Option<String>
    where
        F: Fn(&str, usize, usize, Option<usize>) -> Option<String>,
    {
        let mut out = String::new();
        out.push_str(heading);
        out.push_str(flags);
        for entry in &self.diff_entries {
            match entry {
                DiffEntry::Commit { item_index } => {
                    if let Some(item) = self.items.get(*item_index) {
                        out.push_str(&item.display());
                        out.push('\n');
                    }
                }
                DiffEntry::File(file) => {
                    let default = file.render_compact();
                    let chosen = match replace(
                        &file.path,
                        file.first,
                        file.last,
                        default.as_ref().map(String::len),
                    ) {
                        Some(replaced) => replaced,
                        None => default?,
                    };
                    out.push_str(&chosen);
                }
            }
            if out.len() > budget {
                return None;
            }
        }
        for line in &self.outcomes {
            out.push_str(line);
            out.push('\n');
        }
        let smaller = self.diff_raw_bytes == 0 || out.len() < self.diff_raw_bytes;
        (out.len() <= budget && smaller).then_some(out)
    }

    fn render_hunk_index<F>(&self, heading: &str, flags: &str, replace: &F) -> String
    where
        F: Fn(&str, usize, usize, Option<usize>) -> Option<String>,
    {
        let mut out = String::new();
        out.push_str(heading);
        out.push_str(flags);
        if self.diff_entries.is_empty() {
            let mut i = 0;
            while i < self.items.len() {
                let item = &self.items[i];
                if item.is_commit() {
                    out.push_str(&item.display());
                    out.push('\n');
                    i += 1;
                    continue;
                }
                let mut end = i + 1;
                while end < self.items.len()
                    && !self.items[end].is_commit()
                    && self.items[end].name == item.name
                {
                    end += 1;
                }
                let group = &self.items[i..end];
                let first = group[0].first;
                let last = group.last().map_or(first, |it| it.last);
                let default_len: usize = group.iter().map(|it| it.display().len() + 1).sum();
                if let Some(replaced) = replace(&item.name, first, last, Some(default_len)) {
                    out.push_str(&replaced);
                } else {
                    for it in group {
                        out.push_str(&it.display());
                        out.push('\n');
                    }
                }
                i = end;
            }
        } else {
            for entry in &self.diff_entries {
                match entry {
                    DiffEntry::Commit { item_index } => {
                        if let Some(item) = self.items.get(*item_index) {
                            out.push_str(&item.display());
                            out.push('\n');
                        }
                    }
                    DiffEntry::File(file) => {
                        let default = file.render_hunk_group(&self.items);
                        if let Some(replaced) =
                            replace(&file.path, file.first, file.last, Some(default.len()))
                        {
                            out.push_str(&replaced);
                        } else {
                            out.push_str(&default);
                        }
                    }
                }
            }
        }
        for line in &self.outcomes {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

impl DiffFile {
    fn render_folded(&self) -> String {
        format!(
            "=== {} (generated, +{} -{}) [stdout:{}-{}]\n",
            self.path, self.added, self.deleted, self.first, self.last
        )
    }

    fn render_compact(&self) -> Option<String> {
        if self.generated_folded {
            return Some(self.render_folded());
        }
        let hunks = self.hunks.as_ref()?;
        let mut out = String::new();
        if self.added > 0 || self.deleted > 0 {
            out.push_str(&format!(
                "=== {} (+{} -{}) [stdout:{}-{}]\n",
                self.path, self.added, self.deleted, self.first, self.last
            ));
        } else {
            out.push_str(&format!(
                "=== {} [stdout:{}-{}]\n",
                self.path, self.first, self.last
            ));
        }
        for meta in &self.meta_lines {
            out.push_str(meta);
            out.push('\n');
        }
        for hunk in hunks {
            out.push_str(&hunk.header);
            out.push('\n');
            for line in &hunk.lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        Some(out)
    }

    fn render_hunk_group(&self, items: &[Item]) -> String {
        if self.generated_folded {
            return self.render_folded();
        }
        let slice = items.get(self.items_start..self.items_end).unwrap_or(&[]);
        if slice.is_empty() {
            return String::new();
        }
        if slice.len() == 1 {
            return format!("{}\n", slice[0].display());
        }
        let hunk_count = slice.iter().filter(|i| is_hunk_header(&i.detail)).count();
        let mut out = String::new();
        if hunk_count > 0 {
            let noun = if hunk_count == 1 { "hunk" } else { "hunks" };
            out.push_str(&format!(
                "{} (+{} -{}, {hunk_count} {noun}) [stdout:{}-{}]\n",
                self.path, self.added, self.deleted, self.first, self.last
            ));
        } else {
            out.push_str(&format!(
                "{} [stdout:{}-{}]\n",
                self.path, self.first, self.last
            ));
        }
        for item in slice {
            if is_hunk_header(&item.detail) {
                out.push_str(&format!(
                    "  {} (+{} -{}) [{}:{}-{}]\n",
                    item.detail,
                    item.added,
                    item.deleted,
                    item.stream_name(),
                    item.first,
                    item.last
                ));
            } else {
                out.push_str(&format!(
                    "  {} [{}:{}-{}]\n",
                    item.detail,
                    item.stream_name(),
                    item.first,
                    item.last
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `git show 0940ff5 4e5ef6a` output from this repository.
    const SHOW: &str = include_str!("../fixtures/patches/git_show.txt");

    fn parse(family: &str, text: &str) -> Summary {
        let mut parser = Parser::new(family, None);
        for (index, line) in text.split_inclusive('\n').enumerate() {
            parser.observe(line, index + 1);
        }
        parser.finish()
    }

    #[test]
    fn commit_patches_list_each_header_then_its_hunks() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        for (line, expected) in [
            ("git show HEAD~3", "git show"),
            ("git log -p -2", "git log"),
            ("git log -U1", "git log"),
            ("git log --oneline -5", "git"),
            ("git log -- -p", "git"),
            ("gh pr diff 42", "git diff"),
            ("glab mr diff 17", "git diff"),
        ] {
            assert_eq!(family(&args(line)), expected, "{line}");
        }
        let summary = parse("git show", SHOW);
        // `SHOW` (~1.6 KB) fits in Tier 1 (Compact Diff): preserves 100% of `+`/`-`
        // lines, mode/binary headers, and trims context to 1 line.
        let compact = summary.render(None, 0).unwrap();
        assert!(
            compact.len() < SHOW.len(),
            "{} vs {}",
            compact.len(),
            SHOW.len()
        );
        assert!(
            compact.starts_with(
                "2 commit(s), 4 diff section(s)\n\
                 commit 0940ff5cd49d build(deps): depend on libc on Unix for signal forwarding [stdout:1-7]\n\
                 === Cargo.lock (+1 -0) [stdout:9-20]\n"
            ),
            "{compact}"
        );
        assert!(compact.contains("+ \"libc\",\n"), "{compact}");
        assert!(compact.contains("+libc = \"0.2\"\n"), "{compact}");
        assert!(compact.contains("-# Justless\n"), "{compact}");
        assert!(
            compact.contains(
                "=== assets/logo.png [stdout:62-65]\n\
                 new file mode 100644\n\
                 Binary files /dev/null and b/assets/logo.png differ\n"
            ),
            "{compact}"
        );
        // Context trimmed from 3 lines down to 1 adjacent line: `"anyhow"` and `"clap"` are cut, `"ignore"` and `"proc-macro2"` remain.
        assert!(!compact.contains("\"anyhow\""), "{compact}");
        assert!(
            compact.contains("  \"ignore\",\n+ \"libc\",\n  \"proc-macro2\","),
            "{compact}"
        );
        // Tier 2 (Hunk Index) when compact diff does not fit the budget:
        let index = summary.render_diff_with(10, "", |_, _, _, _| None);
        assert_eq!(
            index,
            "2 commit(s), 4 diff section(s)\n\
             commit 0940ff5cd49d build(deps): depend on libc on Unix for signal forwarding [stdout:1-7]\n\
             Cargo.lock @@ -274,6 +274,7 @@ dependencies = [ [stdout:13-20]\n\
             Cargo.toml @@ -41,3 +41,6 @@ tree-sitter-zig = \"1.1\" [stdout:25-31]\n\
             commit 4e5ef6a7fe67 docs(readme): add the logo and tagline [stdout:33-37]\n\
             README.md @@ -1,4 +1,17 @@ [stdout:43-61]\n\
             assets/logo.png new file mode 100644 [stdout:62-65]\n"
        );
        assert_eq!(
            summary.headline().as_deref(),
            Some("2 commit(s), 4 diff section(s)")
        );
        // A rerun with a changed message reports that header only.
        let reworded = parse("git log", &SHOW.replace("add the logo", "add a logo"));
        let delta = reworded.render(Some(("run-1", &summary)), 0).unwrap();
        assert_eq!(
            delta,
            "Compared with run-1:\n\
             + commit 4e5ef6a7fe67 docs(readme): add a logo and tagline [stdout:33-37]\n\
             - commit 4e5ef6a7fe67 docs(readme): add the logo and tagline [prior stdout:33-37]\n"
        );
        // A rerun that shows only other commits is not compared with them.
        let second = &SHOW[SHOW.find("commit 4e5").unwrap()..];
        let other = second.replacen("4e5ef6a7fe67", "fedcba987654", 1);
        assert!(!parse("git show", second).other_commits(&summary));
        assert!(parse("git show", &other).other_commits(&summary));
        assert!(!summary.other_commits(&Summary::default()));
    }

    #[test]
    fn commit_output_without_a_plain_patch_keeps_the_preview() {
        let header = SHOW.split("diff --git").next().unwrap();
        for text in [
            // `--raw`, `--oneline`, a blank inside a hunk, `git show -s`, and `git show REV:path`.
            SHOW.replacen(
                "\ndiff --git",
                "\n:100644 100644 1111111 2222222 M\tCargo.lock\n\ndiff --git",
                1,
            ),
            SHOW.replacen(
                "commit 0940ff5cd49d5b8fc541e004d5dd8e69e793adad",
                "0940ff5 build(deps)",
                1,
            ),
            SHOW.replacen("  \"clap\",\n", "\n", 1),
            header.to_owned(),
            "[package]\nname = \"justless\"\n".to_owned(),
            // `--stat` alone without a patch still keeps the verbatim preview.
            format!("{header} Cargo.lock | 1 +\n 1 file changed, 1 insertion(+)\n"),
        ] {
            assert!(parse("git show", &text).items.is_empty(), "{text}");
        }
        // `git diff` output with non-stat prose before `diff --git` is discarded.
        assert!(parse("git diff", SHOW).items.is_empty());
        // `git diff --stat` alone (no patch) produces no items so raw stat stays verbatim.
        assert!(
            parse(
                "git diff",
                " src/lib.rs | 4 ++--\n 1 file changed, 2 insertions(+), 2 deletions(-)\n"
            )
            .items
            .is_empty()
        );
    }

    #[test]
    fn stat_blocks_merge_commits_and_tiered_rendering_preserve_all_changes() {
        // 1. `--stat` alongside `-p` on both `git show` and `git diff` is parsed cleanly.
        let with_stat = SHOW.replacen(
            "\ndiff --git",
            "\n Cargo.lock | 1 +\n Cargo.toml | 3 +++\n 2 files changed, 4 insertions(+)\n\ndiff --git",
            1,
        );
        let parsed_stat = parse("git show", &with_stat);
        assert_eq!(parsed_stat.items.len(), 6);
        assert!(
            parsed_stat
                .render(None, 0)
                .unwrap()
                .contains("+libc = \"0.2\""),
            "{:?}",
            parsed_stat.render(None, 0)
        );
        let diff_with_stat = concat!(
            " src/lib.rs | 2 +-\n",
            " 1 file changed, 1 insertion(+), 1 deletion(-)\n",
            "\n",
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1,5 +1,5 @@ fn f() {\n",
            " a\n",
            " b\n",
            "-c\n",
            "+d\n",
            " e\n",
        );
        let parsed_diff_stat = parse("git diff", diff_with_stat);
        let rendered_diff_stat = parsed_diff_stat.render(None, 0).unwrap();
        assert!(
            rendered_diff_stat.contains("=== src/lib.rs (+1 -1) [stdout:4-13]"),
            "{rendered_diff_stat}"
        );
        assert!(
            rendered_diff_stat.contains("-c\n+d\n"),
            "{rendered_diff_stat}"
        );

        // 2. Merge commit (`diff --cc` with `@@@` combined hunk and `mode` header).
        let merge = "\
commit 1234567890abcdef1234567890abcdef12345678
Merge: 1111111 2222222
Author: Dev <dev@example.com>
Date:   Mon Jan 1 00:00:00 2026 +0000

    Merge branch 'feature'

diff --cc src/lib.rs
index 1111111,2222222..3333333
mode 100644,100644..100755
--- a/src/lib.rs
+++ b/src/lib.rs
@@@ -1,5 -1,5 +1,7 @@@ fn main() {
  line1
  line2
  line3
 +from_b
+ from_a
++merged
 -old_b
  line4
  line5
  line6
";
        let merged = parse("git show", merge);
        assert_eq!(merged.items.len(), 2);
        assert_eq!((merged.items[1].added, merged.items[1].deleted), (3, 1));
        let merged_rendered = merged.render(None, 0).unwrap();
        assert!(
            merged_rendered
                .contains("=== src/lib.rs (+3 -1) [stdout:8-23]\nmode 100644,100644..100755\n"),
            "{merged_rendered}"
        );
        assert!(
            merged_rendered.contains(" +from_b\n+ from_a\n++merged\n -old_b\n"),
            "{merged_rendered}"
        );
        assert!(!merged_rendered.contains("  line1\n"), "{merged_rendered}");
        assert!(merged_rendered.contains("  line3\n"), "{merged_rendered}");

        // 3. Large multi-hunk diff exceeds compact tier and falls back to file-grouped hunk index with (+A -D).
        let mut big = String::new();
        big.push_str("diff --git a/src/foo.rs b/src/foo.rs\nindex 1111111..2222222 100644\n--- a/src/foo.rs\n+++ b/src/foo.rs\n");
        for h in 0..3 {
            let line = 10 + h * 100;
            big.push_str(&format!("@@ -{line},4 +{line},60 @@ fn f{h}() {{\n ctx\n"));
            big.push_str(&format!("-old_{h}\n"));
            for k in 0..57 {
                big.push_str(&format!(
                    "+    let value_{h}_{k} = compute_something_long({h}, {k});\n"
                ));
            }
            big.push_str(" end\n");
        }
        let big_summary = parse("git diff", &big);
        let big_rendered = big_summary.render(None, 0).unwrap();
        assert!(
            big_rendered.starts_with("3 diff section(s)\n"),
            "{big_rendered}"
        );
        assert!(
            big_rendered.contains("src/foo.rs (+171 -3, 3 hunks) [stdout:1-"),
            "{big_rendered}"
        );
        assert!(
            big_rendered.contains("  @@ -10,4 +10,60 @@ fn f0() { (+57 -1) [stdout:"),
            "{big_rendered}"
        );
        assert!(
            big_rendered.contains("  @@ -210,4 +210,60 @@ fn f2() { (+57 -1) [stdout:"),
            "{big_rendered}"
        );

        // 4. Huge generated snapshot section folds to 1 line so real code stays in Tier 1 Compact Diff!
        let mut snap = String::from(
            "diff --git a/tests/out.snap b/tests/out.snap\nindex 111..222 100644\n--- a/tests/out.snap\n+++ b/tests/out.snap\n@@ -1,1 +1,200 @@\n-old\n",
        );
        for i in 0..200 {
            snap.push_str(&format!(
                "+generated snapshot output row {i:03} with payload\n"
            ));
        }
        snap.push_str(
            "diff --git a/src/real.rs b/src/real.rs\nindex 333..444 100644\n--- a/src/real.rs\n+++ b/src/real.rs\n@@ -1,5 +1,5 @@ fn real() {\n c1\n c2\n-let x = 1;\n+let x = 2;\n c3\n",
        );
        let snap_summary = parse("git diff", &snap);
        let snap_rendered = snap_summary.render(None, 0).unwrap();
        assert!(
            snap_rendered.len() < 400,
            "got {} bytes:\n{snap_rendered}",
            snap_rendered.len()
        );
        assert!(
            snap_rendered.contains("=== tests/out.snap (generated, +200 -1) [stdout:1-206]\n"),
            "{snap_rendered}"
        );
        assert!(
            snap_rendered.contains("=== src/real.rs (+1 -1) [stdout:207-216]\n"),
            "{snap_rendered}"
        );
        assert!(
            snap_rendered.contains("-let x = 1;\n+let x = 2;\n"),
            "{snap_rendered}"
        );
    }

    #[test]
    fn a_last_hunk_cut_short_is_marked_only_when_the_output_can_end_early() {
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@ fn a\n x\n-y\n+z\n w\n@@ -9 +9,2 @@\n-q\n+r\n+s\n";
        let lines: Vec<&str> = patch.split_inclusive('\n').collect();
        let thinned: String = lines
            .iter()
            .filter(|line| !line.starts_with(' '))
            .copied()
            .collect();
        // The text, whether it can end early, and the last item's detail.
        for (text, ends_early, last) in [
            (patch.to_owned(), true, "@@ -9 +9,2 @@"),
            (lines[..11].concat(), true, "@@ -9 +9,2 @@ (cut off)"),
            (lines[..11].concat(), false, "@@ -9 +9,2 @@"),
            (lines[..7].concat(), true, "@@ -1,3 +1,3 @@ fn a (cut off)"),
            (
                thinned[..thinned.len() - 2].to_owned(),
                true,
                "@@ -9 +9,2 @@",
            ),
        ] {
            let mut parser = Parser::new("git diff", None);
            if ends_early {
                parser.ends_early();
            }
            for (index, line) in text.split_inclusive('\n').enumerate() {
                parser.observe(line, index + 1);
            }
            let summary = parser.finish();
            assert_eq!(summary.items.last().unwrap().detail, last, "{text}");
        }
    }

    #[test]
    fn cargo_nextest_failures_panics_and_summaries_are_parsed() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            family(&args("cargo nextest run --workspace")),
            "cargo nextest"
        );
        assert_eq!(family(&args("cargo nextest list")), "cargo");

        let failing = "\
    Starting 3 tests across 1 binary
        PASS [   0.004s] demo::unit test_ok
        FAIL [   0.012s] (1/2) demo::unit test_bad
--- STDERR:              demo::unit test_bad ---
thread 'test_bad' panicked at src/lib.rs:42:5:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
     TIMEOUT [  60.001s] demo::unit test_hang
------------
     Summary [  60.020s] 3 tests run: 1 passed, 1 failed, 1 timed out, 0 skipped
        FAIL [   0.012s] demo::unit test_bad
     TIMEOUT [  60.001s] demo::unit test_hang
";
        let summary = parse("cargo nextest", failing);
        let rendered = summary.render(None, 100).unwrap();
        assert!(
            rendered.contains("2 reported failing test(s)"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "FAIL demo::unit test_bad [stdout:5-8]\n  thread 'test_bad' panicked at src/lib.rs:42:5:\n  assertion `left == right` failed\n  left: 1\n  right: 2"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("TIMEOUT demo::unit test_hang [stdout:10]"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "Summary [  60.020s] 3 tests run: 1 passed, 1 failed, 1 timed out, 0 skipped"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("stdout: 3 nextest pass/status line(s)"),
            "{rendered}"
        );

        // Unmerged nextest output written to stderr compacts with `[stderr:...]` pointers.
        let mut parser = Parser::new("cargo nextest", None);
        for (index, line) in failing.split_inclusive('\n').enumerate() {
            parser.observe_stderr(line, index + 1);
        }
        let stderr_summary = parser.finish();
        let stderr_rendered = stderr_summary.render(None, 100).unwrap();
        assert!(
            stderr_rendered.contains("FAIL demo::unit test_bad [stderr:5-8]"),
            "{stderr_rendered}"
        );
        assert!(
            stderr_rendered.contains("TIMEOUT demo::unit test_hang [stderr:10]"),
            "{stderr_rendered}"
        );
        assert!(
            stderr_rendered.contains("stderr: 3 nextest pass/status line(s)"),
            "{stderr_rendered}"
        );
        assert_eq!(
            stderr_summary.headline().as_deref(),
            Some("2 failing test(s), first FAIL demo::unit test_bad")
        );

        let passing = "\
    Starting 3 tests across 1 binary
        PASS [   0.004s] demo::unit test_ok
        PASS [   0.005s] demo::unit test_bad
        PASS [   0.006s] demo::unit test_hang
------------
     Summary [   0.015s] 3 tests run: 3 passed, 0 skipped
";
        let fixed = parse("cargo nextest", passing);
        let delta = fixed.render(Some(("run-1", &summary)), 0).unwrap();
        assert!(delta.contains("Compared with run-1:"), "{delta}");
        assert!(delta.contains("- demo::unit test_bad "), "{delta}");
        assert!(delta.contains("- demo::unit test_hang "), "{delta}");
        assert!(
            delta.contains("Summary [   0.015s] 3 tests run: 3 passed, 0 skipped"),
            "{delta}"
        );
    }

    #[test]
    fn go_test_extracts_failure_diagnostics_and_compacts_many_passing_packages() {
        // Default non-verbose `go test ./...` prints diagnostic lines after `--- FAIL:`.
        let non_verbose = "\
--- FAIL: TestAdd (0.00s)
    math_test.go:18: got 3, want 4
FAIL
FAIL\texample.com/math\t0.004s
ok  \texample.com/util\t0.002s
";
        let summary = parse("go test", non_verbose);
        let rendered = summary.render(None, 1).unwrap();
        assert!(
            rendered.contains("1 test(s): TestAdd math_test.go:18: got 3, want 4 [stdout:1-2]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("FAIL\texample.com/math\t0.004s"),
            "{rendered}"
        );
        assert!(
            rendered.contains("ok  \texample.com/util\t0.002s"),
            "{rendered}"
        );

        // Verbose `go test -v ./...` prints diagnostic lines between `=== RUN` and `--- FAIL:` (including subtests).
        let verbose = "\
=== RUN   TestParent
=== RUN   TestParent/sub
    parent_test.go:42: unexpected empty response
    --- FAIL: TestParent/sub (0.00s)
--- FAIL: TestParent (0.00s)
FAIL
FAIL\texample.com/api\t0.010s
";
        let verbose_summary = parse("go test", verbose);
        let verbose_rendered = verbose_summary.render(None, 1).unwrap();
        assert!(
            verbose_rendered.contains(
                "TestParent/sub parent_test.go:42: unexpected empty response [stdout:3-4]"
            ),
            "{verbose_rendered}"
        );

        // 150 passing/no-test packages never hit the 128-outcome cap and collapse to one range line.
        let mut many = String::new();
        for i in 1..=145 {
            many.push_str(&format!("ok  \texample.com/pkg{i}\t0.00{i}s\n"));
        }
        for i in 1..=5 {
            many.push_str(&format!("?\texample.com/empty{i}\t[no test files]\n"));
        }
        let many_summary = parse("go test", &many);
        let many_rendered = many_summary.render(None, 0).unwrap();
        assert!(
            many_rendered
                .contains("go test: 145 passed package(s), 5 without tests [stdout:1-150]"),
            "{many_rendered}"
        );
        assert!(many_rendered.len() < 150, "{many_rendered}");
    }

    #[test]
    fn pytest_verbose_failures_and_error_or_skipped_outcomes_are_captured() {
        let verbose_no_summary = "\
tests/test_a.py::test_ok PASSED [ 50%]
tests/test_a.py::test_bad FAILED [100%]
========================= 1 failed, 1 passed in 0.12s =========================
";
        let summary = parse("pytest", verbose_no_summary);
        let rendered = summary.render(None, 1).unwrap();
        assert!(
            rendered.contains("1 reported failing test(s)"),
            "{rendered}"
        );
        assert!(rendered.contains("tests/test_a.py::test_bad"), "{rendered}");

        // When both verbose FAILED and short-summary FAILED appear, they merge into one entry.
        let both = "\
tests/test_a.py::test_bad FAILED [100%]
FAILED tests/test_a.py::test_bad - AssertionError: assert 1 == 2
========================= 1 failed in 0.05s =========================
";
        let merged = parse("pytest", both);
        assert_eq!(merged.items.len(), 1);
        assert_eq!(merged.items[0].detail, "AssertionError: assert 1 == 2");

        // Error-only and skipped-only summary lines are captured.
        let skipped = "========================= 5 skipped in 0.08s =========================\n";
        let skipped_summary = parse("pytest", skipped);
        assert_eq!(skipped_summary.outcomes, ["5 skipped in 0.08s"]);
    }

    #[test]
    fn terraform_plan_summarizes_resource_changes_and_reruns() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            family(&args("terraform plan -out=tfplan")),
            "terraform plan"
        );
        assert_eq!(family(&args("tofu plan")), "terraform plan");

        let plan1 = "\
Terraform will perform the following actions:

  # aws_instance.web[0] will be created
  + resource \"aws_instance\" \"web\" {
      + ami           = \"ami-111\"
      + instance_type = \"t3.micro\"
    }

  # aws_security_group.api will be updated in-place
  ~ resource \"aws_security_group\" \"api\" {
      ~ description = \"old\" -> \"new\"
    }

  # aws_launch_template.worker must be replaced
-/+ resource \"aws_launch_template\" \"worker\" {
      ~ image_id = \"ami-1\" -> \"ami-2\" # forces replacement
    }

Plan: 2 to add, 1 to change, 1 to destroy.
";
        let summary = parse("terraform plan", plan1);
        let rendered = summary.render(None, 0).unwrap();
        assert!(rendered.contains("3 resource change(s)"), "{rendered}");
        assert!(
            rendered.contains("+ aws_instance.web[0] will be created [stdout:3-7]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("~ aws_security_group.api will be updated in-place [stdout:9-12]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("-/+ aws_launch_template.worker must be replaced [stdout:14-17]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Plan: 2 to add, 1 to change, 1 to destroy."),
            "{rendered}"
        );
        assert_eq!(
            summary.headline().as_deref(),
            Some("Plan: 2 to add, 1 to change, 1 to destroy.")
        );

        // Rerun reports only the resource whose attribute changed.
        let plan2 = plan1.replace("\"t3.micro\"", "\"t3.small\"");
        let summary2 = parse("terraform plan", &plan2);
        let delta = summary2.render(Some(("run-1", &summary)), 0).unwrap();
        assert!(delta.contains("Compared with run-1:"), "{delta}");
        assert!(delta.contains("+ + aws_instance.web[0]"), "{delta}");
        assert!(!delta.contains("aws_security_group.api"), "{delta}");

        // A failing plan with no resource items returns None so error preview stays verbatim.
        let errored = parse(
            "terraform plan",
            "Error: Unsupported argument on main.tf line 12\n",
        );
        assert!(errored.render(None, 1).is_none());
    }

    #[test]
    fn rspec_parses_failures_groups_identical_errors_passes_and_diffs_reruns() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(family(&args("rspec spec/")), "rspec");
        assert_eq!(family(&args("bundle exec rspec spec/")), "rspec");
        assert_eq!(family(&args("bundle exec rspec -f doc")), "rspec");
        assert_eq!(family(&args("rspec --format json")), "other");
        assert_eq!(family(&args("rspec -fj")), "other");

        // Passing documentation-format run compacts dramatically to clean pass summary.
        let mut passing = String::new();
        for i in 1..=80 {
            passing.push_str(&format!(
                "OrderCalculator\n  handles billing scenario case #{i:02} with tax and discount\n"
            ));
        }
        passing.push_str(
            "\nFinished in 1.42 seconds (files took 0.65 seconds to load)\n80 examples, 0 failures\n",
        );
        let pass_summary = parse("rspec", &passing);
        let pass_rendered = pass_summary.render(None, 0).unwrap();
        assert!(
            pass_rendered.contains("0 reported failing test(s)\n80 examples, 0 failures\n"),
            "{pass_rendered}"
        );
        assert!(pass_rendered.len() * 40 < passing.len(), "{pass_rendered}");

        // Failing run with two identical failures (grouped!) plus one distinct failure.
        let run1 = "\
FF.F.....

Failures:

  1) Api#get returns 200
     Failure/Error: expect(status).to eq(200)

       expected: 200
            got: 500

       (compared using ==)
     # ./spec/api_spec.rb:14:in `block (3 levels) in <top (required)>'

  2) Api#post returns 200
     Failure/Error: expect(status).to eq(200)

       expected: 200
            got: 500

       (compared using ==)
     # ./spec/api_spec.rb:28:in `block (3 levels) in <top (required)>'

  3) UserMailer#welcome sends email
     Failure/Error: raise RuntimeError, \"SMTP refused\"

     RuntimeError:
       SMTP refused
     # ./app/mailers/user_mailer.rb:24:in `welcome'

Finished in 0.45 seconds (files took 0.31 seconds to load)
9 examples, 3 failures

Failed examples:

rspec ./spec/api_spec.rb:12 # Api#get returns 200
rspec ./spec/api_spec.rb:26 # Api#post returns 200
rspec ./spec/mailers/user_mailer_spec.rb:10 # UserMailer#welcome sends email
";
        let s1 = parse("rspec", run1);
        assert_eq!(s1.kind, "tests");
        assert_eq!(s1.items.len(), 3);
        let r1 = s1.render(None, 1).unwrap();
        assert!(r1.contains("3 reported failing test(s)"), "{r1}");
        assert!(
            r1.contains("2 test(s): ./spec/api_spec.rb:12 Api#get returns 200 Failure/Error: expect(status).to eq(200) expected: 200 got: 500 [stdout:5-12]"),
            "{r1}"
        );
        assert!(
            r1.contains("  ./spec/api_spec.rb:26 Api#post returns 200 [stdout:14]"),
            "{r1}"
        );
        assert!(
            r1.contains("1 test(s): ./spec/mailers/user_mailer_spec.rb:10 UserMailer#welcome sends email Failure/Error: raise RuntimeError, \"SMTP refused\" RuntimeError: SMTP refused [stdout:23-28]"),
            "{r1}"
        );
        assert!(r1.contains("9 examples, 3 failures"), "{r1}");
        assert!(r1.len() < run1.len(), "{r1}");

        // Rerun after fixing the two Api specs shows exact `Compared with run-1:` diff.
        let run2 = "\
..F......

Failures:

  1) UserMailer#welcome sends email
     Failure/Error: raise RuntimeError, \"SMTP refused\"

     RuntimeError:
       SMTP refused
     # ./app/mailers/user_mailer.rb:24:in `welcome'

Finished in 0.41 seconds (files took 0.30 seconds to load)
9 examples, 1 failure

Failed examples:

rspec ./spec/mailers/user_mailer_spec.rb:10 # UserMailer#welcome sends email
";
        let s2 = parse("rspec", run2);
        let delta = s2.render(Some(("run-1", &s1)), 1).unwrap();
        assert!(delta.contains("Compared with run-1:"), "{delta}");
        assert!(
            delta.contains("- ./spec/api_spec.rb:12 Api#get returns 200"),
            "{delta}"
        );
        assert!(
            delta.contains("- ./spec/api_spec.rb:26 Api#post returns 200"),
            "{delta}"
        );
        assert!(
            !delta.contains("+ ./spec/mailers/user_mailer_spec.rb:10"),
            "{delta}"
        );
        assert!(delta.contains("9 examples, 1 failure"), "{delta}");

        // Crash with no parsed examples returns None so error stays verbatim.
        let crashed = parse("rspec", "LoadError: cannot load such file -- missing\n");
        assert!(crashed.render(None, 1).is_none());
    }

    #[test]
    fn phpunit_paratest_and_pest_parse_pass_fail_and_rerun_diffs() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(family(&args("vendor/bin/phpunit")), "phpunit");
        assert_eq!(
            family(&args("./vendor/bin/paratest --processes=4")),
            "phpunit"
        );
        assert_eq!(family(&args("./vendor/bin/pest")), "phpunit");
        assert_eq!(family(&args("php artisan test")), "phpunit");
        assert_eq!(family(&args("phpunit --log-junit out.xml")), "other");
        assert_eq!(family(&args("phpunit --teamcity")), "other");

        // 1. Passing PHPUnit run with lots of progress dots.
        let mut ok_php = String::from("PHPUnit 11.4.3 by Sebastian Bergmann and contributors.\n\n");
        for i in 1..=20 {
            ok_php.push_str(&format!(
                "............................................................... {:>4} / 1260 ({:>3}%)\n",
                i * 63,
                i * 5
            ));
        }
        ok_php
            .push_str("\nTime: 00:02.110, Memory: 32.00 MB\n\nOK (1260 tests, 3410 assertions)\n");
        let ok_summary = parse("phpunit", &ok_php);
        let ok_rendered = ok_summary.render(None, 0).unwrap();
        assert!(
            ok_rendered.contains("0 reported failing test(s)\nOK (1260 tests, 3410 assertions)\n"),
            "{ok_rendered}"
        );
        assert!(ok_rendered.len() * 15 < ok_php.len(), "{ok_rendered}");

        // 2. Failing PHPUnit run with both error and failure sections + rerun diff.
        let php_fail = "\
PHPUnit 11.4.3 by Sebastian Bergmann and contributors.

Runtime:       PHP 8.3.12

..F..E...                                                           9 / 9 (100%)

Time: 00:00.142, Memory: 18.00 MB

There was 1 error:

1) Tests\\Unit\\OrderTest::testAppliesTax
RuntimeException: Tax rate missing

/app/src/Order.php:42
/app/tests/Unit/OrderTest.php:27

--

There was 1 failure:

1) Tests\\Unit\\CartTest::testComputesTotal
Failed asserting that 100 matches expected 110.

/app/tests/Unit/CartTest.php:19

ERRORS!
Tests: 9, Assertions: 14, Errors: 1, Failures: 1.
";
        let s1 = parse("phpunit", php_fail);
        assert_eq!(s1.items.len(), 2);
        let r1 = s1.render(None, 2).unwrap();
        assert!(r1.contains("2 reported failing test(s)"), "{r1}");
        assert!(
            r1.contains("Tests\\Unit\\OrderTest::testAppliesTax RuntimeException: Tax rate missing [stdout:11-15]"),
            "{r1}"
        );
        assert!(
            r1.contains("Tests\\Unit\\CartTest::testComputesTotal Failed asserting that 100 matches expected 110. [stdout:21-24]"),
            "{r1}"
        );
        assert!(
            r1.contains("ERRORS! Tests: 9, Assertions: 14, Errors: 1, Failures: 1."),
            "{r1}"
        );

        // Rerun after fixing OrderTest shows removal of OrderTest in `Compared with run-1:`.
        let php_fail2 = "\
PHPUnit 11.4.3 by Sebastian Bergmann and contributors.

There was 1 failure:

1) Tests\\Unit\\CartTest::testComputesTotal
Failed asserting that 100 matches expected 110.

/app/tests/Unit/CartTest.php:19

FAILURES!
Tests: 9, Assertions: 14, Failures: 1.
";
        let s2 = parse("phpunit", php_fail2);
        let delta = s2.render(Some(("run-1", &s1)), 1).unwrap();
        assert!(delta.contains("Compared with run-1:"), "{delta}");
        assert!(
            delta.contains("- Tests\\Unit\\OrderTest::testAppliesTax"),
            "{delta}"
        );
        assert!(
            !delta.contains("+ Tests\\Unit\\CartTest::testComputesTotal"),
            "{delta}"
        );
        assert!(
            delta.contains("FAILURES! Tests: 9, Assertions: 14, Failures: 1."),
            "{delta}"
        );

        // 3. Pest output with ANSI colors and snippet lines.
        let pest_raw = "\
   \x1b[32mPASS\x1b[0m  Tests\\Unit\\OrderTest
  ✓ calculates subtotal                                        0.01s

   \x1b[31mFAIL\x1b[0m  Tests\\Unit\\CartTest
  ⨯ computes total                                             0.03s
  ────────────────────────────────────────────────────────────────────
   \x1b[31mFAILED\x1b[0m  Tests\\Unit\\CartTest > computes total
  Failed asserting that 100 is identical to 110.

  at tests/Unit/CartTest.php:19
     18▕     expect($cart->total())->toBe(110);
  ➜  19▕     expect($cart->grandTotal())->toBe(110);

  Tests:    1 failed, 1 passed (4 assertions)
  Duration: 0.18s
";
        let pest_summary = parse("phpunit", pest_raw);
        let pest_rendered = pest_summary.render(None, 1).unwrap();
        assert!(
            pest_rendered.contains("1 reported failing test(s)"),
            "{pest_rendered}"
        );
        assert!(
            pest_rendered.contains("Tests\\Unit\\CartTest > computes total Failed asserting that 100 is identical to 110. [stdout:7-12]"),
            "{pest_rendered}"
        );
        assert!(
            pest_rendered.contains("Tests: 1 failed, 1 passed (4 assertions)"),
            "{pest_rendered}"
        );
    }
}
