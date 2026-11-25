//! Structured streaming parsers and compactors for polyglot test runners:
//! Pytest, Python `unittest`, Go `go test`, .NET `dotnet test`, Maven Surefire/
//! Failsafe, Gradle, Swift (`XCTest` and Swift Testing), CTest, Elixir
//! `mix test`, and `deno test`.
//!
//! Each runner is parsed by a dedicated streaming sub-scanner across both
//! `stdout` and `stderr`, emitting exact 1-based inclusive `[stdout:A-B]` /
//! `[stderr:A-B]` line pointers, root-cause deduplication (normalizing volatile
//! hex addresses, ephemeral tempdirs, UUIDs, timestamps, and durations), and
//! parametrized test family folding.
//!
//! Reports are strictly fail-closed:
//! - A nonzero exit code with zero parsed failures/errors/races/panics returns `None`.
//! - Any contradiction between reported failure counts and parsed failures returns `None`.
//! - Output is never replaced unless strictly smaller than the raw stream(s) it covers
//!   and within `budget`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::capture::{ANSI, Stream, source_digest};
use crate::summary::{Item, short};

/// Lines of `other` stream text kept in a report before diagnostics and tail.
const KEPT_LINES: usize = 2;
const KEPT_DIAGNOSTICS: usize = 8;
/// Hard cap on distinct failures tracked in memory; exceeding this fails closed.
const FAILURE_LIMIT: usize = 2048;
/// Maximum excerpt lines retained per failure block.
const MAX_EXCERPT: usize = 4;
/// Maximum buffered pre-failure log lines per active test (Go `-v`, Swift).
const MAX_BUFFERED_TEST_LINES: usize = 64;
/// Maximum concurrent active tests buffered before dropping oldest tracking.
const MAX_ACTIVE_TESTS: usize = 512;

// ---------------------------------------------------------------------------
// Shared Normalization & Pattern Regexes
// ---------------------------------------------------------------------------

static VOLATILE_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?")
        .expect("volatile timestamp pattern")
});
static VOLATILE_UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("volatile uuid pattern")
});
static VOLATILE_HEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b0x[0-9a-f]{6,}\b").expect("volatile hex pattern"));
static VOLATILE_OFFSET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\+0x[0-9a-f]+\b").expect("volatile offset pattern"));
static VOLATILE_TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b\d+(?:\.\d+)?\s*(?:ns|us|µs|ms|s|sec|seconds?)\b")
        .expect("volatile time pattern")
});
static VOLATILE_GOROUTINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bgoroutine \d+\b").expect("volatile goroutine pattern"));
static EPHEMERAL_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?:",
        r"(?:/private)?/var/folders/[^/\s\x22'`,;)}\]:]+/[^/\s\x22'`,;)}\]:]+/T/(?:[^/\s\x22'`,;)}\]:]+/)*?\.?[A-Za-z0-9._-]+",
        r"|/(?:var/)?tmp/(?:pytest-of-[^/\s\x22'`,;)}\]:]+/)?(?:[^/\s\x22'`,;)}\]:]+/)*?(?:[A-Za-z0-9_-]+[-_])?(?:tmp|Test)[A-Za-z0-9._-]+",
        r"|/(?:var/)?tmp/\.?tmp[A-Za-z0-9._-]+",
        r"|[A-Za-z]:\\Users\\[^\\\s\x22'`,;)}\]:]+\\AppData\\Local\\Temp\\(?:[^\\\s\x22'`,;)}\]:]+\\)*?\.?[A-Za-z0-9._-]+",
        r")",
    ))
    .expect("ephemeral path pattern")
});

// Pytest regexes
static PYTEST_LOC_ERR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s:]+\.py):(\d+):\s+(.+)$").expect("pytest location error pattern")
});
static PYTEST_FILE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*File "([^"]+)", line (\d+)(?:, in .*)?$"#).expect("python file line pattern")
});
static PYTEST_SUMMARY_COUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\d+)\s+(failed|passed|errors?|skipped|xfailed|xpassed|deselected|warnings?)\b")
        .expect("pytest summary count pattern")
});
static PYTEST_SUMMARY_TIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bin\s+\d+(?:\.\d+)?s\b").expect("pytest summary time pattern"));

// Unittest regexes
static UNITTEST_RAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Ran (\d+) tests? in \d+(?:\.\d+)?s$").expect("unittest ran pattern")
});
static UNITTEST_FAILED_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(failures|errors|unexpected successes)=(\d+)").expect("unittest failed field")
});

// Go regexes
static GO_FILE_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([^\s:]+\.go:\d+(?::\d+)?)").expect("go file location pattern"));

// Dotnet regexes
static DOTNET_STACK_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bin\s+(.+?):line\s+(\d+)\s*$").expect("dotnet stack location pattern")
});
static DOTNET_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(Passed!|Failed!)\s+-\s+Failed:\s*(\d+),\s*Passed:\s*(\d+),\s*Skipped:\s*(\d+),\s*Total:\s*(\d+)")
        .expect("dotnet summary pattern")
});

// Maven regexes
static MAVEN_TOTALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Tests run:\s*(\d+),\s*Failures:\s*(\d+),\s*Errors:\s*(\d+),\s*Skipped:\s*(\d+)")
        .expect("maven totals pattern")
});
static JAVA_FRAME_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*at\s+(?:[A-Za-z0-9_.$@-]+/+)?([A-Za-z_][\w$.<>-]*)\(([^():]+\.(?:java|kt|scala|groovy):\d+)\)")
        .expect("java frame loc pattern")
});

// Gradle regexes
static GRADLE_COMPLETED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d+) tests? completed,\s*(\d+) failed(?:,\s*(\d+) skipped)?$")
        .expect("gradle completed pattern")
});
static GRADLE_INLINE_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bat\s+([^\s()]+\.(?:java|kt|scala|groovy):\d+)").expect("gradle inline loc")
});

// Swift regexes
static SWIFT_XCTEST_CASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Test Case '([^']+)' (started|passed|failed)").expect("swift xctest case pattern")
});
static SWIFT_XCTEST_DIAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?\.swift:\d+):\s+error:\s+(.+?)\s*:\s*(.+)$")
        .expect("swift xctest diagnostic pattern")
});
static SWIFT_XCTEST_EXECUTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Executed (\d+) tests?,\s+with (\d+) failures?").expect("swift executed pattern")
});
static SWIFT_TESTING_ISSUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^✘\s+(?:Test "([^"]+)"|Passing .+ to "([^"]+)")\s+recorded an issue at\s+([^\s:]+:\d+(?::\d+)?):\s*(.+)$"#)
        .expect("swift testing issue pattern")
});
static SWIFT_TESTING_FAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^✘\s+Test "([^"]+)"(?: with \d+ test cases)? failed after "#)
        .expect("swift testing fail pattern")
});
static SWIFT_TESTING_PASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^✔\s+Test "([^"]+)"(?: with \d+ test cases)? passed after "#)
        .expect("swift testing pass pattern")
});
static SWIFT_TESTING_RUN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(✔|✘)\s+Test run with (\d+) tests?(?: in \d+ suites?)?\s+(passed|failed) after .+?with (\d+) issues?\.|^(✔)\s+Test run with (\d+) tests?(?: in \d+ suites?)?\s+passed after ")
        .expect("swift testing run pattern")
});

// CTest regexes
static CTEST_RESULT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\d+/\d+\s+Test\s+#\s*(\d+):\s+(\S+)\s+\.+\s*(\*\*\*[^\d\r\n]+|Passed)\s*")
        .expect("ctest result pattern")
});
static CTEST_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d+%\s+tests passed,\s+(\d+)\s+tests? failed out of\s+(\d+)$")
        .expect("ctest total pattern")
});
static CTEST_FAILED_LIST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(\d+)\s+-\s+(\S+)\s+\(([^)]+)\)\s*$").expect("ctest failed list pattern")
});

// Mix regexes
static MIX_FAILURE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(\d+)\)\s+(.+)$").expect("mix failure header pattern"));
static MIX_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|,\s*)(\d+)\s+tests?,\s+(\d+)\s+failures?").expect("mix total pattern")
});
static MIX_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s:]+\.exs?:\d+)(?::\s|$)").expect("mix file location pattern")
});

// Deno regexes
static DENO_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(ok|FAILED)\s*\|\s*(\d+)\s+passed(?:\s*\([^)]*\))?\s*\|\s*(\d+)\s+failed")
        .expect("deno total pattern")
});
static DENO_FILE_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:file://)?((?:[A-Za-z]:[\\/]|/|\./)?[^\s()'`\x22]+?\.(?:ts|tsx|js|jsx|mjs|mts):\d+(?::\d+)?)")
        .expect("deno file loc pattern")
});

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runner {
    Pytest,
    Unittest,
    GoTest,
    Dotnet,
    Maven,
    Gradle,
    Swift,
    Ctest,
    Mix,
    Deno,
}

#[allow(dead_code)]
impl Runner {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pytest => "pytest",
            Self::Unittest => "unittest",
            Self::GoTest => "go test",
            Self::Dotnet => "dotnet test",
            Self::Maven => "mvn test",
            Self::Gradle => "gradle test",
            Self::Swift => "swift test",
            Self::Ctest => "ctest",
            Self::Mix => "mix test",
            Self::Deno => "deno test",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureItem {
    /// Normalized root-cause grouping key (volatile tokens stripped).
    pub key: String,
    /// Failing test / suite / target name.
    pub test: String,
    /// Concise root-cause / assertion / exception summary (with source location if known).
    pub cause: String,
    /// Originating stream (`"stdout"` or `"stderr"`).
    pub stream: &'static str,
    /// 1-based inclusive start line in `stream`.
    pub start: usize,
    /// 1-based inclusive end line in `stream`.
    pub end: usize,
}

#[allow(dead_code)]
impl FailureItem {
    /// Convert to `summary::Item` for rerun comparison in `summary.rs`.
    pub fn to_summary_item(&self) -> Item {
        let stream = if self.stream == "stderr" {
            Stream::Stderr
        } else {
            Stream::Stdout
        };
        let mut item = Item::new_in_stream(&self.test, &self.cause, self.start, stream);
        item.last = self.end;
        let basis = if !self.key.is_empty() {
            self.key.as_str()
        } else {
            self.cause.as_str()
        };
        item.digest = source_digest(basis.as_bytes());
        item
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactReport {
    pub text: String,
    pub items: Vec<FailureItem>,
    pub covers_stderr: bool,
}

/// Format raw `stdout` and `stderr` from a recognized polyglot test runner into
/// a compact, zero-semantic-loss report.
///
/// Returns `None` (fail-closed) when:
/// - output does not contain recognized runner structure/totals,
/// - `exit_code` is nonzero and zero failures/errors/races/panics were parsed,
/// - parsed failure counts contradict the runner's own summary totals,
/// - the compact report is not strictly smaller than the raw stream(s) it replaces, or
/// - the report cannot fit within `budget`.
#[allow(dead_code)]
pub fn format(
    runner: Runner,
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    budget: usize,
) -> Option<CompactReport> {
    let mut scan = Scan::new(runner);
    for (idx, raw) in stdout.lines().enumerate() {
        let clean = strip_ansi(raw);
        scan.observe(&clean, idx + 1, Stream::Stdout);
    }
    for (idx, raw) in stderr.lines().enumerate() {
        let clean = strip_ansi(raw);
        scan.observe(&clean, idx + 1, Stream::Stderr);
    }
    scan.finish(exit_code, stdout.len(), stderr.len(), budget)
}

/// Streaming scanner wrapper for line-by-line integration in `summary.rs`.
#[allow(dead_code)]
pub struct Scan {
    inner: RunnerScan,
}

enum RunnerScan {
    Pytest(PytestScan),
    Unittest(UnittestScan),
    GoTest(GoTestScan),
    Dotnet(DotnetScan),
    Maven(MavenScan),
    Gradle(GradleScan),
    Swift(SwiftScan),
    Ctest(CtestScan),
    Mix(MixScan),
    Deno(DenoScan),
}

#[allow(dead_code)]
impl Scan {
    pub fn new(runner: Runner) -> Self {
        let inner = match runner {
            Runner::Pytest => RunnerScan::Pytest(PytestScan::default()),
            Runner::Unittest => RunnerScan::Unittest(UnittestScan::default()),
            Runner::GoTest => RunnerScan::GoTest(GoTestScan::default()),
            Runner::Dotnet => RunnerScan::Dotnet(DotnetScan::default()),
            Runner::Maven => RunnerScan::Maven(MavenScan::default()),
            Runner::Gradle => RunnerScan::Gradle(GradleScan::default()),
            Runner::Swift => RunnerScan::Swift(SwiftScan::default()),
            Runner::Ctest => RunnerScan::Ctest(CtestScan::default()),
            Runner::Mix => RunnerScan::Mix(MixScan::default()),
            Runner::Deno => RunnerScan::Deno(DenoScan::default()),
        };
        Self { inner }
    }

    /// Observe one ANSI-stripped line with its 1-based line number in `stream`.
    pub fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let line = line.trim_end_matches(['\n', '\r']);
        match &mut self.inner {
            RunnerScan::Pytest(s) => s.observe(line, n, stream),
            RunnerScan::Unittest(s) => s.observe(line, n, stream),
            RunnerScan::GoTest(s) => s.observe(line, n, stream),
            RunnerScan::Dotnet(s) => s.observe(line, n, stream),
            RunnerScan::Maven(s) => s.observe(line, n, stream),
            RunnerScan::Gradle(s) => s.observe(line, n, stream),
            RunnerScan::Swift(s) => s.observe(line, n, stream),
            RunnerScan::Ctest(s) => s.observe(line, n, stream),
            RunnerScan::Mix(s) => s.observe(line, n, stream),
            RunnerScan::Deno(s) => s.observe(line, n, stream),
        }
    }

    pub fn finish(
        self,
        exit_code: Option<i32>,
        stdout_bytes: usize,
        stderr_bytes: usize,
        budget: usize,
    ) -> Option<CompactReport> {
        let parsed = match self.inner {
            RunnerScan::Pytest(s) => s.finish()?,
            RunnerScan::Unittest(s) => s.finish()?,
            RunnerScan::GoTest(s) => s.finish()?,
            RunnerScan::Dotnet(s) => s.finish()?,
            RunnerScan::Maven(s) => s.finish()?,
            RunnerScan::Gradle(s) => s.finish()?,
            RunnerScan::Swift(s) => s.finish()?,
            RunnerScan::Ctest(s) => s.finish()?,
            RunnerScan::Mix(s) => s.finish()?,
            RunnerScan::Deno(s) => s.finish()?,
        };
        finalize_report(parsed, exit_code, stdout_bytes, stderr_bytes, budget)
    }
}

// ---------------------------------------------------------------------------
// Internal Shared Types & Helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct RawFailure {
    test: String,
    location: Option<String>,
    error: String,
    excerpt: Vec<String>,
    stream: Stream,
    start: usize,
    end: usize,
    /// Optional explicit root-cause key override; computed on finalize if empty.
    key: String,
    /// Whether this failure came from a detailed block (higher priority than summary-only fallback).
    detailed: bool,
}

impl RawFailure {
    fn new(test: String, stream: Stream, line: usize) -> Self {
        Self {
            test: short(&test, 260),
            location: None,
            error: String::new(),
            excerpt: Vec::new(),
            stream,
            start: line,
            end: line,
            key: String::new(),
            detailed: true,
        }
    }

    fn cause_text(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !self.error.is_empty() {
            parts.push(&self.error);
        }
        if let Some(loc) = &self.location
            && !loc.is_empty()
            && !self.error.contains(loc.as_str())
            && !self.test.contains(loc.as_str())
        {
            parts.push(loc.as_str());
        }
        if parts.is_empty()
            && let Some(first_ex) = self.excerpt.first()
        {
            parts.push(first_ex.as_str());
        }
        short(&parts.join(" · "), 360)
    }

    fn finalize(&mut self) {
        if self.end < self.start {
            self.end = self.start;
        }
        if !self.key.is_empty() {
            return;
        }
        let norm_err = normalize_root_cause(&self.error);
        let mut key_parts = Vec::new();
        if !norm_err.is_empty() {
            key_parts.push(norm_err);
        }
        for line in self.excerpt.iter().take(2) {
            if is_assertion_excerpt(line) {
                let norm = normalize_root_cause(line);
                if !norm.is_empty() && !key_parts.contains(&norm) {
                    key_parts.push(norm);
                }
            }
        }
        if let Some(loc) = &self.location
            && !loc.is_empty()
        {
            let norm_loc = normalize_root_cause(loc);
            if !key_parts.iter().any(|p| p.contains(&norm_loc)) {
                key_parts.push(norm_loc);
            }
        }
        if key_parts.is_empty() {
            // Never merge causeless failures into one group.
            self.key = format!("test:{}", self.test);
        } else {
            self.key = key_parts.join(" | ");
        }
    }

    fn to_failure_item(&self) -> FailureItem {
        FailureItem {
            key: self.key.clone(),
            test: self.test.clone(),
            cause: self.cause_text(),
            stream: self.stream.name(),
            start: self.start,
            end: self.end,
        }
    }
}

#[derive(Default, Clone, Debug)]
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
}

#[derive(Default)]
struct Other {
    range: Range,
    kept: Vec<(usize, String)>,
    diagnostics: usize,
    last: Option<(usize, String)>,
}

impl Other {
    fn observe(&mut self, line: &str, n: usize) {
        let ordinal = self.range.count;
        self.range.add(n);
        let text = short(line.trim_end(), 200);
        let trimmed = line.trim();
        let diag = crate::filters::diagnostic(trimmed)
            || trimmed.contains("Error:")
            || trimmed.contains("Exception:")
            || trimmed.contains("WARNING:")
            || trimmed.contains("FATAL");
        if (self.kept.len() < KEPT_LINES && self.diagnostics == 0)
            || (diag && self.diagnostics < KEPT_DIAGNOSTICS)
        {
            self.diagnostics += usize::from(diag);
            self.kept.push((ordinal, text));
            self.last = None;
        } else {
            self.last = Some((ordinal, text));
        }
    }

    fn all_shown(&self) -> bool {
        self.kept.len() + usize::from(self.last.is_some()) == self.range.count
    }

