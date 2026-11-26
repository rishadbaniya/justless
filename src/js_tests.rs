//! Structured reports for JavaScript/TypeScript and Playwright test runners:
//! Jest, Vitest, Bun test, Node `node --test` (TAP and spec reporters), and
//! Playwright test.
//!
//! Each runner is parsed by a dedicated streaming sub-scanner across both
//! stdout and stderr (Jest and Bun write their test reports to stderr; Vitest
//! splits passing/totals on stdout from failure blocks on stderr; Node and
//! Playwright write to stdout with optional stderr diagnostics).
//!
//! Reports are fail-closed: if output lacks recognized totals or omits failure
//! blocks for a failing total, `finish` returns `None` so the ordinary preview
//! or `LineFilter` handles the run unchanged.
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::capture::{Stream, source_digest};
use crate::summary::{Item, short};

/// Lines of `other` stream text kept in a report: the first ones and the
/// diagnostics with their locations, besides the last.
const KEPT_LINES: usize = 2;
const KEPT_DIAGNOSTICS: usize = 10;
/// Cap on distinct failure blocks tracked in memory.
const FAILURE_LIMIT: usize = 2048;
/// Maximum excerpt lines retained per failure block.
const MAX_EXCERPT: usize = 6;

static CODE_FRAME_GUTTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*>?\s*\d+\s*\|").expect("code frame gutter pattern"));
static CODE_FRAME_CARET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\|\s*[\^~]*\s*$").expect("code frame caret pattern"));
static CARET_ONLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[\^~]+\s*$").expect("caret only pattern"));
static FILE_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:file://)?((?:[A-Za-z]:[\\/]|/)?[^\s()'`\x22]+?\.[A-Za-z0-9]{1,6}:\d+(?::\d+)?)")
        .expect("file location pattern")
});
static VOLATILE_TIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\s*(?:ms|s|m)\b").expect("volatile time pattern"));
static VOLATILE_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?")
        .expect("volatile timestamp pattern")
});
static TRAILING_TIMING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s+(?:\[\d+(?:\.\d+)?\s*(?:ms|s|m)\]|\(\d+(?:\.\d+)?\s*(?:ms|s|m)\))$")
        .expect("trailing timing pattern")
});
static PW_RUNNING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Running \d+ tests? using \d+ workers?").expect("playwright running pattern")
});
static PW_FAILURE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\)\s+(.+)$").expect("playwright failure header pattern"));
static PW_LINE_PROGRESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\d+/\d+\]\s+").expect("playwright line progress pattern"));
static PW_LIST_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[✓✔✘×°±\-]\s+\d+\s+").expect("playwright list line pattern"));
static PW_TOTAL_PASSED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d+ passed(?:\s+\([^)]+\))?$").expect("playwright passed pattern")
});
static PW_TOTAL_COUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d+) (failed|flaky|skipped|interrupted|did not run)(?:\s+\([^)]+\))?$")
        .expect("playwright total count pattern")
});
static BUN_COUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d+) (pass|fail|skip|todo|errors?|expect\(\) calls)$")
        .expect("bun count pattern")
});
static BUN_RAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Ran \d+ tests? across \d+ files?\.").expect("bun ran pattern"));
static TAP_OK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(not )?ok\s+\d+(?:\s*-\s*(.*))?$").expect("tap ok pattern"));
static TAP_PLAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^1\.\.\d+$").expect("tap plan pattern"));
static NODE_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:#|ℹ)\s+(tests|suites|pass|fail|cancelled|skipped|todo|duration_ms)\s+(.+)$")
        .expect("node total pattern")
});
static VITEST_TESTS_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Tests\s+(?:no tests|.*\b(?:passed|failed|skipped|todo)\b.*\(\d+\))")
        .expect("vitest tests total pattern")
});
static VITEST_FILES_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Test Files\s+(?:no tests|.*\b(?:passed|failed|skipped)\b)")
        .expect("vitest files total pattern")
});
static VITEST_ERRORS_TOTAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Errors\s+(\d+)\s+errors?").expect("vitest errors pattern"));
static FAILED_COUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([1-9]\d*)\s+failed\b").expect("failed count pattern"));

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Runner {
    Jest,
    Vitest,
    Bun,
    NodeTest,
    Playwright,
}

