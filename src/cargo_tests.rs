//! Totals for `cargo test` output: each test binary's `test result:` line, paired
//! with the `Running` or `Doc-tests` line that named the binary, and every other
//! line of both streams accounted for, so a report can stand in for the whole
//! output.
//!
//! Cargo prints a `Running` line on stderr, then the binary prints `running N
//! tests`, its tests and one `test result:` line on stdout. The k-th result
//! belongs to the k-th `Running` line, also when the two streams were archived
//! apart. The totals are given only when they check out: as many names as
//! blocks, each block with one result whose counts add up to its `running N
//! tests`. Captured test output can hold lines that look like these, so any
//! mismatch leaves the ordinary test report.
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::capture::Stream;
use crate::summary::short;

static RUNNING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^running (\d+) tests?$").expect("running pattern"));
/// `cargo test -q` prints one character per test, and a count on long lines.
static QUIET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[.FEi]+(?: \d+/\d+)?$").expect("quiet pattern"));
static DOC_TESTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ +Doc-tests (\S+)$").expect("doc-tests pattern"));

/// Lines of other stream text kept in a report: the first ones and the
/// diagnostics with their locations, besides the last.
const KEPT_LINES: usize = 2;
const KEPT_DIAGNOSTICS: usize = 8;
/// Binaries that ran no test, named in one line.
const NAMED_IDLE: usize = 4;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Totals {
    pub binaries: Vec<Binary>,
    /// Cargo's status lines and any other lines, per stream, as report lines.
    pub notes: Vec<String>,
    /// Whether the notes show every other line of stderr, so the report stands
    /// in for stderr too. Otherwise stderr keeps its own preview, which shows
    /// a compiler warning's location lines, and has no notes.
    #[serde(default)]
    pub stderr: bool,
    /// `cargo nextest run` `Summary [...]` line when this run was a nextest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nextest_summary: Option<String>,
    /// Rendered `cargo nextest run` failure/timeout lines with stream pointers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nextest_failures: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Binary {
    /// `tests/cli.rs`, `unittests src/main.rs` or `doc-tests NAME`.
    pub name: String,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub measured: usize,
    pub filtered: usize,
    /// The `running N tests` line and the `test result:` line.
    pub first: usize,
    pub last: usize,
}

impl Binary {
    fn ran(&self) -> usize {
        self.passed + self.failed + self.ignored + self.measured
    }
}

struct Block {
    declared: usize,
    first: usize,
    result: Option<Binary>,
}

#[derive(Default)]
struct Range {
    count: usize,
    first: usize,
    last: usize,
}

impl Range {
    fn add(&mut self, n: usize) {
        if self.count == 0 {
            self.first = n;
        }
        self.count += 1;
        self.last = n;
    }
}

/// Lines that are neither test output nor cargo status, each kept one with its
/// ordinal among them.
#[derive(Default)]
struct Other {
    range: Range,
    kept: Vec<(usize, String)>,
    diagnostics: usize,
    last: Option<(usize, String)>,
}

struct NextestFailure {
    kind: &'static str,
    name: String,
    stream: Stream,
    first: usize,
    last: usize,
    excerpt: Vec<String>,
    saw_panic: bool,
    stopped: bool,
}

#[derive(Default)]
struct Nextest {
    enabled: bool,
    summary: Option<String>,
    progress: [Range; 2],
    failures: Vec<NextestFailure>,
    in_output: bool,
    current: Option<(usize, Stream)>,
}

#[derive(Default)]
pub struct Scan {
    /// Each binary's name and, for `unittests src/lib.rs`, its crate.
    names: Vec<(String, Option<String>)>,
    blocks: Vec<Block>,
    broken: bool,
    /// Inside a block's `failures:` or `successes:` section, which holds captured
    /// test output, until the block's result.
    section: bool,
    status: [Range; 2],
    other: [Other; 2],
    nextest: Nextest,
}

impl Scan {
    /// Scanner for `cargo nextest run`, which writes `PASS`/`FAIL`/`Summary` to stderr.
    pub fn nextest() -> Self {
        Self {
            nextest: Nextest {
                enabled: true,
                ..Nextest::default()
            },
            ..Self::default()
        }
    }