    fn push_notes(&self, stream: Stream, notes: &mut Vec<String>, compact: bool) {
        if self.range.count == 0 {
            return;
        }
        let name = stream.name();
        notes.push(format!(
            "{name}: {} other line(s) {}",
            self.range.count,
            format_range(name, self.range.first, self.range.last)
        ));
        if compact {
            return;
        }
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

struct PassedSummary {
    label: String,
    stream: Stream,
    first: usize,
    last: usize,
}

impl PassedSummary {
    fn render(&self) -> String {
        format!(
            "{} {}",
            self.label,
            format_range(self.stream.name(), self.first, self.last)
        )
    }
}

struct ParsedReport {
    failures: Vec<RawFailure>,
    passed: Vec<PassedSummary>,
    totals: Vec<String>,
    notes: Vec<String>,
    compact_notes: Vec<String>,
    covers_stderr: bool,
    /// True if runner's own totals indicate failure (used for fail-closed checks).
    reported_failed: bool,
    /// Exact minimum number of failures expected when the runner reports a concrete count.
    min_expected_failures: usize,
    /// True if runner's own totals explicitly indicate zero failures (e.g. `OK`, `0 failed`).
    reported_clean: bool,
    /// True if internal failure cap overflowed (forces fail-closed).
    overflowed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RenderLevel {
    Full,
    Compact,
    Headlines,
}

fn finalize_report(
    mut parsed: ParsedReport,
    exit_code: Option<i32>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    budget: usize,
) -> Option<CompactReport> {
    if parsed.overflowed {
        return None;
    }
    for f in &mut parsed.failures {
        f.finalize();
    }

    // Fail-closed contract checks:
    // 1. Nonzero exit code with zero parsed failures/errors/races/panics -> never hide crash.
    let nonzero_exit = exit_code.is_some_and(|c| c != 0);
    if nonzero_exit && parsed.failures.is_empty() {
        return None;
    }
    // 2. Reported failure totals contradict parsed failures.
    if parsed.reported_failed && parsed.failures.is_empty() {
        return None;
    }
    if parsed.min_expected_failures > 0 && parsed.failures.len() < parsed.min_expected_failures {
        return None;
    }
    // 3. Reported clean/zero failures yet we parsed failures (contradiction).
    if parsed.reported_clean && !parsed.failures.is_empty() {
        return None;
    }

    let raw_replaced = if parsed.covers_stderr {
        stdout_bytes + stderr_bytes
    } else {
        stdout_bytes
    };
    if raw_replaced == 0 {
        return None;
    }

    let items: Vec<FailureItem> = parsed
        .failures
        .iter()
        .map(RawFailure::to_failure_item)
        .collect();

    // Progressive rendering to honor `budget` without losing semantics.
    for level in [
        RenderLevel::Full,
        RenderLevel::Compact,
        RenderLevel::Headlines,
    ] {
        let text = render_report(&parsed, level);
        if text.is_empty() {
            return None;
        }
        if text.len() <= budget && text.len() < raw_replaced {
            return Some(CompactReport {
                text,
                items,
                covers_stderr: parsed.covers_stderr,
            });
        }
    }

    None
}

fn render_report(parsed: &ParsedReport, level: RenderLevel) -> String {
    let mut out = String::new();
    if !parsed.failures.is_empty() {
        out.push_str(&format!(
            "{} reported failing test(s)\n",
            parsed.failures.len()
        ));
        let groups = group_raw_failures(&parsed.failures);
        for group in groups {
            render_group(&mut out, &group, level);
        }
    }
    for passed in &parsed.passed {
        out.push_str(&passed.render());
        out.push('\n');
    }
    for total in &parsed.totals {
        out.push_str(total);
        out.push('\n');
    }
    let notes = match level {
        RenderLevel::Full => &parsed.notes,
        RenderLevel::Compact | RenderLevel::Headlines => &parsed.compact_notes,
    };
    for note in notes {
        out.push_str(note);
        out.push('\n');
    }
    out
}

fn group_raw_failures(failures: &[RawFailure]) -> Vec<Vec<&RawFailure>> {
    let mut groups: Vec<(&str, Vec<&RawFailure>)> = Vec::new();
    for f in failures {
        if !f.key.is_empty()
            && !f.key.starts_with("test:")
            && let Some((_, existing)) = groups.iter_mut().find(|(k, _)| *k == f.key.as_str())
        {
            existing.push(f);
            continue;
        }
        groups.push((f.key.as_str(), vec![f]));
    }
    groups.into_iter().map(|(_, items)| items).collect()
}

fn render_group(out: &mut String, group: &[&RawFailure], level: RenderLevel) {
    if group.len() == 1 {
        let f = group[0];
        let cause = f.cause_text();
        let range = format_range(f.stream.name(), f.start, f.end);
        if cause.is_empty() {
            out.push_str(&format!("1 test(s): {} {range}\n", f.test));
        } else {
            let shown_cause = if level == RenderLevel::Headlines {
                short(&cause, 140)
            } else {
                cause
            };
            out.push_str(&format!("1 test(s): {} · {shown_cause} {range}\n", f.test));
        }
        if level == RenderLevel::Full {
            for ex in f.excerpt.iter().take(3) {
                if !f.error.contains(ex.as_str()) {
                    out.push_str("  ");
                    out.push_str(ex);
                    out.push('\n');
                }
            }
        }
        return;
    }

    let head = group[0];
    let cause = head.cause_text();
    let shown_cause = if level == RenderLevel::Headlines {
        short(&cause, 140)
    } else {
        cause.clone()
    };

    // Check if all tests in this group belong to a single parametrized family.
    let shared_stem = shared_parametrized_stem(group);
    let same_stream = group.iter().all(|f| f.stream == head.stream);
    let min_start = group.iter().map(|f| f.start).min().unwrap_or(head.start);
    let max_end = group.iter().map(|f| f.end).max().unwrap_or(head.end);

    if let Some(stem) = shared_stem {
        let span = if same_stream {
            format_range(head.stream.name(), min_start, max_end)
        } else {
            format_range(head.stream.name(), head.start, head.end)
        };
        if shown_cause.is_empty() {
            out.push_str(&format!("{} test(s): {stem} {span}\n", group.len()));
        } else {
            out.push_str(&format!(
                "{} test(s): {stem} {shown_cause} {span}\n",
                group.len()
            ));
        }
        if level == RenderLevel::Full {
            for ex in head.excerpt.iter().take(2) {
                if !head.error.contains(ex.as_str()) {
                    out.push_str("  ");
                    out.push_str(ex);
                    out.push('\n');
                }
            }
            if group.len() <= 4 {
                for f in group {
                    let r = format_range(f.stream.name(), f.start, f.end);
                    out.push_str(&format!("  {} {r}\n", f.test));
                }
            } else {
                let first = group[0];
                let last = group[group.len() - 1];
                out.push_str(&format!(
                    "  {} {} … {} {} (+{} more)\n",
                    first.test,
                    format_range(first.stream.name(), first.start, first.end),
                    last.test,
                    format_range(last.stream.name(), last.start, last.end),
                    group.len() - 2
                ));
            }
        }
        return;
    }

    // Non-parametrized root-cause group.
    if level == RenderLevel::Headlines {
        let span = format_range(head.stream.name(), min_start, max_end);
        out.push_str(&format!(
            "{} test(s): {} (+{} more) · {shown_cause} {span}\n",
            group.len(),
            head.test,
            group.len() - 1
        ));
        return;
    }

    if shown_cause.is_empty() {
        out.push_str(&format!("{} test(s): {}\n", group.len(), head.test));
    } else {
        out.push_str(&format!("{} test(s): {shown_cause}\n", group.len()));
    }
    if level == RenderLevel::Full {
        for ex in head.excerpt.iter().take(2) {
            if !head.error.contains(ex.as_str()) {
                out.push_str("  ");
                out.push_str(ex);
                out.push('\n');
            }
        }
    }

    let max_members = if level == RenderLevel::Full { 8 } else { 2 };
    if group.len() <= max_members {
        for f in group {
            let r = format_range(f.stream.name(), f.start, f.end);
            out.push_str(&format!("  {} {r}\n", f.test));
        }
    } else {
        let show_front = max_members.saturating_sub(1).max(1);
        for f in &group[..show_front] {
            let r = format_range(f.stream.name(), f.start, f.end);
            out.push_str(&format!("  {} {r}\n", f.test));
        }
        let omitted = group.len() - show_front - 1;
        let last = group[group.len() - 1];
        let last_r = format_range(last.stream.name(), last.start, last.end);
        out.push_str(&format!(
            "  … +{omitted} more, last: {} {last_r}\n",
            last.test
        ));
    }
}

fn shared_parametrized_stem(group: &[&RawFailure]) -> Option<String> {
    let first_stem = parametrized_stem(&group[0].test)?;
    if group
        .iter()
        .skip(1)
        .all(|f| parametrized_stem(&f.test).as_deref() == Some(first_stem.as_str()))
    {
        Some(first_stem)
    } else {
        None
    }
}

/// Normalize parametrized test names:
/// - `tests/test_a.py::test_parse[case_1]` -> `Some("tests/test_a.py::test_parse[...]")`
/// - `OrderTest > parse(String)[1]` -> `Some("OrderTest > parse(String)[...]")`
/// - `Ns.Cls.Method(x: 1, y: 2)` -> `Some("Ns.Cls.Method(...)")`
fn parametrized_stem(name: &str) -> Option<String> {
    if let Some(open) = name.rfind('[')
        && name.ends_with(']')
        && open + 1 < name.len() - 1
    {
        return Some(format!("{}[...]", &name[..open]));
    }
    if let Some(open) = name.rfind('(')
        && name.ends_with(')')
        && open + 1 < name.len() - 1
    {
        let inside = &name[open + 1..name.len() - 1];
        // Avoid treating Python unittest `(pkg.Class)` module suffixes as parameters.
        let before = &name[..open];
        if !before.ends_with(' ') && !inside.is_empty() {
            return Some(format!("{before}(...)"));
        }
    }
    None
}

fn normalize_root_cause(text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let s = VOLATILE_TS.replace_all(text, "<ts>");
    let s = EPHEMERAL_PATH.replace_all(&s, "<tmp>");
    let s = VOLATILE_UUID.replace_all(&s, "<uuid>");
    let s = VOLATILE_OFFSET.replace_all(&s, "+<off>");
    let s = VOLATILE_HEX.replace_all(&s, "0x<addr>");
    let s = VOLATILE_GOROUTINE.replace_all(&s, "goroutine <id>");
    let s = VOLATILE_TIME.replace_all(&s, "<time>");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_ansi(line: &str) -> Cow<'_, str> {
    if line.contains('\x1b') {
        ANSI.replace_all(line, "")
    } else {
        Cow::Borrowed(line)
    }
}

fn format_range(stream: &str, start: usize, end: usize) -> String {
    if start == end {
        format!("[{stream}:{start}]")
    } else {
        format!("[{stream}:{start}-{end}]")
    }
}

fn is_assertion_excerpt(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("Expected:")
        || t.starts_with("Actual:")
        || t.starts_with("Received:")
        || t.starts_with("assert ")
        || t.starts_with("left:")
        || t.starts_with("right:")
        || t.starts_with("code:")
        || t.starts_with("Caused by:")
        || t.starts_with("- ")
        || t.starts_with("+ ")
}

fn build_notes(other: &[Other; 2], has_test_stderr: bool) -> (Vec<String>, Vec<String>, bool) {
    let covers_stderr = has_test_stderr || other[Stream::Stderr as usize].all_shown();
    let mut full_notes = Vec::new();
    let mut compact_notes = Vec::new();
    for stream in Stream::BOTH {
        if stream == Stream::Stderr && !covers_stderr {
            continue;
        }
        other[stream as usize].push_notes(stream, &mut full_notes, false);
        other[stream as usize].push_notes(stream, &mut compact_notes, true);
    }
    (full_notes, compact_notes, covers_stderr)
}

fn push_failure_bounded(
    failures: &mut Vec<RawFailure>,
    overflowed: &mut bool,
    item: RawFailure,
) -> Option<usize> {
    if failures.len() >= FAILURE_LIMIT {
        *overflowed = true;
        return None;
    }
    let idx = failures.len();
    failures.push(item);
    Some(idx)
}

fn is_python_stdlib_or_venv(path: &str) -> bool {
    path.contains("site-packages/")
        || path.contains("site-packages\\")
        || path.contains("dist-packages/")
        || path.contains("/_pytest/")
        || path.contains("\\_pytest\\")
        || path.contains("/pluggy/")
        || path.contains("/unittest/")
        || path.starts_with("/usr/lib/python")
        || path.starts_with("<")
}

fn is_dotnet_framework_frame(trimmed: &str) -> bool {
    let rest = trimmed.strip_prefix("at ").unwrap_or(trimmed);
    rest.starts_with("System.")
        || rest.starts_with("Microsoft.")
        || rest.starts_with("Xunit.")
        || rest.starts_with("NUnit.")
        || rest.starts_with("MSTest.")
        || rest.starts_with("InvokeStub_")
}

fn is_java_framework_frame(symbol: &str) -> bool {
    symbol.starts_with("java.")
        || symbol.starts_with("javax.")
        || symbol.starts_with("jdk.")
        || symbol.starts_with("sun.")
        || symbol.starts_with("org.junit.")
        || symbol.starts_with("org.opentest4j.")
        || symbol.starts_with("org.apache.maven.surefire.")
        || symbol.starts_with("org.gradle.")
        || symbol.starts_with("worker.org.gradle.")
}

fn is_deno_external_path(path: &str) -> bool {
    path.starts_with("https://")
        || path.starts_with("http://")
        || path.starts_with("jsr:")
        || path.starts_with("ext:")
        || path.starts_with("node:")
        || path.contains("/node_modules/")
}

// ---------------------------------------------------------------------------
// 1. Pytest Scanner
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum PytestSection {
    #[default]
    Progress,
    Errors,
    Failures,
    Warnings,
    ShortSummary,
    Durations,
}

struct PytestOpen {
    index: usize,
    stream: Stream,
    in_captured: bool,
    has_in_repo_loc: bool,
}

#[derive(Default)]
struct PytestScan {
    section: PytestSection,
    last_stream: Option<Stream>,
    failures: Vec<RawFailure>,
    open: Option<PytestOpen>,
    saw_marker: bool,
    saw_summary: bool,
    reported_failed: usize,
    reported_errors: usize,
    has_test_stderr: bool,
    passed: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl PytestScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;
        if trimmed.is_empty() {
            return;
        }
        if self.last_stream.is_some_and(|prev| prev != stream) {
            self.section = PytestSection::Progress;
            self.open = None;
        }
        self.last_stream = Some(stream);
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
        }

        // Section banners: `=== ... ===`
        if let Some(banner) = strip_pytest_banner(trimmed, '=') {
            self.open = None;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            if banner == "test session starts" {
                self.saw_marker = true;
                self.section = PytestSection::Progress;
                return;
            }
            if banner == "ERRORS" {
                self.saw_marker = true;
                self.section = PytestSection::Errors;
                return;
            }
            if banner == "FAILURES" {
                self.saw_marker = true;
                self.section = PytestSection::Failures;
                return;
            }
            if banner == "warnings summary" {
                self.saw_marker = true;
                self.section = PytestSection::Warnings;
                return;
            }
            if banner == "short test summary info" {
                self.saw_marker = true;
                self.section = PytestSection::ShortSummary;
                return;
            }
            if banner.starts_with("slowest ") && banner.contains("duration") {
                self.saw_marker = true;
                self.section = PytestSection::Durations;
                return;
            }
            if is_pytest_final_summary(banner) {
                self.record_final_summary(banner);
                return;
            }
        }

        // Unframed final summary line (e.g. `pytest -q` omits `===` padding).
        if is_pytest_final_summary(trimmed)
            && (self.saw_marker || self.passed[idx].count > 0 || !self.failures.is_empty())
        {
            self.open = None;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.record_final_summary(trimmed);
            return;
        }

        match self.section {
            PytestSection::Progress => self.observe_progress(line, trimmed, n, stream),
            PytestSection::Errors | PytestSection::Failures => {
                self.observe_failure_section(line, trimmed, n, stream)
            }
            PytestSection::ShortSummary => self.observe_short_summary(trimmed, n, stream),
            PytestSection::Warnings | PytestSection::Durations => {}
        }
    }

    fn record_final_summary(&mut self, summary: &str) {
        self.section = PytestSection::Progress;
        self.saw_marker = true;
        self.saw_summary = true;
        for cap in PYTEST_SUMMARY_COUNT.captures_iter(summary) {
            let count = cap[1].parse::<usize>().unwrap_or(0);
            match &cap[2] {
                "failed" => self.reported_failed += count,
                "error" | "errors" => self.reported_errors += count,
                _ => {}
            }
        }
        self.totals.push(short(summary, 220));
    }

    fn observe_progress(&mut self, line: &str, trimmed: &str, n: usize, stream: Stream) {
        let idx = stream as usize;
        if trimmed.starts_with("platform ")
            || trimmed.starts_with("cachedir:")
            || trimmed.starts_with("rootdir:")
            || trimmed.starts_with("configfile:")
            || trimmed.starts_with("plugins:")
            || trimmed.starts_with("collecting ")
            || trimmed.starts_with("collected ")
            || trimmed.starts_with("django:")
            || (trimmed.starts_with("gw") && trimmed.contains('['))
        {
            self.saw_marker = true;
            return;
        }
        if is_pytest_verbose_line(trimmed) {
            self.saw_marker = true;
            if trimmed.contains(" PASSED")
                || trimmed.contains(" XFAIL")
                || trimmed.contains(" SKIPPED")
            {
                self.passed[idx].add(n);
            }
            return;
        }
        if is_pytest_dot_progress(trimmed) {
            self.saw_marker = true;
            self.passed[idx].add(n);
            return;
        }
        // `--tb=line` can emit `file.py:42: AssertionError: ...` directly.
        if self.saw_marker
            && let Some(cap) = PYTEST_LOC_ERR.captures(trimmed)
            && is_python_exception_or_assert(&cap[3])
        {
            let loc = format!("{}:{}", &cap[1], &cap[2]);
            let mut f = RawFailure::new(loc.clone(), stream, n);
            f.location = Some(loc);
            f.error = short(cap[3].trim(), 220);
            push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
            return;
        }
        self.other[idx].observe(line, n);
    }

    fn observe_failure_section(&mut self, line: &str, trimmed: &str, n: usize, stream: Stream) {
        // Sub-header: `___________________ test_foo ___________________`
        if let Some(title) = strip_pytest_banner(trimmed, '_') {
            self.open = None;
            let f = RawFailure::new(title.to_owned(), stream, n);
            if let Some(index) = push_failure_bounded(&mut self.failures, &mut self.overflowed, f) {
                self.open = Some(PytestOpen {
                    index,
                    stream,
                    in_captured: false,
                    has_in_repo_loc: false,
                });
            }
            return;
        }

        // Captured output divider inside a failure block.
        if let Some(banner) = strip_pytest_banner(trimmed, '-')
            && banner.starts_with("Captured ")
        {
            if let Some(open) = self.open.as_mut() {
                open.in_captured = true;
                self.failures[open.index].end = n;
            }
            return;
        }

        // `--tb=line` entries directly inside FAILURES/ERRORS without `___` headers.
        if self.open.is_none()
            && let Some(cap) = PYTEST_LOC_ERR.captures(trimmed)
        {
            let loc = format!("{}:{}", &cap[1], &cap[2]);
            let mut f = RawFailure::new(loc.clone(), stream, n);
            f.location = Some(loc);
            f.error = short(cap[3].trim(), 220);
            push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
            return;
        }

        if let Some(open) = self.open.as_mut() {
            let f = &mut self.failures[open.index];
            f.end = n;
            if open.in_captured {
                return;
            }
            // Pytest `E   ...` root-cause lines.
            if let Some(rest) = line
                .strip_prefix("E ")
                .or_else(|| line.strip_prefix("E\t"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if f.error.is_empty()
                    || (f.error.starts_with("assert ") && is_python_exception_name(rest))
                {
                    if !f.error.is_empty() && f.excerpt.len() < MAX_EXCERPT {
                        let prev = std::mem::replace(&mut f.error, short(rest, 220));
                        f.excerpt.push(prev);
                    } else {
                        f.error = short(rest, 220);
                    }
                } else if f.excerpt.len() < MAX_EXCERPT {
                    f.excerpt.push(short(rest, 200));
                }
                return;
            }
            // Pytest bottom location line: `tests/test_a.py:42: AssertionError` or `in fn`
            if let Some(cap) = PYTEST_LOC_ERR.captures(trimmed) {
                let path = &cap[1];
                let loc = format!("{path}:{}", &cap[2]);
                let detail = cap[3].trim();
                let in_repo = !is_python_stdlib_or_venv(path);
                // In pytest tracebacks, later in-repo frames are closer to the actual throw site.
                if in_repo || !open.has_in_repo_loc {
                    f.location = Some(loc);
                    if in_repo {
                        open.has_in_repo_loc = true;
                    }
                }
                if !detail.starts_with("in ")
                    && (f.error.is_empty()
                        || (f.error.starts_with("assert ") && detail.contains(':')))
                {
                    f.error = short(detail, 220);
                }
                return;
            }
            // Standard Python `File "path", line N, in fn` frame (`--tb=native`).
            if let Some(cap) = PYTEST_FILE_LINE.captures(line) {
                let path = &cap[1];
                let loc = format!("{path}:{}", &cap[2]);
                let in_repo = !is_python_stdlib_or_venv(path);
                if in_repo || !open.has_in_repo_loc {
                    f.location = Some(loc);
                    if in_repo {
                        open.has_in_repo_loc = true;
                    }
                }
                return;
            }
            // Bare exception line (`--tb=native` or collection `ImportError while importing...`).
            if f.error.is_empty() && !line.starts_with(' ') && is_python_exception_name(trimmed) {
                f.error = short(trimmed, 220);
            }
            return;
        }

        self.other[stream as usize].observe(line, n);
    }

    fn observe_short_summary(&mut self, trimmed: &str, n: usize, stream: Stream) {
        let Some((is_error, rest)) = trimmed
            .strip_prefix("FAILED ")
            .map(|r| (false, r))
            .or_else(|| trimmed.strip_prefix("ERROR ").map(|r| (true, r)))
        else {
            return;
        };
        let (nodeid, msg) = rest
            .split_once(" - ")
            .map_or((rest.trim(), ""), |(a, b)| (a.trim(), b.trim()));
        if nodeid.is_empty() {
            return;
        }
        // Reconcile against an existing FAILURES/ERRORS block so we get the full nodeid
        // without double-counting.
        if let Some(existing) = self
            .failures
            .iter_mut()
            .find(|f| f.detailed && pytest_nodeid_matches_header(nodeid, &f.test, is_error))
        {
            existing.test = short(nodeid, 260);
            if existing.error.is_empty() && !msg.is_empty() {
                existing.error = short(msg, 220);
            }
            return;
        }
        // Fallback when `--tb=no` or block was absent.
        let mut f = RawFailure::new(nodeid.to_owned(), stream, n);
        f.detailed = false;
        if !msg.is_empty() {
            f.error = short(msg, 220);
        }
        push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
    }

    fn finish(self) -> Option<ParsedReport> {
        if !self.saw_summary {
            return None;
        }
        let expected_bad = self.reported_failed + self.reported_errors;
        let mut passed = Vec::new();
        for stream in Stream::BOTH {
            let r = &self.passed[stream as usize];
            if r.count > 0 {
                passed.push(PassedSummary {
                    label: format!("pytest: {} progress/pass line(s)", r.count),
                    stream,
                    first: r.first,
                    last: r.last,
                });
            }
        }
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed: expected_bad > 0,
            min_expected_failures: expected_bad,
            reported_clean: expected_bad == 0,
            overflowed: self.overflowed,
        })
    }
}

