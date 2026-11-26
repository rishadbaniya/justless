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
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
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
    LazyLock::new(|| Regex::new(r"^ +Doc-tests (\S+)\s*$").expect("doc-tests pattern"));

static RUST_DIAG_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(error|warning)(?:\[([A-Za-z0-9_:-]+)\])?: (.+)$")
        .expect("rust diagnostic header pattern")
});
static CARGO_WARN_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^warning: `[^`]+` \([^)]+\) generated \d+ warnings?(?: \(.+\))?$")
        .expect("cargo warning summary pattern")
});
static RUST_ARROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:-->|:::|╭▸|┌─)\s*(.+?):(\d+)(?::(\d+))?\s*$").expect("rust arrow pattern")
});
static RUST_LINT_ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"#\[(?:warn|deny|forbid|allow|expect)\(([A-Za-z0-9_:]+)\)\]|`-[DWF] ([A-Za-z0-9_:-]+)` implied by|index\.html#([a-z0-9_]+)",
    )
    .expect("rust lint attribute pattern")
});
static RUST_SNIPPET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\d*\s*\||\d+\s*[+~-]\s|\.\.\.\s*$)").expect("rust snippet pattern")
});
static RUSTC_LINTS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (
            r"^value (?:assigned to|captured by) .* is never read",
            "unused_assignments",
        ),
        (
            r"^unused variables?: |^variables? .* (?:is|are) assigned to, but never used",
            "unused_variables",
        ),
        (r"^unused imports?: ", "unused_imports"),
        (r"^variables? does not need to be mutable", "unused_mut"),
        (r"^unused `[^`]+` that must be used", "unused_must_use"),
        (
            r"\b(?:is|are) never (?:used|constructed|read)\b",
            "dead_code",
        ),
        (r"\bshould have a snake case name\b", "non_snake_case"),
        (
            r"\bshould have an upper camel case name\b",
            "non_camel_case_types",
        ),
        (
            r"\bshould have an upper case name\b",
            "non_upper_case_globals",
        ),
        (
            r"^unreachable (?:statement|expression|block|call|arm)\b",
            "unreachable_code",
        ),
        (r"^unreachable patterns?\b", "unreachable_patterns"),
        (r"^unnecessary parentheses around ", "unused_parens"),
        (r"^use of deprecated ", "deprecated"),
        (
            r"^unresolved link to `|^`[^`]+` is both a ",
            "rustdoc::broken_intra_doc_links",
        ),
        (r"^this URL is not a hyperlink$", "rustdoc::bare_urls"),
    ]
    .into_iter()
    .map(|(pattern, lint)| (Regex::new(pattern).expect("rustc lint pattern"), lint))
    .collect()
});
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"`[^`]*`|'[^']*'|"[^"]*""#).expect("quoted pattern"));

/// Ephemeral TempDir paths normalized ONLY inside root-cause grouping keys.
static EPHEMERAL_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?:",
        r"(?:/private)?/var/folders/[^/\s\x22'`,;)}\]:]+/[^/\s\x22'`,;)}\]:]+/T/(?:[^/\s\x22'`,;)}\]:]+/)*?\.?tmp[A-Za-z0-9_-]+",
        r"|(?:/private)?/var/folders/[^/\s\x22'`,;)}\]:]+/[^/\s\x22'`,;)}\]:]+/T/\.?[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*",
        r"|/(?:var/)?tmp/(?:[^/\s\x22'`,;)}\]:]+/)*?\.tmp[A-Za-z0-9_-]+",
        r"|/(?:var/)?tmp/(?:[^/\s\x22'`,;)}\]:]+/)*?(?:[A-Za-z0-9_-]+[-_])?tmp[A-Za-z0-9_-]+",
        r"|[A-Za-z]:\\Users\\[^\\\s\x22'`,;)}\]:]+\\AppData\\Local\\Temp\\(?:[^\\\s\x22'`,;)}\]:]+\\)*?\.?tmp[A-Za-z0-9_-]+",
        r")",
    ))
    .expect("ephemeral path pattern")
});
/// Hex pointer addresses normalized ONLY inside root-cause grouping keys.
static HEX_ADDR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b0x[0-9a-f]{8,}\b").expect("hex address pattern"));
static THREAD_PANIC_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^thread '[^']*'(?: \(\d+\))? panicked at ").expect("thread panic prefix")
});
/// Rust source location (`path/to/file.rs:line` or `path/to/file.rs:line:col`).
static RUST_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s,'(\x22])(?:\./)?([^\s,'()\x22]+?\.rs:\d+(?::\d+)?)")
        .expect("rust location pattern")
});