    /// Read one line, without its line break, of `stream`.
    pub fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        if self.nextest.enabled {
            self.observe_nextest(line, n, stream);
            return;
        }
        if self.broken {
            return;
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            return;
        }
        if let Some(rest) = line.strip_prefix("test result:") {
            self.section = false;
            match self.blocks.last_mut() {
                Some(block) if block.result.is_none() => {
                    let count = |word| count_before(rest, word).unwrap_or(0);
                    let binary = Binary {
                        name: String::new(),
                        passed: count("passed"),
                        failed: count("failed"),
                        ignored: count("ignored"),
                        measured: count("measured"),
                        filtered: count("filtered"),
                        first: block.first,
                        last: n,
                    };
                    self.broken |= binary.ran() != block.declared;
                    block.result = Some(binary);
                }
                _ => self.broken = true,
            }
            return;
        }
        if self.section {
            return;
        }
        if let Some(found) = RUNNING.captures(line) {
            if self
                .blocks
                .last()
                .is_some_and(|block| block.result.is_none())
            {
                self.broken = true;
                return;
            }
            self.blocks.push(Block {
                declared: found[1].parse().unwrap_or(usize::MAX),
                first: n,
                result: None,
            });
            return;
        }
        if line == "failures:"
            || line == "successes:"
            || line.starts_with("---- ") && line.ends_with(" stdout ----")
        {
            self.section = true;
            return;
        }
        // A test's outcome, the note on a slow one, and quiet output's dots.
        if line.starts_with("test ")
            && (line.contains(" ... ") || line.ends_with(" has been running for over 60 seconds"))
            || QUIET.is_match(line)
        {
            return;
        }
        let index = stream as usize;
        if let Some(found) = DOC_TESTS.captures(line) {
            self.names.push((format!("doc-tests {}", &found[1]), None));
            self.status[index].add(n);
            return;
        }
        if crate::lint::CARGO_STATUS.is_match(line) {
            if let Some(rest) = line.trim_start().strip_prefix("Running ") {
                // `Running tests/cli.rs (target/debug/deps/cli-c83678cfeb5efe08)`
                let (name, path) = rest.rsplit_once(" (").unwrap_or((rest, ""));
                let file = path.trim_end_matches(')').rsplit(['/', '\\']).next();
                let krate = file
                    .and_then(|file| file.rsplit_once('-'))
                    .map(|(krate, _)| krate.to_owned())
                    .filter(|_| name.starts_with("unittests "));
                self.names.push((short(name, 160), krate));
            }
            self.status[index].add(n);
            return;
        }
        let other = &mut self.other[index];
        let ordinal = other.range.count;
        other.range.add(n);
        let text = short(line.trim_end(), 200);
        // A warning or error and the `-->` line that locates it, not the `= note:`
        // and source lines that quote a lint's name.
        let trimmed = line.trim_start();
        let diagnostic = trimmed.starts_with("--> ")
            || crate::filters::diagnostic(line) && !trimmed.starts_with(['=', '|']);
        if other.kept.len() < KEPT_LINES && other.diagnostics == 0
            || diagnostic && other.diagnostics < KEPT_DIAGNOSTICS
        {
            other.diagnostics += usize::from(diagnostic);
            other.kept.push((ordinal, text));
            other.last = None;
        } else {
            other.last = Some((ordinal, text));
        }
    }

    fn observe_nextest(&mut self, line: &str, n: usize, stream: Stream) {
        let line = line.trim_end_matches(['\n', '\r']);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let index = stream as usize;
        if crate::lint::CARGO_STATUS.is_match(line) {
            self.nextest.in_output = false;
            self.nextest.current = None;
            self.status[index].add(n);
            return;
        }
        if trimmed.starts_with("Summary [")
            && (trimmed.contains(" tests run:") || trimmed.contains(" test run:"))
        {
            self.nextest.in_output = false;
            self.nextest.current = None;
            self.nextest.summary = Some(short(trimmed, 220));
            return;
        }
        if let Some(name) = trimmed
            .strip_prefix("--- STDOUT:")
            .or_else(|| trimmed.strip_prefix("--- STDERR:"))
            .and_then(|s| s.strip_suffix(" ---"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let pos = self
                .nextest
                .failures
                .iter()
                .position(|f| f.name == name)
                .unwrap_or_else(|| {
                    self.nextest.failures.push(NextestFailure {
                        kind: "FAIL",
                        name: short(name, 200),
                        stream,
                        first: n,
                        last: n,
                        excerpt: Vec::new(),
                        saw_panic: false,
                        stopped: false,
                    });
                    self.nextest.failures.len() - 1
                });
            self.nextest.in_output = true;
            self.nextest.current = Some((pos, stream));
            return;
        }
        if let Some((kind, name)) = nextest_failure(trimmed) {
            self.nextest.in_output = false;
            self.nextest.current = None;
            if !self.nextest.failures.iter().any(|f| f.name == name) {
                self.nextest.failures.push(NextestFailure {
                    kind,
                    name: short(name, 200),
                    stream,
                    first: n,
                    last: n,
                    excerpt: Vec::new(),
                    saw_panic: false,
                    stopped: false,
                });
            }
            return;
        }
        if trimmed.starts_with("PASS [")
            || trimmed.starts_with("SKIP [")
            || trimmed.starts_with("SLOW [")
            || trimmed.starts_with("RETRY ")
            || trimmed.starts_with("Starting ")
            || trimmed.starts_with("Cancelling ")
            || trimmed.starts_with("Nextest run ID ")
            || (trimmed.len() >= 6 && trimmed.bytes().all(|b| b == b'-'))
        {
            self.nextest.in_output = false;
            self.nextest.current = None;
            self.nextest.progress[index].add(n);
            return;
        }
        if self.nextest.in_output
            && let Some((pos, cur_stream)) = self.nextest.current
            && cur_stream == stream
        {
            let failure = &mut self.nextest.failures[pos];
            if !failure.stopped {
                if trimmed.starts_with("note:") || trimmed.starts_with("stack backtrace:") {
                    failure.stopped = true;
                } else if trimmed.contains(" panicked at ") {
                    failure.saw_panic = true;
                    failure.first = n;
                    failure.last = n;
                    failure.excerpt.clear();
                    failure.excerpt.push(short(trimmed, 200));
                } else if failure.excerpt.len() < 6
                    && (failure.saw_panic || failure.excerpt.len() < 3)
                {
                    failure.last = n;
                    failure.excerpt.push(short(trimmed, 200));
                }
            }
            return;
        }
        let other = &mut self.other[index];
        let ordinal = other.range.count;
        other.range.add(n);
        let text = short(line.trim_end(), 200);
        let diagnostic = trimmed.starts_with("--> ")
            || crate::filters::diagnostic(line) && !trimmed.starts_with(['=', '|']);
        if other.kept.len() < KEPT_LINES && other.diagnostics == 0
            || diagnostic && other.diagnostics < KEPT_DIAGNOSTICS
        {
            other.diagnostics += usize::from(diagnostic);
            other.kept.push((ordinal, text));
            other.last = None;
        } else {
            other.last = Some((ordinal, text));
        }
    }

    fn finish_nextest(self) -> Option<Totals> {
        let summary = self.nextest.summary?;
        let mut notes = Vec::new();
        let other = &self.other[Stream::Stderr as usize];
        let stderr = other.kept.len() + usize::from(other.last.is_some()) == other.range.count;
        for stream in Stream::BOTH {
            if stream == Stream::Stderr && !stderr {
                continue;
            }
            let name = stream.name();
            let status = &self.status[stream as usize];
            if status.count > 0 {
                notes.push(format!(
                    "{name}: {} cargo status line(s) [{name}:{}-{}]",
                    status.count, status.first, status.last
                ));
            }
            let progress = &self.nextest.progress[stream as usize];
            if progress.count > 0 {
                notes.push(format!(
                    "{name}: {} nextest pass/status line(s) [{name}:{}-{}]",
                    progress.count, progress.first, progress.last
                ));
            }
            let other = &self.other[stream as usize];
            if other.range.count == 0 {
                continue;
            }
            notes.push(format!(
                "{name}: {} other line(s) [{name}:{}-{}]",
                other.range.count, other.range.first, other.range.last
            ));
            let mut next = 0;
            for (ordinal, text) in other.kept.iter().chain(&other.last) {
                if *ordinal > next {
                    notes.push("  …".into());
                }
                notes.push(format!("  {text}"));
                next = ordinal + 1;
            }
            if other.range.count > next {
                notes.push("  …".into());
            }
        }
        let nextest_failures = self
            .nextest
            .failures
            .into_iter()
            .map(|failure| {
                let range = if failure.first == failure.last {
                    format!("{}:{}", failure.stream.name(), failure.first)
                } else {
                    format!(
                        "{}:{}-{}",
                        failure.stream.name(),
                        failure.first,
                        failure.last
                    )
                };
                let mut block = format!("{} {} [{range}]", failure.kind, failure.name);
                for line in failure.excerpt {
                    block.push_str("\n  ");
                    block.push_str(&line);
                }
                block
            })
            .collect();
        Some(Totals {
            binaries: Vec::new(),
            notes,
            stderr,
            nextest_summary: Some(summary),
            nextest_failures,
        })
    }

    pub fn finish(self) -> Option<Totals> {
        if self.nextest.enabled {
            return self.finish_nextest();
        }
        if self.broken || self.blocks.is_empty() || self.names.len() != self.blocks.len() {
            return None;
        }
        // Each package of a workspace has its `unittests src/lib.rs`, so a name
        // that repeats also names the crate.
        let repeats = |name: &str| self.names.iter().filter(|(n, _)| n == name).count() > 1;
        let names: Vec<String> = self
            .names
            .iter()
            .map(|(name, krate)| match krate {
                Some(krate) if repeats(name) => format!("{name} ({krate})"),
                _ => name.clone(),
            })
            .collect();
        let mut binaries = Vec::new();
        for (block, name) in self.blocks.into_iter().zip(names) {
            let mut binary = block.result?;
            binary.name = name;
            binaries.push(binary);
        }
        let mut notes = Vec::new();
        let other = &self.other[Stream::Stderr as usize];
        let stderr = other.kept.len() + usize::from(other.last.is_some()) == other.range.count;
        for stream in Stream::BOTH {
            if stream == Stream::Stderr && !stderr {
                continue;
            }
            let name = stream.name();
            let status = &self.status[stream as usize];
            if status.count > 0 {
                notes.push(format!(
                    "{name}: {} cargo status line(s) [{name}:{}-{}]",
                    status.count, status.first, status.last
                ));
            }
            let other = &self.other[stream as usize];
            if other.range.count == 0 {
                continue;
            }
            notes.push(format!(
                "{name}: {} other line(s) [{name}:{}-{}]",
                other.range.count, other.range.first, other.range.last
            ));
            // Lines left out between the kept ones show as `…`.
            let mut next = 0;
            for (ordinal, text) in other.kept.iter().chain(&other.last) {
                if *ordinal > next {
                    notes.push("  …".into());
                }
                notes.push(format!("  {text}"));
                next = ordinal + 1;
            }
            if other.range.count > next {
                notes.push("  …".into());
            }
        }
        Some(Totals {
            binaries,
            notes,
            stderr,
            nextest_summary: None,
            nextest_failures: Vec::new(),
        })
    }
}