fn strip_pytest_banner(trimmed: &str, ch: char) -> Option<&str> {
    let min_pad = 3;
    let lead = trimmed.chars().take_while(|&c| c == ch).count();
    let trail = trimmed.chars().rev().take_while(|&c| c == ch).count();
    if lead < min_pad || trail < min_pad {
        return None;
    }
    let inner = trimmed.trim_matches(ch).trim();
    (!inner.is_empty()).then_some(inner)
}

fn is_pytest_final_summary(text: &str) -> bool {
    if !PYTEST_SUMMARY_TIME.is_match(text) {
        return false;
    }
    text.starts_with("no tests ran in ") || PYTEST_SUMMARY_COUNT.is_match(text)
}

fn is_pytest_verbose_line(trimmed: &str) -> bool {
    (trimmed.contains("::") || trimmed.starts_with("[gw"))
        && (trimmed.contains(" PASSED")
            || trimmed.contains(" FAILED")
            || trimmed.contains(" ERROR")
            || trimmed.contains(" SKIPPED")
            || trimmed.contains(" XFAIL")
            || trimmed.contains(" XPASS"))
}

fn is_pytest_dot_progress(trimmed: &str) -> bool {
    // e.g. `tests/test_a.py ....F..s [ 40%]` or `....F.. [100%]`
    let before_pct = match trimmed.rsplit_once('[') {
        Some((head, tail)) if tail.trim_end().ends_with("%]") => head.trim(),
        _ => trimmed,
    };
    let dots = before_pct
        .rsplit_once(' ')
        .map_or(before_pct, |(prefix, tail)| {
            if prefix.ends_with(".py") {
                tail
            } else {
                before_pct
            }
        });
    !dots.is_empty()
        && dots
            .bytes()
            .all(|b| matches!(b, b'.' | b'F' | b'E' | b's' | b'x' | b'X'))
}

fn is_python_exception_name(text: &str) -> bool {
    let head = text.split([':', ' ']).next().unwrap_or("");
    let last = head.rsplit('.').next().unwrap_or(head);
    !last.is_empty()
        && (last.ends_with("Error")
            || last.ends_with("Exception")
            || last.ends_with("Exit")
            || last == "Failed"
            || last == "KeyboardInterrupt")
}

fn is_python_exception_or_assert(text: &str) -> bool {
    text.starts_with("assert ") || is_python_exception_name(text)
}

fn pytest_nodeid_matches_header(nodeid: &str, header: &str, is_error: bool) -> bool {
    if header == nodeid {
        return true;
    }
    if is_error {
        if let Some(collected) = header.strip_prefix("ERROR collecting ") {
            return nodeid == collected
                || nodeid.ends_with(collected)
                || collected.ends_with(nodeid);
        }
        if let Some((_, name)) = header.split_once(" of ") {
            let dot_form = nodeid_dot_suffix(nodeid);
            return name == dot_form || nodeid.ends_with(&format!("::{name}"));
        }
    }
    let dot_form = nodeid_dot_suffix(nodeid);
    header == dot_form
        || nodeid.ends_with(&format!("::{header}"))
        || dot_form.ends_with(&format!(".{header}"))
}

fn nodeid_dot_suffix(nodeid: &str) -> String {
    match nodeid.split_once("::") {
        Some((_, rest)) => rest.replace("::", "."),
        None => nodeid.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// 2. Python Unittest Scanner
// ---------------------------------------------------------------------------

struct UnittestOpen {
    index: usize,
    stream: Stream,
    in_body: bool,
    has_in_repo_loc: bool,
}

#[derive(Default)]
struct UnittestScan {
    failures: Vec<RawFailure>,
    open: Option<UnittestOpen>,
    saw_ran: bool,
    saw_outcome: bool,
    reported_failed_count: usize,
    reported_ok: bool,
    reported_failed_outcome: bool,
    has_test_stderr: bool,
    passed: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl UnittestScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;
        if trimmed.is_empty() {
            return;
        }
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
        }

        // Block separators (`===...` / `---...`).
        if trimmed.len() >= 20 && trimmed.bytes().all(|b| b == b'=') {
            self.open = None;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            return;
        }
        if trimmed.len() >= 20 && trimmed.bytes().all(|b| b == b'-') {
            if let Some(open) = self.open.as_mut() {
                if !open.in_body {
                    open.in_body = true;
                    return;
                }
                self.open = None;
            }
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            return;
        }

        // `FAIL: test_name (pkg.Mod)` or `ERROR: test_name (pkg.Mod)`
        if let Some(rest) = trimmed
            .strip_prefix("FAIL: ")
            .or_else(|| trimmed.strip_prefix("ERROR: "))
            && !rest.is_empty()
        {
            self.open = None;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let f = RawFailure::new(rest.trim().to_owned(), stream, n);
            if let Some(index) = push_failure_bounded(&mut self.failures, &mut self.overflowed, f) {
                self.open = Some(UnittestOpen {
                    index,
                    stream,
                    in_body: false,
                    has_in_repo_loc: false,
                });
            }
            return;
        }

        // `Ran 42 tests in 0.123s`
        if UNITTEST_RAN.is_match(trimmed) {
            self.open = None;
            self.saw_ran = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.totals.push(short(trimmed, 220));
            return;
        }

        // Outcome line after `Ran N tests`: `OK` / `OK (...)` / `FAILED (...)`
        if self.saw_ran
            && !self.saw_outcome
            && (trimmed == "OK"
                || trimmed.starts_with("OK (")
                || trimmed == "FAILED"
                || trimmed.starts_with("FAILED ("))
        {
            self.open = None;
            self.saw_outcome = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            if trimmed == "OK" || trimmed.starts_with("OK (") {
                self.reported_ok = true;
            } else {
                self.reported_failed_outcome = true;
                for cap in UNITTEST_FAILED_FIELD.captures_iter(trimmed) {
                    self.reported_failed_count += cap[2].parse::<usize>().unwrap_or(0);
                }
            }
            self.totals.push(short(trimmed, 220));
            return;
        }

        // Inside an open failure traceback block.
        if let Some(open) = self.open.as_mut() {
            let f = &mut self.failures[open.index];
            f.end = n;
            if let Some(cap) = PYTEST_FILE_LINE.captures(line) {
                let path = &cap[1];
                let loc = format!("{path}:{}", &cap[2]);
                let in_repo = !is_python_stdlib_or_venv(path);
                if in_repo || !open.has_in_repo_loc {
                    f.location = Some(loc);
                    if in_repo {
                        open.has_in_repo_loc = true;
                    }
                }
                return;
            }
            // Final exception / assertion line (non-indented, not `Traceback`).
            if !line.starts_with(' ')
                && !trimmed.starts_with("Traceback ")
                && !trimmed.starts_with("During handling of")
                && !trimmed.starts_with("The above exception")
            {
                if is_python_exception_name(trimmed) || f.error.is_empty() {
                    f.error = short(trimmed, 220);
                } else if f.excerpt.len() < MAX_EXCERPT {
                    f.excerpt.push(short(trimmed, 200));
                }
            } else if !f.error.is_empty()
                && (trimmed.starts_with("- ")
                    || trimmed.starts_with("+ ")
                    || trimmed.starts_with("? "))
                && f.excerpt.len() < MAX_EXCERPT
            {
                f.excerpt.push(short(trimmed, 200));
            }
            return;
        }

        // Progress dots (`...F.E.s`) or verbose lines (`test_x (Mod) ... ok`).
        if trimmed
            .bytes()
            .all(|b| matches!(b, b'.' | b'F' | b'E' | b's' | b'x' | b'u'))
        {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.passed[idx].add(n);
            return;
        }
        if let Some((head, status)) = trimmed.rsplit_once(" ... ") {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            if status == "ok" || status.starts_with("skipped") || status == "expected failure" {
                self.passed[idx].add(n);
                return;
            }
            if status == "FAIL" || status == "ERROR" {
                return;
            }
            if status == "unexpected success" {
                let mut f = RawFailure::new(head.trim().to_owned(), stream, n);
                f.error = "unexpected success".into();
                push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
                return;
            }
        }

        self.other[idx].observe(line, n);
    }

    fn finish(self) -> Option<ParsedReport> {
        if !self.saw_ran || !self.saw_outcome {
            return None;
        }
        let mut passed = Vec::new();
        for stream in Stream::BOTH {
            let r = &self.passed[stream as usize];
            if r.count > 0 {
                passed.push(PassedSummary {
                    label: format!("unittest: {} passed/progress line(s)", r.count),
                    stream,
                    first: r.first,
                    last: r.last,
                });
            }
        }
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed: self.reported_failed_outcome || self.reported_failed_count > 0,
            min_expected_failures: self.reported_failed_count,
            reported_clean: self.reported_ok,
            overflowed: self.overflowed,
        })
    }
}

// ---------------------------------------------------------------------------
// 3. Go `go test` Scanner
// ---------------------------------------------------------------------------

enum GoBlock {
    TestFail { index: usize, stream: Stream },
    DataRace { index: usize, stream: Stream },
    Panic { index: usize, stream: Stream },
}

#[derive(Default)]
struct GoTestScan {
    failures: Vec<RawFailure>,
    /// Active `-v` test logs buffered between `=== RUN`/`=== CONT` and `--- FAIL:`.
    active_logs: HashMap<String, Vec<(usize, String)>>,
    current_test: Option<String>,
    open: Option<GoBlock>,
    /// Build errors seen under `# pkg/path` before `FAIL pkg/path [build failed]`.
    current_build_pkg: Option<String>,
    build_errors: HashMap<String, (Stream, usize, usize, Vec<String>)>,
    /// Packages that reported `FAIL\tpkg` (used for fail-closed verification).
    failed_pkgs: usize,
    saw_standalone_fail: bool,
    saw_marker: bool,
    has_test_stderr: bool,
    ok_pkgs: Range,
    no_test_pkgs: Range,
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl GoTestScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;

        if trimmed.is_empty() {
            // Blank lines inside DATA RACE or panic blocks keep the block open
            // because Go prints a blank line between `panic: ...` and `goroutine N`.
            return;
        }

        // Stream switch closes non-race open block.
        let stream_changed = match &self.open {
            Some(GoBlock::TestFail { stream: s, .. })
            | Some(GoBlock::DataRace { stream: s, .. })
            | Some(GoBlock::Panic { stream: s, .. }) => *s != stream,
            None => false,
        };
        if stream_changed {
            self.open = None;
        }

        // Closing delimiter of a DATA RACE block: `==================`
        if matches!(self.open, Some(GoBlock::DataRace { .. }))
            && trimmed.len() >= 10
            && trimmed.bytes().all(|b| b == b'=')
        {
            if let Some(GoBlock::DataRace { index, .. }) = self.open.take() {
                self.failures[index].end = n;
            }
            return;
        }

        // Start of DATA RACE block (`WARNING: DATA RACE`).
        if trimmed == "WARNING: DATA RACE" {
            self.saw_marker = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let test_name = self
                .current_test
                .clone()
                .unwrap_or_else(|| "DATA RACE".into());
            let mut f = RawFailure::new(test_name, stream, n);
            f.error = "WARNING: DATA RACE".into();
            if let Some(index) = push_failure_bounded(&mut self.failures, &mut self.overflowed, f) {
                self.open = Some(GoBlock::DataRace { index, stream });
            }
            return;
        }

        // Opening delimiter right before `WARNING: DATA RACE` (`==================`).
        if trimmed.len() >= 10 && trimmed.bytes().all(|b| b == b'=') {
            return;
        }

        // Inside DATA RACE block.
        if let Some(GoBlock::DataRace { index, .. }) = &self.open {
            let f = &mut self.failures[*index];
            f.end = n;
            if trimmed.starts_with("Write ")
                || trimmed.starts_with("Read ")
                || trimmed.starts_with("Previous write ")
                || trimmed.starts_with("Previous read ")
            {
                if f.excerpt.len() < MAX_EXCERPT {
                    f.excerpt.push(short(trimmed, 200));
                }
            } else if let Some(cap) = GO_FILE_LINE.captures(trimmed) {
                let loc = cap[1].to_owned();
                if !loc.contains("/runtime/") && f.location.is_none() {
                    f.location = Some(loc);
                }
            }
            return;
        }

        // `panic: ...` or `fatal error: ...`
        if trimmed.starts_with("panic:") || trimmed.starts_with("fatal error:") {
            self.saw_marker = true;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let test_name = self.current_test.clone().unwrap_or_else(|| "panic".into());
            let mut f = RawFailure::new(test_name, stream, n);
            f.error = short(trimmed, 220);
            if let Some(index) = push_failure_bounded(&mut self.failures, &mut self.overflowed, f) {
                self.open = Some(GoBlock::Panic { index, stream });
            }
            return;
        }

        // Compiler package header: `# example.com/pkg [example.com/pkg.test]`
        if let Some(rest) = trimmed.strip_prefix("# ")
            && !rest.is_empty()
            && !rest.contains(':')
        {
            let pkg = rest.split_whitespace().next().unwrap_or(rest).to_owned();
            self.current_build_pkg = Some(pkg);
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            return;
        }

        // Package summary: `ok  \tpkg\t0.1s`
        if line.starts_with("ok  \t") || line.starts_with("ok\t") {
            self.open = None;
            self.current_test = None;
            self.current_build_pkg = None;
            self.saw_marker = true;
            if line.contains("[no tests to run]") {
                self.no_test_pkgs.add(n);
            } else {
                self.ok_pkgs.add(n);
            }
            return;
        }

        // Package with no test files: `?\tpkg\t[no test files]`
        if line.starts_with("?\t") && line.contains("[no test files]") {
            self.open = None;
            self.current_test = None;
            self.current_build_pkg = None;
            self.saw_marker = true;
            self.no_test_pkgs.add(n);
            return;
        }

        // Failing package summary: `FAIL\tpkg\t0.123s` or `FAIL\tpkg [build failed]`
        if let Some(rest) = line.strip_prefix("FAIL\t") {
            self.open = None;
            self.current_test = None;
            self.current_build_pkg = None;
            self.saw_marker = true;
            self.failed_pkgs += 1;
            self.totals.push(short(trimmed, 220));
            let pkg = rest.split_whitespace().next().unwrap_or(rest);
            if rest.contains("[build failed]") || rest.contains("[setup failed]") {
                let tag = if rest.contains("[build failed]") {
                    "[build failed]"
                } else {
                    "[setup failed]"
                };
                if let Some((err_stream, first, last, errs)) = self.build_errors.remove(pkg) {
                    let mut f = RawFailure::new(pkg.to_owned(), err_stream, first);
                    f.end = last;
                    if let Some(first_err) = errs.first() {
                        f.error = short(&format!("{tag} {first_err}"), 220);
                        if let Some(cap) = GO_FILE_LINE.captures(first_err) {
                            f.location = Some(cap[1].to_owned());
                        }
                    } else {
                        f.error = tag.into();
                    }
                    for ex in errs.iter().skip(1).take(MAX_EXCERPT) {
                        f.excerpt.push(short(ex, 200));
                    }
                    push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
                } else {
                    let mut f = RawFailure::new(pkg.to_owned(), stream, n);
                    f.error = tag.into();
                    push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
                }
            }
            return;
        }

        // Standalone package status (`PASS` / `FAIL` / `exit status N`).
        if trimmed == "PASS" || trimmed == "FAIL" || trimmed.starts_with("exit status ") {
            self.open = None;
            self.current_test = None;
            self.saw_marker = true;
            if trimmed == "FAIL" {
                self.saw_standalone_fail = true;
            }
            return;
        }

        // Test lifecycle lines: `=== RUN`, `=== CONT`, `=== PAUSE`, `=== NAME`.
        if let Some(rest) = trimmed
            .strip_prefix("=== RUN")
            .or_else(|| trimmed.strip_prefix("=== CONT"))
            .or_else(|| trimmed.strip_prefix("=== NAME"))
            .map(str::trim_start)
            && !rest.is_empty()
        {
            self.open = None;
            self.saw_marker = true;
            let name = rest.split_whitespace().next().unwrap_or(rest).to_owned();
            self.current_test = Some(name);
            return;
        }
        if trimmed.starts_with("=== PAUSE")
            || trimmed.starts_with("--- PASS:")
            || trimmed.starts_with("--- SKIP:")
            || trimmed.starts_with("coverage:")
        {
            self.open = None;
            self.saw_marker = true;
            if let Some(name) = trimmed
                .strip_prefix("--- PASS:")
                .or_else(|| trimmed.strip_prefix("--- SKIP:"))
                .and_then(|r| r.split_whitespace().next())
            {
                self.active_logs.remove(name);
            }
            return;
        }

        // `--- FAIL: TestName (0.01s)` (may be indented for subtests!).
        if let Some(rest) = trimmed.strip_prefix("--- FAIL:") {
            self.saw_marker = true;
            let name = rest.split_whitespace().next().unwrap_or("").trim();
            if !name.is_empty() {
                self.current_test = Some(name.to_owned());
                let buffered = self.active_logs.remove(name).unwrap_or_default();
                let start = buffered.first().map_or(n, |(ln, _)| *ln);
                let mut f = RawFailure::new(name.to_owned(), stream, start);
                f.end = n;
                for (_, msg) in buffered {
                    push_go_failure_line(&mut f, &msg);
                }
                if let Some(index) =
                    push_failure_bounded(&mut self.failures, &mut self.overflowed, f)
                {
                    self.open = Some(GoBlock::TestFail { index, stream });
                }
                return;
            }
        }

        // Inside an open panic block: collect stack frames until blank line or next boundary.
        if let Some(GoBlock::Panic { index, .. }) = &self.open {
            let f = &mut self.failures[*index];
            f.end = n;
            if let Some(cap) = GO_FILE_LINE.captures(trimmed) {
                let loc = cap[1].to_owned();
                if !loc.contains("/runtime/") && !loc.contains("/testing/") && f.location.is_none()
                {
                    f.location = Some(loc);
                }
            }
            return;
        }

        // Indented diagnostic line belonging to a `--- FAIL:` block or active `-v` test.
        if line.starts_with([' ', '\t']) {
            if let Some(GoBlock::TestFail { index, .. }) = &self.open {
                let f = &mut self.failures[*index];
                f.end = n;
                push_go_failure_line(f, trimmed);
                return;
            }
            if let Some(active) = &self.current_test {
                if self.active_logs.len() < MAX_ACTIVE_TESTS
                    || self.active_logs.contains_key(active)
                {
                    let buf = self.active_logs.entry(active.clone()).or_default();
                    if buf.len() < MAX_BUFFERED_TEST_LINES {
                        buf.push((n, trimmed.to_owned()));
                    }
                }
                return;
            }
        }

        // Compiler error lines under `# pkg`: `pkg/foo.go:12:5: undefined: bar`
        if let Some(pkg) = &self.current_build_pkg
            && (GO_FILE_LINE.is_match(trimmed)
                || trimmed.starts_with("cannot ")
                || trimmed.contains(": "))
        {
            let entry = self
                .build_errors
                .entry(pkg.clone())
                .or_insert_with(|| (stream, n, n, Vec::new()));
            entry.2 = n;
            if entry.3.len() <= MAX_EXCERPT {
                entry.3.push(short(trimmed, 200));
            }
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(mut self) -> Option<ParsedReport> {
        if !self.saw_marker {
            return None;
        }
        // Reconcile compiler errors collected on stderr after `FAIL\tpkg [build failed]` on stdout.
        for f in &mut self.failures {
            if (f.error == "[build failed]" || f.error == "[setup failed]")
                && let Some((err_stream, first, last, errs)) = self.build_errors.remove(&f.test)
            {
                let tag = f.error.clone();
                f.stream = err_stream;
                f.start = first;
                f.end = last;
                if let Some(first_err) = errs.first() {
                    f.error = short(&format!("{tag} {first_err}"), 220);
                    if let Some(cap) = GO_FILE_LINE.captures(first_err) {
                        f.location = Some(cap[1].to_owned());
                    }
                }
                for ex in errs.iter().skip(1).take(MAX_EXCERPT) {
                    f.excerpt.push(short(ex, 200));
                }
            }
        }
        // Drop empty parent test failures when a child subtest (`Parent/child`) failed
        // so subtest runs don't duplicate the container header with no error message.
        let names: HashSet<String> = self.failures.iter().map(|f| f.test.clone()).collect();
        self.failures.retain(|f| {
            if !f.error.is_empty() || !f.excerpt.is_empty() {
                return true;
            }
            let prefix = format!("{}/", f.test);
            !names.iter().any(|other| other.starts_with(&prefix))
        });

        let mut passed = Vec::new();
        if self.ok_pkgs.count > 0 || self.no_test_pkgs.count > 0 {
            let first = match (self.ok_pkgs.first, self.no_test_pkgs.first) {
                (0, b) => b,
                (a, 0) => a,
                (a, b) => a.min(b),
            };
            let last = self.ok_pkgs.last.max(self.no_test_pkgs.last);
            let mut label = format!("go test: {} passed package(s)", self.ok_pkgs.count);
            if self.no_test_pkgs.count > 0 {
                label.push_str(&format!(", {} without tests", self.no_test_pkgs.count));
            }
            passed.push(PassedSummary {
                label,
                stream: Stream::Stdout,
                first,
                last,
            });
        }
        if self.totals.is_empty()
            && self.failed_pkgs == 0
            && !self.saw_standalone_fail
            && self.ok_pkgs.count == 0
            && self.no_test_pkgs.count == 0
            && self.failures.is_empty()
        {
            self.totals.push("PASS".into());
        }
        let reported_failed = self.failed_pkgs > 0 || self.saw_standalone_fail;
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed,
            min_expected_failures: usize::from(reported_failed),
            reported_clean: !reported_failed,
            overflowed: self.overflowed,
        })
    }
}

