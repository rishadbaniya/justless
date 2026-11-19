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
/// diagnostics, besides the last.
const KEPT_LINES: usize = 2;
const KEPT_DIAGNOSTICS: usize = 6;
/// Binaries that ran no test, named in one line.
const NAMED_IDLE: usize = 4;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Totals {
    pub binaries: Vec<Binary>,
    /// Cargo's status lines and any other lines, per stream, as report lines.
    pub notes: Vec<String>,
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
}

impl Scan {
    /// Read one line, without its line break, of `stream`.
    pub fn observe(&mut self, line: &str, n: usize, stream: Stream) {
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
        let diagnostic = crate::filters::diagnostic(line);
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

    pub fn finish(self) -> Option<Totals> {
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
        for stream in Stream::BOTH {
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
        Some(Totals { binaries, notes })
    }
}

impl Totals {
    /// The total line, one line per binary that ran tests, one for the binaries
    /// that ran none, and the notes.
    pub fn render(&self) -> String {
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
        let err = format!("{ERR}{noise}warning: unused import: `Path`\ntrace end\n");
        let notes = totals(&[(OUT, Stream::Stdout), (&err, Stream::Stderr)])
            .unwrap()
            .notes;
        assert_eq!(
            notes,
            [
                "stderr: 15 cargo status line(s) [stderr:1-15]",
                "stderr: 12 other line(s) [stderr:16-27]",
                "  trace 0",
                "  trace 1",
                "  …",
                "  warning: unused import: `Path`",
                "  trace end",
            ]
        );
    }
}