impl Runner {
    pub fn label(self) -> &'static str {
        match self {
            Self::Jest => "jest",
            Self::Vitest => "vitest",
            Self::Bun => "bun test",
            Self::NodeTest => "node --test",
            Self::Playwright => "playwright",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Failure {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excerpt: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub root_cause: String,
    #[serde(default)]
    pub stderr: bool,
    pub first: usize,
    pub last: usize,
}

impl Failure {
    fn stream_name(&self) -> &'static str {
        if self.stderr { "stderr" } else { "stdout" }
    }

    fn range_pointer(&self) -> String {
        if self.first == self.last {
            format!("[{}:{}]", self.stream_name(), self.first)
        } else {
            format!("[{}:{}-{}]", self.stream_name(), self.first, self.last)
        }
    }

    fn compact_detail(&self) -> String {
        let mut parts = Vec::new();
        if !self.error.is_empty() {
            parts.push(self.error.as_str());
        }
        for line in &self.excerpt {
            if is_key_assertion_line(line) && !self.error.contains(line.as_str()) {
                parts.push(line.as_str());
                if parts.len() >= 4 {
                    break;
                }
            }
        }
        if let Some(loc) = &self.location
            && !self.name.contains(loc.as_str())
        {
            parts.push(loc.as_str());
        }
        short(&parts.join(" · "), 360)
    }

    fn finalize(&mut self) {
        self.root_cause =
            normalize_root_cause(&self.error, &self.excerpt, self.location.as_deref());
    }

    fn to_item(&self) -> Item {
        let stream = if self.stderr {
            Stream::Stderr
        } else {
            Stream::Stdout
        };
        let detail = self.compact_detail();
        let mut item = Item::new_in_stream(&self.name, &detail, self.first, stream);
        item.last = self.last;
        let digest_basis = if !self.root_cause.is_empty() {
            self.root_cause.as_str()
        } else {
            detail.as_str()
        };
        item.digest = source_digest(digest_basis.as_bytes());
        item
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PassedRange {
    pub count: usize,
    pub unit: String,
    #[serde(default)]
    pub stderr: bool,
    pub first: usize,
    pub last: usize,
}

impl PassedRange {
    fn render(&self, runner: Runner) -> String {
        let stream = if self.stderr { "stderr" } else { "stdout" };
        let range = if self.first == self.last {
            format!("{stream}:{}", self.first)
        } else {
            format!("{stream}:{}-{}", self.first, self.last)
        };
        format!("{}: {} {} [{range}]", runner.label(), self.count, self.unit)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub runner: Runner,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<Failure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passed: Vec<PassedRange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub totals: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Whether this report covers stderr (true when test output came from
    /// stderr or when all non-test stderr lines fit in `notes`).
    #[serde(default)]
    pub stderr: bool,
}

impl Report {
    /// Convert failing tests into `summary::Item` records for rerun diffing and
    /// `Summary::headline`.
    pub fn summary_items(&self) -> Vec<Item> {
        self.failures.iter().map(Failure::to_item).collect()
    }

    /// Render the complete structured report with root-cause grouping, exact
    /// locations, stream line pointers, passing summaries, totals, and notes.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.failures.is_empty() {
            out.push_str(&format!(
                "{} reported failing test(s)\n",
                self.failures.len()
            ));
            let groups = group_failures(&self.failures);
            for group in groups {
                if group.len() == 1 || group[0].root_cause.is_empty() {
                    for failure in group {
                        out.push_str("1 test(s): ");
                        out.push_str(&failure.name);
                        if let Some(loc) = &failure.location
                            && !failure.name.contains(loc.as_str())
                        {
                            out.push_str(" · ");
                            out.push_str(loc);
                        }
                        out.push(' ');
                        out.push_str(&failure.range_pointer());
                        out.push('\n');
                        if !failure.error.is_empty() {
                            out.push_str("  ");
                            out.push_str(&failure.error);
                            out.push('\n');
                        }
                        for line in &failure.excerpt {
                            out.push_str("  ");
                            out.push_str(line);
                            out.push('\n');
                        }
                    }
                } else {
                    let head = group[0];
                    out.push_str(&format!("{} test(s): ", group.len()));
                    if !head.error.is_empty() {
                        out.push_str(&head.error);
                    } else {
                        out.push_str(&head.name);
                    }
                    if let Some(loc) = &head.location
                        && !head.error.contains(loc.as_str())
                    {
                        out.push_str(" · ");
                        out.push_str(loc);
                    }
                    out.push('\n');
                    for line in &head.excerpt {
                        out.push_str("  ");
                        out.push_str(line);
                        out.push('\n');
                    }
                    for failure in group {
                        out.push_str("  ");
                        out.push_str(&failure.name);
                        out.push(' ');
                        out.push_str(&failure.range_pointer());
                        out.push('\n');
                    }
                }
            }
        }
        self.push_footer(&mut out);
        out
    }

    /// Render the tail of a rerun diff: any still-failing tests, passing suite
    /// ranges, exact totals, and `other` notes.
    pub fn render_after_diff(&self, remaining: &[&Item]) -> String {
        let mut out = String::new();
        if !remaining.is_empty() {
            out.push_str(&format!("Still failing ({}):\n", remaining.len()));
            for item in remaining {
                let range = if item.first == item.last {
                    format!("{}:{}", item.stream_name(), item.first)
                } else {
                    format!("{}:{}-{}", item.stream_name(), item.first, item.last)
                };
                if item.detail.is_empty() {
                    out.push_str(&format!("  {} [{range}]\n", item.name));
                } else {
                    out.push_str(&format!(
                        "  {} · {} [{range}]\n",
                        item.name,
                        short(&item.detail, 140)
                    ));
                }
            }
        }
        self.push_footer(&mut out);
        out
    }

    fn push_footer(&self, out: &mut String) {
        for range in &self.passed {
            out.push_str(&range.render(self.runner));
            out.push('\n');
        }
        for line in &self.totals {
            out.push_str(line);
            out.push('\n');
        }
        for note in &self.notes {
            out.push_str(note);
            out.push('\n');
        }
    }
}

/// Group failures by `root_cause` while preserving first-appearance order.
fn group_failures(failures: &[Failure]) -> Vec<Vec<&Failure>> {
    let mut groups: Vec<(&str, Vec<&Failure>)> = Vec::new();
    for failure in failures {
        if !failure.root_cause.is_empty()
            && let Some((_, existing)) = groups
                .iter_mut()
                .find(|(key, _)| *key == failure.root_cause)
        {
            existing.push(failure);
            continue;
        }
        groups.push((failure.root_cause.as_str(), vec![failure]));
    }
    groups.into_iter().map(|(_, items)| items).collect()
}

#[derive(Default)]
struct Range {
    count: usize,
    first: usize,
    last: usize,
}

impl Range {
    fn add(&mut self, n: usize) {
        if self.first == 0 {
            self.first = n;
        }
        self.count += 1;
        self.last = n;
    }

    /// Extend the line span of the range without incrementing the item count
    /// (used for multi-line YAML blocks belonging to an already-counted passing test).
    fn extend_line(&mut self, n: usize) {
        if self.first == 0 {
            self.first = n;
        }
        self.last = n;
    }
}

#[derive(Default)]
struct Other {
    range: Range,
    kept: Vec<(usize, String)>,
    diagnostics: usize,
    last: Option<(usize, String)>,
    /// Follow-up lines kept after a `console.error` / `console.warn` / unhandled header.
    keep_next: usize,
}

impl Other {
    fn observe(&mut self, line: &str, n: usize) {
        let ordinal = self.range.count;
        self.range.add(n);
        let trimmed_end = line.trim_end();
        let trimmed = trimmed_end.trim();
        let is_console_header = trimmed == "console.error"
            || trimmed == "console.warn"
            || trimmed.starts_with("console.error ")
            || trimmed.starts_with("console.warn ");
        let diag = is_js_diagnostic(trimmed);
        let follow = self.keep_next > 0;
        if follow {
            self.keep_next -= 1;
        }
        if is_console_header {
            self.keep_next = 2;
        }
        let clipped = short_slice(trimmed_end, 200);
        if self.kept.len() < KEPT_LINES && self.diagnostics == 0
            || (diag || follow) && self.diagnostics < KEPT_DIAGNOSTICS
        {
            self.diagnostics += usize::from(diag || follow);
            self.kept.push((ordinal, clipped.to_owned()));
            self.last = None;
        } else if let Some((slot_ord, slot_buf)) = self.last.as_mut() {
            *slot_ord = ordinal;
            slot_buf.clear();
            slot_buf.push_str(clipped);
        } else {
            self.last = Some((ordinal, clipped.to_owned()));
        }
    }

    fn all_shown(&self) -> bool {
        self.kept.len() + usize::from(self.last.is_some()) == self.range.count
    }

    fn push_notes(&self, stream: Stream, notes: &mut Vec<String>) {
        if self.range.count == 0 {
            return;
        }
        let name = stream.name();
        notes.push(format!(
            "{name}: {} other line(s) [{name}:{}-{}]",
            self.range.count, self.range.first, self.range.last
        ));
        let mut next = 0;
        for (ordinal, text) in self.kept.iter().chain(&self.last) {
            if *ordinal > next {
                notes.push("  …".into());
            }
            notes.push(format!("  {text}"));
            next = ordinal + 1;
        }
        if self.range.count > next {
            notes.push("  …".into());
        }
    }
}

/// Top-level streaming scanner that feeds all 5 runner sub-scanners and selects
/// the recognized report (or `None` fail-closed).
#[derive(Default)]
pub struct Scan {
    jest: JestScan,
    vitest: VitestScan,
    bun: BunScan,
    node: NodeTestScan,
    playwright: PlaywrightScan,
    broken: bool,
}

impl Scan {
    pub fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        if self.broken {
            return;
        }
        let clean = strip_ansi(line.trim_end_matches(['\n', '\r']));
        let line = clean.as_ref();
        self.jest.observe(line, n, stream);
        self.vitest.observe(line, n, stream);
        self.bun.observe(line, n, stream);
        self.node.observe(line, n, stream);
        self.playwright.observe(line, n, stream);
    }

    pub fn finish(self) -> Option<Report> {
        if self.broken {
            return None;
        }
        let candidates: Vec<(usize, Report)> = [
            self.jest.finish(),
            self.vitest.finish(),
            self.bun.finish(),
            self.node.finish(),
            self.playwright.finish(),
        ]
        .into_iter()
        .flatten()
        .collect();
        // Prefer the runner with the fewest unexplained `other` lines.
        candidates
            .into_iter()
            .min_by_key(|(other_count, _)| *other_count)
            .map(|(_, report)| report)
    }
}

// ---------------------------------------------------------------------------
// Shared parsing & normalization helpers
// ---------------------------------------------------------------------------

/// Strip ANSI escape sequences only when an ESC byte is present (zero-alloc fast path).
fn strip_ansi(line: &str) -> Cow<'_, str> {
    if !line.contains('\x1b') {
        return Cow::Borrowed(line);
    }
    crate::capture::ANSI.replace_all(line, "")
}

fn short_slice(s: &str, len: usize) -> &str {
    &s[..s.floor_char_boundary(s.len().min(len))]
}

/// Push an excerpt line while keeping `Expected`/`Received`/`-`/`+`/`Locator`/`Timeout`
/// diff lines crisp even when unchanged multi-line diff context would fill `MAX_EXCERPT`.
fn push_excerpt_line(excerpt: &mut Vec<String>, trimmed: &str) {
    let line = short(trimmed, 200);
    if excerpt.len() < MAX_EXCERPT {
        excerpt.push(line);
        return;
    }
    if is_key_assertion_line(trimmed)
        && let Some(pos) = excerpt.iter().rposition(|l| !is_key_assertion_line(l))
    {
        excerpt.remove(pos);
        excerpt.push(line);
    }
}

/// Extract the line number from a code-frame gutter line such as `> 14 | expect(...)` or `14 | ...`.
fn code_frame_line_number(line: &str) -> Option<usize> {
    let s = line.trim_start();
    let s = s.strip_prefix('>').unwrap_or(s).trim_start();
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let after = s[digits..].trim_start();
    if !after.starts_with('|') {
        return None;
    }
    s[..digits].parse().ok()
}

fn is_code_frame_line(line: &str) -> bool {
    CODE_FRAME_GUTTER.is_match(line) || CODE_FRAME_CARET.is_match(line) || CARET_ONLY.is_match(line)
}

fn is_explicit_diff_header_or_value(trimmed: &str) -> bool {
    trimmed.starts_with("Expected:")
        || trimmed.starts_with("Received:")
        || trimmed.starts_with("Expected ")
        || trimmed.starts_with("Received ")
        || trimmed.starts_with("Locator:")
        || trimmed.starts_with("Timeout:")
        || trimmed.starts_with("- Expected")
        || trimmed.starts_with("+ Received")
}

fn is_key_assertion_line(line: &str) -> bool {
    let trimmed = line.trim();
    is_explicit_diff_header_or_value(trimmed)
        || trimmed.starts_with("- ")
        || trimmed.starts_with("+ ")
        || trimmed.starts_with("Cause:")
        || trimmed.starts_with("Caused by:")
}

fn is_js_error_header(trimmed: &str) -> bool {
    if trimmed.starts_with("error:") {
        return true;
    }
    let Some((head, _)) = trimmed.split_once(':') else {
        return false;
    };
    let word = head.split_whitespace().next().unwrap_or("");
    !word.is_empty()
        && word.as_bytes()[0].is_ascii_uppercase()
        && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && (word.ends_with("Error") || word.ends_with("Exception"))
}

fn is_js_diagnostic(trimmed: &str) -> bool {
    crate::filters::diagnostic(trimmed)
        || trimmed.starts_with("console.error")
        || trimmed.starts_with("console.warn")
        || trimmed.contains("UnhandledPromiseRejection")
        || trimmed.contains("Unhandled Rejection")
        || trimmed.contains("DeprecationWarning")
        || trimmed.contains("ExperimentalWarning")
        || trimmed.contains("MaxListenersExceededWarning")
        || trimmed.contains("SyntaxError")
        || trimmed.contains("ReferenceError")
        || trimmed.contains("TypeError")
        || trimmed.contains("RangeError")
        || trimmed.contains("AggregateError")
        || trimmed.contains("ERR_")
}

/// Package manager script echoes (`> app@1.0.0 test`, `yarn run v1.22`, `$ jest`).
fn is_wrapper_header(trimmed: &str) -> bool {
    trimmed.starts_with("> ")
        || trimmed.starts_with("$ ")
        || trimmed.starts_with("yarn run v")
        || (trimmed.starts_with("Done in ") && trimmed.ends_with("s."))
}

fn is_in_repo_path(path: &str) -> bool {
    !(path.contains("node_modules/")
        || path.contains("node_modules\\")
        || path.starts_with("node:")
        || path.contains("node:internal")
        || path.starts_with("internal/")
        || path.starts_with("bun:")
        || path.contains("<anonymous>")
        || path.contains("<unknown>"))
}

/// Extract a `file:line:col` (or `file:line`) location from a stack frame or
/// header line, returning `(location, is_in_repo)`.
fn extract_location(line: &str) -> Option<(String, bool)> {
    // Prefer parenthesized location as in `at fn (src/a.ts:14:19)`.
    if let Some((_, inside)) = line.rsplit_once('(')
        && let Some((candidate, _)) = inside.split_once(')')
        && let Some(found) = match_file_loc(candidate)
    {
        return Some(found);
    }
    match_file_loc(line)
}

fn match_file_loc(text: &str) -> Option<(String, bool)> {
    for cap in FILE_LOC.captures_iter(text) {
        let raw = cap[1].trim_end_matches([',', ';', ')', ']', '\'', '"']);
        if raw.starts_with("http://") || raw.starts_with("https://") {
            continue;
        }
        return Some((raw.to_owned(), is_in_repo_path(raw)));
    }
    None
}

fn strip_trailing_timing(text: &str) -> &str {
    TRAILING_TIMING
        .find(text)
        .map_or(text, |m| text[..m.start()].trim_end())
}

fn strip_box_trailing(text: &str) -> &str {
    text.trim_end_matches(['─', '⎯', '━', '=', ' '])
}

fn normalize_root_cause(error: &str, excerpt: &[String], location: Option<&str>) -> String {
    let mut parts = Vec::new();
    if !error.is_empty() {
        parts.push(normalize_text(error));
    }
    for line in excerpt {
        if is_key_assertion_line(line) {
            parts.push(normalize_text(line));
        }
    }
    if let Some(loc) = location
        && !loc.is_empty()
    {
        parts.push(loc.to_owned());
    }
    parts.join(" | ")
}

fn normalize_text(text: &str) -> String {
    let text = VOLATILE_TS.replace_all(text, "<ts>");
    let text = VOLATILE_TIME.replace_all(&text, "<time>");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn build_notes(
    other: &[Other; 2],
    has_test_stderr: bool,
    status: &[Range; 2],
    status_label: &str,
) -> (Vec<String>, bool, usize) {
    let covers_stderr = has_test_stderr || other[Stream::Stderr as usize].all_shown();
    let mut notes = Vec::new();
    for stream in Stream::BOTH {
        if stream == Stream::Stderr && !covers_stderr {
            continue;
        }
        let idx = stream as usize;
        let name = stream.name();
        if status[idx].count > 0 && other[idx].range.count > 0 {
            let st = &status[idx];
            notes.push(format!(
                "{name}: {} {status_label} [{name}:{}-{}]",
                st.count, st.first, st.last
            ));
        }
        other[idx].push_notes(stream, &mut notes);
    }
    let total_other = other[0].range.count + other[1].range.count;
    (notes, covers_stderr, total_other)
}

fn push_passed_ranges(passed: &[Range; 2], unit: &str, out: &mut Vec<PassedRange>) {
    for stream in Stream::BOTH {
        let range = &passed[stream as usize];
        if range.count > 0 {
            out.push(PassedRange {
                count: range.count,
                unit: unit.to_owned(),
                stderr: stream == Stream::Stderr,
                first: range.first,
                last: range.last,
            });
        }
    }
}

fn record_location(failure: &mut Failure, has_in_repo: &mut bool, loc: String, in_repo: bool) {
    if !*has_in_repo && (in_repo || failure.location.is_none()) {
        failure.location = Some(loc);
        if in_repo {
            *has_in_repo = true;
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Jest Scanner
// ---------------------------------------------------------------------------

struct OpenFailure {
    index: usize,
    stream: Stream,
    has_in_repo_loc: bool,
    saw_stack: bool,
}

#[derive(Default)]
struct JestScan {
    failures: Vec<Failure>,
    seen_keys: HashSet<(String, String)>,
    open: Option<OpenFailure>,
    current_fail_file: String,
    in_pass_suite: bool,
    in_summary_repeat: bool,
    skip_duplicate_block: bool,
    saw_totals: bool,
    reported_failed: bool,
    has_test_stderr: bool,
    passed: [Range; 2],
    status: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
}

impl JestScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let idx = stream as usize;
        // Stream switch closes any open block from the other stream.
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
            self.skip_duplicate_block = false;
            self.in_pass_suite = false;
        }

        // Totals lines.
        if trimmed.starts_with("Test Suites:")
            || (trimmed.starts_with("Tests:") && trimmed.contains("total"))
            || trimmed.starts_with("Snapshots:")
            || trimmed.starts_with("Time:")
            || trimmed.starts_with("Ran all test suites")
        {
            self.open = None;
            self.in_pass_suite = false;
            self.in_summary_repeat = false;
            self.skip_duplicate_block = false;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            if trimmed.starts_with("Test Suites:") || trimmed.starts_with("Tests:") {
                self.saw_totals = true;
                if FAILED_COUNT.is_match(trimmed) {
                    self.reported_failed = true;
                }
            }
            if !trimmed.starts_with("Ran all test suites") {
                self.totals.push(short(trimmed, 220));
            } else {
                self.status[idx].add(n);
            }
            return;
        }

        if trimmed == "Summary of all failing tests" {
            self.open = None;
            self.in_pass_suite = false;
            self.in_summary_repeat = true;
            self.skip_duplicate_block = false;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.status[idx].add(n);
            return;
        }

        if trimmed == "Determining test suites to run..." {
            self.status[idx].add(n);
            return;
        }

        // Passing suite line: `PASS src/a.test.ts` (never `PASS [` from nextest).
        if let Some(rest) = line.trim_start().strip_prefix("PASS ")
            && !rest.trim_start().starts_with('[')
            && !rest.trim().is_empty()
        {
            self.open = None;
            self.in_pass_suite = true;
            self.skip_duplicate_block = false;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.passed[idx].add(n);
            return;
        }

        // Failing suite line: `FAIL src/a.test.ts` (never `FAIL [` or `FAIL\t` or Vitest `FAIL  a > b`).
        if let Some(rest) = line.trim_start().strip_prefix("FAIL ")
            && !rest.trim_start().starts_with('[')
            && !rest.contains(" > ")
            && !rest.trim().is_empty()
        {
            self.open = None;
            self.in_pass_suite = false;
            self.skip_duplicate_block = false;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let file = strip_trailing_timing(rest.trim()).trim();
            self.current_fail_file = file.to_owned();
            self.status[idx].add(n);
            return;
        }

        // Individual failure block header: `● Suite › test`.
        if let Some(test_name) = trimmed.strip_prefix("● ").map(str::trim)
            && !test_name.is_empty()
        {
            self.open = None;
            self.in_pass_suite = false;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let key = (self.current_fail_file.clone(), test_name.to_owned());
            if !self.seen_keys.insert(key) {
                self.skip_duplicate_block = true;
                self.status[idx].add(n);
                return;
            }
            self.skip_duplicate_block = false;
            if self.failures.len() >= FAILURE_LIMIT {
                return;
            }
            let display_name =
                if test_name == "Test suite failed to run" && !self.current_fail_file.is_empty() {
                    format!("{} › {test_name}", self.current_fail_file)
                } else {
                    test_name.to_owned()
                };
            let failure_idx = self.failures.len();
            self.failures.push(Failure {
                name: short(&display_name, 260),
                location: None,
                error: String::new(),
                excerpt: Vec::new(),
                root_cause: String::new(),
                stderr: stream == Stream::Stderr,
                first: n,
                last: n,
            });
            self.open = Some(OpenFailure {
                index: failure_idx,
                stream,
                has_in_repo_loc: false,
                saw_stack: false,
            });
            return;
        }

        if self.skip_duplicate_block {
            self.status[idx].add(n);
            return;
        }

        // Body of an open Jest failure block.
        if let Some(open) = self.open.as_mut() {
            let failure = &mut self.failures[open.index];
            failure.last = n;
            if is_code_frame_line(line) {
                return;
            }
            if trimmed.starts_with("at ") {
                open.saw_stack = true;
                if let Some((loc, in_repo)) = extract_location(trimmed) {
                    record_location(failure, &mut open.has_in_repo_loc, loc, in_repo);
                }
                return;
            }
            if !open.saw_stack {
                if failure.error.is_empty() {
                    failure.error = short(trimmed, 220);
                } else {
                    push_excerpt_line(&mut failure.excerpt, trimmed);
                }
            }
            return;
        }

        // Verbose per-test lines under PASS or FAIL before `●`.
        if line.starts_with(' ')
            && (trimmed.starts_with("✓ ")
                || trimmed.starts_with("√ ")
                || trimmed.starts_with("○ ")
                || trimmed.starts_with("✎ ")
                || trimmed.starts_with("✕ ")
                || trimmed.starts_with("× "))
        {
            if self.in_pass_suite {
                self.passed[idx].extend_line(n);
            } else {
                self.status[idx].add(n);
            }
            return;
        }

        // Verbose describe-block header inside a PASS suite (indented, non-diagnostic).
        if self.in_pass_suite && line.starts_with("  ") && !is_js_diagnostic(trimmed) {
            self.passed[idx].extend_line(n);
            return;
        }

        if is_wrapper_header(trimmed) {
            self.status[idx].add(n);
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(mut self) -> Option<(usize, Report)> {
        if !self.saw_totals {
            return None;
        }
        if self.reported_failed && self.failures.is_empty() {
            return None;
        }
        for f in &mut self.failures {
            f.finalize();
        }
        let mut passed = Vec::new();
        push_passed_ranges(&self.passed, "passed suite(s)", &mut passed);
        let (notes, stderr, other_count) = build_notes(
            &self.other,
            self.has_test_stderr,
            &self.status,
            "status line(s)",
        );
        Some((
            other_count,
            Report {
                runner: Runner::Jest,
                failures: self.failures,
                passed,
                totals: self.totals,
                notes,
                stderr,
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// 2. Vitest Scanner
// ---------------------------------------------------------------------------

#[derive(Default)]
struct VitestScan {
    failures: Vec<Failure>,
    fallback_failures: Vec<Failure>,
    open: Option<OpenFailure>,
    open_fallback: Option<(usize, Stream)>,
    in_failed_section: bool,
    in_unhandled: bool,
    saw_marker: bool,
    saw_totals: bool,
    reported_failed: bool,
    has_test_stderr: bool,
    passed: [Range; 2],
    status: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
}

impl VitestScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let idx = stream as usize;
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
            self.in_unhandled = false;
        }
        if self.open_fallback.is_some_and(|(_, s)| s != stream) {
            self.open_fallback = None;
        }

        // Vitest banner: `RUN  v2.1.8 /repo`.
        if trimmed.starts_with("RUN  v") || trimmed.starts_with("DEV  v") {
            self.saw_marker = true;
            self.status[idx].add(n);
            return;
        }

        // Section dividers (`⎯⎯⎯⎯⎯⎯ Failed Tests 2 ⎯⎯⎯⎯⎯⎯`, `⎯⎯⎯⎯⎯[1/2]⎯`, or box `───`).
        if trimmed.starts_with("⎯⎯⎯")
            || (trimmed.starts_with("───")
                && (trimmed.contains("Failed Tests")
                    || trimmed.contains("Failed Suites")
                    || trimmed.contains("Unhandled Error")))
        {
            self.open = None;
            self.open_fallback = None;
            self.saw_marker = true;
            self.in_unhandled = trimmed.contains("Unhandled Error");
            self.in_failed_section =
                trimmed.contains("Failed Tests") || trimmed.contains("Failed Suites");
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.status[idx].add(n);
            return;
        }

        // Totals lines.
        if VITEST_FILES_TOTAL.is_match(trimmed) || VITEST_TESTS_TOTAL.is_match(trimmed) {
            self.open = None;
            self.open_fallback = None;
            self.in_failed_section = false;
            self.in_unhandled = false;
            self.saw_totals = true;
            if FAILED_COUNT.is_match(trimmed) {
                self.reported_failed = true;
            }
            self.totals.push(short(trimmed, 220));
            return;
        }
        if let Some(cap) = VITEST_ERRORS_TOTAL.captures(trimmed) {
            self.open = None;
            self.open_fallback = None;
            self.in_failed_section = false;
            self.in_unhandled = false;
            if cap[1].parse::<usize>().unwrap_or(0) > 0 {
                self.reported_failed = true;
            }
            self.totals.push(short(trimmed, 220));
            return;
        }
        if trimmed.starts_with("Duration  ") {
            self.open = None;
            self.open_fallback = None;
            self.in_failed_section = false;
            self.totals.push(short(trimmed, 220));
            return;
        }
        if trimmed.starts_with("Start at  ") {
            self.status[idx].add(n);
            return;
        }

        // Detailed failure header: `FAIL  src/a.test.ts > Suite > test`,
        // `FAIL  src/a.test.ts [ src/a.test.ts ]`, or standalone `FAIL  src/a.test.ts`
        // inside Vitest v2/v3's Failed Tests / Failed Suites section.
        if let Some(rest) = trimmed.strip_prefix("FAIL ")
            && (rest.contains(" > ") || rest.contains(" [ ") || self.in_failed_section)
            && !rest.trim().is_empty()
        {
            self.open = None;
            self.open_fallback = None;
            self.in_unhandled = false;
            self.saw_marker = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            if self.failures.len() >= FAILURE_LIMIT {
                return;
            }
            let name = strip_trailing_timing(rest.trim());
            let header_loc = extract_location(name).map(|(loc, _)| loc);
            let failure_idx = self.failures.len();
            self.failures.push(Failure {
                name: short(name, 260),
                location: header_loc,
                error: String::new(),
                excerpt: Vec::new(),
                root_cause: String::new(),
                stderr: stream == Stream::Stderr,
                first: n,
                last: n,
            });
            self.open = Some(OpenFailure {
                index: failure_idx,
                stream,
                has_in_repo_loc: false,
                saw_stack: false,
            });
            return;
        }

        // Unhandled error block inside `⎯⎯⎯⎯⎯⎯ Unhandled Errors ⎯⎯⎯⎯⎯⎯`.
        if self.in_unhandled && self.open.is_none() && is_js_diagnostic(trimmed) {
            if self.failures.len() < FAILURE_LIMIT {
                let failure_idx = self.failures.len();
                self.failures.push(Failure {
                    name: "Unhandled Error".into(),
                    location: None,
                    error: short(trimmed, 220),
                    excerpt: Vec::new(),
                    root_cause: String::new(),
                    stderr: stream == Stream::Stderr,
                    first: n,
                    last: n,
                });
                self.open = Some(OpenFailure {
                    index: failure_idx,
                    stream,
                    has_in_repo_loc: false,
                    saw_stack: false,
                });
            }
            return;
        }

        // Body of an open Vitest failure block.
        if let Some(open) = self.open.as_mut() {
            let failure = &mut self.failures[open.index];
            failure.last = n;
            if is_code_frame_line(line) {
                return;
            }
            if trimmed.starts_with("❯ ") || trimmed.starts_with("at ") {
                open.saw_stack = true;
                if let Some((loc, in_repo)) = extract_location(trimmed) {
                    record_location(failure, &mut open.has_in_repo_loc, loc, in_repo);
                }
                return;
            }
            if !open.saw_stack {
                if failure.error.is_empty() {
                    failure.error = short(trimmed, 220);
                } else {
                    push_excerpt_line(&mut failure.excerpt, trimmed);
                }
            } else {
                // Vitest v3 can emit `- Expected` / `+ Received` / `Expected:` / `Received:`
                // diffs after a `❯ file:line:col` pointer; keep those crisp diff lines too.
                let saw_diff_hdr = failure
                    .excerpt
                    .iter()
                    .any(|l| is_explicit_diff_header_or_value(l));
                if is_explicit_diff_header_or_value(trimmed)
                    || (saw_diff_hdr && (trimmed.starts_with("- ") || trimmed.starts_with("+ ")))
                {
                    push_excerpt_line(&mut failure.excerpt, trimmed);
                }
            }
            return;
        }

        // Passing test file / test line: `✓ src/a.test.ts (5 tests) 12ms`.
        if (trimmed.starts_with("✓ ") || trimmed.starts_with("✔ "))
            && !PW_LIST_LINE.is_match(trimmed)
        {
            self.open_fallback = None;
            self.saw_marker = true;
            self.passed[idx].add(n);
            return;
        }

        // Skipped line: `↓ ...` or `· ...`.
        if trimmed.starts_with("↓ ") || trimmed.starts_with("· ") {
            self.open_fallback = None;
            self.status[idx].add(n);
            return;
        }

        // Stdout failing file summary: `❯ src/b.test.ts (5 tests | 2 failed) 18ms`.
        if trimmed.starts_with("❯ ") && (trimmed.contains(" failed)") || trimmed.contains(" test"))
        {
            self.open_fallback = None;
            self.saw_marker = true;
            self.status[idx].add(n);
            return;
        }

        // Stdout inline failure line (`× Suite > test 4ms` + `→ message`).
        if let Some(rest) = trimmed.strip_prefix("× ")
            && !PW_LIST_LINE.is_match(trimmed)
        {
            self.saw_marker = true;
            if rest.contains(" failed)") {
                self.status[idx].add(n);
            } else if self.fallback_failures.len() < FAILURE_LIMIT {
                let name = strip_trailing_timing(rest.trim());
                let fb_idx = self.fallback_failures.len();
                self.fallback_failures.push(Failure {
                    name: short(name, 260),
                    location: None,
                    error: String::new(),
                    excerpt: Vec::new(),
                    root_cause: String::new(),
                    stderr: stream == Stream::Stderr,
                    first: n,
                    last: n,
                });
                self.open_fallback = Some((fb_idx, stream));
            }
            return;
        }
        if let Some(msg) = trimmed.strip_prefix("→ ")
            && let Some((fb_idx, _)) = self.open_fallback
        {
            let fb = &mut self.fallback_failures[fb_idx];
            if fb.error.is_empty() {
                fb.error = short(msg.trim(), 220);
            }
            fb.last = n;
            return;
        }

        if is_wrapper_header(trimmed) {
            self.status[idx].add(n);
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(self) -> Option<(usize, Report)> {
        if !self.saw_totals {
            return None;
        }
        let mut failures = if !self.failures.is_empty() {
            self.failures
        } else {
            self.fallback_failures
        };
        if self.reported_failed && failures.is_empty() {
            return None;
        }
        for f in &mut failures {
            f.finalize();
        }
        let mut passed = Vec::new();
        push_passed_ranges(&self.passed, "passed test file(s)", &mut passed);
        let (notes, stderr, other_count) = build_notes(
            &self.other,
            self.has_test_stderr,
            &self.status,
            "status line(s)",
        );
        Some((
            other_count,
            Report {
                runner: Runner::Vitest,
                failures,
                passed,
                totals: self.totals,
                notes,
                stderr,
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// 3. Bun test Scanner
// ---------------------------------------------------------------------------

struct PendingBunError {
    stream: Stream,
    first: usize,
    last: usize,
    error: String,
    excerpt: Vec<String>,
    location: Option<String>,
    code_frame_line: Option<usize>,
    has_in_repo_loc: bool,
    saw_stack: bool,
}

#[derive(Default)]
struct BunScan {
    failures: Vec<Failure>,
    pending: Option<PendingBunError>,
    open_after: Option<OpenFailure>,
    current_file: String,
    saw_banner: bool,
    saw_outcome: bool,
    saw_pass_fail: bool,
    saw_ran: bool,
    reported_fail: usize,
    has_test_stderr: bool,
    passed: [Range; 2],
    status: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
}

impl BunScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let idx = stream as usize;
        if self.pending.as_ref().is_some_and(|p| p.stream != stream) {
            self.flush_unclaimed();
        }
        if self.open_after.as_ref().is_some_and(|o| o.stream != stream) {
            self.open_after = None;
        }

        if trimmed.starts_with("bun test v") {
            self.saw_banner = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.status[idx].add(n);
            return;
        }

        if BUN_RAN.is_match(trimmed) {
            self.flush_unclaimed();
            self.open_after = None;
            self.saw_ran = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.totals.push(short(trimmed, 220));
            return;
        }

        if let Some(cap) = BUN_COUNT.captures(trimmed) {
            self.flush_unclaimed();
            self.open_after = None;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let count = cap[1].parse::<usize>().unwrap_or(0);
            match &cap[2] {
                "pass" => self.saw_pass_fail = true,
                "fail" => {
                    self.saw_pass_fail = true;
                    self.reported_fail = self.reported_fail.max(count);
                }
                "error" | "errors" => {
                    self.reported_fail = self.reported_fail.max(count);
                }
                _ => {}
            }
            self.totals.push(short(trimmed, 220));
            return;
        }

        if trimmed.starts_with("(pass) ") {
            self.pending = None;
            self.open_after = None;
            self.saw_outcome = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.passed[idx].add(n);
            return;
        }

        if trimmed.starts_with("(skip) ") || trimmed.starts_with("(todo) ") {
            self.pending = None;
            self.open_after = None;
            self.saw_outcome = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.status[idx].add(n);
            return;
        }

        if let Some(rest) = trimmed.strip_prefix("(fail) ") {
            self.open_after = None;
            self.saw_outcome = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            if self.failures.len() >= FAILURE_LIMIT {
                self.pending = None;
                return;
            }
            let raw_name = strip_trailing_timing(rest.trim());
            let name = if !self.current_file.is_empty() && !raw_name.contains(&self.current_file) {
                format!("{} › {raw_name}", self.current_file)
            } else {
                raw_name.to_owned()
            };
            if let Some(pending) = self.pending.take()
                && (!pending.error.is_empty()
                    || pending.location.is_some()
                    || pending.code_frame_line.is_some())
            {
                let location = pending.location.or_else(|| {
                    pending
                        .code_frame_line
                        .filter(|_| !self.current_file.is_empty())
                        .map(|ln| format!("{}:{ln}", self.current_file))
                });
                self.failures.push(Failure {
                    name: short(&name, 260),
                    location,
                    error: pending.error,
                    excerpt: pending.excerpt,
                    root_cause: String::new(),
                    stderr: stream == Stream::Stderr,
                    first: pending.first.min(n),
                    last: n,
                });
            } else {
                let failure_idx = self.failures.len();
                self.failures.push(Failure {
                    name: short(&name, 260),
                    location: None,
                    error: String::new(),
                    excerpt: Vec::new(),
                    root_cause: String::new(),
                    stderr: stream == Stream::Stderr,
                    first: n,
                    last: n,
                });
                self.open_after = Some(OpenFailure {
                    index: failure_idx,
                    stream,
                    has_in_repo_loc: false,
                    saw_stack: false,
                });
            }
            return;
        }

        // File header: `src/billing.test.ts:`.
        if is_bun_file_header(line) {
            self.flush_unclaimed();
            self.open_after = None;
            self.current_file = trimmed.trim_end_matches(':').to_owned();
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.status[idx].add(n);
            return;
        }

        // Code frame lines preceding `error:` in Bun.
        if is_code_frame_line(line) {
            let frame_line = code_frame_line_number(line);
            if self.pending.is_none() && self.open_after.is_none() {
                self.pending = Some(PendingBunError {
                    stream,
                    first: n,
                    last: n,
                    error: String::new(),
                    excerpt: Vec::new(),
                    location: None,
                    code_frame_line: frame_line,
                    has_in_repo_loc: false,
                    saw_stack: false,
                });
            } else if let Some(pending) = self.pending.as_mut() {
                pending.last = n;
                if frame_line.is_some() {
                    pending.code_frame_line = frame_line;
                }
            } else if let Some(open) = self.open_after.as_mut() {
                self.failures[open.index].last = n;
            }
            return;
        }

        // Error / assertion / stack lines (either before `(fail)` or right after `(fail)`).
        let starts_error = is_js_error_header(trimmed);

        if let Some(open) = self.open_after.as_mut() {
            let failure = &mut self.failures[open.index];
            if trimmed.starts_with("at ") {
                open.saw_stack = true;
                failure.last = n;
                if let Some((loc, in_repo)) = extract_location(trimmed) {
                    record_location(failure, &mut open.has_in_repo_loc, loc, in_repo);
                }
                return;
            }
            if starts_error || is_key_assertion_line(trimmed) {
                failure.last = n;
                if failure.error.is_empty() {
                    failure.error = short(trimmed, 220);
                } else {
                    push_excerpt_line(&mut failure.excerpt, trimmed);
                }
                return;
            }
        }

        if starts_error {
            let pending = self.pending.get_or_insert_with(|| PendingBunError {
                stream,
                first: n,
                last: n,
                error: String::new(),
                excerpt: Vec::new(),
                location: None,
                code_frame_line: None,
                has_in_repo_loc: false,
                saw_stack: false,
            });
            pending.last = n;
            if pending.error.is_empty() {
                pending.error = short(trimmed, 220);
            } else {
                push_excerpt_line(&mut pending.excerpt, trimmed);
            }
            return;
        }

        if let Some(pending) = self.pending.as_mut() {
            if trimmed.starts_with("at ") {
                pending.saw_stack = true;
                pending.last = n;
                if let Some((loc, in_repo)) = extract_location(trimmed)
                    && !pending.has_in_repo_loc
                    && (in_repo || pending.location.is_none())
                {
                    pending.location = Some(loc);
                    if in_repo {
                        pending.has_in_repo_loc = true;
                    }
                }
                return;
            }
            if !pending.saw_stack && is_key_assertion_line(trimmed) {
                pending.last = n;
                push_excerpt_line(&mut pending.excerpt, trimmed);
                return;
            }
            // Absorb unchanged structural lines inside Bun's `toEqual` object/array diff
            // (`  {`, `  "status": "ok",`, `  }`) so they don't pollute `other` notes.
            if !pending.saw_stack
                && !pending.error.is_empty()
                && (line.starts_with("  ") || matches!(trimmed, "{" | "}" | "[" | "]"))
            {
                pending.last = n;
                return;
            }
        }

        // Duplicate failure summary bullet `✗ ...` printed at end of some Bun runs.
        if trimmed.starts_with("✗ ") && !self.failures.is_empty() {
            self.status[idx].add(n);
            return;
        }

        if is_wrapper_header(trimmed) {
            self.status[idx].add(n);
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn flush_unclaimed(&mut self) {
        if let Some(pending) = self.pending.take()
            && !pending.error.is_empty()
            && self.failures.len() < FAILURE_LIMIT
        {
            let name = if !self.current_file.is_empty() {
                format!("{} › error", self.current_file)
            } else {
                "Test file error".into()
            };
            let location = pending.location.or_else(|| {
                pending
                    .code_frame_line
                    .filter(|_| !self.current_file.is_empty())
                    .map(|ln| format!("{}:{ln}", self.current_file))
            });
            self.failures.push(Failure {
                name: short(&name, 260),
                location,
                error: pending.error,
                excerpt: pending.excerpt,
                root_cause: String::new(),
                stderr: pending.stream == Stream::Stderr,
                first: pending.first,
                last: pending.last,
            });
        }
    }

    fn finish(mut self) -> Option<(usize, Report)> {
        let recognized =
            self.saw_ran || (self.saw_pass_fail && (self.saw_banner || self.saw_outcome));
        if !recognized {
            return None;
        }
        self.flush_unclaimed();
        if self.reported_fail > 0 && self.failures.is_empty() {
            return None;
        }
        for f in &mut self.failures {
            f.finalize();
        }
        let mut passed = Vec::new();
        push_passed_ranges(&self.passed, "passed test(s)", &mut passed);
        let (notes, stderr, other_count) = build_notes(
            &self.other,
            self.has_test_stderr,
            &self.status,
            "status line(s)",
        );
        Some((
            other_count,
            Report {
                runner: Runner::Bun,
                failures: self.failures,
                passed,
                totals: self.totals,
                notes,
                stderr,
            },
        ))
    }
}

fn is_bun_file_header(line: &str) -> bool {
    if line.starts_with([' ', '\t']) {
        return false;
    }
    let trimmed = line.trim();
    let Some(path) = trimmed.strip_suffix(':') else {
        return false;
    };
    !path.is_empty()
        && !path.contains(' ')
        && (path.contains('/')
            || path.contains('\\')
            || path.contains(".test.")
            || path.contains(".spec."))
        && [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"]
            .iter()
            .any(|ext| path.ends_with(ext))
}

// ---------------------------------------------------------------------------
// 4. Node `--test` Scanner (TAP 13 + spec reporter)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum YamlTarget {
    Passing,
    Failure(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum YamlBlock {
    None,
    Error,
    Stack,
}

struct NodeTapFailure {
    failure: Failure,
    is_subtests_failed: bool,
    err_name: String,
    err_code: String,
    err_lines: Vec<String>,
    actual: Option<String>,
    expected: Option<String>,
    yaml_location: Option<String>,
    has_in_repo_stack: bool,
}

impl NodeTapFailure {
    fn into_failure(mut self) -> Failure {
        let mut error = self
            .err_lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        if error.is_empty() && !self.err_name.is_empty() {
            error = self.err_name.clone();
        } else if !self.err_name.is_empty()
            && !error.starts_with(&self.err_name)
            && !error.starts_with("Error:")
        {
            let prefix = if self.err_code.is_empty() {
                format!("{}: ", self.err_name)
            } else {
                format!("{} [{}]: ", self.err_name, self.err_code)
            };
            error = format!("{prefix}{error}");
        }
        if self.failure.error.is_empty() && !error.is_empty() {
            self.failure.error = short(&error, 220);
        }
        if let Some(expected) = &self.expected
            && !self.failure.error.contains(expected.as_str())
            && self.failure.excerpt.len() < MAX_EXCERPT
        {
            self.failure
                .excerpt
                .push(short(&format!("Expected: {expected}"), 200));
        }
        if let Some(actual) = &self.actual
            && !self.failure.error.contains(actual.as_str())
            && self.failure.excerpt.len() < MAX_EXCERPT
        {
            self.failure
                .excerpt
                .push(short(&format!("Received: {actual}"), 200));
        }
        if self.failure.location.is_none() {
            self.failure.location = self.yaml_location;
        }
        self.failure.finalize();
        self.failure
    }
}

#[derive(Default)]
struct NodeTestScan {
    failures: Vec<NodeTapFailure>,
    suite_stack: Vec<(usize, String)>,
    last_subtest: Option<(usize, Stream)>,
    awaiting_yaml: Option<(YamlTarget, Stream)>,
    in_yaml: Option<(YamlTarget, Stream, usize, YamlBlock)>,
    // Spec reporter state (`✖ failing tests:`).
    in_spec_failures: bool,
    spec_pending_loc: Option<String>,
    spec_open: Option<OpenFailure>,
    saw_tests_total: bool,
    saw_pass_or_fail: bool,
    reported_fail: usize,
    passed: [Range; 2],
    status: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
}

impl NodeTestScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let idx = stream as usize;
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        // Stream switch terminates active YAML / spec blocks.
        if self.in_yaml.is_some_and(|(_, s, _, _)| s != stream) {
            self.in_yaml = None;
        }
        if self.awaiting_yaml.is_some_and(|(_, s)| s != stream) {
            self.awaiting_yaml = None;
        }
        if self.spec_open.as_ref().is_some_and(|o| o.stream != stream) {
            self.spec_open = None;
        }

        // Handle active TAP YAML block (`---` ... `...`).
        if let Some((target, ystream, yaml_indent, mut block)) = self.in_yaml {
            if trimmed.is_empty() {
                if let YamlTarget::Failure(fidx) = target {
                    self.failures[fidx].failure.last = n;
                } else {
                    self.passed[idx].extend_line(n);
                }
                return;
            }
            if trimmed == "..." && indent <= yaml_indent {
                if let YamlTarget::Failure(fidx) = target {
                    self.failures[fidx].failure.last = n;
                } else {
                    self.passed[idx].extend_line(n);
                }
                self.in_yaml = None;
                return;
            }
            // Guard against an unclosed YAML block if a top-level TAP line appears.
            if indent < yaml_indent
                && (TAP_OK.is_match(trimmed)
                    || NODE_TOTAL.is_match(trimmed)
                    || TAP_PLAN.is_match(trimmed)
                    || trimmed.starts_with("# Subtest:"))
            {
                self.in_yaml = None;
                // Fall through to parse this line normally.
            } else {
                match target {
                    YamlTarget::Passing => {
                        self.passed[idx].extend_line(n);
                    }
                    YamlTarget::Failure(fidx) => {
                        let entry = &mut self.failures[fidx];
                        entry.failure.last = n;
                        if block != YamlBlock::None && indent > yaml_indent {
                            match block {
                                YamlBlock::Error => {
                                    if entry.err_lines.len() < 4 {
                                        entry.err_lines.push(short(trimmed, 200));
                                    }
                                }
                                YamlBlock::Stack => {
                                    if let Some((loc, in_repo)) = extract_location(trimmed) {
                                        record_location(
                                            &mut entry.failure,
                                            &mut entry.has_in_repo_stack,
                                            loc,
                                            in_repo,
                                        );
                                    }
                                }
                                YamlBlock::None => {}
                            }
                            self.in_yaml = Some((target, ystream, yaml_indent, block));
                            return;
                        }
                        block = YamlBlock::None;
                        if let Some(val) = trimmed.strip_prefix("failureType:") {
                            if unquote_yaml(val) == "subtestsFailed" {
                                entry.is_subtests_failed = true;
                            }
                        } else if let Some(val) = trimmed.strip_prefix("location:") {
                            if let Some((loc, _)) = extract_location(&unquote_yaml(val)) {
                                entry.yaml_location = Some(loc);
                            }
                        } else if let Some(val) = trimmed.strip_prefix("name:") {
                            entry.err_name = unquote_yaml(val);
                        } else if let Some(val) = trimmed.strip_prefix("code:") {
                            entry.err_code = unquote_yaml(val);
                        } else if let Some(val) = trimmed.strip_prefix("expected:") {
                            let v = unquote_yaml(val);
                            if !v.is_empty() && v != "|-" && v != "|" {
                                entry.expected = Some(short(&v, 120));
                            }
                        } else if let Some(val) = trimmed.strip_prefix("actual:") {
                            let v = unquote_yaml(val);
                            if !v.is_empty() && v != "|-" && v != "|" {
                                entry.actual = Some(short(&v, 120));
                            }
                        } else if let Some(val) = trimmed.strip_prefix("error:") {
                            let v = val.trim();
                            if v == "|-" || v == "|" || v == ">-" || v == ">" {
                                block = YamlBlock::Error;
                            } else {
                                let unquoted = unquote_yaml(v);
                                if !unquoted.is_empty() {
                                    entry.err_lines.push(short(&unquoted, 200));
                                }
                            }
                        } else if let Some(val) = trimmed.strip_prefix("stack:") {
                            let v = val.trim();
                            if v == "|-" || v == "|" {
                                block = YamlBlock::Stack;
                            } else if let Some((loc, in_repo)) = extract_location(&unquote_yaml(v))
                            {
                                record_location(
                                    &mut entry.failure,
                                    &mut entry.has_in_repo_stack,
                                    loc,
                                    in_repo,
                                );
                            }
                        }
                        self.in_yaml = Some((target, ystream, yaml_indent, block));
                    }
                }
                return;
            }
        }

        if trimmed.is_empty() {
            return;
        }

        // Start of TAP YAML block right after `ok` / `not ok`.
        if trimmed == "---"
            && let Some((target, ystream)) = self.awaiting_yaml.take()
        {
            if let YamlTarget::Failure(fidx) = target {
                self.failures[fidx].failure.last = n;
            } else {
                self.passed[idx].extend_line(n);
            }
            self.in_yaml = Some((target, ystream, indent, YamlBlock::None));
            return;
        }
        self.awaiting_yaml = None;

        // TAP header & plan lines.
        if trimmed == "TAP version 13" || TAP_PLAN.is_match(trimmed) {
            self.status[idx].add(n);
            return;
        }

        // Totals lines (`# tests 10`, `# pass 9`, `# fail 1`, `ℹ tests 10`, ...).
        if let Some(cap) = NODE_TOTAL.captures(trimmed) {
            self.spec_open = None;
            match &cap[1] {
                "tests" => self.saw_tests_total = true,
                "pass" => self.saw_pass_or_fail = true,
                "fail" => {
                    self.saw_pass_or_fail = true;
                    self.reported_fail = cap[2].trim().parse().unwrap_or(0);
                }
                _ => {}
            }
            self.totals.push(short(trimmed, 220));
            return;
        }

        // TAP subtest heading (`# Subtest: Math`).
        if let Some(subtest) = trimmed.strip_prefix("# Subtest:").map(str::trim) {
            while self.suite_stack.last().is_some_and(|(d, _)| *d >= indent) {
                self.suite_stack.pop();
            }
            if !subtest.is_empty() {
                self.suite_stack.push((indent, subtest.to_owned()));
            }
            self.last_subtest = Some((n, stream));
            self.status[idx].add(n);
            return;
        }

        // TAP `ok N - ...` / `not ok N - ...`.
        if let Some(cap) = TAP_OK.captures(trimmed) {
            let failed = cap.get(1).is_some();
            let raw_name = cap.get(2).map_or("", |m| m.as_str()).trim();
            let subtest_start = self
                .last_subtest
                .take()
                .filter(|(_, s)| *s == stream)
                .map(|(line_no, _)| line_no);
            // Pop same-or-deeper subtest entries; remaining entries are ancestor suites.
            while self.suite_stack.last().is_some_and(|(d, _)| *d >= indent) {
                self.suite_stack.pop();
            }
            if !failed {
                if let Some(start) = subtest_start {
                    self.passed[idx].extend_line(start);
                }
                self.passed[idx].add(n);
                self.awaiting_yaml = Some((YamlTarget::Passing, stream));
            } else if self.failures.len() < FAILURE_LIMIT {
                let name = if self.suite_stack.is_empty() {
                    raw_name.to_owned()
                } else {
                    let prefix = self
                        .suite_stack
                        .iter()
                        .map(|(_, s)| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" › ");
                    if raw_name.is_empty() {
                        prefix
                    } else {
                        format!("{prefix} › {raw_name}")
                    }
                };
                let fidx = self.failures.len();
                self.failures.push(NodeTapFailure {
                    failure: Failure {
                        name: short(&name, 260),
                        location: None,
                        error: String::new(),
                        excerpt: Vec::new(),
                        root_cause: String::new(),
                        stderr: stream == Stream::Stderr,
                        first: subtest_start.unwrap_or(n),
                        last: n,
                    },
                    is_subtests_failed: false,
                    err_name: String::new(),
                    err_code: String::new(),
                    err_lines: Vec::new(),
                    actual: None,
                    expected: None,
                    yaml_location: None,
                    has_in_repo_stack: false,
                });
                self.awaiting_yaml = Some((YamlTarget::Failure(fidx), stream));
            }
            return;
        }

        // Node spec reporter: `✖ failing tests:` section.
        if trimmed == "✖ failing tests:" {
            self.in_spec_failures = true;
            self.spec_open = None;
            self.spec_pending_loc = None;
            self.status[idx].add(n);
            return;
        }
        if self.in_spec_failures {
            if let Some(rest) = trimmed.strip_prefix("test at ") {
                self.spec_open = None;
                self.spec_pending_loc = extract_location(rest).map(|(loc, _)| loc);
                return;
            }
            if let Some(rest) = trimmed.strip_prefix("✖ ") {
                let name = strip_trailing_timing(rest.trim());
                let pending_loc = self.spec_pending_loc.take();
                // Update an existing spec tree failure if present, else push new.
                let fidx =
                    if let Some(pos) = self.failures.iter().position(|f| f.failure.name == name) {
                        let f = &mut self.failures[pos];
                        f.failure.first = n;
                        f.failure.last = n;
                        f.failure.stderr = stream == Stream::Stderr;
                        if f.yaml_location.is_none() {
                            f.yaml_location = pending_loc;
                        }
                        pos
                    } else if self.failures.len() < FAILURE_LIMIT {
                        let pos = self.failures.len();
                        self.failures.push(NodeTapFailure {
                            failure: Failure {
                                name: short(name, 260),
                                location: None,
                                error: String::new(),
                                excerpt: Vec::new(),
                                root_cause: String::new(),
                                stderr: stream == Stream::Stderr,
                                first: n,
                                last: n,
                            },
                            is_subtests_failed: false,
                            err_name: String::new(),
                            err_code: String::new(),
                            err_lines: Vec::new(),
                            actual: None,
                            expected: None,
                            yaml_location: pending_loc,
                            has_in_repo_stack: false,
                        });
                        pos
                    } else {
                        return;
                    };
                self.spec_open = Some(OpenFailure {
                    index: fidx,
                    stream,
                    has_in_repo_loc: false,
                    saw_stack: false,
                });
                return;
            }
            if let Some(open) = self.spec_open.as_mut() {
                let entry = &mut self.failures[open.index];
                entry.failure.last = n;
                if trimmed.starts_with("at ") {
                    open.saw_stack = true;
                    if let Some((loc, in_repo)) = extract_location(trimmed) {
                        record_location(
                            &mut entry.failure,
                            &mut open.has_in_repo_loc,
                            loc,
                            in_repo,
                        );
                    }
                    return;
                }
                if !open.saw_stack {
                    if entry.failure.error.is_empty() {
                        entry.failure.error = short(trimmed, 220);
                    } else if entry.failure.excerpt.len() < MAX_EXCERPT {
                        entry.failure.excerpt.push(short(trimmed, 200));
                    }
                }
                return;
            }
        }

        // Spec tree lines (`▶ suite`, `✔ test (0.4ms)`, `✖ test (1.2ms)`).
        if trimmed.starts_with("▶ ") {
            self.status[idx].add(n);
            return;
        }
        if trimmed.starts_with("✔ ") && trimmed.ends_with("ms)") {
            self.passed[idx].add(n);
            return;
        }
        if let Some(rest) = trimmed.strip_prefix("✖ ")
            && trimmed.ends_with("ms)")
        {
            if self.failures.len() < FAILURE_LIMIT {
                let name = strip_trailing_timing(rest.trim());
                if !self.failures.iter().any(|f| f.failure.name == name) {
                    self.failures.push(NodeTapFailure {
                        failure: Failure {
                            name: short(name, 260),
                            location: None,
                            error: String::new(),
                            excerpt: Vec::new(),
                            root_cause: String::new(),
                            stderr: stream == Stream::Stderr,
                            first: n,
                            last: n,
                        },
                        is_subtests_failed: false,
                        err_name: String::new(),
                        err_code: String::new(),
                        err_lines: Vec::new(),
                        actual: None,
                        expected: None,
                        yaml_location: None,
                        has_in_repo_stack: false,
                    });
                }
            }
            return;
        }

        if is_wrapper_header(trimmed) {
            self.status[idx].add(n);
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(self) -> Option<(usize, Report)> {
        if !(self.saw_tests_total && self.saw_pass_or_fail) {
            return None;
        }
        let has_leaf = self.failures.iter().any(|f| !f.is_subtests_failed);
        let failures: Vec<Failure> = self
            .failures
            .into_iter()
            .filter(|f| !has_leaf || !f.is_subtests_failed)
            .map(NodeTapFailure::into_failure)
            .collect();
        if self.reported_fail > 0 && failures.is_empty() {
            return None;
        }
        let mut passed = Vec::new();
        push_passed_ranges(&self.passed, "passed test(s)", &mut passed);
        let (notes, stderr, other_count) =
            build_notes(&self.other, false, &self.status, "status line(s)");
        Some((
            other_count,
            Report {
                runner: Runner::NodeTest,
                failures,
                passed,
                totals: self.totals,
                notes,
                stderr,
            },
        ))
    }
}

fn unquote_yaml(val: &str) -> String {
    let v = val.trim();
    if let Some(inner) = v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        inner.replace("''", "'")
    } else if let Some(inner) = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        inner.to_owned()
    } else {
        v.to_owned()
    }
}

// ---------------------------------------------------------------------------
// 5. Playwright Scanner
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PwSummarySection {
    Failed,
    Flaky,
    Other,
}

#[derive(Default)]
struct PlaywrightScan {
    failures: Vec<Failure>,
    open: Option<OpenFailure>,
    in_call_log: bool,
    summary_section: Option<PwSummarySection>,
    saw_marker: bool,
    saw_totals: bool,
    reported_fail: usize,
    saw_global_error_note: bool,
    passed: [Range; 2],
    progress_lines: [Range; 2],
    status: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
}

impl PlaywrightScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            self.in_call_log = false;
            return;
        }
        let idx = stream as usize;
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
            self.in_call_log = false;
        }

        if PW_RUNNING.is_match(trimmed) {
            self.saw_marker = true;
            self.status[idx].add(n);
            return;
        }

        // Global non-test error marker -> fail closed if no failure captured.
        if trimmed.contains("was not a part of any test")
            || trimmed.contains("were not a part of any test")
        {
            self.saw_global_error_note = true;
            self.other[idx].observe(line, n);
            return;
        }

        // Totals lines at the end of a Playwright run.
        if PW_TOTAL_PASSED.is_match(trimmed) {
            self.open = None;
            self.in_call_log = false;
            self.summary_section = None;
            self.saw_totals = true;
            self.totals.push(short(trimmed, 220));
            return;
        }
        if let Some(cap) = PW_TOTAL_COUNT.captures(trimmed) {
            self.open = None;
            self.in_call_log = false;
            self.saw_totals = true;
            let count = cap[1].parse::<usize>().unwrap_or(0);
            self.summary_section = match &cap[2] {
                "failed" => {
                    self.reported_fail = self.reported_fail.max(count);
                    Some(PwSummarySection::Failed)
                }
                "flaky" => Some(PwSummarySection::Flaky),
                _ => Some(PwSummarySection::Other),
            };
            self.totals.push(short(trimmed, 220));
            return;
        }

        // Summary section recap lines (`[chromium] › file:line:col › test ───`).
        if let Some(section) = self.summary_section
            && trimmed.contains(" › ")
        {
            let clean = strip_box_trailing(trimmed);
            match section {
                PwSummarySection::Flaky => {
                    self.totals.push(short(&format!("  flaky: {clean}"), 220));
                }
                PwSummarySection::Failed | PwSummarySection::Other => {
                    self.status[idx].add(n);
                }
            }
            return;
        }

        // HTML report hint after totals.
        if trimmed.starts_with("To open last HTML report run:")
            || trimmed == "npx playwright show-report"
            || trimmed.starts_with("Slow test file:")
        {
            self.open = None;
            self.summary_section = None;
            self.status[idx].add(n);
            return;
        }

        // Detailed failure header: `1) [chromium] › tests/auth.spec.ts:24:5 › Auth › test ───`.
        if let Some(cap) = PW_FAILURE_HEADER.captures(trimmed)
            && cap[2].contains(" › ")
        {
            self.open = None;
            self.in_call_log = false;
            self.summary_section = None;
            self.saw_marker = true;
            if self.failures.len() >= FAILURE_LIMIT {
                return;
            }
            let header = strip_box_trailing(cap[2].trim());
            let header_loc = extract_location(header).map(|(loc, _)| loc);
            let failure_idx = self.failures.len();
            self.failures.push(Failure {
                name: short(header, 260),
                location: header_loc,
                error: String::new(),
                excerpt: Vec::new(),
                root_cause: String::new(),
                stderr: stream == Stream::Stderr,
                first: n,
                last: n,
            });
            self.open = Some(OpenFailure {
                index: failure_idx,
                stream,
                has_in_repo_loc: false,
                saw_stack: false,
            });
            return;
        }

        // Body of an open Playwright failure block.
        if let Some(open) = self.open.as_mut() {
            let failure = &mut self.failures[open.index];
            failure.last = n;
            if trimmed.starts_with("Retry #") {
                // Keep Attempt 1 details cleanly without duplicating them on subsequent retries;
                // if Attempt 1 lacked an error line, allow the retry to populate it.
                self.in_call_log = false;
                open.saw_stack = !failure.error.is_empty();
                return;
            }
            if trimmed == "Call log:" {
                self.in_call_log = true;
                return;
            }
            if self.in_call_log {
                if trimmed.starts_with("at ")
                    || is_code_frame_line(line)
                    || trimmed.starts_with("Error Context:")
                    || trimmed.starts_with("attachment #")
                    || trimmed.starts_with("──")
                    || trimmed.starts_with("⎯⎯")
                {
                    self.in_call_log = false;
                } else {
                    return;
                }
            }
            if is_code_frame_line(line)
                || trimmed.starts_with("Error Context:")
                || trimmed.starts_with("attachment #")
                || trimmed.starts_with("──")
                || trimmed.starts_with("⎯⎯")
            {
                return;
            }
            if trimmed.starts_with("at ") {
                open.saw_stack = true;
                if let Some((loc, in_repo)) = extract_location(trimmed) {
                    record_location(failure, &mut open.has_in_repo_loc, loc, in_repo);
                }
                return;
            }
            if !open.saw_stack {
                if failure.error.is_empty() {
                    failure.error = short(trimmed, 220);
                } else {
                    push_excerpt_line(&mut failure.excerpt, trimmed);
                }
            }
            return;
        }

        // List/line/dot reporter progress lines.
        if PW_LIST_LINE.is_match(trimmed) && trimmed.contains(" › ") {
            self.saw_marker = true;
            if trimmed.starts_with(['✓', '✔']) {
                self.passed[idx].add(n);
            } else {
                self.status[idx].add(n);
            }
            return;
        }
        if PW_LINE_PROGRESS.is_match(trimmed) && trimmed.contains(" › ") {
            self.saw_marker = true;
            self.passed[idx].add(n);
            return;
        }
        if self.saw_marker
            && trimmed
                .chars()
                .all(|c| matches!(c, '.' | '·' | 'F' | '×' | 'x' | '°' | '±' | '-' | 'T' | 'I'))
        {
            self.progress_lines[idx].add(n);
            return;
        }

        if is_wrapper_header(trimmed) {
            self.status[idx].add(n);
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(mut self) -> Option<(usize, Report)> {
        if !(self.saw_marker && self.saw_totals) {
            return None;
        }
        if (self.reported_fail > 0 || self.saw_global_error_note) && self.failures.is_empty() {
            return None;
        }
        for f in &mut self.failures {
            f.finalize();
        }
        let mut passed = Vec::new();
        push_passed_ranges(&self.passed, "passed test(s)", &mut passed);
        push_passed_ranges(&self.progress_lines, "progress line(s)", &mut passed);
        let (notes, stderr, other_count) =
            build_notes(&self.other, false, &self.status, "status line(s)");
        Some((
            other_count,
            Report {
                runner: Runner::Playwright,
                failures: self.failures,
                passed,
                totals: self.totals,
                notes,
                stderr,
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(streams: &[(&str, Stream)]) -> Option<Report> {
        let mut s = Scan::default();
        for (text, stream) in streams {
            for (i, line) in text.lines().enumerate() {
                s.observe(line, i + 1, *stream);
            }
        }
        s.finish()
    }

    #[test]
    fn jest_passing_stderr_compacts_suites_and_preserves_totals() {
        let mut err = String::new();
        for i in 1..=80 {
            err.push_str(&format!("PASS src/mod_{i:03}.test.ts (0.12 s)\n"));
        }
        err.push_str(
            "\nTest Suites: 80 passed, 80 total\n\
             Tests:       420 passed, 420 total\n\
             Snapshots:   4 total\n\
             Time:        3.42 s\n\
             Ran all test suites.\n",
        );
        let report = scan(&[("", Stream::Stdout), (&err, Stream::Stderr)]).unwrap();
        assert_eq!(report.runner, Runner::Jest);
        assert!(report.stderr);
        assert!(report.failures.is_empty());
        let rendered = report.render();
        assert!(
            rendered.contains("jest: 80 passed suite(s) [stderr:1-80]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Test Suites: 80 passed, 80 total"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Tests:       420 passed, 420 total"),
            "{rendered}"
        );
        assert!(rendered.contains("Snapshots:   4 total"), "{rendered}");
        assert!(rendered.contains("Time:        3.42 s"), "{rendered}");
        assert!(
            rendered.len() * 10 < err.len(),
            "expected >90% reduction: {rendered}"
        );
    }

    #[test]
    fn jest_failing_groups_shared_root_cause_dedups_summary_and_keeps_console_errors() {
        let out =
            "  console.error\n    Pool exhausted on shard-3\n\n      at pool (src/db.ts:9:5)\n";
        let err = "\
PASS src/ok.test.ts
FAIL src/billing.test.ts
  ● Billing › calculates subtotal

    TypeError: Cannot read properties of undefined (reading 'rate')

      22 |   return amount * cfg.rate;
    > 23 |   throw new Error();
         |   ^

      at Promise.then (node_modules/jest-circus/build/utils.js:298:28)
      at calcTax (src/pricing.ts:23:28)
      at Object.<anonymous> (src/billing.test.ts:14:19)

  ● Billing › calculates grand total

    TypeError: Cannot read properties of undefined (reading 'rate')

      at calcTax (src/pricing.ts:23:28)
      at Object.<anonymous> (src/billing.test.ts:28:19)

  ● Billing › applies discount

    expect(received).toBe(expected) // Object.is equality

    Expected: 90
    Received: 100

      35 |     expect(discount(100)).toBe(90);
         |                           ^

      at Object.toBe (src/billing.test.ts:35:27)

Summary of all failing tests
FAIL src/billing.test.ts
  ● Billing › calculates subtotal

    TypeError: Cannot read properties of undefined (reading 'rate')

      at calcTax (src/pricing.ts:23:28)

Test Suites: 1 failed, 1 passed, 2 total
Tests:       3 failed, 10 passed, 13 total
Time:        1.42 s
Ran all test suites.
";
        let report = scan(&[(out, Stream::Stdout), (err, Stream::Stderr)]).unwrap();
        assert_eq!(report.runner, Runner::Jest);
        assert_eq!(report.failures.len(), 3, "{:?}", report.failures);
        // Skips node_modules frame and picks first in-repo frame.
        assert_eq!(
            report.failures[0].location.as_deref(),
            Some("src/pricing.ts:23:28")
        );
        let rendered = report.render();
        assert!(
            rendered.contains("3 reported failing test(s)"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "2 test(s): TypeError: Cannot read properties of undefined (reading 'rate') · src/pricing.ts:23:28"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("  Billing › calculates subtotal [stderr:3-13]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  Billing › calculates grand total [stderr:15-20]"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "1 test(s): Billing › applies discount · src/billing.test.ts:35:27 [stderr:22-32]"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("  Expected: 90\n  Received: 100"),
            "{rendered}"
        );
        assert!(rendered.contains("Pool exhausted on shard-3"), "{rendered}");
    }

    #[test]
    fn vitest_split_stdout_and_stderr_extracts_failures_locations_and_totals() {
        let mut out = String::from(" RUN  v2.1.8 /repo/web\n\n");
        for i in 1..=50 {
            out.push_str(&format!(" ✓ src/mod_{i:02}.test.ts (4 tests) 5ms\n"));
        }
        out.push_str(
            " ❯ src/billing.test.ts (3 tests | 1 failed) 18ms\n\
               × Billing > calculates tax 6ms\n\
                 → expected 110 to be 120 // Object.is equality\n\n\
             Test Files  1 failed | 50 passed (51)\n\
                  Tests  1 failed | 202 passed (203)\n\
               Start at  19:40:12\n\
               Duration  1.12s (transform 80ms, tests 240ms)\n",
        );
        let err = "\
⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯

 FAIL  src/billing.test.ts > Billing > calculates tax
AssertionError: expected 110 to be 120 // Object.is equality

- Expected
+ Received

- 120
+ 110

 ❯ src/billing.test.ts:14:19
     12|   test('calculates tax', () => {
     13|     const total = calcTax(100);
     14|     expect(total).toBe(120);
       |                   ^
     15|   });

⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[1/1]⎯
";
        let report = scan(&[(&out, Stream::Stdout), (err, Stream::Stderr)]).unwrap();
        assert_eq!(report.runner, Runner::Vitest);
        assert!(report.stderr);
        assert_eq!(report.failures.len(), 1);
        let rendered = report.render();
        assert!(
            rendered.contains("1 reported failing test(s)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("src/billing.test.ts > Billing > calculates tax · src/billing.test.ts:14:19 [stderr:3-17]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("AssertionError: expected 110 to be 120 // Object.is equality"),
            "{rendered}"
        );
        assert!(
            rendered.contains("vitest: 50 passed test file(s) [stdout:3-52]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Test Files  1 failed | 50 passed (51)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Tests  1 failed | 202 passed (203)"),
            "{rendered}"
        );
    }

    #[test]
    fn bun_test_captures_pre_fail_errors_and_compacts_passes() {
        let mut err = String::from("bun test v1.1.38 (abcdef12)\n\nsrc/math.test.ts:\n");
        for i in 1..=60 {
            err.push_str(&format!("(pass) Math > case {i:02} [0.12ms]\n"));
        }
        err.push_str(
            "\nsrc/billing.test.ts:\n\
             12 |   test(\"charges tax\", () => {\n\
             14 |     expect(tax(100)).toBe(120);\n\
                                   ^\n\
             error: expect(received).toBe(expected)\n\n\
             Expected: 120\n\
             Received: 110\n\n\
                   at <anonymous> (src/billing.test.ts:14:21)\n\
             (fail) Billing > charges tax [1.05ms]\n\n\
              60 pass\n\
              1 fail\n\
              61 expect() calls\n\
             Ran 61 tests across 2 files. [42.10ms]\n",
        );
        let report = scan(&[("", Stream::Stdout), (&err, Stream::Stderr)]).unwrap();
        assert_eq!(report.runner, Runner::Bun);
        assert!(report.stderr);
        assert_eq!(report.failures.len(), 1);
        let rendered = report.render();
        assert!(
            rendered.contains(
                "1 test(s): src/billing.test.ts › Billing > charges tax · src/billing.test.ts:14:21 [stderr:66-75]"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("  error: expect(received).toBe(expected)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  Expected: 120\n  Received: 110"),
            "{rendered}"
        );
        assert!(
            rendered.contains("bun test: 60 passed test(s) [stderr:4-63]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Ran 61 tests across 2 files. [42.10ms]"),
            "{rendered}"
        );
    }

    #[test]
    fn node_test_tap_suppresses_passing_yaml_and_omits_wrapper_subtests_failed() {
        let mut out = String::from("TAP version 13\n# Subtest: Math\n");
        for i in 1..=40 {
            out.push_str(&format!(
                "    # Subtest: case {i}\n    ok {i} - case {i}\n      ---\n      duration_ms: 0.31\n      type: 'test'\n      ...\n"
            ));
        }
        out.push_str(concat!(
            "    # Subtest: divides safely\n",
            "    not ok 41 - divides safely\n",
            "      ---\n",
            "      duration_ms: 1.42\n",
            "      type: 'test'\n",
            "      location: '/repo/test/math.test.js:50:3'\n",
            "      failureType: 'testCodeFailure'\n",
            "      error: |-\n",
            "        Expected values to be strictly equal:\n",
            "\n",
            "        3 !== 4\n",
            "      code: 'ERR_ASSERTION'\n",
            "      name: 'AssertionError'\n",
            "      expected: 4\n",
            "      actual: 3\n",
            "      operator: 'strictEqual'\n",
            "      stack: |-\n",
            "        TestContext.<anonymous> (/repo/test/math.test.js:51:10)\n",
            "        Test.runInAsyncScope (node:async_hooks:206:9)\n",
            "      ...\n",
            "    1..41\n",
            "not ok 1 - Math\n",
            "  ---\n",
            "  duration_ms: 18.2\n",
            "  type: 'suite'\n",
            "  location: '/repo/test/math.test.js:3:1'\n",
            "  failureType: 'subtestsFailed'\n",
            "  error: '1 subtest failed'\n",
            "  code: 'ERR_TEST_FAILURE'\n",
            "  ...\n",
            "1..1\n",
            "# tests 41\n",
            "# suites 1\n",
            "# pass 40\n",
            "# fail 1\n",
            "# cancelled 0\n",
            "# skipped 0\n",
            "# todo 0\n",
            "# duration_ms 54.12\n",
        ));
        let report = scan(&[(&out, Stream::Stdout)]).unwrap();
        assert_eq!(report.runner, Runner::NodeTest);
        // Only the leaf failure is reported; the parent `subtestsFailed` wrapper is omitted.
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
        assert_eq!(report.failures[0].name, "Math › divides safely");
        assert_eq!(
            report.failures[0].location.as_deref(),
            Some("/repo/test/math.test.js:51:10")
        );
        let rendered = report.render();
        assert!(
            rendered.contains("1 reported failing test(s)"),
            "{rendered}"
        );
        assert!(rendered.contains("Math › divides safely"), "{rendered}");
        assert!(rendered.contains("3 !== 4"), "{rendered}");
        assert!(
            rendered.contains("node --test: 40 passed test(s) [stdout:3-242]"),
            "{rendered}"
        );
        assert!(rendered.contains("# tests 41"), "{rendered}");
        assert!(rendered.contains("# fail 1"), "{rendered}");
        assert!(!rendered.contains("duration_ms: 0.31"), "{rendered}");
        assert!(
            rendered.len() * 6 < out.len(),
            "expected >80% reduction: {rendered}"
        );
    }

    #[test]
    fn playwright_groups_multi_browser_failures_and_keeps_flaky_and_totals() {
        let mut out = String::from("Running 40 tests using 4 workers\n\n");
        for i in 1..=37 {
            out.push_str(&format!(
                "  ✓  {i} [chromium] › tests/home.spec.ts:{i}:5 › Home › test {i} (180ms)\n"
            ));
        }
        out.push_str(
            "  ✘  38 [chromium] › tests/auth.spec.ts:24:5 › Auth › rejects bad password (5.2s)\n\
               ✘  39 [firefox] › tests/auth.spec.ts:24:5 › Auth › rejects bad password (5.1s)\n\n\
               1) [chromium] › tests/auth.spec.ts:24:5 › Auth › rejects bad password ───────────────────\n\n\
                 Error: expect(locator).toBeVisible() failed\n\n\
                 Locator:  getByText('Invalid credentials')\n\
                 Expected: visible\n\
                 Received: hidden\n\
                 Timeout:  5000ms\n\n\
                 Call log:\n\
                   - Expect \"toBeVisible\" with timeout 5000ms\n\
                   - waiting for getByText('Invalid credentials')\n\n\
                   25 |     await page.fill('#password', 'wrong');\n\
                 > 27 |     await expect(page.getByText('Invalid credentials')).toBeVisible();\n\
                      |                                                         ^\n\n\
                     at /repo/tests/auth.spec.ts:27:57\n\n\
               2) [firefox] › tests/auth.spec.ts:24:5 › Auth › rejects bad password ────────────────────\n\n\
                 Error: expect(locator).toBeVisible() failed\n\n\
                 Locator:  getByText('Invalid credentials')\n\
                 Expected: visible\n\
                 Received: hidden\n\
                 Timeout:  5000ms\n\n\
                     at /repo/tests/auth.spec.ts:27:57\n\n\
               2 failed\n\
                 [chromium] › tests/auth.spec.ts:24:5 › Auth › rejects bad password ────────────────────\n\
                 [firefox] › tests/auth.spec.ts:24:5 › Auth › rejects bad password ─────────────────────\n\
               1 flaky\n\
                 [chromium] › tests/cart.spec.ts:10:5 › Cart › updates badge ───────────────────────────\n\
               37 passed (18.4s)\n",
        );
        let report = scan(&[(&out, Stream::Stdout)]).unwrap();
        assert_eq!(report.runner, Runner::Playwright);
        assert_eq!(report.failures.len(), 2);
        let rendered = report.render();
        assert!(
            rendered.contains("2 reported failing test(s)"),
            "{rendered}"
        );
        // Both browsers share the exact same root cause + assertion line, so they group!
        assert!(
            rendered.contains(
                "2 test(s): Error: expect(locator).toBeVisible() failed · /repo/tests/auth.spec.ts:27:57"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("  Locator:  getByText('Invalid credentials')"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[chromium] › tests/auth.spec.ts:24:5 › Auth › rejects bad password"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[firefox] › tests/auth.spec.ts:24:5 › Auth › rejects bad password"),
            "{rendered}"
        );
        assert!(
            rendered.contains("playwright: 37 passed test(s) [stdout:3-39]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("flaky: [chromium] › tests/cart.spec.ts:10:5 › Cart › updates badge"),
            "{rendered}"
        );
        assert!(rendered.contains("37 passed (18.4s)"), "{rendered}");
    }

    #[test]
    fn unrecognized_json_list_or_crash_output_fails_closed() {
        for sample in [
            // Jest --json output
            "{\"numFailedTestSuites\":0,\"numPassedTestSuites\":1,\"success\":true}\n",
            // Playwright --list output
            "Listing tests:\n  [chromium] › tests/auth.spec.ts:3:5 › login\nTotal: 1 test in 1 file\n",
            // Crash before tests run
            "Error: Cannot find module './jest.config.js'\n    at Function.Module._resolveFilename (node:internal/modules/cjs/loader:1145:15)\n",
            // Custom reporter / arbitrary stdout
            "Running custom check...\nAll 12 checks succeeded!\n",
            // Jest totals reporting failures without any failure blocks
            "Test Suites: 1 failed, 1 total\nTests:       1 failed, 1 total\n",
            // Playwright non-test error with no captured failure blocks
            "Running 1 test using 1 worker\nSyntaxError: Unexpected token\n1 error was not a part of any test, see above for details\n",
        ] {
            assert!(
                scan(&[(sample, Stream::Stdout)]).is_none(),
                "expected None for:\n{sample}"
            );
            assert!(
                scan(&[("", Stream::Stdout), (sample, Stream::Stderr)]).is_none(),
                "expected None on stderr for:\n{sample}"
            );
        }
    }

    #[test]
    fn node_spec_reporter_vitest_unhandled_and_jest_suite_load_errors_are_captured() {
        let node_spec = "\
▶ Math
  ✔ adds numbers (0.42ms)
  ✖ subtracts numbers (1.83ms)
ℹ tests 2
ℹ suites 1
ℹ pass 1
ℹ fail 1
ℹ cancelled 0
ℹ skipped 0
ℹ todo 0
ℹ duration_ms 12.40

✖ failing tests:

test at test/math.test.js:12:3
✖ subtracts numbers (1.83ms)
  AssertionError [ERR_ASSERTION]: 3 !== 4
    at TestContext.<anonymous> (test/math.test.js:13:10)
";
        let report = scan(&[(node_spec, Stream::Stdout)]).unwrap();
        assert_eq!(report.runner, Runner::NodeTest);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].name, "subtracts numbers");
        assert_eq!(
            report.failures[0].location.as_deref(),
            Some("test/math.test.js:13:10")
        );
        assert!(report.failures[0].error.contains("3 !== 4"));

        let vitest_unhandled = "\
 RUN  v2.1.8 /repo/web
 ✓ src/ok.test.ts (2 tests) 4ms
 Test Files  1 passed (1)
      Tests  2 passed (2)
     Errors  1 error
   Duration  120ms
";
        let vitest_err = "\
⎯⎯⎯⎯⎯⎯ Unhandled Errors ⎯⎯⎯⎯⎯⎯

TypeError: Cannot read properties of null (reading 'close')
 ❯ teardown src/db.ts:88:12
";
        let report = scan(&[
            (vitest_unhandled, Stream::Stdout),
            (vitest_err, Stream::Stderr),
        ])
        .unwrap();
        assert_eq!(report.runner, Runner::Vitest);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].name, "Unhandled Error");
        assert_eq!(
            report.failures[0].location.as_deref(),
            Some("src/db.ts:88:12")
        );

        let jest_suite_err = "\
FAIL src/broken.test.ts
  ● Test suite failed to run

    Cannot find module './missing' from 'src/broken.test.ts'

      at Resolver._throwModNotFoundError (node_modules/jest-resolve/build/index.js:863:11)
      at Object.<anonymous> (src/broken.test.ts:1:1)

Test Suites: 1 failed, 1 total
Tests:       0 total
Time:        0.51 s
";
        let report = scan(&[("", Stream::Stdout), (jest_suite_err, Stream::Stderr)]).unwrap();
        assert_eq!(report.runner, Runner::Jest);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0].name,
            "src/broken.test.ts › Test suite failed to run"
        );
        assert_eq!(
            report.failures[0].location.as_deref(),
            Some("src/broken.test.ts:1:1")
        );

        // Playwright dot reporter labels dot lines as progress lines, not passed tests.
        let pw_dot = format!(
            "Running 120 tests using 4 workers\n{}\n{}\n  120 passed (9.2s)\n",
            ".".repeat(80),
            ".".repeat(40)
        );
        let report = scan(&[(&pw_dot, Stream::Stdout)]).unwrap();
        assert_eq!(report.runner, Runner::Playwright);
        let rendered = report.render();
        assert!(
            rendered.contains("playwright: 2 progress line(s) [stdout:2-3]"),
            "{rendered}"
        );
        assert!(rendered.contains("120 passed (9.2s)"), "{rendered}");

        // `summary::family` selects `"playwright test"` only for display invocations.
        let args = |cmd: &str| {
            cmd.split(' ')
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            crate::summary::family(&args("playwright test")),
            "playwright test"
        );
        assert_eq!(
            crate::summary::family(&args("npx playwright test --project=chromium")),
            "playwright test"
        );
        assert_eq!(
            crate::summary::family(&args("playwright test --reporter=list")),
            "playwright test"
        );
        assert_eq!(
            crate::summary::family(&args("playwright test --reporter=json")),
            "other"
        );
        assert_eq!(
            crate::summary::family(&args("playwright test --reporter junit")),
            "other"
        );
        assert_eq!(
            crate::summary::family(&args("playwright test --list")),
            "other"
        );
    }

    #[test]
    fn vitest_v3_playwright_and_bun_preserve_locations_expected_received_diffs_and_ansi_parity() {
        // Zero-alloc ANSI fast path check.
        assert!(matches!(strip_ansi("PASS src/a.test.ts"), Cow::Borrowed(_)));
        assert!(matches!(
            strip_ansi("\x1b[32mPASS\x1b[0m src/a.test.ts"),
            Cow::Owned(ref s) if s == "PASS src/a.test.ts"
        ));

        // 1. Vitest v3 with large object diff (many unchanged fields before -/+ diff) + ANSI colors.
        let vitest_out = "\
 RUN  v3.0.5 /repo/web

 ✓ src/ok.test.ts (8 tests) 4ms
 Test Files  1 failed | 1 passed (2)
      Tests  1 failed | 8 passed (9)
   Duration  310ms
";
        let vitest_err_plain = "\
⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯

 FAIL  src/invoice.test.ts > Invoice > computes grand total 8ms
AssertionError: expected { total: 110, ... } to deeply equal { total: 120, ... }
 ❯ src/invoice.test.ts:42:19
     40|   const res = computeInvoice(cart);
     41|   expect(res).toEqual(expected);
       |               ^
- Expected
+ Received

  Object {
    \"currency\": \"USD\",
    \"customerId\": \"cus_123\",
    \"discount\": 0,
    \"region\": \"us-east-1\",
    \"subtotal\": 100,
-   \"total\": 120,
+   \"total\": 110,
  }

⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[1/1]⎯
";
        let vitest_err_ansi = vitest_err_plain
            .replace(" FAIL ", "\x1b[31m FAIL \x1b[39m")
            .replace("- Expected", "\x1b[32m- Expected\x1b[39m")
            .replace("+ Received", "\x1b[31m+ Received\x1b[39m");

        let v_plain = scan(&[
            (vitest_out, Stream::Stdout),
            (vitest_err_plain, Stream::Stderr),
        ])
        .expect("vitest v3 plain")
        .render();
        let v_ansi = scan(&[
            (vitest_out, Stream::Stdout),
            (&vitest_err_ansi, Stream::Stderr),
        ])
        .expect("vitest v3 ansi")
        .render();
        assert_eq!(v_plain, v_ansi);
        assert!(v_plain.contains("src/invoice.test.ts:42:19"), "{v_plain}");
        assert!(v_plain.contains("- Expected"), "{v_plain}");
        assert!(v_plain.contains("+ Received"), "{v_plain}");
        assert!(v_plain.contains("-   \"total\": 120,"), "{v_plain}");
        assert!(v_plain.contains("+   \"total\": 110,"), "{v_plain}");

        // 2. Playwright with modern Call log (`8 × locator resolved...`) + Retry #1 + Expected/Received.
        let pw_out = "\
Running 12 tests using 4 workers

  1) [chromium] › tests/nav.spec.ts:18:5 › Nav › opens dashboard ──────────────────────

    Error: expect(locator).toHaveText(expected) failed

    Locator:  getByRole('heading')
    Expected string: \"Dashboard\"
    Received string: \"Login\"
    Timeout:  5000ms

    Call log:
      - expect.toHaveText with timeout 5000ms
      - waiting for getByRole('heading')
        8 × locator resolved to <h1>Login</h1>
          - unexpected value \"Login\"

      23 |   await page.goto('/dashboard');
    > 25 |   await expect(page.getByRole('heading')).toHaveText('Dashboard');
         |                                           ^

        at /repo/tests/nav.spec.ts:25:43

    Retry #1 ──────────────────────────────────────────────────────────────────────────

    Error: expect(locator).toHaveText(expected) failed

    Locator:  getByRole('heading')
    Expected string: \"Dashboard\"
    Received string: \"Login\"

  1 failed
    [chromium] › tests/nav.spec.ts:18:5 › Nav › opens dashboard ───────────────────────
  11 passed (6.1s)
";
        let pw_report = scan(&[(pw_out, Stream::Stdout)]).expect("playwright report");
        let pw_rendered = pw_report.render();
        assert!(
            pw_rendered.contains("/repo/tests/nav.spec.ts:25:43"),
            "{pw_rendered}"
        );
        assert!(
            pw_rendered.contains("Expected string: \"Dashboard\""),
            "{pw_rendered}"
        );
        assert!(
            pw_rendered.contains("Received string: \"Login\""),
            "{pw_rendered}"
        );
        assert!(
            !pw_rendered.contains("locator resolved"),
            "Call log retry chatter must not leak into excerpt:\n{pw_rendered}"
        );
        assert_eq!(
            pw_rendered
                .matches("Expected string: \"Dashboard\"")
                .count(),
            1,
            "Retry #1 must not duplicate Expected/Received lines:\n{pw_rendered}"
        );

        // 3. Bun `toEqual` object diff without `at` frame falls back to code-frame `file:line`
        //    and keeps diff lines crisp without polluting `other` notes.
        let bun_err = "\
bun test v1.2.4 (abcdef12)

src/cart.test.ts:
(pass) Cart > empty cart [0.10ms]
14 |     expect(res).toEqual({ total: 120, status: \"ok\" });
                     ^
\x1b[31merror\x1b[0m: expect(received).toEqual(expected)

  {
-   \"total\": 120,
+   \"total\": 110,
    \"status\": \"ok\",
  }

- Expected  - 1
+ Received  + 1

(fail) Cart > applies tax [1.20ms]

 1 pass
 1 fail
Ran 2 tests across 1 file. [14.2ms]
";
        let bun_report =
            scan(&[("", Stream::Stdout), (bun_err, Stream::Stderr)]).expect("bun report");
        assert_eq!(
            bun_report.failures[0].location.as_deref(),
            Some("src/cart.test.ts:14")
        );
        assert!(
            bun_report
                .notes
                .iter()
                .all(|n| !n.contains("other line(s)")),
            "unchanged object diff braces must not pollute other notes: {:?}",
            bun_report.notes
        );
        let bun_rendered = bun_report.render();
        assert!(
            bun_rendered.contains("src/cart.test.ts:14"),
            "{bun_rendered}"
        );
        assert!(
            bun_rendered.contains("-   \"total\": 120,"),
            "{bun_rendered}"
        );
        assert!(
            bun_rendered.contains("+   \"total\": 110,"),
            "{bun_rendered}"
        );
    }
}