fn push_go_failure_line(f: &mut RawFailure, trimmed: &str) {
    if let Some(cap) = GO_FILE_LINE.captures(trimmed) {
        let loc = cap[1].to_owned();
        if f.location.is_none() {
            f.location = Some(loc.clone());
        }
        // Strip leading `file_test.go:42: ` prefix for cleaner root-cause text if followed by message.
        let msg = trimmed
            .strip_prefix(&format!("{loc}:"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(trimmed);
        if f.error.is_empty() {
            f.error = short(msg, 220);
        } else if f.excerpt.len() < MAX_EXCERPT {
            f.excerpt.push(short(msg, 200));
        }
    } else if f.error.is_empty() {
        f.error = short(trimmed, 220);
    } else if f.excerpt.len() < MAX_EXCERPT {
        f.excerpt.push(short(trimmed, 200));
    }
}

// ---------------------------------------------------------------------------
// 4. .NET `dotnet test` Scanner
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum DotnetBlockPhase {
    Header,
    ErrorMessage,
    StackTrace,
    StdOut,
}

struct DotnetOpen {
    index: usize,
    stream: Stream,
    phase: DotnetBlockPhase,
    has_workspace_frame: bool,
}

#[derive(Default)]
struct DotnetScan {
    failures: Vec<RawFailure>,
    open: Option<DotnetOpen>,
    saw_summary: bool,
    reported_failed: usize,
    reported_clean_any: bool,
    reported_failed_any: bool,
    has_test_stderr: bool,
    passed: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl DotnetScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;
        if trimmed.is_empty() {
            return;
        }
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
        }

        // Assembly summary: `Failed!  - Failed: 2, Passed: 40, Skipped: 1, Total: 43, ...`
        if let Some(cap) = DOTNET_SUMMARY.captures(trimmed) {
            self.open = None;
            self.saw_summary = true;
            let failed = cap[2].parse::<usize>().unwrap_or(0);
            self.reported_failed += failed;
            if &cap[1] == "Failed!" || failed > 0 {
                self.reported_failed_any = true;
            } else {
                self.reported_clean_any = true;
            }
            self.totals.push(short(trimmed, 220));
            return;
        }

        if trimmed == "Test Run Successful." || trimmed == "Test Run Failed." {
            self.open = None;
            self.saw_summary = true;
            if trimmed == "Test Run Failed." {
                self.reported_failed_any = true;
            } else {
                self.reported_clean_any = true;
            }
            self.totals.push(trimmed.to_owned());
            return;
        }

        // Build / tool preamble chatter.
        if trimmed.starts_with("Determining projects to restore")
            || trimmed.starts_with("All projects are up-to-date")
            || trimmed.starts_with("Restored ")
            || trimmed.starts_with("Test run for ")
            || trimmed.starts_with("Microsoft (R) Test Execution")
            || trimmed.starts_with("VSTest version ")
            || trimmed.starts_with("Copyright (c) Microsoft")
            || trimmed.starts_with("Starting test execution")
            || trimmed.starts_with("A total of ")
            || (line.starts_with("  ") && trimmed.contains(" -> ") && trimmed.ends_with(".dll"))
            || trimmed.starts_with("Total tests:")
            || trimmed.starts_with("Total time:")
        {
            return;
        }

        // Passing / skipped test line: `  Passed Namespace.Class.Method [12 ms]`
        if let Some(rest) = trimmed
            .strip_prefix("Passed ")
            .or_else(|| trimmed.strip_prefix("Skipped "))
            && rest.contains('[')
            && rest.ends_with(']')
        {
            self.open = None;
            self.passed[idx].add(n);
            return;
        }

        // Failing test header: `  Failed Namespace.Class.Method [12 ms]`
        if let Some(rest) = trimmed.strip_prefix("Failed ")
            && !rest.starts_with('-')
            && !rest.is_empty()
        {
            self.open = None;
            let name = strip_dotnet_timing(rest);
            let f = RawFailure::new(name.to_owned(), stream, n);
            if let Some(index) = push_failure_bounded(&mut self.failures, &mut self.overflowed, f) {
                self.open = Some(DotnetOpen {
                    index,
                    stream,
                    phase: DotnetBlockPhase::Header,
                    has_workspace_frame: false,
                });
            }
            return;
        }

        if let Some(open) = self.open.as_mut() {
            let f = &mut self.failures[open.index];
            f.end = n;
            if trimmed == "Error Message:" {
                open.phase = DotnetBlockPhase::ErrorMessage;
                return;
            }
            if trimmed == "Stack Trace:" {
                open.phase = DotnetBlockPhase::StackTrace;
                return;
            }
            if trimmed == "Standard Output Messages:" || trimmed == "Standard Error Messages:" {
                open.phase = DotnetBlockPhase::StdOut;
                return;
            }
            match open.phase {
                DotnetBlockPhase::ErrorMessage | DotnetBlockPhase::Header => {
                    if f.error.is_empty() {
                        f.error = short(trimmed, 220);
                    } else if f.excerpt.len() < MAX_EXCERPT {
                        f.excerpt.push(short(trimmed, 200));
                    }
                }
                DotnetBlockPhase::StackTrace => {
                    if !open.has_workspace_frame && trimmed.starts_with("at ") {
                        let is_fw = is_dotnet_framework_frame(trimmed);
                        let loc = DOTNET_STACK_LOC
                            .captures(trimmed)
                            .map(|cap| format!("{}:{}", cap[1].trim(), &cap[2]))
                            .unwrap_or_else(|| short(trimmed, 160));
                        if !is_fw {
                            f.location = Some(loc);
                            open.has_workspace_frame = true;
                        } else if f.location.is_none() {
                            f.location = Some(loc);
                        }
                    }
                }
                DotnetBlockPhase::StdOut => {}
            }
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(self) -> Option<ParsedReport> {
        if !self.saw_summary {
            return None;
        }
        let mut passed = Vec::new();
        for stream in Stream::BOTH {
            let r = &self.passed[stream as usize];
            if r.count > 0 {
                passed.push(PassedSummary {
                    label: format!("dotnet test: {} passed/skipped test(s)", r.count),
                    stream,
                    first: r.first,
                    last: r.last,
                });
            }
        }
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed: self.reported_failed_any || self.reported_failed > 0,
            min_expected_failures: self.reported_failed,
            reported_clean: self.reported_clean_any
                && !self.reported_failed_any
                && self.reported_failed == 0,
            overflowed: self.overflowed,
        })
    }
}

fn strip_dotnet_timing(rest: &str) -> &str {
    if let Some((head, tail)) = rest.rsplit_once(" [")
        && tail.ends_with(']')
        && (tail.contains("ms") || tail.contains(" s") || tail.ends_with("s]"))
    {
        return head.trim();
    }
    rest.trim()
}

// ---------------------------------------------------------------------------
// 5. Maven `mvn test` / `mvnd test` Scanner
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum MavenSection {
    #[default]
    Normal,
    Results,
    FailuresList,
    ErrorsList,
}

struct MavenOpen {
    index: usize,
    stream: Stream,
    has_workspace_frame: bool,
}

#[derive(Default)]
struct MavenScan {
    section: MavenSection,
    last_stream: Option<Stream>,
    failures: Vec<RawFailure>,
    open: Option<MavenOpen>,
    saw_totals: bool,
    reported_failures: usize,
    reported_errors: usize,
    saw_build_failure: bool,
    saw_build_success: bool,
    has_test_stderr: bool,
    passed_classes: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl MavenScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;
        if trimmed.is_empty() {
            return;
        }
        if self.last_stream.is_some_and(|prev| prev != stream) {
            self.section = MavenSection::Normal;
            self.open = None;
        }
        self.last_stream = Some(stream);
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
        }

        // Strip Maven log level prefix (`[INFO]`, `[ERROR]`, `[WARNING]`).
        let (level, body) = split_maven_level(trimmed);

        if body.is_empty() {
            if matches!(
                self.section,
                MavenSection::FailuresList | MavenSection::ErrorsList
            ) {
                self.section = MavenSection::Results;
            }
            return;
        }

        // Section transitions under `[INFO] Results:`.
        if body == "Results:" {
            self.open = None;
            self.section = MavenSection::Results;
            return;
        }
        if level == Some("ERROR") && body == "Failures:" {
            self.open = None;
            self.section = MavenSection::FailuresList;
            return;
        }
        if level == Some("ERROR") && body == "Errors:" {
            self.open = None;
            self.section = MavenSection::ErrorsList;
            return;
        }

        if body == "BUILD SUCCESS" {
            self.open = None;
            self.section = MavenSection::Normal;
            self.saw_build_success = true;
            self.totals.push(short(trimmed, 220));
            return;
        }
        if body == "BUILD FAILURE" {
            self.open = None;
            self.section = MavenSection::Normal;
            self.saw_build_failure = true;
            self.totals.push(short(trimmed, 220));
            return;
        }

        // `Tests run: N, Failures: F, Errors: E, Skipped: S`
        if let Some(cap) = MAVEN_TOTALS.captures(body) {
            self.open = None;
            let fails = cap[2].parse::<usize>().unwrap_or(0);
            let errs = cap[3].parse::<usize>().unwrap_or(0);
            let is_per_class = body.contains("Time elapsed:") || body.contains(" in ");
            if is_per_class {
                if fails == 0 && errs == 0 {
                    self.passed_classes[idx].add(n);
                }
            } else {
                // Module-level summary total!
                self.section = MavenSection::Normal;
                self.saw_totals = true;
                self.reported_failures += fails;
                self.reported_errors += errs;
                self.totals.push(short(trimmed, 220));
            }
            return;
        }

        // Entries inside `[ERROR] Failures:` / `[ERROR] Errors:`.
        if matches!(
            self.section,
            MavenSection::FailuresList | MavenSection::ErrorsList
        ) && level == Some("ERROR")
        {
            // e.g. `[ERROR]   OrderTest.testCreate:42 expected: <200> but was: <500>`
            // or   `[ERROR]   PaymentTest.testCharge:91 » NullPointer Cannot invoke ...`
            if let Some((test_and_loc, msg)) = parse_maven_summary_failure(body) {
                let (test_short, loc_line) = split_test_and_line(test_and_loc);
                if let Some(existing) = self
                    .failures
                    .iter_mut()
                    .find(|f| f.detailed && maven_name_matches(&f.test, test_short))
                {
                    if existing.error.is_empty() && !msg.is_empty() {
                        existing.error = short(msg, 220);
                    }
                    if existing.location.is_none()
                        && let Some(ln) = loc_line
                    {
                        existing.location = Some(format!("{test_short}:{ln}"));
                    }
                } else {
                    let mut f = RawFailure::new(test_short.to_owned(), stream, n);
                    f.detailed = false;
                    if let Some(ln) = loc_line {
                        f.location = Some(format!("{test_short}:{ln}"));
                    }
                    if !msg.is_empty() {
                        f.error = short(msg, 220);
                    }
                    push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
                }
                return;
            }
        }

        // Inline Surefire test failure header:
        // `[ERROR] com.example.OrderTest.testCreate -- Time elapsed: 0.05 s <<< FAILURE!`
        // or `testCreate(com.example.OrderTest)  Time elapsed: 0.05 s  <<< ERROR!`
        if (body.ends_with("<<< FAILURE!") || body.ends_with("<<< ERROR!"))
            && !body.starts_with("Tests run:")
        {
            self.open = None;
            let test_name = parse_maven_inline_test_name(body);
            if !test_name.is_empty() {
                let f = RawFailure::new(test_name, stream, n);
                if let Some(index) =
                    push_failure_bounded(&mut self.failures, &mut self.overflowed, f)
                {
                    self.open = Some(MavenOpen {
                        index,
                        stream,
                        has_workspace_frame: false,
                    });
                }
                return;
            }
        }

        // Inside an open inline Maven failure stacktrace.
        if let Some(open) = self.open.as_mut() {
            // A new `[INFO]` or `[ERROR]` Maven log line ends the inline Java stacktrace.
            if level.is_some() {
                self.open = None;
            } else {
                let f = &mut self.failures[open.index];
                f.end = n;
                if let Some(cap) = JAVA_FRAME_LOC.captures(line) {
                    let sym = &cap[1];
                    let loc = cap[2].to_owned();
                    let is_fw = is_java_framework_frame(sym);
                    if !open.has_workspace_frame && (!is_fw || f.location.is_none()) {
                        f.location = Some(loc);
                        if !is_fw {
                            open.has_workspace_frame = true;
                        }
                    }
                } else if trimmed.starts_with("Caused by:") {
                    if f.excerpt.len() < MAX_EXCERPT {
                        f.excerpt.push(short(trimmed, 200));
                    }
                } else if f.error.is_empty()
                    && !trimmed.starts_with("at ")
                    && !trimmed.starts_with("... ")
                {
                    f.error = short(trimmed, 220);
                } else if !trimmed.starts_with("at ")
                    && !trimmed.starts_with("... ")
                    && f.excerpt.len() < MAX_EXCERPT
                {
                    f.excerpt.push(short(trimmed, 200));
                }
                return;
            }
        }

        // Routine Maven INFO chatter (`[INFO] Running ...`, `--- surefire:... ---`, etc.).
        if level == Some("INFO")
            && (body.starts_with("Running ")
                || body.starts_with("Scanning for projects")
                || body.starts_with("Building ")
                || body.starts_with("--- ")
                || body.starts_with("T E S T S")
                || body.starts_with("Total time:")
                || body.starts_with("Finished at:")
                || body.starts_with("Reactor Summary")
                || body.bytes().all(|b| b == b'-'))
        {
            return;
        }
        if level == Some("ERROR")
            && (body.starts_with("Failed to execute goal ")
                || body.starts_with("To see the full stack trace")
                || body.starts_with("Re-run Maven using")
                || body.starts_with("For more information about the errors")
                || body.starts_with("[Help ")
                || body.starts_with("-> [Help "))
        {
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(self) -> Option<ParsedReport> {
        if !self.saw_totals {
            return None;
        }
        let expected_bad = self.reported_failures + self.reported_errors;
        let mut passed = Vec::new();
        for stream in Stream::BOTH {
            let r = &self.passed_classes[stream as usize];
            if r.count > 0 {
                passed.push(PassedSummary {
                    label: format!("mvn test: {} passed test class(es)", r.count),
                    stream,
                    first: r.first,
                    last: r.last,
                });
            }
        }
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed: expected_bad > 0 || self.saw_build_failure,
            min_expected_failures: expected_bad,
            reported_clean: expected_bad == 0 && !self.saw_build_failure && self.saw_build_success,
            overflowed: self.overflowed,
        })
    }
}

