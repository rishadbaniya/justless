//! Compact records point back to inclusive line ranges in the original stdout.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
}

fn is_false(value: &bool) -> bool {
    !*value
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
    match words.as_slice() {
        ["git", "diff", ..] => "git diff",
        ["git", "show", ..] => "git show",
        ["git", "log", ..] if patch() => "git log",
        ["cargo", "test", ..] => "cargo test",
        ["cargo", "nextest", "run", ..] => "cargo nextest",
        ["go", "test", ..] => "go test",
        ["npm", "test", ..] => "npm test",
        ["playwright", "test", ..] if playwright_display() => "playwright test",
        ["pytest", ..] | ["python" | "python3", "-m", "pytest"] => "pytest",
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
    /// A file's extended header lines, from `diff --git` to its first hunk.
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
            "pytest" | "cargo test" | "cargo nextest" | "go test" | "npm test" => "tests",
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
        self.hunk = remaining.or_else(|| hunk_counts(line));
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
        let line = text.trim_end_matches('\n');
        if self.commits {
            self.discarded = !self.commit(line, number);
            return;
        }
        if self.summary.kind == "diff"
            && self.file.is_empty()
            && !line.is_empty()
            && !line.starts_with("diff --git ")
        {
            self.discarded = true;
            return;
        }
        if self.summary.kind == "diff" {
            self.diff(line, number);
        } else if self.summary.kind == "plan" {
            self.plan(line, number);
        } else if let Some(js) = &mut self.js_tests {
            js.observe(line, number, crate::capture::Stream::Stdout);
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

    /// Read one line of `git show` or `git log -p` output. Each commit header, from
    /// `commit SHA` through its message, is an item before the commit's file and
    /// hunk items. A line out of place, such as a `--stat` line, a combined
    /// `diff --cc` or a custom `--format`, returns false.
    fn commit(&mut self, line: &str, n: usize) -> bool {
        const FIELDS: [&str; 6] = [
            "Merge: ",
            "Author: ",
            "AuthorDate: ",
            "Commit: ",
            "CommitDate: ",
            "Date: ",
        ];
        const HEADERS: [&str; 14] = [
            "index ",
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
        ];
        let place = self.place;
        if let Some(id) = commit_id(line)
            && place != Place::Fields
        {
            self.flush();
            self.file.clear();
            let name = format!("commit {}", short(id, 12));
            self.pending = Some(Item::new(&name, NO_MESSAGE, n));
            self.place = Place::Fields;
        } else if line.is_empty() {
            // A blank line ends the fields, and the patch before the next commit.
            // It belongs to no item.
            self.place = match place {
                Place::Fields | Place::Message => Place::Message,
                _ => Place::Between,
            };
            return true;
        } else if line.starts_with("diff --git ") && place != Place::Between {
            self.patched = true;
            self.place = Place::File;
        } else if line.starts_with("@@ ") && matches!(place, Place::File | Place::Hunk) {
            self.place = Place::Hunk;
        } else {
            let fits = match place {
                Place::Between => false,
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
        if let Some(names) = line.strip_prefix("diff --git ") {
            self.flush();
            self.file = names
                .rsplit_once(" b/")
                .map_or(names, |(_, new)| new)
                .to_owned();
            self.pending = Some(Item::new(&self.file, "file metadata", n));
        } else if line.starts_with("@@ ") && !self.file.is_empty() {
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
            self.pending = Some(Item::new(&self.file, line, n));
        } else if (line.starts_with("old mode ")
            || line.starts_with("rename from ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("copy from "))
            && let Some(item) = &mut self.pending
        {
            item.detail = short(line, 360);
        }
        if let Some(item) = &mut self.pending {
            // A hunk is compared by its lines, not by where it starts, so hunks
            // keep their identity when an earlier change shifts them.
            if !line.starts_with("@@ ") {
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
        // A hunk that a filter such as `grep` thinned out tells nothing about where
        // the output stopped.
        if self.ends_early
            && self.whole_hunks
            && self.hunk.is_some_and(|counts| counts != (0, 0))
            && let Some(item) = &mut self.pending
        {
            item.detail.push_str(" (cut off)");
        }
        self.flush();
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
    fn is_commit(&self) -> bool {
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
        }
    }

    pub(crate) fn stream_name(&self) -> &'static str {
        if self.stderr { "stderr" } else { "stdout" }
    }
    /// The identity used to compare runs. A patch hunk drops its line ranges,
    /// which shift whenever an earlier hunk grows or shrinks.
    fn key(&self) -> (&str, &str, &str) {
        let detail = match self.detail.strip_prefix("@@ ") {
            Some(ranges) => ranges
                .split_once(" @@")
                .map_or(ranges, |(_, context)| context),
            None => &self.detail,
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
                r"\bFAILED\b|\bFAIL\b|(?:^|[^0-9])[1-9][0-9]* (?:failed|failing|timed out)\b",
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
        } else {
            if self.kind == "diff" {
                report.push_str(&self.patch_heading());
                report.push('\n');
            } else if self.kind == "plan" && !self.items.is_empty() {
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
        ] {
            assert_eq!(family(&args(line)), expected, "{line}");
        }
        let summary = parse("git show", SHOW);
        assert_eq!(
            summary.render(None, 0).unwrap(),
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
            // `--stat`, a combined diff, `--oneline`, a blank inside a hunk,
            // `git show -s` and `git show REV:path`.
            SHOW.replacen("\ndiff --git", "\n Cargo.lock | 1 +\n\ndiff --git", 1),
            SHOW.replace("diff --git a/README.md b/README.md", "diff --cc README.md"),
            SHOW.replacen(
                "commit 0940ff5cd49d5b8fc541e004d5dd8e69e793adad",
                "0940ff5 build(deps)",
                1,
            ),
            SHOW.replacen("  \"clap\",\n", "\n", 1),
            header.to_owned(),
            "[package]\nname = \"justless\"\n".to_owned(),
        ] {
            assert!(parse("git show", &text).items.is_empty(), "{text}");
        }
        // `git diff` output still needs `diff --git` first.
        assert!(parse("git diff", SHOW).items.is_empty());
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
}