fn nextest_failure(trimmed: &str) -> Option<(&'static str, &str)> {
    let (kind, rest) = if let Some(rest) = trimmed.strip_prefix("FAIL [") {
        ("FAIL", rest)
    } else {
        let rest = trimmed.strip_prefix("TIMEOUT [")?;
        ("TIMEOUT", rest)
    };
    let (_, tail) = rest.split_once("] ")?;
    let name = tail.trim_start();
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
    (!name.is_empty()).then_some((kind, name))
}

impl Totals {
    /// The total line, one line per binary that ran tests, one for the binaries
    /// that ran none, and the notes.
    pub fn render(&self) -> String {
        if let Some(summary) = &self.nextest_summary {
            let mut report = String::new();
            if !self.nextest_failures.is_empty() {
                report.push_str(&format!(
                    "{} reported failing test(s)\n",
                    self.nextest_failures.len()
                ));
                for failure in &self.nextest_failures {
                    report.push_str(failure);
                    report.push('\n');
                }
            }
            report.push_str(summary);
            report.push('\n');
            for note in &self.notes {
                report.push_str(note);
                report.push('\n');
            }
            return report;
        }
        let sum = |count: fn(&Binary) -> usize| self.binaries.iter().map(count).sum::<usize>();
        let failed = sum(|b| b.failed);
        let mut report = format!(
            "test result: {}. {} passed; {failed} failed; {} ignored",
            if failed > 0 { "FAILED" } else { "ok" },
            sum(|b| b.passed),
            sum(|b| b.ignored),
        );
        for (count, word) in [
            (sum(|b| b.measured), "measured"),
            (sum(|b| b.filtered), "filtered out"),
        ] {
            if count > 0 {
                report.push_str(&format!("; {count} {word}"));
            }
        }
        report.push_str(&format!(" · {} test binaries\n", self.binaries.len()));
        let mut idle = Vec::new();
        for binary in &self.binaries {
            if binary.ran() == 0 {
                idle.push(binary.name.as_str());
                continue;
            }
            let mut counts = format!("{} passed", binary.passed);
            for (count, word) in [
                (binary.failed, "failed"),
                (binary.ignored, "ignored"),
                (binary.measured, "measured"),
            ] {
                if count > 0 {
                    counts.push_str(&format!(", {count} {word}"));
                }
            }
            report.push_str(&format!(
                "  {counts}  {} [stdout:{}-{}]\n",
                binary.name, binary.first, binary.last
            ));
        }
        if !idle.is_empty() {
            let mut names = idle[..idle.len().min(NAMED_IDLE)].join(", ");
            if idle.len() > NAMED_IDLE {
                names.push_str(&format!(" +{} more", idle.len() - NAMED_IDLE));
            }
            report.push_str(&format!("  {} ran no tests: {names}\n", idle.len()));
        }
        for note in &self.notes {
            report.push_str(note);
            report.push('\n');
        }
        report
    }