fn split_maven_level(trimmed: &str) -> (Option<&'static str>, &str) {
    if let Some(rest) = trimmed.strip_prefix("[INFO]") {
        (Some("INFO"), rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix("[ERROR]") {
        (Some("ERROR"), rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix("[WARNING]") {
        (Some("WARNING"), rest.trim())
    } else {
        (None, trimmed)
    }
}

fn parse_maven_summary_failure(body: &str) -> Option<(&str, &str)> {
    let mut parts = body.splitn(2, char::is_whitespace);
    let head = parts.next()?.trim();
    if !head.contains('.') {
        return None;
    }
    let msg = parts.next().map_or("", str::trim);
    Some((head, msg))
}

fn split_test_and_line(head: &str) -> (&str, Option<&str>) {
    if let Some((prefix, line)) = head.rsplit_once(':')
        && !line.is_empty()
        && line.bytes().all(|b| b.is_ascii_digit())
    {
        return (prefix, Some(line));
    }
    (head, None)
}

fn parse_maven_inline_test_name(body: &str) -> String {
    let head = body
        .split_once(" -- Time elapsed:")
        .or_else(|| body.split_once("  Time elapsed:"))
        .or_else(|| body.split_once(" Time elapsed:"))
        .map_or(body, |(h, _)| h)
        .trim();
    // Convert legacy `method(com.foo.BarTest)` into `com.foo.BarTest.method`.
    if let Some((method, rest)) = head.split_once('(')
        && let Some(class) = rest.strip_suffix(')')
        && class.contains('.')
        && !method.contains('.')
    {
        return format!("{class}.{method}");
    }
    head.to_owned()
}

fn maven_name_matches(full: &str, short_name: &str) -> bool {
    full == short_name || full.ends_with(&format!(".{short_name}"))
}

// ---------------------------------------------------------------------------
// 6. Gradle `gradle test` / `./gradlew test` Scanner
// ---------------------------------------------------------------------------

struct GradleOpen {
    index: usize,
    stream: Stream,
    has_workspace_frame: bool,
}

#[derive(Default)]
struct GradleScan {
    failures: Vec<RawFailure>,
    open: Option<GradleOpen>,
    saw_test_task: bool,
    saw_completed: bool,
    saw_build_success: bool,
    saw_build_failed: bool,
    reported_failed: usize,
    has_test_stderr: bool,
    passed: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl GradleScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;
        if trimmed.is_empty() {
            return;
        }
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
        }

        // Gradle task lines: `> Task :module:test FAILED`
        if let Some(rest) = trimmed.strip_prefix("> Task ") {
            self.open = None;
            let is_test = rest.contains(":test") || rest.starts_with("test");
            if is_test {
                self.saw_test_task = true;
            }
            if rest.ends_with(" FAILED") {
                self.saw_build_failed = true;
                self.totals.push(short(trimmed, 220));
            }
            return;
        }

        if trimmed.starts_with("BUILD SUCCESSFUL") {
            self.open = None;
            self.saw_build_success = true;
            self.totals.push(short(trimmed, 220));
            return;
        }
        if trimmed.starts_with("BUILD FAILED") {
            self.open = None;
            self.saw_build_failed = true;
            self.totals.push(short(trimmed, 220));
            return;
        }
        if trimmed.contains("actionable task") || trimmed.starts_with("Deprecated Gradle features")
        {
            return;
        }

        // `42 tests completed, 3 failed`
        if let Some(cap) = GRADLE_COMPLETED.captures(trimmed) {
            self.open = None;
            self.saw_completed = true;
            self.reported_failed += cap[2].parse::<usize>().unwrap_or(0);
            self.totals.push(short(trimmed, 220));
            return;
        }

        // `ClassName > methodName() FAILED` / `PASSED` / `SKIPPED`
        if !line.starts_with([' ', '\t']) && trimmed.contains(" > ") {
            if let Some(name) = trimmed.strip_suffix(" FAILED") {
                self.open = None;
                self.saw_test_task = true;
                let f = RawFailure::new(name.trim().to_owned(), stream, n);
                if let Some(index) =
                    push_failure_bounded(&mut self.failures, &mut self.overflowed, f)
                {
                    self.open = Some(GradleOpen {
                        index,
                        stream,
                        has_workspace_frame: false,
                    });
                }
                return;
            }
            if trimmed.ends_with(" PASSED") || trimmed.ends_with(" SKIPPED") {
                self.open = None;
                self.saw_test_task = true;
                self.passed[idx].add(n);
                return;
            }
        }

        // Gradle failure boilerplate footer (`FAILURE: Build failed...`, `* What went wrong:`, `* Try:`).
        if trimmed == "FAILURE: Build failed with an exception."
            || trimmed == "* What went wrong:"
            || trimmed == "* Try:"
            || trimmed == "* Get more help at https://help.gradle.org"
            || trimmed.starts_with("> Run with --")
            || trimmed.starts_with("> Get more help")
            || trimmed.starts_with("Execution failed for task ")
            || trimmed.starts_with("> There were failing tests. See the report at:")
        {
            self.open = None;
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            return;
        }

        // Indented failure body lines under `ClassName > method() FAILED`.
        if let Some(open) = self.open.as_mut() {
            if line.starts_with([' ', '\t']) {
                let f = &mut self.failures[open.index];
                f.end = n;
                if let Some(cap) = JAVA_FRAME_LOC.captures(line) {
                    let sym = &cap[1];
                    let loc = cap[2].to_owned();
                    let is_fw = is_java_framework_frame(sym);
                    if !open.has_workspace_frame && (!is_fw || f.location.is_none()) {
                        f.location = Some(loc);
                        if !is_fw {
                            open.has_workspace_frame = true;
                        }
                    }
                    return;
                }
                if let Some(cap) = GRADLE_INLINE_LOC.captures(trimmed)
                    && !open.has_workspace_frame
                {
                    f.location = Some(cap[1].to_owned());
                    open.has_workspace_frame = true;
                }
                if f.error.is_empty() {
                    f.error = short(trimmed, 220);
                } else if f.excerpt.len() < MAX_EXCERPT {
                    f.excerpt.push(short(trimmed, 200));
                }
                return;
            }
            self.open = None;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(self) -> Option<ParsedReport> {
        let recognized = self.saw_completed
            || (self.saw_test_task && (self.saw_build_success || self.saw_build_failed));
        if !recognized {
            return None;
        }
        let mut passed = Vec::new();
        for stream in Stream::BOTH {
            let r = &self.passed[stream as usize];
            if r.count > 0 {
                passed.push(PassedSummary {
                    label: format!("gradle test: {} passed/skipped test(s)", r.count),
                    stream,
                    first: r.first,
                    last: r.last,
                });
            }
        }
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed: self.reported_failed > 0 || self.saw_build_failed,
            min_expected_failures: self.reported_failed,
            reported_clean: self.saw_build_success
                && !self.saw_build_failed
                && self.reported_failed == 0,
            overflowed: self.overflowed,
        })
    }
}

// ---------------------------------------------------------------------------
// 7. Swift `swift test` Scanner (XCTest + Swift Testing)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SwiftScan {
    failures: Vec<RawFailure>,
    /// Pre-failure XCTest diagnostics buffered per test case name.
    xctest_diags: HashMap<String, Vec<(Stream, usize, String, String)>>,
    /// Swift Testing recorded issues buffered per test display name.
    testing_issues: HashMap<String, Vec<(Stream, usize, String, String)>>,
    last_suite_name: String,
    xctest_total_line: Option<String>,
    xctest_reported_failures: usize,
    xctest_saw_all_tests: bool,
    saw_xctest_total: bool,
    saw_testing_total: bool,
    testing_reported_failed: bool,
    has_test_stderr: bool,
    passed: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl SwiftScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;
        if trimmed.is_empty() {
            return;
        }

        // Build chatter.
        if trimmed.starts_with("Building for ")
            || trimmed.starts_with("Build complete!")
            || trimmed.starts_with("Planning build")
            || trimmed.starts_with("Testing started")
            || (trimmed.starts_with('[') && trimmed.contains("] Compiling "))
            || (trimmed.starts_with('[') && trimmed.contains("] Linking "))
        {
            return;
        }

        // XCTest Suite header (`Test Suite 'All tests' failed at ...`).
        if let Some(rest) = trimmed.strip_prefix("Test Suite '")
            && let Some((name, _)) = rest.split_once('\'')
        {
            self.last_suite_name = name.to_owned();
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            return;
        }

        // XCTest summary: `Executed 42 tests, with 2 failures (0 unexpected) in ...`
        if let Some(cap) = SWIFT_XCTEST_EXECUTED.captures(trimmed) {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.saw_xctest_total = true;
            let count = cap[2].parse::<usize>().unwrap_or(0);
            if self.last_suite_name == "All tests" || !self.xctest_saw_all_tests {
                if self.last_suite_name == "All tests" {
                    self.xctest_saw_all_tests = true;
                }
                self.xctest_reported_failures = count;
                self.xctest_total_line = Some(short(trimmed, 220));
            }
            return;
        }

        // XCTest diagnostic line BEFORE `Test Case '...' failed`:
        // `/repo/Tests/A.swift:42: error: -[Pkg.Suite testFoo] : XCTAssertEqual failed: ...`
        if let Some(cap) = SWIFT_XCTEST_DIAG.captures(trimmed) {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let loc = cap[1].to_owned();
            let case_name = cap[2].trim().to_owned();
            let msg = cap[3].trim().to_owned();
            if self.xctest_diags.len() < MAX_ACTIVE_TESTS
                || self.xctest_diags.contains_key(&case_name)
            {
                let buf = self.xctest_diags.entry(case_name).or_default();
                if buf.len() < MAX_BUFFERED_TEST_LINES {
                    buf.push((stream, n, loc, msg));
                }
            }
            return;
        }

        // XCTest case lifecycle: `Test Case '...' started/passed/failed`.
        if let Some(cap) = SWIFT_XCTEST_CASE.captures(trimmed) {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let name = cap[1].to_owned();
            match &cap[2] {
                "started" => return,
                "passed" => {
                    self.xctest_diags.remove(&name);
                    self.passed[idx].add(n);
                    return;
                }
                "failed" => {
                    let diags = self.xctest_diags.remove(&name).unwrap_or_default();
                    let (fail_stream, start) =
                        diags.first().map_or((stream, n), |(s, ln, _, _)| (*s, *ln));
                    let mut f = RawFailure::new(name, fail_stream, start);
                    f.end = if fail_stream == stream {
                        n
                    } else {
                        diags.last().map_or(start, |(_, ln, _, _)| *ln)
                    };
                    for (_, _, loc, msg) in diags {
                        if f.location.is_none() {
                            f.location = Some(loc);
                        }
                        if f.error.is_empty() {
                            f.error = short(&msg, 220);
                        } else if f.excerpt.len() < MAX_EXCERPT {
                            f.excerpt.push(short(&msg, 200));
                        }
                    }
                    push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
                    return;
                }
                _ => {}
            }
        }

        // Swift Testing: `✘ Test "..." recorded an issue at File.swift:42:5: ...`
        if let Some(cap) = SWIFT_TESTING_ISSUE.captures(trimmed) {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let name = cap
                .get(1)
                .or_else(|| cap.get(2))
                .map(|m| m.as_str().to_owned())
                .unwrap_or_default();
            let loc = cap[3].to_owned();
            let msg = cap[4].trim().to_owned();
            if self.testing_issues.len() < MAX_ACTIVE_TESTS
                || self.testing_issues.contains_key(&name)
            {
                let buf = self.testing_issues.entry(name).or_default();
                if buf.len() < MAX_BUFFERED_TEST_LINES {
                    buf.push((stream, n, loc, msg));
                }
            }
            return;
        }

        // Swift Testing: `✘ Test "..." failed after ...`
        if let Some(cap) = SWIFT_TESTING_FAIL.captures(trimmed) {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            let name = cap[1].to_owned();
            let issues = self.testing_issues.remove(&name).unwrap_or_default();
            let (fail_stream, start) = issues
                .first()
                .map_or((stream, n), |(s, ln, _, _)| (*s, *ln));
            let mut f = RawFailure::new(name, fail_stream, start);
            f.end = if fail_stream == stream {
                n
            } else {
                issues.last().map_or(start, |(_, ln, _, _)| *ln)
            };
            for (_, _, loc, msg) in issues {
                if f.location.is_none() {
                    f.location = Some(loc);
                }
                if f.error.is_empty() {
                    f.error = short(&msg, 220);
                } else if f.excerpt.len() < MAX_EXCERPT {
                    f.excerpt.push(short(&msg, 200));
                }
            }
            push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
            return;
        }

        // Swift Testing: `✔ Test "..." passed after ...`
        if SWIFT_TESTING_PASS.is_match(trimmed) {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.passed[idx].add(n);
            return;
        }

        // Swift Testing run summary: `✘ Test run with N tests failed after ... with M issues.`
        if let Some(cap) = SWIFT_TESTING_RUN.captures(trimmed) {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            self.saw_testing_total = true;
            if cap.get(1).is_some_and(|m| m.as_str() == "✘")
                || cap.get(3).is_some_and(|m| m.as_str() == "failed")
            {
                self.testing_reported_failed = true;
            }
            self.totals.push(short(trimmed, 220));
            return;
        }

        // Swift Testing neutral lifecycle lines (`◇ ...`, `✔ Suite ...`, `✘ Suite ...`).
        if trimmed.starts_with("◇ ")
            || trimmed.starts_with("✔ Suite ")
            || trimmed.starts_with("✘ Suite ")
        {
            if stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(mut self) -> Option<ParsedReport> {
        if !self.saw_xctest_total && !self.saw_testing_total {
            return None;
        }
        // Reconcile any diagnostics/issues that arrived on stderr after `Test Case` on stdout.
        for f in &mut self.failures {
            if f.error.is_empty()
                && let Some(diags) = self
                    .xctest_diags
                    .remove(&f.test)
                    .or_else(|| self.testing_issues.remove(&f.test))
            {
                if let Some((s, first, _, _)) = diags.first() {
                    f.stream = *s;
                    f.start = *first;
                    f.end = diags.last().map_or(*first, |(_, last, _, _)| *last);
                }
                for (_, _, loc, msg) in diags {
                    if f.location.is_none() {
                        f.location = Some(loc);
                    }
                    if f.error.is_empty() {
                        f.error = short(&msg, 220);
                    } else if f.excerpt.len() < MAX_EXCERPT {
                        f.excerpt.push(short(&msg, 200));
                    }
                }
            }
        }
        if let Some(line) = self.xctest_total_line {
            self.totals.insert(0, line);
        }
        let mut passed = Vec::new();
        for stream in Stream::BOTH {
            let r = &self.passed[stream as usize];
            if r.count > 0 {
                passed.push(PassedSummary {
                    label: format!("swift test: {} passed test(s)", r.count),
                    stream,
                    first: r.first,
                    last: r.last,
                });
            }
        }
        let reported_failed = self.xctest_reported_failures > 0 || self.testing_reported_failed;
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed,
            // In both XCTest and Swift Testing, one test case can emit multiple assertion issues.
            min_expected_failures: usize::from(reported_failed),
            reported_clean: !reported_failed,
            overflowed: self.overflowed,
        })
    }
}

// ---------------------------------------------------------------------------
// 8. CTest Scanner
// ---------------------------------------------------------------------------

struct CtestOpen {
    index: usize,
    stream: Stream,
}

#[derive(Default)]
struct CtestScan {
    failures: Vec<RawFailure>,
    by_num: HashMap<String, usize>,
    last_stream: Option<Stream>,
    open: Option<CtestOpen>,
    in_failed_list: bool,
    saw_total: bool,
    reported_failed: usize,
    has_test_stderr: bool,
    passed: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl CtestScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;
        if trimmed.is_empty() {
            return;
        }
        if self.last_stream.is_some_and(|prev| prev != stream) {
            self.in_failed_list = false;
            self.open = None;
        }
        self.last_stream = Some(stream);
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
        }

        if trimmed.starts_with("Test project ")
            || trimmed.starts_with("Start ")
            || trimmed == "Errors while running CTest"
            || trimmed.starts_with("Output from these tests are in:")
            || trimmed.starts_with("Use \"--rerun-failed")
        {
            self.open = None;
            if trimmed == "Errors while running CTest" && stream == Stream::Stderr {
                self.has_test_stderr = true;
            }
            return;
        }

        if trimmed == "The following tests FAILED:" {
            self.open = None;
            self.in_failed_list = true;
            return;
        }

        // `85% tests passed, 3 tests failed out of 20`
        if let Some(cap) = CTEST_TOTAL.captures(trimmed) {
            self.open = None;
            self.in_failed_list = false;
            self.saw_total = true;
            self.reported_failed = cap[1].parse::<usize>().unwrap_or(0);
            self.totals.push(short(trimmed, 220));
            return;
        }

        if trimmed.starts_with("Total Test time (real)") {
            self.open = None;
            self.totals.push(short(trimmed, 220));
            return;
        }

        // ` 1/20 Test  #1: test_math .......   Passed   0.01 sec`
        // ` 3/20 Test  #3: test_parse ......***Failed   0.15 sec`
        if let Some(cap) = CTEST_RESULT.captures(line) {
            self.open = None;
            self.in_failed_list = false;
            let num = cap[1].to_owned();
            let name = cap[2].to_owned();
            let status = cap[3].trim();
            if status == "Passed" {
                self.passed[idx].add(n);
                return;
            }
            if status.starts_with("***Skipped") {
                return;
            }
            let status_clean = status.trim_start_matches('*').trim();
            let mut f = RawFailure::new(name, stream, n);
            f.error = short(status_clean, 220);
            if let Some(index) = push_failure_bounded(&mut self.failures, &mut self.overflowed, f) {
                self.by_num.insert(num, index);
                self.open = Some(CtestOpen { index, stream });
            }
            return;
        }

        // Reconcile `The following tests FAILED:` summary list (`  3 - test_parse (Failed)`).
        if self.in_failed_list
            && let Some(cap) = CTEST_FAILED_LIST.captures(line)
        {
            let num = &cap[1];
            let name = cap[2].to_owned();
            let reason = cap[3].trim();
            if let Some(&existing_idx) = self.by_num.get(num) {
                let f = &mut self.failures[existing_idx];
                if f.error.is_empty() || f.error == "Failed" {
                    f.error = short(reason, 220);
                }
            } else {
                let mut f = RawFailure::new(name, stream, n);
                f.error = short(reason, 220);
                f.detailed = false;
                if let Some(index) =
                    push_failure_bounded(&mut self.failures, &mut self.overflowed, f)
                {
                    self.by_num.insert(num.to_owned(), index);
                }
            }
            return;
        }

        // Captured `--output-on-failure` output right after `***Failed`.
        if let Some(open) = self.open.as_mut() {
            let f = &mut self.failures[open.index];
            f.end = n;
            let is_diagnostic = trimmed.contains("Assertion")
                || trimmed.contains("ASSERT")
                || trimmed.contains("FAILED")
                || trimmed.contains("Error")
                || trimmed.contains("error:")
                || trimmed.contains("Expected")
                || trimmed.contains("terminate called")
                || trimmed.contains("Segmentation fault");
            if is_diagnostic && (f.error == "Failed" || f.error.starts_with("Exception")) {
                f.error = short(trimmed, 220);
            } else if is_diagnostic && f.excerpt.len() < MAX_EXCERPT {
                f.excerpt.push(short(trimmed, 200));
            }
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(self) -> Option<ParsedReport> {
        if !self.saw_total {
            return None;
        }
        let mut passed = Vec::new();
        for stream in Stream::BOTH {
            let r = &self.passed[stream as usize];
            if r.count > 0 {
                passed.push(PassedSummary {
                    label: format!("ctest: {} passed test(s)", r.count),
                    stream,
                    first: r.first,
                    last: r.last,
                });
            }
        }
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed: self.reported_failed > 0,
            min_expected_failures: self.reported_failed,
            reported_clean: self.reported_failed == 0,
            overflowed: self.overflowed,
        })
    }
}

// ---------------------------------------------------------------------------
// 9. Elixir `mix test` Scanner
// ---------------------------------------------------------------------------

struct MixOpen {
    index: usize,
    stream: Stream,
    in_stacktrace: bool,
}

#[derive(Default)]
struct MixScan {
    failures: Vec<RawFailure>,
    open: Option<MixOpen>,
    saw_total: bool,
    reported_failures: usize,
    has_test_stderr: bool,
    passed: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl MixScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;
        if trimmed.is_empty() {
            return;
        }
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
        }

        // Header / footer chatter.
        if trimmed.starts_with("Running ExUnit with seed:")
            || trimmed.starts_with("Excluded tags:")
            || trimmed.starts_with("Including tags:")
            || trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Generated ")
            || trimmed.starts_with("Randomized with seed ")
        {
            return;
        }
        if trimmed.starts_with("Finished in ") {
            self.open = None;
            self.totals.push(short(trimmed, 220));
            return;
        }

        // Totals: `42 tests, 2 failures` or `3 doctests, 42 tests, 2 failures, 1 skipped`
        if !line.starts_with(' ')
            && let Some(cap) = MIX_TOTAL.captures(trimmed)
        {
            self.open = None;
            self.saw_total = true;
            self.reported_failures = cap[2].parse::<usize>().unwrap_or(0);
            self.totals.push(short(trimmed, 220));
            return;
        }

        // Failure block header: `  1) test creates order (MyApp.OrderTest)`
        if !line.starts_with("     ")
            && let Some(cap) = MIX_FAILURE_HEADER.captures(line)
            && (cap[2].starts_with("test ")
                || cap[2].starts_with("doctest ")
                || cap[2].starts_with("property ")
                || cap[2].ends_with(')'))
        {
            self.open = None;
            let f = RawFailure::new(cap[2].trim().to_owned(), stream, n);
            if let Some(index) = push_failure_bounded(&mut self.failures, &mut self.overflowed, f) {
                self.open = Some(MixOpen {
                    index,
                    stream,
                    in_stacktrace: false,
                });
            }
            return;
        }

        // Inside an open Mix failure block.
        if let Some(open) = self.open.as_mut() {
            if line.starts_with([' ', '\t']) {
                let f = &mut self.failures[open.index];
                f.end = n;
                if trimmed == "stacktrace:" {
                    open.in_stacktrace = true;
                    return;
                }
                if open.in_stacktrace {
                    if f.location.is_none()
                        && !trimmed.starts_with("(elixir ")
                        && !trimmed.starts_with("(ex_unit ")
                        && let Some(cap) = MIX_LOC.captures(trimmed)
                    {
                        f.location = Some(cap[1].to_owned());
                    }
                    return;
                }
                if f.location.is_none() && MIX_LOC.is_match(trimmed) && !trimmed.contains(' ') {
                    f.location = Some(trimmed.to_owned());
                    return;
                }
                if f.error.is_empty() {
                    f.error = short(trimmed, 220);
                } else if (trimmed.starts_with("code:")
                    || trimmed.starts_with("left:")
                    || trimmed.starts_with("right:")
                    || trimmed.starts_with("lhs:")
                    || trimmed.starts_with("rhs:")
                    || trimmed.starts_with("arguments:"))
                    && f.excerpt.len() < MAX_EXCERPT
                {
                    f.excerpt.push(short(trimmed, 200));
                }
                return;
            }
            self.open = None;
        }

        // Dot progress (`....*..`) or `--trace` passing line (`  * test foo (1.2ms)`).
        if trimmed.bytes().all(|b| matches!(b, b'.' | b'*'))
            || (line.starts_with("  * ") && trimmed.contains("ms)"))
        {
            self.passed[idx].add(n);
            return;
        }

        self.other[idx].observe(line, n);
    }

    fn finish(self) -> Option<ParsedReport> {
        if !self.saw_total {
            return None;
        }
        let mut passed = Vec::new();
        for stream in Stream::BOTH {
            let r = &self.passed[stream as usize];
            if r.count > 0 {
                passed.push(PassedSummary {
                    label: format!("mix test: {} passed/progress line(s)", r.count),
                    stream,
                    first: r.first,
                    last: r.last,
                });
            }
        }
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed: self.reported_failures > 0,
            min_expected_failures: self.reported_failures,
            reported_clean: self.reported_failures == 0,
            overflowed: self.overflowed,
        })
    }
}

// ---------------------------------------------------------------------------
// 10. Deno `deno test` Scanner
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum DenoSection {
    #[default]
    Progress,
    Errors,
    Failures,
}

struct DenoOpen {
    index: usize,
    stream: Stream,
    has_stack_loc: bool,
}

#[derive(Default)]
struct DenoScan {
    section: DenoSection,
    last_stream: Option<Stream>,
    failures: Vec<RawFailure>,
    seen_names: HashSet<String>,
    open: Option<DenoOpen>,
    saw_total: bool,
    reported_ok: bool,
    reported_failed_count: usize,
    reported_failed_banner: bool,
    has_test_stderr: bool,
    passed: [Range; 2],
    totals: Vec<String>,
    other: [Other; 2],
    overflowed: bool,
}

impl DenoScan {
    fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let idx = stream as usize;
        if trimmed.is_empty() {
            return;
        }
        if self.last_stream.is_some_and(|prev| prev != stream) {
            self.section = DenoSection::Progress;
            self.open = None;
        }
        self.last_stream = Some(stream);
        if self.open.as_ref().is_some_and(|o| o.stream != stream) {
            self.open = None;
        }

        // Final totals line: `ok | 42 passed | 0 failed (125ms)` or `FAILED | ...`
        if let Some(cap) = DENO_TOTAL.captures(trimmed) {
            self.open = None;
            self.section = DenoSection::Progress;
            self.saw_total = true;
            self.reported_failed_count = cap[3].parse::<usize>().unwrap_or(0);
            if &cap[1] == "ok" && self.reported_failed_count == 0 {
                self.reported_ok = true;
            } else {
                self.reported_failed_banner = true;
            }
            self.totals.push(short(trimmed, 220));
            return;
        }

        if trimmed == "ERRORS" {
            self.open = None;
            self.section = DenoSection::Errors;
            return;
        }
        if trimmed == "FAILURES" {
            self.open = None;
            self.section = DenoSection::Failures;
            return;
        }