/// Lines of other stream text kept in a report: the first ones and the
/// diagnostics with their locations, besides the last.
const KEPT_LINES: usize = 2;
const KEPT_DIAGNOSTICS: usize = 8;
/// Binaries that ran no test, named in one line when few; more collapse to a range line.
const NAMED_IDLE: usize = 4;
/// Locations shown inline per grouped compiler warning before `+N more`.
const SHOWN_WARNING_LOCS: usize = 4;
/// Maximum panic/assertion lines collected after `panicked at ...:`.
const MAX_PANIC_LINES: usize = 5;

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Totals {
    pub binaries: Vec<Binary>,
    /// Cargo's status lines, grouped warnings, verbatim errors, and any other lines per stream.
    pub notes: Vec<String>,
    /// Whether the notes show every other line of stderr, so the report stands
    /// in for stderr too. Otherwise stderr keeps its own preview, which shows
    /// a compiler warning's location lines, and has no notes.
    #[serde(default)]
    pub stderr: bool,
    /// `cargo nextest run` `Summary [...]` line when this run was a nextest run,
    /// or an empty sentinel when `cargo test` owns normalized failure grouping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nextest_summary: Option<String>,
    /// Rendered `cargo nextest run` failure/timeout lines with stream pointers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nextest_failures: Vec<String>,
    /// Root-cause grouped failure blocks when normalization collapses failures.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    grouped_failures: Vec<String>,
    /// Total failing tests represented by `grouped_failures` for `cargo test`.
    #[serde(default, skip_serializing_if = "is_zero")]
    failure_count: usize,
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

    fn extend(&mut self, first: usize, last: usize) {
        if first == 0 {
            return;
        }
        if self.count == 0 {
            self.first = first;
            self.last = last;
        } else {
            self.first = self.first.min(first);
            self.last = self.last.max(last);
        }
        self.count += 1;
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

impl Other {
    fn observe(&mut self, line: &str, n: usize) {
        let ordinal = self.range.count;
        self.range.add(n);
        let trimmed_end = line.trim_end();
        // A warning or error and the `-->` line that locates it, not the `= note:`
        // and source lines that quote a lint's name.
        let trimmed = trimmed_end.trim_start();
        let diagnostic = trimmed.starts_with("--> ")
            || crate::filters::diagnostic(line) && !trimmed.starts_with(['=', '|']);
        let clipped = short_slice(trimmed_end, 200);
        if self.kept.len() < KEPT_LINES && self.diagnostics == 0
            || diagnostic && self.diagnostics < KEPT_DIAGNOSTICS
        {
            self.diagnostics += usize::from(diagnostic);
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
        // Lines left out between the kept ones show as `…`.
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiagKind {
    Warning,
    Error,
}

struct PendingDiag {
    kind: DiagKind,
    rule: Option<String>,
    message: String,
    location: Option<String>,
    first: usize,
    last: usize,
    saw_blank: bool,
    /// Verbatim lines retained for `error:` / `error[E...]:` blocks.
    lines: Vec<String>,
}

struct CompilerWarning {
    rule: Option<String>,
    message: String,
    location: Option<String>,
    first: usize,
    last: usize,
}

struct CompilerError {
    first: usize,
    last: usize,
    lines: Vec<String>,
}

/// Streaming parser for rustc/Cargo `warning:` and `error:` / `error[E...]:` blocks.
#[derive(Default)]
struct CompilerDiag {
    pending: Option<PendingDiag>,
    warnings: Vec<CompilerWarning>,
    errors: Vec<CompilerError>,
    warning_span: Range,
    error_span: Range,
}

impl CompilerDiag {
    fn observe_blank(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.saw_blank = true;
        }
    }

    fn close(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match pending.kind {
            DiagKind::Warning => {
                self.warning_span.extend(pending.first, pending.last);
                self.warnings.push(CompilerWarning {
                    rule: pending.rule,
                    message: pending.message,
                    location: pending.location,
                    first: pending.first,
                    last: pending.last,
                });
            }
            DiagKind::Error => {
                self.error_span.extend(pending.first, pending.last);
                self.errors.push(CompilerError {
                    first: pending.first,
                    last: pending.last,
                    lines: pending.lines,
                });
            }
        }
    }

    /// Consume `line` if it belongs to a rustc warning/error block or Cargo warning summary.
    fn observe(&mut self, line: &str, n: usize) -> bool {
        if CARGO_WARN_SUMMARY.is_match(line) {
            self.close();
            self.warning_span.extend(n, n);
            return true;
        }
        if let Some(cap) = RUST_DIAG_HEADER.captures(line) {
            self.close();
            let kind = if &cap[1] == "error" {
                DiagKind::Error
            } else {
                DiagKind::Warning
            };
            let rule = cap.get(2).map(|m| m.as_str().to_owned());
            let message = short(cap[3].trim(), 240);
            let lines = if kind == DiagKind::Error {
                vec![line.trim_end().to_owned()]
            } else {
                Vec::new()
            };
            self.pending = Some(PendingDiag {
                kind,
                rule,
                message,
                location: None,
                first: n,
                last: n,
                saw_blank: false,
                lines,
            });
            return true;
        }
        if let Some(pending) = &mut self.pending {
            if is_diag_continuation(line, pending.saw_blank) {
                pending.last = n;
                pending.saw_blank = false;
                if pending.kind == DiagKind::Error {
                    pending.lines.push(line.trim_end().to_owned());
                } else {
                    if pending.location.is_none()
                        && let Some(cap) = RUST_ARROW.captures(line)
                    {
                        pending.location = Some(format!("{}:{}", &cap[1], &cap[2]));
                    }
                    if pending.rule.is_none()
                        && !RUST_SNIPPET.is_match(line)
                        && let Some(rule) = lint_attribute(line)
                    {
                        pending.rule = Some(rule);
                    }
                }
                return true;
            }
            self.close();
        }
        false
    }

    fn has_grouped_warnings(&self) -> bool {
        self.warnings.len() >= 2
    }

    fn push_notes(&self, stream: Stream, notes: &mut Vec<String>) {
        let name = stream.name();
        if !self.errors.is_empty() {
            notes.push(format!(
                "{name}: {} error(s) [{}]",
                self.errors.len(),
                format_range(name, self.error_span.first, self.error_span.last)
            ));
            let multiple = self.errors.len() > 1;
            for err in &self.errors {
                if multiple {
                    notes.push(format!("  [{}]", format_range(name, err.first, err.last)));
                }
                for line in &err.lines {
                    notes.push(format!("  {line}"));
                }
            }
        }
        if !self.warnings.is_empty() {
            let mut learned: BTreeMap<String, String> = BTreeMap::new();
            for warning in &self.warnings {
                if let Some(rule) = &warning.rule {
                    learned
                        .entry(normalize_warning_msg(&warning.message))
                        .or_insert_with(|| rule.clone());
                }
            }
            let mut groups: BTreeMap<String, Vec<&CompilerWarning>> = BTreeMap::new();
            for warning in &self.warnings {
                let normalized = normalize_warning_msg(&warning.message);
                let key = warning
                    .rule
                    .clone()
                    .or_else(|| learned.get(&normalized).cloned())
                    .or_else(|| rustc_lint(&warning.message).map(str::to_owned))
                    .unwrap_or(normalized);
                groups.entry(key).or_default().push(warning);
            }
            let mut ordered: Vec<(String, Vec<&CompilerWarning>)> = groups.into_iter().collect();
            ordered.sort_by(|(ak, av), (bk, bv)| bv.len().cmp(&av.len()).then(ak.cmp(bk)));
            notes.push(format!(
                "{name}: {} compiler warning(s) in {} group(s) [{}]",
                self.warnings.len(),
                ordered.len(),
                format_range(name, self.warning_span.first, self.warning_span.last)
            ));
            for (key, items) in ordered {
                let first = items.iter().map(|w| w.first).min().unwrap_or(0);
                let last = items.iter().map(|w| w.last).max().unwrap_or(0);
                let locs: Vec<&str> = items.iter().filter_map(|w| w.location.as_deref()).collect();
                let range = format_range(name, first, last);
                if locs.is_empty() {
                    notes.push(format!("  warning {key} ×{} [{range}]", items.len()));
                } else {
                    let mut loc_list = locs[..locs.len().min(SHOWN_WARNING_LOCS)].join(", ");
                    if locs.len() > SHOWN_WARNING_LOCS {
                        loc_list.push_str(&format!(" (+{} more)", locs.len() - SHOWN_WARNING_LOCS));
                    }
                    notes.push(format!(
                        "  warning {key} ×{}: {loc_list} [{range}]",
                        items.len()
                    ));
                }
            }
        }
    }
}

struct CargoFailure {
    block: usize,
    name: String,
    stream: Stream,
    first: usize,
    last: usize,
    detail_lines: Vec<String>,
    panic_loc: Option<String>,
    has_workspace_loc: bool,
    in_backtrace: bool,
    saw_panic: bool,
}

impl CargoFailure {
    fn detail(&self) -> String {
        let mut joined = self.detail_lines.join(" ");
        if let Some(loc) = &self.panic_loc
            && !joined.contains(loc.as_str())
        {
            if !joined.is_empty() {
                joined.push_str(" (at ");
                joined.push_str(loc);
                joined.push(')');
            } else {
                joined.push_str(loc);
            }
        }
        short_preserving_loc(&joined, 440, self.panic_loc.as_deref())
    }

    fn normalized_key(&self) -> String {
        let detail = self.detail();
        if detail.is_empty() {
            String::new()
        } else {
            normalize_root_cause(&detail)
        }
    }
}

struct NextestFailure {
    kind: &'static str,
    name: String,
    stream: Stream,
    first: usize,
    last: usize,
    excerpt: Vec<String>,
    panic_loc: Option<String>,
    has_workspace_loc: bool,
    in_backtrace: bool,
    saw_panic: bool,
    stopped: bool,
}

impl NextestFailure {
    fn render_single(&self) -> String {
        let range = format_range(self.stream.name(), self.first, self.last);
        let mut block = format!("{} {} [{range}]", self.kind, self.name);
        let mut has_loc = false;
        for line in &self.excerpt {
            if let Some(loc) = &self.panic_loc
                && line.contains(loc.as_str())
            {
                has_loc = true;
            }
            block.push_str("\n  ");
            block.push_str(line);
        }
        if let Some(loc) = &self.panic_loc
            && !has_loc
        {
            block.push_str("\n  at ");
            block.push_str(loc);
        }
        block
    }

    fn normalized_key(&self) -> String {
        if self.excerpt.is_empty() && self.panic_loc.is_none() {
            return String::new();
        }
        let mut joined = self
            .excerpt
            .iter()
            .enumerate()
            .map(|(idx, line)| {
                if idx == 0 {
                    THREAD_PANIC_PREFIX
                        .replace(line, "panicked at ")
                        .into_owned()
                } else {
                    line.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(loc) = &self.panic_loc
            && !joined.contains(loc.as_str())
        {
            if !joined.is_empty() {
                joined.push('\n');
            }
            joined.push_str("at ");
            joined.push_str(loc);
        }
        format!("{}\n{}", self.kind, normalize_root_cause(&joined))
    }
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
    in_failures: bool,
    cargo_failures: Vec<CargoFailure>,
    current_failure: Option<usize>,
    panic_lines: usize,
    status: [Range; 2],
    other: [Other; 2],
    other_non_diag: [Other; 2],
    diag: [CompilerDiag; 2],
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

    fn observe_other_or_diag(&mut self, line: &str, n: usize, stream: Stream) {
        let index = stream as usize;
        self.other[index].observe(line, n);
        if !self.diag[index].observe(line, n) {
            self.other_non_diag[index].observe(line, n);
        }
    }

    fn observe_failure_line(&mut self, line: &str, n: usize, stream: Stream) {
        if !self.in_failures {
            return;
        }
        let trimmed = line.trim();
        if trimmed == "failures:" {
            self.current_failure = None;
            self.panic_lines = 0;
            return;
        }
        if let Some(name) = trimmed
            .strip_prefix("---- ")
            .and_then(|s| {
                s.strip_suffix(" stdout ----")
                    .or_else(|| s.strip_suffix(" stderr ----"))
            })
            .filter(|s| !s.is_empty())
        {
            let block = self.blocks.len().saturating_sub(1);
            let name_short = short(name, 240);
            let pos = self
                .cargo_failures
                .iter()
                .rposition(|f| {
                    f.block == block
                        && (f.name == name_short
                            || f.name.strip_suffix(" - compile") == Some(name_short.as_str()))
                })
                .unwrap_or_else(|| {
                    self.cargo_failures.push(CargoFailure {
                        block,
                        name: name_short,
                        stream,
                        first: n,
                        last: n,
                        detail_lines: Vec::new(),
                        panic_loc: None,
                        has_workspace_loc: false,
                        in_backtrace: false,
                        saw_panic: false,
                    });
                    self.cargo_failures.len() - 1
                });
            self.current_failure = Some(pos);
            self.panic_lines = 0;
            return;
        }
        let Some(pos) = self.current_failure else {
            return;
        };
        let failure = &mut self.cargo_failures[pos];
        let mut captured_msg = false;
        if self.panic_lines > 0 {
            if trimmed.starts_with("stack backtrace:") {
                self.panic_lines = 0;
                failure.in_backtrace = true;
            } else if trimmed.starts_with("note:") {
                self.panic_lines = 0;
            } else {
                let loc = extract_rust_location(trimmed);
                if let Some(l) = loc {
                    record_rust_loc(&mut failure.panic_loc, &mut failure.has_workspace_loc, l);
                }
                failure
                    .detail_lines
                    .push(short_preserving_loc(trimmed, 200, loc));
                failure.last = n;
                self.panic_lines -= 1;
                captured_msg = true;
            }
        }
        if let Some((_, location)) = trimmed.split_once(" panicked at ") {
            failure.first = n;
            failure.last = n;
            failure.saw_panic = true;
            failure.in_backtrace = false;
            failure.detail_lines.clear();
            let loc = extract_rust_location(location);
            if let Some(l) = loc {
                record_rust_loc(&mut failure.panic_loc, &mut failure.has_workspace_loc, l);
            }
            failure
                .detail_lines
                .push(short_preserving_loc(location, 200, loc));
            self.panic_lines = if location.ends_with(':')
                || (location.starts_with('\'') && !location.contains("', "))
            {
                MAX_PANIC_LINES
            } else {
                0
            };
        } else if trimmed.starts_with("stack backtrace:") {
            self.panic_lines = 0;
            failure.in_backtrace = true;
        } else if failure.in_backtrace {
            // Scan `RUST_BACKTRACE=1` frames only to recover the first workspace `src/...:line:col`
            // location when the panic itself occurred in std/core/alloc.
            if !failure.has_workspace_loc
                && trimmed.starts_with("at ")
                && let Some(loc) = extract_rust_location(trimmed)
                && is_workspace_rust_loc(loc)
            {
                record_rust_loc(&mut failure.panic_loc, &mut failure.has_workspace_loc, loc);
            }
        } else if !captured_msg
            && failure.saw_panic
            && (trimmed.starts_with("assertion ")
                || trimmed.starts_with("left:")
                || trimmed.starts_with("right:"))
            && n.saturating_sub(failure.last) <= 2
        {
            failure.detail_lines.push(short(trimmed, 200));
            failure.last = n;
        } else if !failure.saw_panic && failure.detail_lines.len() < 3 {
            if trimmed.starts_with("Error: ")
                || (RUST_DIAG_HEADER.is_match(trimmed) && trimmed.starts_with("error"))
            {
                if failure.detail_lines.is_empty() {
                    failure.first = n;
                }
                failure.last = n;
                let loc = extract_rust_location(trimmed);
                if let Some(l) = loc {
                    record_rust_loc(&mut failure.panic_loc, &mut failure.has_workspace_loc, l);
                }
                failure
                    .detail_lines
                    .push(short_preserving_loc(trimmed, 200, loc));
            } else if !failure.detail_lines.is_empty()
                && let Some(cap) = RUST_ARROW.captures(trimmed)
            {
                let loc = match cap.get(3) {
                    Some(col) => format!("{}:{}:{}", &cap[1], &cap[2], col.as_str()),
                    None => format!("{}:{}", &cap[1], &cap[2]),
                };
                record_rust_loc(&mut failure.panic_loc, &mut failure.has_workspace_loc, &loc);
                failure.last = n;
            }
        }
    }

    /// Read one line, without its line break, of `stream`.
    pub fn observe(&mut self, line: &str, n: usize, stream: Stream) {
        let clean = strip_ansi(line.trim_end_matches(['\n', '\r']));
        let line = clean.as_ref();
        if self.nextest.enabled {
            self.observe_nextest(line, n, stream);
            return;
        }
        if self.broken {
            return;
        }
        let index = stream as usize;
        if line.trim().is_empty() {
            self.diag[index].observe_blank();
            if self.section && stream == Stream::Stdout {
                self.panic_lines = 0;
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("test result:") {
            self.diag[index].close();
            self.section = false;
            self.in_failures = false;
            self.current_failure = None;
            self.panic_lines = 0;
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
        if self.section && stream == Stream::Stdout {
            self.observe_failure_line(line, n, stream);
            return;
        }
        if let Some(found) = RUNNING.captures(line) {
            self.diag[index].close();
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
            self.diag[index].close();
            self.section = true;
            if line == "failures:" {
                self.in_failures = true;
                self.current_failure = None;
                self.panic_lines = 0;
            } else if line == "successes:" {
                self.in_failures = false;
                self.current_failure = None;
                self.panic_lines = 0;
            } else {
                let block = self.blocks.len().saturating_sub(1);
                let name = line
                    .strip_prefix("---- ")
                    .and_then(|s| s.strip_suffix(" stdout ----"))
                    .unwrap_or("");
                let name_short = short(name, 240);
                if let Some(pos) = self
                    .cargo_failures
                    .iter()
                    .rposition(|f| f.block == block && f.name == name_short)
                {
                    self.in_failures = true;
                    self.current_failure = Some(pos);
                    self.panic_lines = 0;
                } else {
                    self.in_failures = false;
                }
            }
            return;
        }
        // A test's outcome, the note on a slow one, and quiet output's dots.
        if line.starts_with("test ")
            && (line.contains(" ... ") || line.ends_with(" has been running for over 60 seconds"))
            || QUIET.is_match(line)
        {
            self.diag[index].close();
            if let Some(name) = line
                .strip_prefix("test ")
                .and_then(|s| s.strip_suffix(" ... FAILED"))
            {
                let block = self.blocks.len().saturating_sub(1);
                let name_short = short(name, 240);
                if !self
                    .cargo_failures
                    .iter()
                    .any(|f| f.block == block && f.name == name_short)
                {
                    self.cargo_failures.push(CargoFailure {
                        block,
                        name: name_short,
                        stream,
                        first: n,
                        last: n,
                        detail_lines: Vec::new(),
                        panic_loc: None,
                        has_workspace_loc: false,
                        in_backtrace: false,
                        saw_panic: false,
                    });
                }
            }
            return;
        }
        if let Some(found) = DOC_TESTS.captures(line) {
            self.diag[index].close();
            self.names.push((format!("doc-tests {}", &found[1]), None));
            self.status[index].add(n);
            return;
        }
        if crate::lint::CARGO_STATUS.is_match(line) {
            self.diag[index].close();
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
        self.observe_other_or_diag(line, n, stream);
    }

    fn observe_nextest(&mut self, line: &str, n: usize, stream: Stream) {
        let trimmed = line.trim();
        let index = stream as usize;
        if trimmed.is_empty() {
            self.diag[index].observe_blank();
            return;
        }
        if crate::lint::CARGO_STATUS.is_match(line) {
            self.diag[index].close();
            self.nextest.in_output = false;
            self.nextest.current = None;
            self.status[index].add(n);
            return;
        }
        if trimmed.starts_with("Summary [")
            && (trimmed.contains(" tests run:") || trimmed.contains(" test run:"))
        {
            self.diag[index].close();
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
            self.diag[index].close();
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
                        panic_loc: None,
                        has_workspace_loc: false,
                        in_backtrace: false,
                        saw_panic: false,
                        stopped: false,
                    });
                    self.nextest.failures.len() - 1
                });
            // A `note:` inside STDOUT must never block capturing the actual panic in STDERR.
            if !self.nextest.failures[pos].saw_panic {
                self.nextest.failures[pos].stopped = false;
                self.nextest.failures[pos].in_backtrace = false;
            }
            self.nextest.in_output = true;
            self.nextest.current = Some((pos, stream));
            return;
        }
        if let Some((kind, name)) = nextest_failure(trimmed) {
            self.diag[index].close();
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
                    panic_loc: None,
                    has_workspace_loc: false,
                    in_backtrace: false,
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
            self.diag[index].close();
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
            if let Some((_, location)) = trimmed.split_once(" panicked at ") {
                failure.saw_panic = true;
                failure.stopped = false;
                failure.in_backtrace = false;
                failure.first = n;
                failure.last = n;
                failure.excerpt.clear();
                let loc = extract_rust_location(location);
                if let Some(l) = loc {
                    record_rust_loc(&mut failure.panic_loc, &mut failure.has_workspace_loc, l);
                }
                failure
                    .excerpt
                    .push(short_preserving_loc(trimmed, 200, loc));
            } else if failure.in_backtrace {
                // Recover first workspace `src/...:line:col` frame from `RUST_BACKTRACE=1`
                // without dumping backtrace noise into the excerpt.
                if !failure.has_workspace_loc
                    && trimmed.starts_with("at ")
                    && let Some(loc) = extract_rust_location(trimmed)
                    && is_workspace_rust_loc(loc)
                {
                    record_rust_loc(&mut failure.panic_loc, &mut failure.has_workspace_loc, loc);
                }
            } else if !failure.stopped {
                if trimmed.starts_with("stack backtrace:") {
                    failure.stopped = true;
                    failure.in_backtrace = true;
                } else if trimmed.starts_with("note:") {
                    failure.stopped = true;
                } else if failure.excerpt.len() < 6
                    && (failure.saw_panic || failure.excerpt.len() < 3)
                {
                    let loc = extract_rust_location(trimmed);
                    if let Some(l) = loc {
                        record_rust_loc(&mut failure.panic_loc, &mut failure.has_workspace_loc, l);
                    }
                    failure.last = n;
                    failure
                        .excerpt
                        .push(short_preserving_loc(trimmed, 200, loc));
                }
            }
            return;
        }
        self.observe_other_or_diag(line, n, stream);
    }

    fn stderr_covered(&self) -> bool {
        let idx = Stream::Stderr as usize;
        if self.diag[idx].has_grouped_warnings() {
            self.other_non_diag[idx].all_shown()
        } else {
            self.other[idx].all_shown()
        }
    }

    fn push_stream_notes(&self, stream: Stream, include_nextest: bool, notes: &mut Vec<String>) {
        let idx = stream as usize;
        let name = stream.name();
        let grouped_diag = self.diag[idx].has_grouped_warnings();
        if grouped_diag {
            self.diag[idx].push_notes(stream, notes);
        }
        let status = &self.status[idx];
        if status.count > 0 {
            notes.push(format!(
                "{name}: {} cargo status line(s) [{name}:{}-{}]",
                status.count, status.first, status.last
            ));
        }
        if include_nextest {
            let progress = &self.nextest.progress[idx];
            if progress.count > 0 {
                notes.push(format!(
                    "{name}: {} nextest pass/status line(s) [{name}:{}-{}]",
                    progress.count, progress.first, progress.last
                ));
            }
        }
        let other = if grouped_diag {
            &self.other_non_diag[idx]
        } else {
            &self.other[idx]
        };
        other.push_notes(stream, notes);
    }

    fn finish_nextest(mut self) -> Option<Totals> {
        for diag in &mut self.diag {
            diag.close();
        }
        let summary = self.nextest.summary.clone()?;
        let mut notes = Vec::new();
        let stderr = self.stderr_covered();
        for stream in Stream::BOTH {
            if stream == Stream::Stderr && !stderr {
                continue;
            }
            self.push_stream_notes(stream, true, &mut notes);
        }
        let nextest_failures: Vec<String> = self
            .nextest
            .failures
            .iter()
            .map(NextestFailure::render_single)
            .collect();
        let grouped_failures = group_nextest_failures(&self.nextest.failures);
        Some(Totals {
            binaries: Vec::new(),
            notes,
            stderr,
            nextest_summary: Some(summary),
            nextest_failures,
            grouped_failures,
            failure_count: 0,
        })
    }

    pub fn finish(mut self) -> Option<Totals> {
        if self.nextest.enabled {
            return self.finish_nextest();
        }
        for diag in &mut self.diag {
            diag.close();
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
        for (block, name) in std::mem::take(&mut self.blocks).into_iter().zip(names) {
            let mut binary = block.result?;
            binary.name = name;
            binaries.push(binary);
        }
        let mut notes = Vec::new();
        let stderr = self.stderr_covered();
        for stream in Stream::BOTH {
            if stream == Stream::Stderr && !stderr {
                continue;
            }
            self.push_stream_notes(stream, false, &mut notes);
        }
        let (grouped_failures, failure_count, nextest_summary) =
            match group_cargo_failures(&self.cargo_failures) {
                Some(groups) => (groups, self.cargo_failures.len(), Some(String::new())),
                None => (Vec::new(), 0, None),
            };
        Some(Totals {
            binaries,
            notes,
            stderr,
            nextest_summary,
            nextest_failures: Vec::new(),
            grouped_failures,
            failure_count,
        })
    }
}

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

fn extract_rust_location(text: &str) -> Option<&str> {
    RUST_LOC
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

fn is_workspace_rust_loc(loc: &str) -> bool {
    !(loc.starts_with("/rustc/")
        || loc.contains("/.cargo/registry/")
        || loc.contains("\\.cargo\\registry\\")
        || loc.contains("/.rustup/")
        || loc.starts_with("library/core/")
        || loc.starts_with("library/std/")
        || loc.starts_with("library/alloc/")
        || loc.contains("/library/core/src/")
        || loc.contains("/library/std/src/")
        || loc.contains("/library/alloc/src/"))
}

fn record_rust_loc(slot: &mut Option<String>, has_workspace: &mut bool, loc: &str) {
    let ws = is_workspace_rust_loc(loc);
    if !*has_workspace && (ws || slot.is_none()) {
        *slot = Some(loc.to_owned());
        if ws {
            *has_workspace = true;
        }
    }
}

/// Truncate `text` to at most `limit` UTF-8 bytes while pinning `loc` (`src/...:line:col`)
/// so panic locations are never lost even under tight per-line or per-failure budgets.
fn short_preserving_loc(text: &str, limit: usize, loc: Option<&str>) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let Some(loc) = loc.filter(|l| !l.is_empty()) else {
        return short(text, limit);
    };
    let truncated = short(text, limit);
    if truncated.contains(loc) {
        return truncated;
    }
    if loc.len() >= limit {
        return short(loc, limit);
    }
    let sep = " … ";
    let head_budget = limit.saturating_sub(loc.len() + sep.len());
    if head_budget < 8 {
        return loc.to_owned();
    }
    let head = short_slice(text, head_budget).trim_end();
    format!("{head}{sep}{loc}")
}

fn format_range(stream: &str, first: usize, last: usize) -> String {
    if first == last {
        format!("{stream}:{first}")
    } else {
        format!("{stream}:{first}-{last}")
    }
}

fn is_diag_continuation(line: &str, saw_blank: bool) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("note:")
        || trimmed.starts_with("help:")
        || line.starts_with("For more information about ")
        || line.starts_with("Some errors have detailed explanations:")
    {
        return true;
    }
    if saw_blank {
        return false;
    }
    RUST_ARROW.is_match(line)
        || RUST_SNIPPET.is_match(line)
        || trimmed.starts_with("= note:")
        || trimmed.starts_with("= help:")
        || line.starts_with("  ")
}

fn lint_attribute(line: &str) -> Option<String> {
    let cap = RUST_LINT_ATTR.captures(line)?;
    cap.get(1)
        .or_else(|| cap.get(2))
        .or_else(|| cap.get(3))
        .map(|m| m.as_str().to_owned())
}

fn rustc_lint(message: &str) -> Option<&'static str> {
    RUSTC_LINTS
        .iter()
        .find_map(|(pattern, lint)| pattern.is_match(message).then_some(*lint))
}

fn normalize_warning_msg(message: &str) -> String {
    QUOTED.replace_all(message.trim(), "`_`").into_owned()
}

/// Normalize ephemeral tempdir paths and long hex pointer addresses ONLY for
/// root-cause grouping keys. Representative messages stay verbatim.
fn normalize_root_cause(text: &str) -> String {
    let paths = EPHEMERAL_PATH.replace_all(text, "<tmp>");
    HEX_ADDR.replace_all(&paths, "0x<addr>").into_owned()
}

/// Group `cargo test` failures by normalized root cause. Returns `Some(blocks)`
/// when normalization collapses failures into strictly fewer groups than exact
/// unnormalized details (so `summary.rs` delegates first-run rendering to us).
fn group_cargo_failures(failures: &[CargoFailure]) -> Option<Vec<String>> {
    if failures.len() < 2 {
        return None;
    }
    let mut raw_keys = HashSet::new();
    let mut groups: Vec<(String, Vec<&CargoFailure>)> = Vec::new();
    for failure in failures {
        let raw = failure.detail();
        raw_keys.insert(if raw.is_empty() {
            format!("\0{}", failure.name)
        } else {
            raw
        });
        let key = failure.normalized_key();
        if !key.is_empty()
            && let Some((_, existing)) = groups.iter_mut().find(|(k, _)| *k == key)
        {
            existing.push(failure);
            continue;
        }
        groups.push((key, vec![failure]));
    }
    if groups.len() >= raw_keys.len() {
        return None;
    }
    let mut rendered = Vec::with_capacity(groups.len());
    for (_, items) in groups {
        let rep = items[0];
        let rep_detail = rep.detail();
        let rep_range = format_range(rep.stream.name(), rep.first, rep.last);
        if rep_detail.is_empty() {
            for item in items {
                let range = format_range(item.stream.name(), item.first, item.last);
                rendered.push(format!("{}  [{range}]", item.name));
            }
            continue;
        }
        let mut block = format!(
            "{} test(s): {} {rep_detail} [{rep_range}]",
            items.len(),
            rep.name
        );
        for item in &items[1..] {
            let range = format_range(item.stream.name(), item.first, item.last);
            block.push_str(&format!("\n  {} [{range}]", item.name));
        }
        rendered.push(block);
    }
    Some(rendered)
}

/// Group `cargo nextest` failures by normalized root cause when at least two
/// failures share a root cause. Singleton groups keep the exact single-failure shape.
fn group_nextest_failures(failures: &[NextestFailure]) -> Vec<String> {
    if failures.len() < 2 {
        return Vec::new();
    }
    let mut groups: Vec<(String, Vec<&NextestFailure>)> = Vec::new();
    for failure in failures {
        let key = failure.normalized_key();
        if !key.is_empty()
            && let Some((_, existing)) = groups.iter_mut().find(|(k, _)| *k == key)
        {
            existing.push(failure);
            continue;
        }
        groups.push((key, vec![failure]));
    }
    if !groups.iter().any(|(_, items)| items.len() > 1) {
        return Vec::new();
    }
    let mut rendered = Vec::with_capacity(groups.len());
    for (_, items) in groups {
        if items.len() == 1 {
            rendered.push(items[0].render_single());
            continue;
        }
        let rep = items[0];
        let rep_range = format_range(rep.stream.name(), rep.first, rep.last);
        let mut block = format!(
            "{} test(s): {} {} [{rep_range}]",
            items.len(),
            rep.kind,
            rep.name
        );
        let mut has_loc = false;
        for line in &rep.excerpt {
            if let Some(loc) = &rep.panic_loc
                && line.contains(loc.as_str())
            {
                has_loc = true;
            }
            block.push_str("\n  ");
            block.push_str(line);
        }
        if let Some(loc) = &rep.panic_loc
            && !has_loc
        {
            block.push_str("\n  at ");
            block.push_str(loc);
        }
        for item in &items[1..] {
            let range = format_range(item.stream.name(), item.first, item.last);
            block.push_str(&format!("\n  {} [{range}]", item.name));
        }
        rendered.push(block);
    }
    rendered
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
        if self.binaries.is_empty()
            && let Some(summary) = &self.nextest_summary
        {
            let mut report = String::new();
            if !self.nextest_failures.is_empty() {
                report.push_str(&format!(
                    "{} reported failing test(s)\n",
                    self.nextest_failures.len()
                ));
                let list = if self.grouped_failures.is_empty() {
                    &self.nextest_failures
                } else {
                    &self.grouped_failures
                };
                for failure in list {
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
        let mut report = String::new();
        if !self.grouped_failures.is_empty() {
            report.push_str(&format!(
                "{} reported failing test(s)\n",
                self.failure_count
            ));
            for group in &self.grouped_failures {
                report.push_str(group);
                report.push('\n');
            }
        }
        self.push_cargo_totals(&mut report);
        report
    }

    fn push_cargo_totals(&self, report: &mut String) {
        let sum = |count: fn(&Binary) -> usize| self.binaries.iter().map(count).sum::<usize>();
        let failed = sum(|b| b.failed);
        report.push_str(&format!(
            "test result: {}. {} passed; {failed} failed; {} ignored",
            if failed > 0 { "FAILED" } else { "ok" },
            sum(|b| b.passed),
            sum(|b| b.ignored),
        ));
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
                idle.push(binary);
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
            if idle.len() <= NAMED_IDLE {
                let names = idle
                    .iter()
                    .map(|binary| binary.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                report.push_str(&format!("  {} ran no tests: {names}\n", idle.len()));
            } else {
                let first = idle.iter().map(|b| b.first).min().unwrap_or(0);
                let last = idle.iter().map(|b| b.last).max().unwrap_or(0);
                report.push_str(&format!(
                    "  {} empty target(s) omitted [stdout:{first}-{last}]\n",
                    idle.len()
                ));
            }
        }
        for note in &self.notes {
            report.push_str(note);
            report.push('\n');
        }
    }

    /// Nextest summary line + status/pass range notes without the failure list
    /// (for merged reruns whose `Compared with` section already listed changes),
    /// or `cargo test` totals without the failure list on reruns.
    pub fn render_nextest_summary(&self) -> String {
        let mut report = String::new();
        if !self.binaries.is_empty() {
            self.push_cargo_totals(&mut report);
            return report;
        }
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

/// The number before `word` in `passed; 2 failed; 1 ignored` (zero-alloc scan).
fn count_before(line: &str, word: &str) -> Option<usize> {
    let mut prev: Option<&str> = None;
    for tok in line.split_whitespace() {
        if tok.trim_end_matches([',', ';']) == word {
            return prev.and_then(|p| p.parse().ok());
        }
        prev = Some(tok);
    }
    None
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

    fn nextest_totals(streams: &[(&str, Stream)]) -> Option<Totals> {
        let mut scan = Scan::nextest();
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

    #[test]
    fn workspace_with_25_crates_collapses_empty_targets_and_doctests_accurately() {
        // 25-crate workspace: 7 active targets (including 1 active doc-test) + 18 empty targets
        // (10 empty unit/bin targets + 8 empty doc-tests), plus ignored and filtered counts.
        let mut out = String::new();
        let mut err = String::new();
        for i in 1..=25 {
            err.push_str(&format!(
                "   Compiling svc_{i:02} v0.1.0 (/work/ws/crates/svc_{i:02})\n"
            ));
        }
        err.push_str("    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.42s\n");

        let mut expected_passed = 0;
        let mut expected_ignored = 0;
        let mut expected_filtered = 0;

        // 6 active unit targets.
        for i in 1..=6 {
            err.push_str(&format!(
                "     Running unittests src/lib.rs (/work/ws/target/debug/deps/svc_{i:02}-aaaa{i:04})\n"
            ));
            let pass_n = i + 2;
            let ign_n = usize::from(i == 6);
            let filt_n = 2;
            expected_passed += pass_n;
            expected_ignored += ign_n;
            expected_filtered += filt_n;
            out.push_str(&format!("\nrunning {} tests\n", pass_n + ign_n));
            for t in 1..=pass_n {
                out.push_str(&format!("test svc_{i:02}::tests::case_{t:02} ... ok\n"));
            }
            if ign_n > 0 {
                out.push_str("test svc_06::tests::slow_case ... ignored\n");
            }
            out.push_str(&format!(
                "\ntest result: ok. {pass_n} passed; 0 failed; {ign_n} ignored; 0 measured; {filt_n} filtered out; finished in 0.01s\n\n"
            ));
        }

        // 10 empty unit/bin targets.
        for i in 7..=16 {
            err.push_str(&format!(
                "     Running unittests src/main.rs (/work/ws/target/debug/deps/svc_{i:02}-bbbb{i:04})\n"
            ));
            expected_filtered += 1;
            out.push_str(
                "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s\n\n",
            );
        }

        // 1 active doc-test target.
        err.push_str("   Doc-tests svc_01\n");
        expected_passed += 2;
        out.push_str(concat!(
            "\nrunning 2 tests\n",
            "test crates/svc_01/src/lib.rs - init (line 5) ... ok\n",
            "test crates/svc_01/src/lib.rs - run (line 18) ... ok\n",
            "\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.34s\n\n",
        ));

        // 8 empty doc-test targets.
        for i in 17..=24 {
            err.push_str(&format!("   Doc-tests svc_{i:02}\n"));
            out.push_str(
                "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n",
            );
        }

        let raw_bytes = out.len() + err.len();
        let result = totals(&[(&out, Stream::Stdout), (&err, Stream::Stderr)]).unwrap();
        assert!(result.stderr, "all stderr status lines must be covered");
        let rendered = result.render();

        assert!(
            rendered.starts_with(&format!(
                "test result: ok. {expected_passed} passed; 0 failed; {expected_ignored} ignored; {expected_filtered} filtered out · 25 test binaries\n"
            )),
            "{rendered}"
        );
        assert!(
            rendered.contains("  18 empty target(s) omitted [stdout:"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  2 passed  doc-tests svc_01 [stdout:"),
            "{rendered}"
        );
        // Active targets stay named; empty targets are collapsed without repeating boilerplate.
        assert!(
            rendered.contains("unittests src/lib.rs (svc_01)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("unittests src/lib.rs (svc_06)"),
            "{rendered}"
        );
        assert!(!rendered.contains("svc_17"), "{rendered}");
        assert!(!rendered.contains("ran no tests"), "{rendered}");
        // Substantial byte reduction vs raw output.
        assert!(
            rendered.len() * 6 < raw_bytes,
            "expected >83% byte reduction (raw={raw_bytes}, compact={}):\n{rendered}",
            rendered.len()
        );
    }

    #[test]
    fn repeated_stderr_compiler_warnings_group_while_keeping_errors_and_failing_test_clear() {
        let mut err = String::from("   Compiling billing v0.1.0 (/work/billing)\n");
        // 25 dead_code warnings (only first emits `= note: #[warn(dead_code)]`, like real rustc).
        for i in 0..25 {
            let line_no = 10 + i * 5;
            let file = if i % 2 == 0 {
                "src/engine.rs"
            } else {
                "src/ledger.rs"
            };
            err.push_str(&format!(
                "warning: function `unused_fn_{i:02}` is never used\n \
                  --> {file}:{line_no}:4\n  \
                   |\n\
                 {line_no} | fn unused_fn_{i:02}() {{}}\n  \
                   |    ^^^^^^^^^^^^\n"
            ));
            if i == 0 {
                err.push_str("  |\n  = note: `#[warn(dead_code)]` on by default\n");
            }
            err.push('\n');
        }
        // 15 unused_variables warnings (only first emits `= note: #[warn(unused_variables)]`).
        for i in 0..15 {
            let line_no = 20 + i * 4;
            err.push_str(&format!(
                "warning: unused variable: `tmp_{i:02}`\n \
                  --> src/calc.rs:{line_no}:9\n  \
                   |\n\
                 {line_no} |     let tmp_{i:02} = compute();\n  \
                   |         ^^^^^^ help: if this is intentional, prefix it with an underscore: `_tmp_{i:02}`\n"
            ));
            if i == 0 {
                err.push_str("  |\n  = note: `#[warn(unused_variables)]` on by default\n");
            }
            err.push('\n');
        }
        err.push_str(concat!(
            "warning: `billing` (lib test) generated 40 warnings (run `cargo fix --lib -p billing --tests` to apply 15 suggestions)\n",
            "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.85s\n",
            "     Running unittests src/lib.rs (/work/target/debug/deps/billing-11223344)\n",
            "     Running tests/checkout.rs (/work/target/debug/deps/checkout-55667788)\n",
            "error: test failed, to rerun pass `--test checkout`\n",
        ));

        let raw_bytes = FAILING.len() + err.len();

        // Full pipeline via `summary::Parser` verifies stderr coverage + crystal-clear failure.
        let mut parser = crate::summary::Parser::new("cargo test", None);
        for (i, line) in FAILING.split_inclusive('\n').enumerate() {
            parser.observe(line, i + 1);
        }
        for (i, line) in err.split_inclusive('\n').enumerate() {
            parser.observe_stderr(line, i + 1);
        }
        let summary = parser.finish();
        let cargo = summary.cargo.as_ref().expect("cargo totals");
        assert!(
            cargo.stderr,
            "grouped warnings + verbatim error must cover stderr so raw warnings are suppressed"
        );
        let rendered = summary.render(None, 101).expect("rendered report");

        // Failing test + assertion diff stay crystal clear at the top.
        assert!(
            rendered.starts_with(
                "1 reported failing test(s)\n1 test(s): total_includes_fee tests/checkout.rs:11:5: \
                 assertion `left == right` failed left: 106 right: 105 [stdout:20-23]\n"
            ),
            "{rendered}"
        );
        // Repetitive warnings collapse into 2 grouped lines with file:line lists & [stderr:A-B].
        assert!(
            rendered.contains("stderr: 40 compiler warning(s) in 2 group(s) [stderr:"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "  warning dead_code ×25: src/engine.rs:10, src/ledger.rs:15, src/engine.rs:20, src/ledger.rs:25 (+21 more) [stderr:"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "  warning unused_variables ×15: src/calc.rs:20, src/calc.rs:24, src/calc.rs:28, src/calc.rs:32 (+11 more) [stderr:"
            ),
            "{rendered}"
        );
        // Cargo error stays verbatim.
        assert!(
            rendered.contains("  error: test failed, to rerun pass `--test checkout`\n"),
            "{rendered}"
        );
        // Massive byte savings over raw stderr warning flood + stdout.
        assert!(
            rendered.len() * 8 < raw_bytes,
            "expected >87% byte reduction (raw={raw_bytes}, compact={}):\n{rendered}",
            rendered.len()
        );

        // Also verify real `error[E0308]:` compiler errors mixed with warnings stay 100% verbatim.
        let mut err_with_compile_error = err.clone();
        err_with_compile_error.push_str(concat!(
            "error[E0308]: mismatched types\n",
            "  --> src/tax.rs:42:18\n",
            "   |\n",
            "42 |     let rate: u32 = \"0.20\";\n",
            "   |               ---   ^^^^^^ expected `u32`, found `&str`\n",
            "   |               |\n",
            "   |               expected due to this\n",
            "\n",
            "error: could not compile `billing` (bin \"demo\") due to 1 previous error\n",
        ));
        let with_err = totals(&[
            (FAILING, Stream::Stdout),
            (&err_with_compile_error, Stream::Stderr),
        ])
        .unwrap();
        assert!(with_err.stderr);
        let rendered_err = with_err.render();
        for expected in [
            "error[E0308]: mismatched types",
            "  --> src/tax.rs:42:18",
            "42 |     let rate: u32 = \"0.20\";",
            "expected `u32`, found `&str`",
            "expected due to this",
            "error: could not compile `billing` (bin \"demo\") due to 1 previous error",
            "warning dead_code ×25:",
            "warning unused_variables ×15:",
        ] {
            assert!(
                rendered_err.contains(expected),
                "missing `{expected}` in:\n{rendered_err}"
            );
        }
    }

    #[test]
    fn twelve_tempdir_and_hex_failures_collapse_into_one_root_cause_preserving_real_message() {
        let tempdir_paths = [
            "/tmp/.tmpAb12Cd/workspace/config.toml",
            "/tmp/.tmpEf34Gh/workspace/config.toml",
            "/tmp/.tmpIj56Kl/workspace/config.toml",
            "/tmp/.tmpMn78Op/workspace/config.toml",
            "/var/tmp/ci-runner/tmp112233/workspace/config.toml",
            "/var/tmp/ci-runner/tmp445566/workspace/config.toml",
            "/var/tmp/justless-suite/.tmpQr90St/workspace/config.toml",
            "/var/tmp/.tmpUv12Wx/workspace/config.toml",
            "/private/var/folders/2z/9k12ab34_56/T/.tmpYz34Ab/workspace/config.toml",
            "/private/var/folders/ab/cdef987654/T/.tmpCd56Ef/workspace/config.toml",
            "/var/folders/ab/cdef987654/T/tmp778899/workspace/config.toml",
            "/tmp/.tmpGh78Ij/workspace/config.toml",
        ];
        let hex_addrs = [
            "0x7f8a12345678",
            "0x7f8a12349999",
            "0x7f8a88776655",
            "0x00007ffd5e8a1c00",
            "0x55aabbccdd00",
            "0x55aabbccdd10",
            "0x7fff00112233",
            "0x7fff44556677",
            "0x104a8c000",
            "0x104a8c040",
            "0xdeadbeef0001",
            "0xdeadbeef0002",
        ];

        let mut out = String::from("\nrunning 12 tests\n");
        for i in 1..=12 {
            out.push_str(&format!("test sync_case_{i:02} ... FAILED\n"));
        }
        out.push_str("\nfailures:\n\n");
        for i in 0..12 {
            let n = i + 1;
            out.push_str(&format!(
                "---- sync_case_{n:02} stdout ----\n\
                 thread 'sync_case_{n:02}' ({tid}) panicked at tests/integration.rs:42:5:\n\
                 assertion `left == right` failed: handle {addr} mismatch\n  \
                   left: \"{path}\"\n \
                  right: \"expected/config.toml\"\n\
                 note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n",
                tid = 10000 + n,
                addr = hex_addrs[i],
                path = tempdir_paths[i],
            ));
        }
        out.push_str("\nfailures:\n");
        for i in 1..=12 {
            out.push_str(&format!("    sync_case_{i:02}\n"));
        }
        out.push_str(
            "\ntest result: FAILED. 0 passed; 12 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.24s\n",
        );

        let err = concat!(
            "   Compiling app v0.1.0 (/work/app)\n",
            "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.40s\n",
            "     Running tests/integration.rs (/work/target/debug/deps/integration-aabbccdd)\n",
            "error: test failed, to rerun pass `--test integration`\n",
        );

        let raw_bytes = out.len() + err.len();

        // Exercise the full `summary::Parser` -> `Summary::render` path!
        let mut parser = crate::summary::Parser::new("cargo test", None);
        for (i, line) in out.split_inclusive('\n').enumerate() {
            parser.observe(line, i + 1);
        }
        for (i, line) in err.split_inclusive('\n').enumerate() {
            parser.observe_stderr(line, i + 1);
        }
        let summary = parser.finish();
        let rendered = summary.render(None, 101).expect("rendered report");

        // All 12 failures collapse into ONE grouped root cause (`12 test(s): ...`).
        assert!(
            rendered.starts_with("12 reported failing test(s)\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "12 test(s): sync_case_01 tests/integration.rs:42:5: \
                 assertion `left == right` failed: handle 0x7f8a12345678 mismatch \
                 left: \"/tmp/.tmpAb12Cd/workspace/config.toml\" \
                 right: \"expected/config.toml\" [stdout:"
            ),
            "{rendered}"
        );
        // Representative message is kept verbatim — placeholder tokens never leak into output.
        assert!(!rendered.contains("<tmp>"), "{rendered}");
        assert!(!rendered.contains("0x<addr>"), "{rendered}");
        // Only 1 grouped assertion block (not 12!).
        assert_eq!(
            rendered.matches("assertion `left == right` failed").count(),
            1,
            "{rendered}"
        );
        // All 12 failing test names and their `[stdout:A-B]` pointers are preserved.
        for i in 1..=12 {
            let name = format!("sync_case_{i:02}");
            assert!(rendered.contains(&name), "missing {name} in:\n{rendered}");
            if i > 1 {
                assert!(
                    rendered.contains(&format!("  {name} [stdout:")),
                    "missing pointer for {name} in:\n{rendered}"
                );
            }
        }
        assert!(
            rendered.contains(
                "test result: FAILED. 0 passed; 12 failed; 0 ignored · 1 test binaries\n"
            ),
            "{rendered}"
        );
        // Strong byte reduction vs raw output.
        assert!(
            rendered.len() * 3 < raw_bytes,
            "expected >66% byte reduction (raw={raw_bytes}, compact={}):\n{rendered}",
            rendered.len()
        );

        // Rerun diffing via `Summary::render(Some(...))` stays intact and omits duplicate failure blocks.
        let mut rerun_parser = crate::summary::Parser::new("cargo test", None);
        for (i, line) in out.split_inclusive('\n').enumerate() {
            rerun_parser.observe(line, i + 1);
        }
        for (i, line) in err.split_inclusive('\n').enumerate() {
            rerun_parser.observe_stderr(line, i + 1);
        }
        let rerun = rerun_parser.finish();
        let delta = rerun.render(Some(("run-1", &summary)), 101).expect("rerun");
        assert!(
            delta.contains("Compared with run-1:\nIndexed items unchanged"),
            "{delta}"
        );
        assert!(
            delta.contains(
                "test result: FAILED. 0 passed; 12 failed; 0 ignored · 1 test binaries\n"
            ),
            "{delta}"
        );
        assert!(!delta.contains("12 test(s):"), "{delta}");

        // Nextest also groups repeated TempDir/hex failures and preserves verbatim representative output
        // plus exact failing count in `Summary::headline()`.
        let mut nx = String::from("    Starting 12 tests across 1 binary\n");
        for i in 0..12 {
            let n = i + 1;
            nx.push_str(&format!(
                "        FAIL [   0.012s] app::integration sync_case_{n:02}\n\
                 --- STDERR:              app::integration sync_case_{n:02} ---\n\
                 thread 'sync_case_{n:02}' ({tid}) panicked at tests/integration.rs:42:5:\n\
                 assertion `left == right` failed: handle {addr} mismatch\n  \
                   left: \"{path}\"\n \
                  right: \"expected/config.toml\"\n\
                 note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n",
                tid = 20000 + n,
                addr = hex_addrs[i],
                path = tempdir_paths[i],
            ));
        }
        nx.push_str(
            "------------\n     Summary [   0.350s] 12 tests run: 0 passed, 12 failed, 0 skipped\n",
        );
        let nx_totals = nextest_totals(&[(&nx, Stream::Stderr)]).unwrap();
        assert_eq!(nx_totals.nextest_failures.len(), 12);
        let nx_rendered = nx_totals.render();
        assert!(
            nx_rendered.starts_with("12 reported failing test(s)\n"),
            "{nx_rendered}"
        );
        assert!(
            nx_rendered.contains("12 test(s): FAIL app::integration sync_case_01 [stderr:"),
            "{nx_rendered}"
        );
        assert!(
            nx_rendered.contains("left: \"/tmp/.tmpAb12Cd/workspace/config.toml\""),
            "{nx_rendered}"
        );
        assert_eq!(
            nx_rendered
                .matches("assertion `left == right` failed")
                .count(),
            1,
            "{nx_rendered}"
        );
        assert!(!nx_rendered.contains("<tmp>"), "{nx_rendered}");
        assert!(
            nx_rendered.len() * 3 < nx.len(),
            "expected >66% nextest byte reduction (raw={}, compact={}):\n{nx_rendered}",
            nx.len(),
            nx_rendered.len()
        );
    }

    #[test]
    fn distinct_files_and_short_hex_constants_do_not_falsely_merge() {
        assert_eq!(
            normalize_root_cause("missing /tmp/.tmpAb12Cd/alpha.txt at 0x7f8a12345678"),
            "missing <tmp>/alpha.txt at 0x<addr>"
        );
        assert_ne!(
            normalize_root_cause("missing /tmp/.tmpAb12Cd/alpha.txt"),
            normalize_root_cause("missing /tmp/.tmpEf34Gh/beta.txt")
        );
        assert_ne!(
            normalize_root_cause("expected 0x01 got 0x02"),
            normalize_root_cause("expected 0x01 got 0x03")
        );
        assert_ne!(
            normalize_root_cause("failed at /tmp/fixed_a/out.txt"),
            normalize_root_cause("failed at /tmp/fixed_b/out.txt")
        );
    }

    #[test]
    fn nextest_doctest_and_backtrace_panics_pin_source_location_with_ansi_parity() {
        // Zero-alloc ANSI fast path check.
        assert!(matches!(strip_ansi("running 3 tests"), Cow::Borrowed(_)));
        assert!(matches!(
            strip_ansi("\x1b[31mFAILED\x1b[0m"),
            Cow::Owned(ref s) if s == "FAILED"
        ));

        // Tight-budget location pinning never truncates away `src/...:line:col`.
        let long_single_line = format!(
            "'{}', src/config/loader.rs:88:17",
            "expected valid schema field ".repeat(12)
        );
        let pinned = short_preserving_loc(
            &long_single_line,
            120,
            extract_rust_location(&long_single_line),
        );
        assert!(pinned.len() <= 120, "len={}: {pinned}", pinned.len());
        assert!(
            pinned.contains("src/config/loader.rs:88:17"),
            "panic location must survive tight cap: {pinned}"
        );

        // 1. `cargo nextest run` with ANSI colors, `note:` in STDOUT before STDERR panic,
        //    `RUST_BACKTRACE=1` where panic site is in `library/core` and workspace site is in backtrace,
        //    plus a second failure with an oversized single-line panic message.
        let build_nx = |colored: bool| -> String {
            let (red, bold, reset) = if colored {
                ("\x1b[31m", "\x1b[1m", "\x1b[0m")
            } else {
                ("", "", "")
            };
            format!(
                "    Starting 4 tests across 1 binary\n\
                 {red}        FAIL{reset} [   0.025s] billing::engine calculates_tax\n\
                 --- STDOUT:              billing::engine calculates_tax ---\n\
                 note: using synthetic fixture catalog\n\
                 --- STDERR:              billing::engine calculates_tax ---\n\
                 {bold}thread 'engine::calculates_tax' panicked at /rustc/abcdef/library/core/src/panicking.rs:220:5:{reset}\n\
                 index out of bounds: the len is 2 but the index is 5\n\
                 stack backtrace:\n\
                    0: rust_begin_unwind\n\
                              at /rustc/abcdef/library/std/src/panicking.rs:665:5\n\
                    1: core::panicking::panic_bounds_check\n\
                              at /rustc/abcdef/library/core/src/panicking.rs:280:5\n\
                    2: billing::engine::tax_bracket\n\
                              at ./src/engine/billing.rs:142:9\n\
                    3: billing::engine::tests::calculates_tax\n\
                              at ./src/engine/billing.rs:310:14\n\
                 {red}        FAIL{reset} [   0.019s] billing::config loads_schema\n\
                 --- STDERR:              billing::config loads_schema ---\n\
                 thread 'workspace::deeply::nested::billing_service::config_loader::loads_schema' panicked at {long_single_line}\n\
                 stack backtrace:\n\
                    0: rust_begin_unwind\n\
                              at /rustc/abcdef/library/std/src/panicking.rs:665:5\n\
                 ------------\n\
                      Summary [   0.090s] 4 tests run: 2 passed, 2 failed, 0 skipped\n"
            )
        };

        let nx_plain = nextest_totals(&[(&build_nx(false), Stream::Stderr)])
            .expect("plain nextest totals")
            .render();
        let nx_ansi = nextest_totals(&[(&build_nx(true), Stream::Stderr)])
            .expect("ansi nextest totals")
            .render();
        assert_eq!(nx_plain, nx_ansi);
        assert!(
            nx_plain.contains("src/engine/billing.rs:142:9"),
            "workspace frame from RUST_BACKTRACE=1 must be pinned:\n{nx_plain}"
        );
        assert!(
            nx_plain.contains("src/config/loader.rs:88:17"),
            "long single-line panic location must stay pinned:\n{nx_plain}"
        );
        assert!(
            !nx_plain.contains("rust_begin_unwind"),
            "backtrace noise must not bloat report:\n{nx_plain}"
        );

        // 2. Workspace doc-test failures with `RUST_BACKTRACE=1` (two failing doc-tests sharing
        //    the same root cause with tempdir variance -> grouped with pinned `crates/core/src/lib.rs:30:9`).
        let doc_out = "\
\nrunning 2 tests\n\
test crates/core/src/lib.rs - parse_a (line 20) ... FAILED\n\
test crates/core/src/lib.rs - parse_b (line 45) ... FAILED\n\
\nfailures:\n\n\
---- crates/core/src/lib.rs - parse_a (line 20) stdout ----\n\
Test executable failed (exit status: 101).\n\n\
stderr:\n\
thread 'main' panicked at crates/core/src/lib.rs:30:9:\n\
failed to load /tmp/.tmpAa11Bb/cfg.toml: invalid header\n\
stack backtrace:\n\
   0: rust_begin_unwind\n\
             at /rustc/abcdef/library/std/src/panicking.rs:665:5\n\
   1: rust_out::main\n\
             at ./crates/core/src/lib.rs:30:9\n\n\
---- crates/core/src/lib.rs - parse_b (line 45) stdout ----\n\
Test executable failed (exit status: 101).\n\n\
stderr:\n\
thread 'main' panicked at crates/core/src/lib.rs:30:9:\n\
failed to load /tmp/.tmpCc22Dd/cfg.toml: invalid header\n\
stack backtrace:\n\
   0: rust_begin_unwind\n\
             at /rustc/abcdef/library/std/src/panicking.rs:665:5\n\
   1: rust_out::main\n\
             at ./crates/core/src/lib.rs:30:9\n\n\
failures:\n\
    crates/core/src/lib.rs - parse_a (line 20)\n\
    crates/core/src/lib.rs - parse_b (line 45)\n\n\
test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.42s\n";
        let doc_err = "\x1b[1m\x1b[32m   Doc-tests\x1b[0m core\n\
            \x1b[1m\x1b[31merror\x1b[0m: doctest failed, to rerun pass `-p core --doc`\n";

        let doc_totals = totals(&[(doc_out, Stream::Stdout), (doc_err, Stream::Stderr)])
            .expect("workspace doc-test totals");
        let doc_rendered = doc_totals.render();
        assert!(
            doc_rendered.contains("2 test(s): crates/core/src/lib.rs - parse_a (line 20)"),
            "{doc_rendered}"
        );
        assert!(
            doc_rendered.contains("crates/core/src/lib.rs:30:9"),
            "{doc_rendered}"
        );
        assert!(
            doc_rendered.contains("0 passed, 2 failed  doc-tests core [stdout:"),
            "{doc_rendered}"
        );
        assert!(
            !doc_rendered.contains("rust_begin_unwind"),
            "{doc_rendered}"
        );
    }
}