    /// Nextest summary line + status/pass range notes without the failure list
    /// (for merged reruns whose `Compared with` section already listed changes).
    pub fn render_nextest_summary(&self) -> String {
        let mut report = String::new();
        if let Some(summary) = &self.nextest_summary {
            report.push_str(summary);
            report.push('\n');
        }
        for note in &self.notes {
            report.push_str(note);
            report.push('\n');
        }
        report
    }
}

/// The number before `word` in `passed; 2 failed; 1 ignored`.
fn count_before(line: &str, word: &str) -> Option<usize> {
    let words: Vec<&str> = line.split_whitespace().collect();
    words.windows(2).find_map(|pair| {
        (pair[1].trim_end_matches([',', ';']) == word)
            .then(|| pair[0].parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `cargo test` output of `fixtures/affected`, a three-package workspace.
    const OUT: &str = include_str!("../fixtures/cargo/test_workspace.out");
    const ERR: &str = include_str!("../fixtures/cargo/test_workspace.err");
    /// Real `cargo test` output of `fixtures/failing`, its trace lines cut short.
    const FAILING: &str = include_str!("../fixtures/cargo/test_failing.out");
    const FAILING_ERR: &str = include_str!("../fixtures/cargo/test_failing.err");

    fn totals(streams: &[(&str, Stream)]) -> Option<Totals> {
        let mut scan = Scan::default();
        for (text, stream) in streams {
            for (index, line) in text.lines().enumerate() {
                scan.observe(line, index + 1, *stream);
            }
        }
        scan.finish()
    }

    /// The two streams as `2>&1` joins them: each binary's status lines before
    /// its block.
    fn merged(out: &str, err: &str) -> String {
        let mut groups = vec![String::new()];
        for line in err.split_inclusive('\n') {
            groups.last_mut().unwrap().push_str(line);
            if line.contains(" Running ") || line.contains(" Doc-tests ") {
                groups.push(String::new());
            }
        }
        let rest = groups.pop().unwrap();
        let mut groups = groups.into_iter();
        let mut text = String::new();
        for line in out.split_inclusive('\n') {
            if line.starts_with("running ") {
                text.extend(groups.next());
            }
            text.push_str(line);
        }
        text + &rest
    }

    #[test]
    fn totals_name_each_binary_whether_or_not_the_streams_were_joined() {
        let split = totals(&[(OUT, Stream::Stdout), (ERR, Stream::Stderr)]).unwrap();
        assert_eq!(
            split.render(),
            "test result: ok. 21 passed; 0 failed; 0 ignored · 11 test binaries\n  \
             2 passed  unittests src/lib.rs (api) [stdout:2-6]\n  \
             1 passed  tests/cli.rs [stdout:14-17]\n  \
             5 passed  unittests src/lib.rs (ledger_core) [stdout:20-27]\n  \
             1 passed  tests/data.rs [stdout:30-33]\n  \
             2 passed  tests/totals.rs [stdout:36-40]\n  \
             8 passed  unittests src/lib.rs (reports) [stdout:43-53]\n  \
             1 passed  tests/summary.rs [stdout:56-59]\n  \
             1 passed  doc-tests ledger_core [stdout:67-70]\n  \
             3 ran no tests: unittests src/main.rs, doc-tests api, doc-tests reports\n\
             stderr: 15 cargo status line(s) [stderr:1-15]\n"
        );
        let joined = totals(&[(&merged(OUT, ERR), Stream::Stdout)]).unwrap();
        let names = |totals: &Totals| -> Vec<String> {
            totals.binaries.iter().map(|b| b.name.clone()).collect()
        };
        assert_eq!(names(&joined), names(&split));
        assert_eq!(
            joined.notes,
            ["stdout: 15 cargo status line(s) [stdout:2-87]"]
        );
    }

    #[test]
    fn totals_that_do_not_add_up_are_left_out() {
        let failing = totals(&[(FAILING, Stream::Stdout), (FAILING_ERR, Stream::Stderr)]).unwrap();
        assert!(failing.render().starts_with(
            "test result: FAILED. 0 passed; 1 failed; 0 ignored · 2 test binaries\n  \
             0 passed, 1 failed  tests/checkout.rs [stdout:7-30]\n"
        ));
        for (out, err) in [
            // A block whose result counts another number of tests.
            (
                OUT.replacen("running 2 tests", "running 3 tests", 1),
                ERR.to_owned(),
            ),
            // A result with no block.
            (OUT.replacen("running 2 tests\n", "", 1), ERR.to_owned()),
            // A binary that no `Running` line named, as `cargo test -q` prints.
            (
                OUT.to_owned(),
                ERR.replacen("     Running tests/cli.rs", "", 1),
            ),
            (OUT.to_owned(), String::new()),
            // Captured output that looks like a result ends the failure section
            // early, and the real result then has no block.
            (
                FAILING.replacen(
                    "billing-trace step=000",
                    "test result: ok. 1 passed; 0 failed\nbilling-trace step=000",
                    1,
                ),
                FAILING_ERR.to_owned(),
            ),
        ] {
            assert!(
                totals(&[(&out, Stream::Stdout), (&err, Stream::Stderr)]).is_none(),
                "{out}{err}"
            );
        }
    }

    #[test]
    fn other_lines_keep_their_diagnostics_and_mark_what_they_left_out() {
        let noise: String = (0..10).map(|i| format!("trace {i}\n")).collect();
        let warning = "warning: unused import: `Path`\n --> src/lib.rs:1:5\n  |\n";
        let tail = format!("{noise}{warning}trace end\n");
        let joined = merged(OUT, ERR) + &tail;
        let notes = totals(&[(&joined, Stream::Stdout)]).unwrap().notes;
        assert_eq!(
            notes,
            [
                "stdout: 15 cargo status line(s) [stdout:2-87]",
                "stdout: 14 other line(s) [stdout:92-105]",
                "  trace 0",
                "  trace 1",
                "  …",
                "  warning: unused import: `Path`",
                "   --> src/lib.rs:1:5",
                "  …",
                "  trace end",
            ]
        );
        // Apart, stderr whose other lines do not all show keeps its own preview,
        // and three or fewer show whole.
        for (err, whole) in [
            (format!("{ERR}{tail}"), false),
            (format!("{ERR}a\nb\nc\n"), true),
        ] {
            let totals = totals(&[(OUT, Stream::Stdout), (&err, Stream::Stderr)]).unwrap();
            assert_eq!(totals.stderr, whole);
            assert_eq!(
                totals.notes.len(),
                if whole { 5 } else { 0 },
                "{:?}",
                totals.notes
            );
        }
    }
}