        match self.section {
            DenoSection::Errors => {
                // Header: `test_name => ./tests/a_test.ts:14:6`
                if !line.starts_with([' ', '\t'])
                    && let Some((name, loc)) = trimmed.split_once(" => ")
                    && !name.trim().is_empty()
                    && !loc.trim().is_empty()
                {
                    self.open = None;
                    let test_name = name.trim().to_owned();
                    self.seen_names.insert(test_name.clone());
                    let mut f = RawFailure::new(test_name, stream, n);
                    let loc = loc.trim();
                    if !loc.starts_with("<anonymous>") {
                        f.location = Some(loc.to_owned());
                    }
                    if let Some(index) =
                        push_failure_bounded(&mut self.failures, &mut self.overflowed, f)
                    {
                        self.open = Some(DenoOpen {
                            index,
                            stream,
                            has_stack_loc: false,
                        });
                    }
                    return;
                }
                if let Some(open) = self.open.as_mut() {
                    let f = &mut self.failures[open.index];
                    f.end = n;
                    if trimmed.starts_with("at ") {
                        if !open.has_stack_loc
                            && let Some(cap) = DENO_FILE_LOC.captures(trimmed)
                        {
                            let loc = cap[1].to_owned();
                            if !is_deno_external_path(&loc) {
                                f.location = Some(loc);
                                open.has_stack_loc = true;
                            }
                        }
                        return;
                    }
                    // Skip decorative diff headers / caret pointers / throw line.
                    if trimmed == "[Diff] Actual / Expected"
                        || trimmed.chars().all(|c| c == '^' || c == '~')
                        || trimmed.starts_with("throw new ")
                    {
                        return;
                    }
                    if f.error.is_empty() {
                        let clean = trimmed.strip_prefix("error: ").unwrap_or(trimmed);
                        f.error = short(clean, 220);
                    } else if (trimmed.starts_with('-') || trimmed.starts_with('+'))
                        && f.excerpt.len() < MAX_EXCERPT
                    {
                        f.excerpt.push(short(trimmed, 200));
                    }
                    return;
                }
            }
            DenoSection::Failures => {
                // Summary repeat `test_name => file:line:col` after `ERRORS`.
                if let Some((name, loc)) = trimmed.split_once(" => ") {
                    let test_name = name.trim().to_owned();
                    if !test_name.is_empty() && !self.seen_names.contains(&test_name) {
                        self.seen_names.insert(test_name.clone());
                        let mut f = RawFailure::new(test_name, stream, n);
                        f.detailed = false;
                        if !loc.trim().starts_with("<anonymous>") {
                            f.location = Some(loc.trim().to_owned());
                        }
                        push_failure_bounded(&mut self.failures, &mut self.overflowed, f);
                    }
                    return;
                }
            }
            DenoSection::Progress => {
                if trimmed.starts_with("Check ")
                    || trimmed.starts_with("running ")
                    || trimmed.starts_with("------- output")
                    || trimmed.starts_with("----- output end")
                {
                    if trimmed.starts_with("Check ") && stream == Stream::Stderr {
                        self.has_test_stderr = true;
                    }
                    return;
                }
                if trimmed.contains(" ... ok ")
                    || trimmed.ends_with(" ... ok")
                    || trimmed.contains(" ... ignored ")
                {
                    self.passed[idx].add(n);
                    return;
                }
                if trimmed.contains(" ... FAILED ") || trimmed.ends_with(" ... FAILED") {
                    return;
                }
            }
        }

        self.other[idx].observe(line, n);
    }

    fn finish(self) -> Option<ParsedReport> {
        if !self.saw_total {
            return None;
        }
        let mut passed = Vec::new();
        for stream in Stream::BOTH {
            let r = &self.passed[stream as usize];
            if r.count > 0 {
                passed.push(PassedSummary {
                    label: format!("deno test: {} passed/ignored test(s)", r.count),
                    stream,
                    first: r.first,
                    last: r.last,
                });
            }
        }
        let (notes, compact_notes, covers_stderr) = build_notes(&self.other, self.has_test_stderr);
        Some(ParsedReport {
            failures: self.failures,
            passed,
            totals: self.totals,
            notes,
            compact_notes,
            covers_stderr,
            reported_failed: self.reported_failed_banner || self.reported_failed_count > 0,
            min_expected_failures: self.reported_failed_count,
            reported_clean: self.reported_ok,
            overflowed: self.overflowed,
        })
    }
}

// ---------------------------------------------------------------------------
// Comprehensive Unit Tests (All 10 Runners)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_BUDGET: usize = 8192;

    /// Extract 1-based inclusive line slice `[start..=end]` from a stream.
    fn slice_lines(raw: &str, start: usize, end: usize) -> String {
        let lines: Vec<&str> = raw.lines().collect();
        assert!(start >= 1, "start must be 1-based, got {start}");
        assert!(end >= start, "end {end} < start {start}");
        assert!(
            end <= lines.len(),
            "end {end} exceeds stream line count {}",
            lines.len()
        );
        lines[start - 1..end].join("\n")
    }

    fn assert_valid_pointers(report: &CompactReport, stdout: &str, stderr: &str) {
        for item in &report.items {
            let src = match item.stream {
                "stdout" => stdout,
                "stderr" => stderr,
                other => panic!("unexpected stream {other}"),
            };
            let excerpt = slice_lines(src, item.start, item.end);
            assert!(
                !excerpt.trim().is_empty(),
                "empty slice for {} at {}:{}-{}",
                item.test,
                item.stream,
                item.start,
                item.end
            );
        }
    }

    // -----------------------------------------------------------------------
    // 1. Pytest
    // -----------------------------------------------------------------------

    #[test]
    fn pytest_clean_pass_compacts_verbose_output() {
        let mut out = String::from(
            "============================= test session starts ==============================\n\
             platform linux -- Python 3.12.3, pytest-8.2.0, pluggy-1.5.0\n\
             rootdir: /repo\n\
             collected 40 items\n\n",
        );
        for i in 1..=40 {
            out.push_str(&format!(
                "tests/test_orders.py::test_order_case_{i:02} PASSED [{:>3}%]\n",
                i * 100 / 40
            ));
        }
        out.push_str(
            "\n============================== 40 passed in 1.24s ==============================\n",
        );

        let report = format(Runner::Pytest, &out, "", Some(0), DEFAULT_BUDGET).unwrap();
        assert!(report.items.is_empty());
        assert!(report.covers_stderr);
        assert!(
            report.text.contains("40 passed in 1.24s"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("pytest: 40 progress/pass line(s) [stdout:6-45]"),
            "{}",
            report.text
        );
        assert!(
            report.text.len() < out.len() / 5,
            "got {} vs {}",
            report.text.len(),
            out.len()
        );
    }

    #[test]
    fn pytest_failures_errors_and_parametrized_grouping() {
        let mut out = String::from(
            "============================= test session starts ==============================\n\
             platform linux -- Python 3.12.3, pytest-8.2.0, pluggy-1.5.0\n\
             collected 55 items / 1 error\n\n\
             tests/test_parser.py FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF [ 90%]\n\
             tests/test_billing.py .F.. [100%]\n\n\
             ==================================== ERRORS ====================================\n\
             _____________________ ERROR collecting tests/test_broken.py ____________________\n\
             tests/test_broken.py:3: in <module>\n\
                 import missing_pkg\n\
             E   ModuleNotFoundError: No module named 'missing_pkg'\n\
             =================================== FAILURES ===================================\n",
        );
        for i in 1..=50 {
            out.push_str(&format!(
                "___________________________ test_parse[case_{i}] ___________________________\n\
                 x = 'case_{i}'\n\n\
                     def test_parse(x):\n\
                 >       assert parse(x) == 200\n\
                 E       AssertionError: expected status 200, got 500 at 0x7f123456{i:04x}\n\
                 E       assert 500 == 200\n\n\
                 tests/test_parser.py:42: AssertionError\n"
            ));
        }
        out.push_str(
            "___________________________ test_charge_tax ____________________________\n\
                 def test_charge_tax():\n\
             >       compute_tax(-1)\n\
             E       ValueError: negative amount not allowed\n\n\
             src/billing.py:88: ValueError\n\
             =========================== short test summary info ============================\n",
        );
        for i in 1..=50 {
            out.push_str(&format!(
                "FAILED tests/test_parser.py::test_parse[case_{i}] - AssertionError: expected status 200, got 500\n"
            ));
        }
        out.push_str(
            "FAILED tests/test_billing.py::test_charge_tax - ValueError: negative amount not allowed\n\
             ERROR tests/test_broken.py - ModuleNotFoundError: No module named 'missing_pkg'\n\
             =================== 51 failed, 3 passed, 1 error in 2.41s ======================\n",
        );

        let report = format(Runner::Pytest, &out, "", Some(1), DEFAULT_BUDGET).unwrap();
        assert_eq!(report.items.len(), 52);
        assert_valid_pointers(&report, &out, "");

        // Parametrized family collapsed with normalized hex address.
        assert!(
            report
                .text
                .contains("50 test(s): tests/test_parser.py::test_parse[...]"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("AssertionError: expected status 200, got 500"),
            "{}",
            report.text
        );
        // Collection error & distinct ValueError both preserved with root causes.
        assert!(
            report
                .text
                .contains("ModuleNotFoundError: No module named 'missing_pkg'"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("ValueError: negative amount not allowed"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("51 failed, 3 passed, 1 error in 2.41s"),
            "{}",
            report.text
        );

        // Verify exact line pointers for collection error and test_charge_tax.
        let broken = report
            .items
            .iter()
            .find(|i| i.test == "tests/test_broken.py")
            .unwrap();
        let broken_slice = slice_lines(&out, broken.start, broken.end);
        assert!(broken_slice.contains("ERROR collecting tests/test_broken.py"));
        assert!(broken_slice.contains("ModuleNotFoundError"));

        let tax = report
            .items
            .iter()
            .find(|i| i.test == "tests/test_billing.py::test_charge_tax")
            .unwrap();
        let tax_slice = slice_lines(&out, tax.start, tax.end);
        assert!(tax_slice.contains("test_charge_tax"));
        assert!(tax_slice.contains("ValueError: negative amount not allowed"));

        assert!(
            report.text.len() * 10 < out.len(),
            "expected >90% reduction: {} vs {}",
            report.text.len(),
            out.len()
        );
    }

    #[test]
    fn pytest_fails_closed_on_crash_and_count_contradiction() {
        // Nonzero exit with zero failures parsed (e.g. startup crash).
        let crash =
            "INTERNALERROR> Traceback (most recent call last):\nRuntimeError: broken plugin\n";
        assert!(format(Runner::Pytest, crash, "", Some(3), DEFAULT_BUDGET).is_none());

        // Truncated output: claims 5 failed, only 1 failure present.
        let truncated = "\
            =================================== FAILURES ===================================\n\
            _________________________________ test_only_one ________________________________\n\
            E   AssertionError: boom\n\
            tests/test_a.py:10: AssertionError\n\
            =========================== 5 failed, 10 passed in 0.50s =======================\n";
        assert!(format(Runner::Pytest, truncated, "", Some(1), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // 2. Python unittest
    // -----------------------------------------------------------------------

    #[test]
    fn unittest_clean_pass_on_stderr() {
        let mut err = String::new();
        for i in 1..=30 {
            err.push_str(&format!(
                "test_case_{i:02} (tests.test_service.TestService) ... ok\n"
            ));
        }
        err.push_str(
            "\n----------------------------------------------------------------------\n\
             Ran 30 tests in 0.142s\n\n\
             OK\n",
        );

        let report = format(Runner::Unittest, "", &err, Some(0), DEFAULT_BUDGET).unwrap();
        assert!(report.items.is_empty());
        assert!(report.covers_stderr);
        assert!(
            report.text.contains("Ran 30 tests in 0.142s"),
            "{}",
            report.text
        );
        assert!(report.text.contains("OK"), "{}", report.text);
        assert!(report.text.contains("[stderr:1-30]"), "{}", report.text);
        assert!(report.text.len() < err.len() / 5);
    }

    #[test]
    fn unittest_failures_and_errors_with_dedup_and_pointers() {
        let mut err = String::new();
        for i in 1..=20 {
            err.push_str(&format!(
                "test_ok_{i:02} (tests.test_calc.TestCalc) ... ok\n"
            ));
        }
        err.push_str(
            "test_div_a (tests.test_calc.TestCalc) ... FAIL\n\
             test_div_b (tests.test_calc.TestCalc) ... FAIL\n\
             test_connect (tests.test_db.TestDb) ... ERROR\n\
             ======================================================================\n\
             ERROR: test_connect (tests.test_db.TestDb)\n\
             ----------------------------------------------------------------------\n\
             Traceback (most recent call last):\n\
               File \"/repo/tests/test_db.py\", line 19, in test_connect\n\
                 connect('/tmp/tmp8a9b1c/db.sock')\n\
               File \"/repo/src/db.py\", line 44, in connect\n\
                 raise ConnectionError('socket missing at /tmp/tmp8a9b1c/db.sock')\n\
             ConnectionError: socket missing at /tmp/tmp8a9b1c/db.sock\n\n\
             ======================================================================\n\
             FAIL: test_div_a (tests.test_calc.TestCalc)\n\
             ----------------------------------------------------------------------\n\
             Traceback (most recent call last):\n\
               File \"/repo/tests/test_calc.py\", line 31, in test_div_a\n\
                 self.assertEqual(div(10, 2), 6)\n\
             AssertionError: 5 != 6\n\n\
             ======================================================================\n\
             FAIL: test_div_b (tests.test_calc.TestCalc)\n\
             ----------------------------------------------------------------------\n\
             Traceback (most recent call last):\n\
               File \"/repo/tests/test_calc.py\", line 31, in test_div_b\n\
                 self.assertEqual(div(10, 2), 6)\n\
             AssertionError: 5 != 6\n\n\
             ----------------------------------------------------------------------\n\
             Ran 23 tests in 0.088s\n\n\
             FAILED (failures=2, errors=1)\n",
        );

        let report = format(Runner::Unittest, "", &err, Some(1), DEFAULT_BUDGET).unwrap();
        assert_eq!(report.items.len(), 3);
        assert!(report.covers_stderr);
        assert_valid_pointers(&report, "", &err);
        assert!(
            report.text.contains("2 test(s): AssertionError: 5 != 6"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("ConnectionError: socket missing"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("FAILED (failures=2, errors=1)"),
            "{}",
            report.text
        );

        let conn = report
            .items
            .iter()
            .find(|i| i.test.contains("test_connect"))
            .unwrap();
        assert_eq!(conn.stream, "stderr");
        let conn_slice = slice_lines(&err, conn.start, conn.end);
        assert!(conn_slice.contains("ERROR: test_connect"));
        assert!(conn_slice.contains("ConnectionError: socket missing"));
        assert!(report.text.len() < err.len() / 2);
    }

    #[test]
    fn unittest_fails_closed_on_crash() {
        let err = "Traceback (most recent call last):\nImportError: cannot import name 'foo'\n";
        assert!(format(Runner::Unittest, "", err, Some(1), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // 3. Go `go test`
    // -----------------------------------------------------------------------

    #[test]
    fn go_test_collapses_many_ok_packages() {
        let mut out = String::new();
        for i in 1..=35 {
            out.push_str(&format!(
                "ok  \tgithub.com/acme/platform/pkg/service{i:02}\t0.{i:03}s\n"
            ));
        }
        out.push_str("?\tgithub.com/acme/platform/cmd/cli\t[no test files]\n");
        out.push_str("?\tgithub.com/acme/platform/cmd/worker\t[no test files]\n");

        let report = format(Runner::GoTest, &out, "", Some(0), DEFAULT_BUDGET).unwrap();
        assert!(report.items.is_empty());
        assert!(
            report
                .text
                .contains("go test: 35 passed package(s), 2 without tests [stdout:1-37]"),
            "{}",
            report.text
        );
        assert!(report.text.len() * 10 < out.len());
    }

    #[test]
    fn go_test_failures_data_race_panic_and_build_error() {
        let mut out = String::new();
        for i in 1..=25 {
            out.push_str(&format!("ok  \tgithub.com/acme/app/pkg/p{i:02}\t0.012s\n"));
        }
        out.push_str(concat!(
            "--- FAIL: TestOrder (0.02s)\n",
            "    --- FAIL: TestOrder/case_1 (0.01s)\n",
            "        order_test.go:42: expected status 200, got 500\n",
            "    --- FAIL: TestOrder/case_2 (0.01s)\n",
            "        order_test.go:42: expected status 200, got 500\n",
            "==================\n",
            "WARNING: DATA RACE\n",
            "Write at 0x00c00012a010 by goroutine 7:\n",
            "  github.com/acme/app/cache.(*Store).Set()\n",
            "      /repo/cache/store.go:42 +0x88\n\n",
            "Previous read at 0x00c00012a010 by goroutine 6:\n",
            "  github.com/acme/app/cache.(*Store).Get()\n",
            "      /repo/cache/store.go:28 +0x64\n",
            "==================\n",
            "panic: runtime error: index out of range [3] with length 2\n\n",
            "goroutine 19 [running]:\n",
            "github.com/acme/app/parser.Parse(...)\n",
            "\t/repo/parser/parse.go:91 +0x120\n",
            "FAIL\tgithub.com/acme/app/order\t0.140s\n",
            "FAIL\tgithub.com/acme/app/broken [build failed]\n",
        ));
        let err = "\
            # github.com/acme/app/broken [github.com/acme/app/broken.test]\n\
            broken/main_test.go:15:10: undefined: MissingSymbol\n";

        let report = format(Runner::GoTest, &out, err, Some(1), DEFAULT_BUDGET).unwrap();
        // 2 subtests (empty parent TestOrder folded away) + 1 DATA RACE + 1 panic + 1 build failed = 5
        assert_eq!(report.items.len(), 5, "{:#?}", report.items);
        assert_valid_pointers(&report, &out, err);
        assert!(
            report
                .text
                .contains("go test: 25 passed package(s) [stdout:1-25]"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("2 test(s): expected status 200, got 500"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("WARNING: DATA RACE"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("panic: runtime error: index out of range"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("[build failed] broken/main_test.go:15:10: undefined: MissingSymbol"),
            "{}",
            report.text
        );
        assert!(report.text.len() < out.len() + err.len());
    }

    #[test]
    fn go_test_fails_closed_on_missing_failure_details() {
        let out = "FAIL\tgithub.com/acme/app/mystery\t0.010s\n";
        assert!(format(Runner::GoTest, out, "", Some(1), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // 4. .NET `dotnet test`
    // -----------------------------------------------------------------------

    #[test]
    fn dotnet_clean_pass_and_multi_failure_with_workspace_frame() {
        let mut pass_out = String::from(
            "  Determining projects to restore...\n\
               All projects are up-to-date for restore.\n\
               MyApp -> /repo/src/MyApp/bin/Debug/net8.0/MyApp.dll\n\
               MyApp.Tests -> /repo/tests/MyApp.Tests/bin/Debug/net8.0/MyApp.Tests.dll\n\
             Test run for /repo/tests/MyApp.Tests/bin/Debug/net8.0/MyApp.Tests.dll (.NETCoreApp,Version=v8.0)\n\
             Microsoft (R) Test Execution Command Line Tool Version 17.9.0\n\
             Starting test execution, please wait...\n\
             A total of 1 test files matched the specified pattern.\n",
        );
        for i in 1..=25 {
            pass_out.push_str(&format!(
                "  Passed MyApp.Tests.OrderTests.TestCase{i:02} [3 ms]\n"
            ));
        }
        pass_out.push_str(
            "\nPassed!  - Failed:     0, Passed:    25, Skipped:     0, Total:    25, Duration: 412 ms - MyApp.Tests.dll (net8.0)\n",
        );
        let pass = format(Runner::Dotnet, &pass_out, "", Some(0), DEFAULT_BUDGET).unwrap();
        assert!(pass.items.is_empty());
        assert!(
            pass.text
                .contains("Passed!  - Failed:     0, Passed:    25")
        );
        assert!(pass.text.len() < pass_out.len() / 5);

        let mut fail_out = String::from(
            "  Determining projects to restore...\n\
               All projects are up-to-date for restore.\n\
             Test run for /repo/tests/MyApp.Tests/bin/Debug/net8.0/MyApp.Tests.dll (.NETCoreApp,Version=v8.0)\n\
             Starting test execution, please wait...\n",
        );
        for i in 1..=15 {
            fail_out.push_str(&format!(
                "  Passed MyApp.Tests.OrderTests.Ok{i:02} [2 ms]\n"
            ));
        }
        fail_out.push_str(
            "  Failed MyApp.Tests.OrderTests.Create(qty: -1) [12 ms]\n\
               Error Message:\n\
                Assert.Equal() Failure: Values differ\n\
                Expected: 200\n\
                Actual:   400\n\
               Stack Trace:\n\
                  at System.RuntimeMethodHandle.InvokeMethod(Object target)\n\
                  at MyApp.Tests.OrderTests.Create(Int32 qty) in /repo/tests/OrderTests.cs:line 42\n\
               Failed MyApp.Tests.OrderTests.Create(qty: 0) [9 ms]\n\
               Error Message:\n\
                Assert.Equal() Failure: Values differ\n\
                Expected: 200\n\
                Actual:   400\n\
               Stack Trace:\n\
                  at MyApp.Tests.OrderTests.Create(Int32 qty) in /repo/tests/OrderTests.cs:line 42\n\
               Failed MyApp.Tests.BillingTests.Tax [15 ms]\n\
               Error Message:\n\
                System.InvalidOperationException : Currency not configured\n\
               Stack Trace:\n\
                  at MyApp.Billing.Calculate() in /repo/src/Billing.cs:line 77\n\n\
             Failed!  - Failed:     3, Passed:    15, Skipped:     0, Total:    18, Duration: 280 ms - MyApp.Tests.dll (net8.0)\n",
        );
        let report = format(Runner::Dotnet, &fail_out, "", Some(1), DEFAULT_BUDGET).unwrap();
        assert_eq!(report.items.len(), 3);
        assert_valid_pointers(&report, &fail_out, "");
        assert!(
            report
                .text
                .contains("2 test(s): MyApp.Tests.OrderTests.Create(...)"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("/repo/tests/OrderTests.cs:42"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("Currency not configured"),
            "{}",
            report.text
        );
        assert!(report.text.len() < fail_out.len() / 2);

        // Fail-closed on build crash before tests.
        let compile_err = "src/Order.cs(10,5): error CS1002: ; expected\nBuild FAILED.\n";
        assert!(format(Runner::Dotnet, compile_err, "", Some(1), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // 5. Maven `mvn test`
    // -----------------------------------------------------------------------

    #[test]
    fn maven_pass_failures_reconciliation_and_fail_closed() {
        let mut pass_out = String::from(
            "[INFO] Scanning for projects...\n\
             [INFO] --- surefire:3.2.5:test (default-test) @ demo-app ---\n\
             [INFO] -------------------------------------------------------\n\
             [INFO]  T E S T S\n\
             [INFO] -------------------------------------------------------\n",
        );
        for i in 1..=20 {
            pass_out.push_str(&format!(
                "[INFO] Running com.example.Service{i:02}Test\n\
                 [INFO] Tests run: 5, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.120 s -- in com.example.Service{i:02}Test\n"
            ));
        }
        pass_out.push_str(
            "[INFO] \n\
             [INFO] Results:\n\
             [INFO] \n\
             [INFO] Tests run: 100, Failures: 0, Errors: 0, Skipped: 0\n\
             [INFO] \n\
             [INFO] BUILD SUCCESS\n",
        );
        let pass = format(Runner::Maven, &pass_out, "", Some(0), DEFAULT_BUDGET).unwrap();
        assert!(pass.items.is_empty());
        assert!(pass.text.contains("mvn test: 20 passed test class(es)"));
        assert!(
            pass.text
                .contains("Tests run: 100, Failures: 0, Errors: 0, Skipped: 0")
        );
        assert!(pass.text.len() < pass_out.len() / 8);

        let mut fail_out = String::from(
            "[INFO] Scanning for projects...\n\
             [INFO] --- surefire:3.2.5:test (default-test) @ demo-app ---\n\
             [INFO]  T E S T S\n",
        );
        for i in 1..=12 {
            fail_out.push_str(&format!(
                "[INFO] Running com.example.Pass{i:02}Test\n\
                 [INFO] Tests run: 4, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.050 s -- in com.example.Pass{i:02}Test\n"
            ));
        }
        fail_out.push_str(
            "[INFO] Running com.example.OrderTest\n\
             [ERROR] Tests run: 5, Failures: 2, Errors: 1, Skipped: 0, Time elapsed: 0.210 s <<< FAILURE! -- in com.example.OrderTest\n\
             [ERROR] com.example.OrderTest.testCreate -- Time elapsed: 0.041 s <<< FAILURE!\n\
             org.opentest4j.AssertionFailedError: expected: <200> but was: <500>\n\
             \tat org.junit.jupiter.api.AssertionFailureBuilder.build(AssertionFailureBuilder.java:151)\n\
             \tat com.example.OrderTest.testCreate(OrderTest.java:42)\n\
             [ERROR] com.example.OrderTest.testUpdate -- Time elapsed: 0.039 s <<< FAILURE!\n\
             org.opentest4j.AssertionFailedError: expected: <200> but was: <500>\n\
             \tat com.example.OrderTest.testUpdate(OrderTest.java:42)\n\
             [ERROR] com.example.OrderTest.testCharge -- Time elapsed: 0.022 s <<< ERROR!\n\
             java.lang.NullPointerException: Cannot invoke \"Gateway.pay()\" because \"gw\" is null\n\
             \tat com.example.OrderTest.testCharge(OrderTest.java:91)\n\
             [INFO] \n\
             [INFO] Results:\n\
             [INFO] \n\
             [ERROR] Failures: \n\
             [ERROR]   OrderTest.testCreate:42 expected: <200> but was: <500>\n\
             [ERROR]   OrderTest.testUpdate:42 expected: <200> but was: <500>\n\
             [ERROR] Errors: \n\
             [ERROR]   OrderTest.testCharge:91 » NullPointer Cannot invoke \"Gateway.pay()\"\n\
             [INFO] \n\
             [ERROR] Tests run: 53, Failures: 2, Errors: 1, Skipped: 0\n\
             [INFO] \n\
             [INFO] BUILD FAILURE\n",
        );

        let report = format(Runner::Maven, &fail_out, "", Some(1), DEFAULT_BUDGET).unwrap();
        // Must reconcile inline + summary blocks to exact 3 failures (never 6!).
        assert_eq!(report.items.len(), 3, "{:#?}", report.items);
        assert_valid_pointers(&report, &fail_out, "");
        assert!(
            report
                .text
                .contains("2 test(s): org.opentest4j.AssertionFailedError: expected: <200> but was: <500> · OrderTest.java:42"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("NullPointerException"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("[ERROR] Tests run: 53, Failures: 2, Errors: 1, Skipped: 0"),
            "{}",
            report.text
        );
        assert!(report.text.len() < fail_out.len() / 2);

        // Fail-closed on javac compilation failure.
        let compile_fail = "\
            [INFO] Compiling 12 source files\n\
            [ERROR] COMPILATION ERROR :\n\
            [ERROR] /repo/src/Foo.java:[10,5] cannot find symbol\n\
            [INFO] BUILD FAILURE\n";
        assert!(format(Runner::Maven, compile_fail, "", Some(1), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // 6. Gradle `gradle test`
    // -----------------------------------------------------------------------

    #[test]
    fn gradle_pass_failures_and_fail_closed() {
        let mut pass_out = String::from(
            "> Task :core:compileJava UP-TO-DATE\n\
             > Task :core:compileTestJava UP-TO-DATE\n\
             > Task :core:test\n",
        );
        for i in 1..=25 {
            pass_out.push_str(&format!("com.example.CoreTest > testCase{i:02}() PASSED\n"));
        }
        pass_out.push_str(
            "\nBUILD SUCCESSFUL in 2s\n\
             3 actionable tasks: 1 executed, 2 up-to-date\n",
        );
        let pass = format(Runner::Gradle, &pass_out, "", Some(0), DEFAULT_BUDGET).unwrap();
        assert!(pass.items.is_empty());
        assert!(pass.text.contains("gradle test: 25 passed/skipped test(s)"));
        assert!(pass.text.contains("BUILD SUCCESSFUL in 2s"));
        assert!(pass.text.len() < pass_out.len() / 5);

        let mut fail_out = String::from(
            "> Task :api:compileJava UP-TO-DATE\n\
             > Task :api:test\n",
        );
        for i in 1..=20 {
            fail_out.push_str(&format!("com.example.ApiTest > testPass{i:02}() PASSED\n"));
        }
        fail_out.push_str(
            "\ncom.example.ApiTest > parse(String)[1] FAILED\n\
             \torg.opentest4j.AssertionFailedError: expected: <200> but was: <500> at ApiTest.java:42\n\
             \t\tCaused by: java.lang.IllegalStateException: empty body\n\n\
             com.example.ApiTest > parse(String)[2] FAILED\n\
             \torg.opentest4j.AssertionFailedError: expected: <200> but was: <500> at ApiTest.java:42\n\
             \t\tCaused by: java.lang.IllegalStateException: empty body\n\n\
             com.example.BillingTest > charge() FAILED\n\
             \tjava.lang.RuntimeException: timeout\n\
             \t\tat app//com.example.BillingTest.charge(BillingTest.java:71)\n\n\
             23 tests completed, 3 failed\n\n\
             > Task :api:test FAILED\n\n\
             BUILD FAILED in 4s\n",
        );
        let fail_err = "\
            FAILURE: Build failed with an exception.\n\n\
            * What went wrong:\n\
            Execution failed for task ':api:test'.\n\
            > There were failing tests. See the report at: file:///repo/api/build/reports/tests/test/index.html\n\n\
            * Try:\n\
            > Run with --stacktrace option to get the stack trace.\n";

        let report = format(Runner::Gradle, &fail_out, fail_err, Some(1), DEFAULT_BUDGET).unwrap();
        assert_eq!(report.items.len(), 3);
        assert!(report.covers_stderr);
        assert_valid_pointers(&report, &fail_out, fail_err);
        assert!(
            report
                .text
                .contains("2 test(s): com.example.ApiTest > parse(String)[...]"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("> Task :api:test FAILED"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("23 tests completed, 3 failed"),
            "{}",
            report.text
        );
        assert!(report.text.len() < (fail_out.len() + fail_err.len()) / 2);

        // Fail-closed on compileJava failure.
        let compile_fail = "> Task :api:compileJava FAILED\nBUILD FAILED in 1s\n";
        assert!(format(Runner::Gradle, compile_fail, "", Some(1), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // 7. Swift `swift test` (XCTest + Swift Testing)
    // -----------------------------------------------------------------------

    #[test]
    fn swift_xctest_and_swift_testing_pass_fail_and_fail_closed() {
        let mut pass_out = String::from(
            "Building for debugging...\n\
             [1/3] Compiling MyPkg Order.swift\n\
             Build complete! (0.85s)\n\
             Test Suite 'All tests' started at 2026-11-25 10:00:00.000.\n\
             Test Suite 'MyPkgTests.xctest' started at 2026-11-25 10:00:00.001.\n\
             Test Suite 'OrderTests' started at 2026-11-25 10:00:00.002.\n",
        );
        for i in 1..=25 {
            pass_out.push_str(&format!(
                "Test Case '-[MyPkgTests.OrderTests testCase{i:02}]' started.\n\
                 Test Case '-[MyPkgTests.OrderTests testCase{i:02}]' passed (0.001 seconds).\n"
            ));
        }
        pass_out.push_str(
            "Test Suite 'OrderTests' passed at 2026-11-25 10:00:00.050.\n\
             \t Executed 25 tests, with 0 failures (0 unexpected) in 0.025 (0.048) seconds\n\
             Test Suite 'All tests' passed at 2026-11-25 10:00:00.051.\n\
             \t Executed 25 tests, with 0 failures (0 unexpected) in 0.025 (0.050) seconds\n",
        );
        let pass = format(Runner::Swift, &pass_out, "", Some(0), DEFAULT_BUDGET).unwrap();
        assert!(pass.items.is_empty());
        assert!(pass.text.contains("swift test: 25 passed test(s)"));
        assert!(pass.text.contains("Executed 25 tests, with 0 failures"));
        assert!(pass.text.len() < pass_out.len() / 10);

        // Mixed XCTest + Swift Testing failures!
        let mut fail_out = String::from(
            "Building for debugging...\n\
             Build complete! (0.40s)\n\
             Test Suite 'All tests' started at 2026-11-25 10:00:00.000.\n",
        );
        for i in 1..=15 {
            fail_out.push_str(&format!(
                "Test Case 'OrderTests.testOk{i:02}' started.\n\
                 Test Case 'OrderTests.testOk{i:02}' passed (0.001 seconds).\n"
            ));
        }
        fail_out.push_str(
            "Test Case 'OrderTests.testTotal' started.\n\
             /repo/Tests/OrderTests.swift:42: error: OrderTests.testTotal : XCTAssertEqual failed: (\"500\") is not equal to (\"200\")\n\
             Test Case 'OrderTests.testTotal' failed (0.003 seconds).\n\
             Test Suite 'All tests' failed at 2026-11-25 10:00:00.100.\n\
             \t Executed 16 tests, with 1 failure (0 unexpected) in 0.020 (0.030) seconds\n\
             ◇ Test run started.\n\
             ◇ Suite \"ParserTests\" started.\n\
             ✔ Test \"parses valid header\" passed after 0.001 seconds.\n\
             ✘ Test \"rejects empty body\" recorded an issue at ParserTests.swift:18:5: Expectation failed: (code → 500) == 400\n\
             ✘ Test \"rejects empty body\" failed after 0.002 seconds with 1 issue.\n\
             ✘ Test run with 2 tests failed after 0.005 seconds with 1 issue.\n",
        );
        let report = format(Runner::Swift, &fail_out, "", Some(1), DEFAULT_BUDGET).unwrap();
        assert_eq!(report.items.len(), 2, "{:#?}", report.items);
        assert_valid_pointers(&report, &fail_out, "");
        assert!(
            report.text.contains("XCTAssertEqual failed"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("Expectation failed: (code → 500) == 400"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("Executed 16 tests, with 1 failure"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("Test run with 2 tests failed"),
            "{}",
            report.text
        );
        assert!(report.text.len() < fail_out.len() / 2);

        // Fail-closed on Swift compiler error.
        let compile_err = "/repo/Sources/App.swift:12:5: error: cannot find 'foo' in scope\n";
        assert!(format(Runner::Swift, "", compile_err, Some(1), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // 8. CTest
    // -----------------------------------------------------------------------

    #[test]
    fn ctest_pass_failures_and_fail_closed() {
        let mut pass_out = String::from("Test project /repo/build\n");
        for i in 1..=30 {
            let tag = format!("#{i}");
            pass_out.push_str(&format!(
                "      Start {i:>2}: test_unit_{i:02}\n\
                 {i:>2}/30 Test {tag:>3}: test_unit_{i:02} ......................   Passed    0.01 sec\n"
            ));
        }
        pass_out.push_str(
            "\n100% tests passed, 0 tests failed out of 30\n\n\
             Total Test time (real) =   0.35 sec\n",
        );
        let pass = format(Runner::Ctest, &pass_out, "", Some(0), DEFAULT_BUDGET).unwrap();
        assert!(pass.items.is_empty());
        assert!(pass.text.contains("ctest: 30 passed test(s)"));
        assert!(
            pass.text
                .contains("100% tests passed, 0 tests failed out of 30")
        );
        assert!(pass.text.len() < pass_out.len() / 10);

        let mut fail_out = String::from("Test project /repo/build\n");
        for i in 1..=18 {
            let tag = format!("#{i}");
            fail_out.push_str(&format!(
                "      Start {i:>2}: test_ok_{i:02}\n\
                 {i:>2}/20 Test {tag:>3}: test_ok_{i:02} ........................   Passed    0.01 sec\n"
            ));
        }
        fail_out.push_str(
            "      Start 19: test_parser\n\
             19/20 Test #19: test_parser ......................***Failed    0.15 sec\n\
             Running parser suite...\n\
             tests/parser_test.cpp:42: Assertion `status == 200` failed.\n\
                   Start 20: test_network\n\
             20/20 Test #20: test_network .....................***Timeout   5.00 sec\n\n\
             90% tests passed, 2 tests failed out of 20\n\n\
             Total Test time (real) =   5.35 sec\n\n\
             The following tests FAILED:\n\
             \t 19 - test_parser (Failed)\n\
             \t 20 - test_network (Timeout)\n",
        );
        let fail_err = "Errors while running CTest\n";

        let report = format(Runner::Ctest, &fail_out, fail_err, Some(8), DEFAULT_BUDGET).unwrap();
        assert_eq!(report.items.len(), 2, "{:#?}", report.items);
        assert!(report.covers_stderr);
        assert_valid_pointers(&report, &fail_out, fail_err);
        assert!(
            report
                .text
                .contains("tests/parser_test.cpp:42: Assertion `status == 200` failed."),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("test_network · Timeout"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("90% tests passed, 2 tests failed out of 20")
        );
        assert!(report.text.len() < fail_out.len() / 3);

        // Fail-closed if summary reports 3 failed but only 1 parsed.
        let bad = "1/3 Test #1: only_one ...***Failed 0.1 sec\n67% tests passed, 3 tests failed out of 3\n";
        assert!(format(Runner::Ctest, bad, "", Some(8), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // 9. Elixir `mix test`
    // -----------------------------------------------------------------------

    #[test]
    fn mix_pass_failures_and_fail_closed() {
        let mut pass_out = String::from(
            "Compiling 2 files (.ex)\n\
             Running ExUnit with seed: 123456, max_cases: 16\n\n",
        );
        for i in 1..=25 {
            pass_out.push_str(&format!("  * test order case {i:02} (1.2ms) [L#{i}]\n"));
        }
        pass_out.push_str(
            "\nFinished in 0.4 seconds (0.1s async, 0.3s sync)\n\
             25 tests, 0 failures\n\n\
             Randomized with seed 123456\n",
        );
        let pass = format(Runner::Mix, &pass_out, "", Some(0), DEFAULT_BUDGET).unwrap();
        assert!(pass.items.is_empty());
        assert!(pass.text.contains("mix test: 25 passed/progress line(s)"));
        assert!(pass.text.contains("25 tests, 0 failures"));
        assert!(pass.text.len() < pass_out.len() / 6);

        let fail_out = concat!(
            "Running ExUnit with seed: 987654, max_cases: 16\n\n",
            "......................\n\n",
            "  1) test creates an order (MyApp.OrderTest)\n",
            "     test/my_app/order_test.exs:42\n",
            "     Assertion with == failed\n",
            "     code:  assert Order.create(params) == {:ok, 200}\n",
            "     left:  {:error, :invalid}\n",
            "     right: {:ok, 200}\n",
            "     stacktrace:\n",
            "       test/my_app/order_test.exs:45: (test)\n\n",
            "  2) test updates an order (MyApp.OrderTest)\n",
            "     test/my_app/order_test.exs:42\n",
            "     Assertion with == failed\n",
            "     code:  assert Order.create(params) == {:ok, 200}\n",
            "     left:  {:error, :invalid}\n",
            "     right: {:ok, 200}\n",
            "     stacktrace:\n",
            "       test/my_app/order_test.exs:60: (test)\n\n",
            "  3) test charges card (MyApp.BillingTest)\n",
            "     test/my_app/billing_test.exs:18\n",
            "     ** (RuntimeError) gateway unavailable\n",
            "     stacktrace:\n",
            "       (my_app 0.1.0) lib/my_app/billing.ex:33: MyApp.Billing.charge/1\n",
            "       test/my_app/billing_test.exs:20: (test)\n\n",
            "Finished in 0.5 seconds (0.2s async, 0.3s sync)\n",
            "25 tests, 3 failures\n\n",
            "Randomized with seed 987654\n",
        );

        let report = format(Runner::Mix, fail_out, "", Some(2), DEFAULT_BUDGET).unwrap();
        assert_eq!(report.items.len(), 3, "{:#?}", report.items);
        assert_valid_pointers(&report, fail_out, "");
        assert!(
            report
                .text
                .contains("2 test(s): Assertion with == failed · test/my_app/order_test.exs:42"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("** (RuntimeError) gateway unavailable"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("25 tests, 3 failures"),
            "{}",
            report.text
        );
        assert!(report.text.len() < fail_out.len());

        // Fail-closed on Elixir CompileError.
        let compile_err = "** (CompileError) lib/my_app/order.ex:12: undefined function foo/1\n";
        assert!(format(Runner::Mix, "", compile_err, Some(1), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // 10. Deno `deno test`
    // -----------------------------------------------------------------------

    #[test]
    fn deno_pass_failures_and_fail_closed() {
        let mut pass_out = String::from("running 30 tests from ./tests/orders_test.ts\n");
        for i in 1..=30 {
            pass_out.push_str(&format!("order case {i:02} ... ok (2ms)\n"));
        }
        pass_out.push_str("\nok | 30 passed | 0 failed (88ms)\n");
        let pass = format(Runner::Deno, &pass_out, "", Some(0), DEFAULT_BUDGET).unwrap();
        assert!(pass.items.is_empty());
        assert!(pass.text.contains("deno test: 30 passed/ignored test(s)"));
        assert!(pass.text.contains("ok | 30 passed | 0 failed (88ms)"));
        assert!(pass.text.len() < pass_out.len() / 6);

        let mut fail_out = String::from("running 22 tests from ./tests/parser_test.ts\n");
        for i in 1..=20 {
            fail_out.push_str(&format!("valid case {i:02} ... ok (1ms)\n"));
        }
        fail_out.push_str(
            "parses header ... FAILED (4ms)\n\
             validates email ... FAILED (3ms)\n\n\
             ERRORS\n\n\
             parses header => ./tests/parser_test.ts:14:6\n\
             error: AssertionError: Values are not equal.\n\n\n\
                 [Diff] Actual / Expected\n\n\n\
             -   500\n\
             +   200\n\n\
               throw new AssertionError(message);\n\
                     ^\n\
                 at assertEquals (https://deno.land/std@0.224.0/assert/assert_equals.ts:51:9)\n\
                 at file:///repo/tests/parser_test.ts:18:3\n\n\
             validates email => ./tests/user_test.ts:22:6\n\
             error: TypeError: Cannot read properties of undefined (reading 'trim')\n\
                 at validateEmail (file:///repo/src/user.ts:9:14)\n\
                 at file:///repo/tests/user_test.ts:24:3\n\n\
             FAILURES\n\n\
             parses header => ./tests/parser_test.ts:14:6\n\
             validates email => ./tests/user_test.ts:22:6\n\n\
             FAILED | 20 passed | 2 failed (64ms)\n",
        );

        let report = format(Runner::Deno, &fail_out, "", Some(1), DEFAULT_BUDGET).unwrap();
        // Must deduplicate FAILURES section against ERRORS (exact 2 failures!).
        assert_eq!(report.items.len(), 2, "{:#?}", report.items);
        assert_valid_pointers(&report, &fail_out, "");
        assert!(
            report.text.contains(
                "AssertionError: Values are not equal. · /repo/tests/parser_test.ts:18:3"
            ),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("TypeError: Cannot read properties of undefined"),
            "{}",
            report.text
        );
        assert!(report.text.contains("FAILED | 20 passed | 2 failed (64ms)"));
        assert!(report.text.len() < fail_out.len() / 2);

        // Fail-closed on Deno typecheck failure before tests run.
        let ts_err = "TS2345 [ERROR]: Argument of type 'string' is not assignable to parameter of type 'number'.\n";
        assert!(format(Runner::Deno, "", ts_err, Some(1), DEFAULT_BUDGET).is_none());
    }

    // -----------------------------------------------------------------------
    // Cross-Cutting Invariants: ANSI stripping, Budget, Summary Item Bridge
    // -----------------------------------------------------------------------

    #[test]
    fn ansi_codes_budget_tightening_and_summary_item_conversion() {
        let mut colored = String::from(
            "\x1b[1m============================= test session starts ==============================\x1b[0m\n\
             collected 10 items\n\n\
             =================================== FAILURES ===================================\n",
        );
        for i in 1..=10 {
            colored.push_str(&format!(
                "\x1b[31m___________________________ test_item_{i:02} ___________________________\x1b[0m\n\
                 \x1b[31mE       RuntimeError: db pool exhausted at 2026-11-25T16:57:{i:02}Z\x1b[0m\n\
                 tests/test_db.py:77: RuntimeError\n"
            ));
        }
        colored.push_str(
            "\x1b[31m============================== 10 failed in 0.42s ==============================\x1b[0m\n",
        );

        // Tight budget forces headline-tier compaction while staying strictly <= budget.
        let tight = format(Runner::Pytest, &colored, "", Some(1), 240).unwrap();
        assert!(
            tight.text.len() <= 240,
            "got {}:\n{}",
            tight.text.len(),
            tight.text
        );
        assert_eq!(tight.items.len(), 10);
        // Volatile timestamps normalized -> all 10 items share identical grouping key!
        assert!(tight.items.iter().all(|i| i.key == tight.items[0].key));
        // Impossibly tiny budget fails closed rather than truncating arbitrarily.
        assert!(format(Runner::Pytest, &colored, "", Some(1), 20).is_none());

        // Verify bridge to `summary::Item`.
        let summary_item = tight.items[0].to_summary_item();
        assert_eq!(summary_item.name, tight.items[0].test);
        assert_eq!(summary_item.first, tight.items[0].start);
        assert_eq!(summary_item.last, tight.items[0].end);
        assert!(!summary_item.digest.is_empty());
    }

    #[test]
    fn additional_edge_cases_verbose_go_tb_no_stderr_preservation_and_size_guard() {
        // 1. Go `-v` mode attaches pre-fail logs printed between `=== RUN` and `--- FAIL:`.
        let mut go_v = String::new();
        for i in 1..=15 {
            go_v.push_str(&format!(
                "=== RUN   TestOk{i:02}\n--- PASS: TestOk{i:02} (0.00s)\n"
            ));
        }
        go_v.push_str(concat!(
            "=== RUN   TestRefund\n",
            "    refund_test.go:27: expected refund 100, got 0\n",
            "--- FAIL: TestRefund (0.01s)\n",
            "FAIL\n",
            "FAIL\texample.com/app/refund\t0.042s\n",
        ));
        let go_rep = format(Runner::GoTest, &go_v, "", Some(1), DEFAULT_BUDGET).unwrap();
        assert_eq!(go_rep.items.len(), 1);
        assert_eq!(go_rep.items[0].test, "TestRefund");
        assert!(
            go_rep.items[0]
                .cause
                .contains("expected refund 100, got 0 · refund_test.go:27"),
            "{:?}",
            go_rep.items[0]
        );
        assert_valid_pointers(&go_rep, &go_v, "");

        // 2. Pytest `--tb=no` falls back to `short test summary info`, and huge unrelated
        //    stderr keeps `covers_stderr == false` so stderr diagnostics are never hidden.
        let mut pytest_tb_no = String::from(
            "============================= test session starts ==============================\n\
             collected 20 items\n\n",
        );
        for i in 1..=18 {
            pytest_tb_no.push_str(&format!("tests/test_x.py::test_ok_{i:02} PASSED\n"));
        }
        pytest_tb_no.push_str(concat!(
            "tests/test_x.py::test_a FAILED\n",
            "tests/test_x.py::test_b FAILED\n",
            "=========================== short test summary info ============================\n",
            "FAILED tests/test_x.py::test_a - KeyError: 'token'\n",
            "FAILED tests/test_x.py::test_b - KeyError: 'token'\n",
            "========================= 2 failed, 18 passed in 0.30s =========================\n",
        ));
        let mut big_stderr = String::new();
        for i in 1..=30 {
            big_stderr.push_str(&format!("custom stderr warning line {i:02}\n"));
        }
        let py_rep = format(
            Runner::Pytest,
            &pytest_tb_no,
            &big_stderr,
            Some(1),
            DEFAULT_BUDGET,
        )
        .unwrap();
        assert_eq!(py_rep.items.len(), 2);
        assert!(
            !py_rep.covers_stderr,
            "large unrelated stderr must not be marked covered"
        );
        assert!(
            py_rep.text.contains("2 test(s): KeyError: 'token'"),
            "{}",
            py_rep.text
        );

        // 3. Strict size-reduction guard: compact output must be strictly smaller than raw.
        let tiny_pass = "ok  \tpkg/a\t0.01s\n";
        assert!(format(Runner::GoTest, tiny_pass, "", Some(0), DEFAULT_BUDGET).is_none());

        // 4. Nonzero exit code contradicting a clean passing summary fails closed
        //    even when the input would otherwise compact cleanly on exit 0.
        let mut clean_pass = String::from("running 30 tests from ./tests/orders_test.ts\n");
        for i in 1..=30 {
            clean_pass.push_str(&format!("order case {i:02} ... ok (2ms)\n"));
        }
        clean_pass.push_str("\nok | 30 passed | 0 failed (50ms)\n");
        assert!(format(Runner::Deno, &clean_pass, "", Some(0), DEFAULT_BUDGET).is_some());
        assert!(format(Runner::Deno, &clean_pass, "", Some(1), DEFAULT_BUDGET).is_none());

        // 5. Swift Testing: 1 failing test with 2 `#expect` issues must pass fail-closed check.
        let mut swift_multi = String::from("◇ Test run started.\n");
        for i in 1..=15 {
            swift_multi.push_str(&format!(
                "✔ Test \"ok case {i:02}\" passed after 0.001 seconds.\n"
            ));
        }
        swift_multi.push_str(concat!(
            "✘ Test \"validates payload\" recorded an issue at PayloadTests.swift:12:5: Expectation failed: a == 1\n",
            "✘ Test \"validates payload\" recorded an issue at PayloadTests.swift:14:5: Expectation failed: b == 2\n",
            "✘ Test \"validates payload\" failed after 0.002 seconds with 2 issues.\n",
            "✘ Test run with 16 tests failed after 0.020 seconds with 2 issues.\n",
        ));
        let sw_rep = format(Runner::Swift, &swift_multi, "", Some(1), DEFAULT_BUDGET).unwrap();
        assert_eq!(sw_rep.items.len(), 1);
        assert!(sw_rep.text.contains("Expectation failed: a == 1"));
        assert!(sw_rep.text.contains("Expectation failed: b == 2"));
        assert_valid_pointers(&sw_rep, &swift_multi, "");
    }

    #[test]
    fn measured_byte_reductions_all_ten_runners() {
        // Measure realistic pass & failure reductions for all 10 runners.
        let mut cases: Vec<(&str, Runner, String, String, Option<i32>)> = Vec::new();

        // 1. Pytest (50 parametrized + 1 error + 1 failure)
        let mut py = String::from(
            "============================= test session starts ==============================\n\
             collected 55 items / 1 error\n\n\
             ==================================== ERRORS ====================================\n\
             _____________________ ERROR collecting tests/test_broken.py ____________________\n\
             tests/test_broken.py:3: in <module>\n\
                 import missing_pkg\n\
             E   ModuleNotFoundError: No module named 'missing_pkg'\n\
             =================================== FAILURES ===================================\n",
        );
        for i in 1..=50 {
            py.push_str(&format!(
                "___________________________ test_parse[case_{i}] ___________________________\n\
                 E       AssertionError: expected status 200, got 500\n\
                 tests/test_parser.py:42: AssertionError\n"
            ));
        }
        py.push_str(
            "=========================== short test summary info ============================\n",
        );
        for i in 1..=50 {
            py.push_str(&format!(
                "FAILED tests/test_parser.py::test_parse[case_{i}] - AssertionError: expected status 200, got 500\n"
            ));
        }
        py.push_str(
            "ERROR tests/test_broken.py - ModuleNotFoundError: No module named 'missing_pkg'\n",
        );
        py.push_str(
            "=================== 50 failed, 3 passed, 1 error in 2.41s ======================\n",
        );
        cases.push((
            "pytest (50 param + err)",
            Runner::Pytest,
            py,
            String::new(),
            Some(1),
        ));

        // 2. Unittest (20 ok + 2 dup fail + 1 error on stderr)
        let mut ut = String::new();
        for i in 1..=20 {
            ut.push_str(&format!(
                "test_ok_{i:02} (tests.test_calc.TestCalc) ... ok\n"
            ));
        }
        ut.push_str(concat!(
            "======================================================================\n",
            "FAIL: test_a (tests.test_calc.TestCalc)\n",
            "----------------------------------------------------------------------\n",
            "Traceback (most recent call last):\n",
            "  File \"/repo/tests/test_calc.py\", line 31, in test_a\n",
            "    self.assertEqual(div(10, 2), 6)\n",
            "AssertionError: 5 != 6\n\n",
            "======================================================================\n",
            "FAIL: test_b (tests.test_calc.TestCalc)\n",
            "----------------------------------------------------------------------\n",
            "Traceback (most recent call last):\n",
            "  File \"/repo/tests/test_calc.py\", line 31, in test_b\n",
            "    self.assertEqual(div(10, 2), 6)\n",
            "AssertionError: 5 != 6\n\n",
            "----------------------------------------------------------------------\n",
            "Ran 22 tests in 0.088s\n\n",
            "FAILED (failures=2)\n",
        ));
        cases.push((
            "unittest (22 tests, 2 fail)",
            Runner::Unittest,
            String::new(),
            ut,
            Some(1),
        ));

        // 3. Go test (35 ok pkgs + 2 subtests + race + panic)
        let mut go = String::new();
        for i in 1..=35 {
            go.push_str(&format!(
                "ok  \tgithub.com/acme/platform/pkg/service{i:02}\t0.015s\n"
            ));
        }
        go.push_str(concat!(
            "--- FAIL: TestOrder (0.02s)\n",
            "    --- FAIL: TestOrder/case_1 (0.01s)\n",
            "        order_test.go:42: expected 200, got 500\n",
            "    --- FAIL: TestOrder/case_2 (0.01s)\n",
            "        order_test.go:42: expected 200, got 500\n",
            "FAIL\tgithub.com/acme/platform/pkg/order\t0.100s\n",
        ));
        cases.push((
            "go test (35 ok + 2 fail)",
            Runner::GoTest,
            go,
            String::new(),
            Some(1),
        ));

        // 4. Dotnet (25 passed + 2 failed)
        let mut dn = String::from(
            "  Determining projects to restore...\n\
             Test run for /repo/tests/MyApp.Tests/bin/Debug/net8.0/MyApp.Tests.dll (.NETCoreApp,Version=v8.0)\n",
        );
        for i in 1..=25 {
            dn.push_str(&format!(
                "  Passed MyApp.Tests.OrderTests.Ok{i:02} [2 ms]\n"
            ));
        }
        dn.push_str(concat!(
            "  Failed MyApp.Tests.OrderTests.Create(qty: -1) [12 ms]\n",
            "  Error Message:\n",
            "   Assert.Equal() Failure: Expected 200, Actual 400\n",
            "  Stack Trace:\n",
            "     at MyApp.Tests.OrderTests.Create(Int32 qty) in /repo/tests/OrderTests.cs:line 42\n",
            "  Failed MyApp.Tests.OrderTests.Create(qty: 0) [9 ms]\n",
            "  Error Message:\n",
            "   Assert.Equal() Failure: Expected 200, Actual 400\n",
            "  Stack Trace:\n",
            "     at MyApp.Tests.OrderTests.Create(Int32 qty) in /repo/tests/OrderTests.cs:line 42\n",
            "Failed!  - Failed:     2, Passed:    25, Skipped:     0, Total:    27, Duration: 280 ms - MyApp.Tests.dll (net8.0)\n",
        ));
        cases.push((
            "dotnet test (25 ok + 2 fail)",
            Runner::Dotnet,
            dn,
            String::new(),
            Some(1),
        ));

        // 5. Maven (20 classes + 2 failures)
        let mut mv = String::from("[INFO] Scanning for projects...\n[INFO]  T E S T S\n");
        for i in 1..=20 {
            mv.push_str(&format!(
                "[INFO] Running com.example.Pass{i:02}Test\n\
                 [INFO] Tests run: 5, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.050 s -- in com.example.Pass{i:02}Test\n"
            ));
        }
        mv.push_str(concat!(
            "[INFO] Results:\n",
            "[ERROR] Failures:\n",
            "[ERROR]   OrderTest.testCreate:42 expected: <200> but was: <500>\n",
            "[ERROR]   OrderTest.testUpdate:42 expected: <200> but was: <500>\n",
            "[ERROR] Tests run: 102, Failures: 2, Errors: 0, Skipped: 0\n",
            "[INFO] BUILD FAILURE\n",
        ));
        cases.push((
            "mvn test (20 cls + 2 fail)",
            Runner::Maven,
            mv,
            String::new(),
            Some(1),
        ));

        // 6. Gradle (25 passed + 2 failed)
        let mut gr = String::from("> Task :api:test\n");
        for i in 1..=25 {
            gr.push_str(&format!("com.example.ApiTest > testPass{i:02}() PASSED\n"));
        }
        gr.push_str(concat!(
            "com.example.ApiTest > parse(String)[1] FAILED\n",
            "\torg.opentest4j.AssertionFailedError: expected 200 at ApiTest.java:42\n",
            "com.example.ApiTest > parse(String)[2] FAILED\n",
            "\torg.opentest4j.AssertionFailedError: expected 200 at ApiTest.java:42\n",
            "27 tests completed, 2 failed\n",
            "> Task :api:test FAILED\n",
            "BUILD FAILED in 4s\n",
        ));
        cases.push((
            "gradle test (25 ok + 2 fail)",
            Runner::Gradle,
            gr,
            String::new(),
            Some(1),
        ));

        // 7. Swift (25 XCTest ok + 1 fail)
        let mut sw = String::from(
            "Building for debugging...\nBuild complete! (0.40s)\nTest Suite 'All tests' started at 2026-11-25 10:00:00.000.\n",
        );
        for i in 1..=25 {
            sw.push_str(&format!(
                "Test Case 'OrderTests.testOk{i:02}' started.\nTest Case 'OrderTests.testOk{i:02}' passed (0.001 seconds).\n"
            ));
        }
        sw.push_str(concat!(
            "Test Case 'OrderTests.testTotal' started.\n",
            "/repo/Tests/OrderTests.swift:42: error: OrderTests.testTotal : XCTAssertEqual failed: (\"500\") is not equal to (\"200\")\n",
            "Test Case 'OrderTests.testTotal' failed (0.003 seconds).\n",
            "Test Suite 'All tests' failed at 2026-11-25 10:00:00.100.\n",
            "\t Executed 26 tests, with 1 failure (0 unexpected) in 0.020 (0.030) seconds\n",
        ));
        cases.push((
            "swift test (25 ok + 1 fail)",
            Runner::Swift,
            sw,
            String::new(),
            Some(1),
        ));

        // 8. CTest (25 passed + 2 failed)
        let mut ct = String::from("Test project /repo/build\n");
        for i in 1..=25 {
            let tag = format!("#{i}");
            ct.push_str(&format!(
                "      Start {i:>2}: test_ok_{i:02}\n{i:>2}/27 Test {tag:>3}: test_ok_{i:02} ........................   Passed    0.01 sec\n"
            ));
        }
        ct.push_str(concat!(
            "      Start 26: test_parser\n",
            "26/27 Test #26: test_parser ......................***Failed    0.15 sec\n",
            "tests/parser_test.cpp:42: Assertion `status == 200` failed.\n",
            "      Start 27: test_network\n",
            "27/27 Test #27: test_network .....................***Timeout   5.00 sec\n",
            "93% tests passed, 2 tests failed out of 27\n",
            "Total Test time (real) =   5.35 sec\n",
        ));
        cases.push((
            "ctest (25 ok + 2 fail)",
            Runner::Ctest,
            ct,
            String::new(),
            Some(8),
        ));

        // 9. Mix (25 trace pass + 2 dup failures)
        let mut mx = String::from("Running ExUnit with seed: 123456, max_cases: 16\n");
        for i in 1..=25 {
            mx.push_str(&format!("  * test order case {i:02} (1.2ms) [L#{i}]\n"));
        }
        mx.push_str(concat!(
            "  1) test creates an order (MyApp.OrderTest)\n",
            "     test/my_app/order_test.exs:42\n",
            "     Assertion with == failed\n",
            "     left:  {:error, :invalid}\n",
            "     right: {:ok, 200}\n",
            "     stacktrace:\n",
            "       test/my_app/order_test.exs:45: (test)\n\n",
            "  2) test updates an order (MyApp.OrderTest)\n",
            "     test/my_app/order_test.exs:42\n",
            "     Assertion with == failed\n",
            "     left:  {:error, :invalid}\n",
            "     right: {:ok, 200}\n",
            "     stacktrace:\n",
            "       test/my_app/order_test.exs:60: (test)\n\n",
            "Finished in 0.5 seconds (0.2s async, 0.3s sync)\n",
            "27 tests, 2 failures\n",
        ));
        cases.push((
            "mix test (25 ok + 2 fail)",
            Runner::Mix,
            mx,
            String::new(),
            Some(2),
        ));

        // 10. Deno (25 ok + 2 failures)
        let mut dn_ts = String::from("running 27 tests from ./tests/parser_test.ts\n");
        for i in 1..=25 {
            dn_ts.push_str(&format!("valid case {i:02} ... ok (1ms)\n"));
        }
        dn_ts.push_str(concat!(
            "parses header ... FAILED (4ms)\n",
            "validates email ... FAILED (3ms)\n\n",
            "ERRORS\n\n",
            "parses header => ./tests/parser_test.ts:14:6\n",
            "error: AssertionError: Values are not equal.\n",
            "    at file:///repo/tests/parser_test.ts:18:3\n\n",
            "validates email => ./tests/user_test.ts:22:6\n",
            "error: TypeError: Cannot read properties of undefined\n",
            "    at file:///repo/src/user.ts:9:14\n\n",
            "FAILURES\n\n",
            "parses header => ./tests/parser_test.ts:14:6\n",
            "validates email => ./tests/user_test.ts:22:6\n\n",
            "FAILED | 25 passed | 2 failed (64ms)\n",
        ));
        cases.push((
            "deno test (25 ok + 2 fail)",
            Runner::Deno,
            dn_ts,
            String::new(),
            Some(1),
        ));

        for (label, runner, out, err, code) in cases {
            let raw = out.len() + err.len();
            let rep = format(runner, &out, &err, code, DEFAULT_BUDGET)
                .unwrap_or_else(|| panic!("{label} unexpectedly returned None"));
            let pct = 100.0 * (1.0 - (rep.text.len() as f64 / raw as f64));
            println!(
                "BYTE_REDUCTION | {label:<28} | raw={raw:>5} B -> compact={:>4} B ({pct:.1}% saved)",
                rep.text.len()
            );
            assert!(rep.text.len() < raw, "{label} not smaller");
            assert_valid_pointers(&rep, &out, &err);
        }
    }
}
