//! Group compiler and linter findings by rule across both output streams.
//!
//! Recognized shapes:
//! - rustc, Clippy and Ruff blocks: a `warning:`/`error[E0308]:`/`F401` header followed
//!   by a ` --> path:line:col` location and indented detail lines.
//! - ShellCheck's default `In path line N:` blocks with `^-- SC2086 (info): message` rows.
//! - One finding per line: `path:line[:col]: message` (Ruff and flake8 concise output,
//!   pylint, mypy, rustc `--message-format=short`, ShellCheck `-f gcc`, golangci-lint,
//!   go vet, staticcheck, RuboCop, SwiftLint, gcc and clang) and TypeScript's
//!   `path(line,col): error TS2322: message`. Indented lines that follow one of these,
//!   such as a TypeScript elaboration, are kept as its detail.
//! - ESLint's default formatter: a file name line followed by `line:col  severity  message  rule`.
//! - Pyright's `  path:line:col - error: message (rule)` rows and hadolint's
//!   `path:line DL3008 warning: message`.
//!
//! Errors without a source location and the tools' own totals stay visible as outcome
//! lines. A stream that produced no finding is summarized by its line range, so other
//! output such as a crash is never silently hidden. Findings are only reported when
//! at least one was recognized, so other output keeps the ordinary preview.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::capture::Stream;
use crate::summary::{Item, Summary, short};

const MAX_FINDINGS: usize = 20_000;
const EXCERPT_LINES: usize = 10;
const SHOWN_LOCATIONS: usize = 4;
const SHOWN_OUTCOMES: usize = 4;
const SHOWN_CHANGES: usize = 24;
const SHOWN_PACKAGES: usize = 6;
const SHOWN_TARGETS: usize = 4;

static RUST_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(error|warning)(?:\[([A-Za-z0-9_-]+)\])?: (.+)$").unwrap());
static CODE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Z]{1,4}[0-9]{2,5}) (?:\[\*\] )?(.+)$").unwrap());
static OXLINT_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*([×✖!⚠])\s+([a-z0-9_-]+\([a-z0-9_/-]+\)):\s+(.+)$").unwrap());
static BIOME_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\S.*?):(\d+):(\d+)\s+((?:lint|format|organizeImports|assist|parse)[/\w-]+)\s+━+")
        .unwrap()
});
static BIOME_MSG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+([×✖!⚠ℹ])\s+(.+)$").unwrap());
static ARROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:(?:-->|╭▸|┌─)\s*(.+?):(\d+)(?::(\d+))?\s*|╭─\[(.+?):(\d+)(?::(\d+))?\]\s*)$",
    )
    .unwrap()
});
static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"#\[(?:warn|deny|forbid|allow|expect)\(([A-Za-z0-9_:]+)\)\]|`-[DWF] ([A-Za-z0-9_:-]+)` implied by|index\.html#([a-z0-9_]+)",
    )
    .unwrap()
});
// Source excerpts and suggested edits inside a rustc block, which can quote attributes.
static SNIPPET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:\d*\s*\||\d+\s*[+~-]\s)").unwrap());
static TSC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\S.*?)\((\d+),(\d+)\): (error|warning) (TS\d+): (.+)$").unwrap()
});
// A path may contain spaces and a colon that is not followed by whitespace or a digit,
// such as a Windows drive, so `note: see a.rs:3: x` and `warning: …` never match.
static LOCATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s:](?:[^:]|:[^\s\d])*?\.[A-Za-z0-9]+):(\d+)(?::(\d+))?:\s+(.+)$").unwrap()
});
static SEVERITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?i)(fatal error|error|warning|note|info|style|hint|refactor|convention)(?:\[([A-Za-z0-9_]+)\])?:\s*(.+)$",
    )
    .unwrap()
});
static RUBOCOP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([CWEFR]): (?:\[Correctable\] )?([A-Z][A-Za-z]+/[A-Za-z0-9]+): (.+)$").unwrap()
});
static LEADING_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Z]{1,4}[0-9]{2,5})(:?) (?:\[\*\] )?(.+)$").unwrap());
static TRAILING_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?)\s+\[(-?[A-Za-z][\w.:/-]*)\]$").unwrap());
static TRAILING_PAREN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?)\s+\(([a-z][a-z0-9_-]*|[A-Z]{1,4}[0-9]{2,5})\)$").unwrap());
// ESLint prints absolute file names, which may contain spaces. Relative names stay
// strict so source lines such as ShellCheck's `ls $DIR/*.txt` are never headers.
static ESLINT_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?:/|[A-Za-z]:[\\/])[^:]*?|(?:\.{0,2}/)?[^\s:]+)\.[A-Za-z0-9]+$").unwrap()
});
static ESLINT_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.+?)(?:\s{2,}(@?[\w./-]+))?$").unwrap()
});
static PYRIGHT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(\S.*?):(\d+):(\d+) - (error|warning|information): (.*)$").unwrap()
});
static PYRIGHT_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)\s*\((report[A-Za-z]+)\)\s*$").unwrap());
static HADOLINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\S.*?):(\d+)(?::(\d+))? ((?:DL|SC)\d{4}) (error|warning|info|style): (.+)$")
        .unwrap()
});
static SHELLCHECK_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^In (.+) line (\d+):$").unwrap());
static SHELLCHECK_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\^-*\^?\s*(SC\d+) \((\w+)\): (.+)$").unwrap());
// A tool's own count, kept so a mismatch with the grouped header stays visible.
static TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Found \d+ (?:errors?|warnings?|problems?|issues?|diagnostics?)\b|\d+ (?:errors?|problems?|issues?|offenses?)\b|\d+ files? inspected\b)",
    )
    .unwrap()
});
// The package and, in recent Cargo, the target, such as `(lib test)` or `(bin "x")`.
static UNCOMPILED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^error: could not compile `([^`]+)`(?: \(([^)]+)\))?").unwrap());
pub(crate) static CARGO_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^ +(?:Compiling|Checking|Finished|Building|Blocking|Fresh|Documenting|Generated|Scraping|Downloaded|Downloading|Updating|Locking|Adding|Running|Packaging|Verifying) \S",
    )
    .unwrap()
});
// rustc names a lint only on its first diagnostic, and the wording of later ones
// varies with the item kind and plural forms. `--message-format=short` names none.
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
        (
            r"^unresolved link to `|^`[^`]+` is both a ",
            "rustdoc::broken_intra_doc_links",
        ),
        (r"^this URL is not a hyperlink$", "rustdoc::bare_urls"),
    ]
    .into_iter()
    .map(|(pattern, lint)| (Regex::new(pattern).unwrap(), lint))
    .collect()
});
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"`[^`]*`|'[^']*'|"[^"]*""#).unwrap());

#[derive(Clone)]
struct Finding {
    severity: Severity,
    rule: Option<String>,
    message: String,
    file: String,
    line: usize,
    column: Option<usize>,
    stream: Stream,
    first: usize,
    last: usize,
    excerpt: Vec<String>,
}

impl Finding {
    fn new(severity: Severity, rule: Option<String>, message: &str, stream: Stream) -> Self {
        Self {
            severity,
            rule,
            message: short(message.trim(), 300),
            file: String::new(),
            line: 0,
            column: None,
            stream,
            first: 0,
            last: 0,
            excerpt: Vec::new(),
        }
    }

    fn at(mut self, root: &str, file: &str, line: &str, column: Option<regex::Match>) -> Self {
        (self.file, self.line, self.column) = position(root, file, line, column);
        self
    }

    fn lines(mut self, n: usize, excerpt: &str) -> Self {
        self.first = n;
        self.last = n;
        self.excerpt = vec![short(excerpt, 240)];
        self
    }

    fn position(&self) -> String {
        match self.column {
            Some(column) => format!("{}:{column}", self.line),
            None => self.line.to_string(),
        }
    }

    fn place(&self) -> String {
        format!("{}:{}", self.file, self.position())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn parse(text: &str) -> Self {
        let text = text.to_ascii_lowercase();
        if text.contains("error") || matches!(text.as_str(), "e" | "f") {
            Self::Error
        } else {
            Self::Warning
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// Lines of one stream that no shape recognized.
#[derive(Default)]
struct Other {
    count: usize,
    first: usize,
    last: usize,
    head: String,
    tail: VecDeque<String>,
}

impl Other {
    fn note(&mut self, n: usize, line: &str) {
        let line = short(line.trim_end(), 240);
        if self.count == 0 {
            self.first = n;
            self.head = line;
        } else {
            self.tail.push_back(line);
            if self.tail.len() > 2 {
                self.tail.pop_front();
            }
        }
        self.count += 1;
        self.last = n;
    }
}

pub struct Lint {
    root: String,
    findings: Vec<Finding>,
    block: Option<Finding>,
    /// The last one-line finding, which indented detail lines extend.
    open: Option<usize>,
    eslint_file: Option<String>,
    shellcheck_line: Option<(String, String)>,
    outcomes: Vec<(String, Vec<String>)>,
    hidden_outcomes: usize,
    /// Packages that failed to compile, each with its failed targets in order. An
    /// empty target stands for a line that named none.
    uncompiled: Vec<(String, Vec<String>)>,
    uncompiled_line: String,
    uncompiled_at: String,
    unlocated_errors: usize,
    other: [Other; 2],
    /// Cargo's status lines among `other`, per stream: count, first and last line.
    status: [(usize, usize, usize); 2],
    /// Non-blank lines per stream, whatever shape read them.
    lines: [usize; 2],
    /// The last `Finished` status line.
    finished: String,
    overflow: bool,
}

impl Lint {
    pub fn new(root: &Path) -> Self {
        let mut root = root.to_string_lossy().into_owned();
        if !root.ends_with('/') {
            root.push('/');
        }
        Self {
            root,
            findings: Vec::new(),
            block: None,
            open: None,
            eslint_file: None,
            shellcheck_line: None,
            outcomes: Vec::new(),
            hidden_outcomes: 0,
            uncompiled: Vec::new(),
            uncompiled_line: String::new(),
            uncompiled_at: String::new(),
            unlocated_errors: 0,
            other: Default::default(),
            status: [(0, 0, 0); 2],
            lines: [0; 2],
            finished: String::new(),
            overflow: false,
        }
    }

    pub fn observe(&mut self, text: &str, n: usize, stream: Stream) {
        if self.overflow {
            return;
        }
        if self.findings.len() >= MAX_FINDINGS || text.len() > 64 * 1024 {
            self.overflow = true;
            return;
        }
        let line = text.trim_end_matches(['\n', '\r']);
        if !line.trim().is_empty() {
            self.lines[stream as usize] += 1;
        }
        if !self.classify(line, n, stream) && !line.trim().is_empty() {
            self.other[stream as usize].note(n, line);
            if CARGO_STATUS.is_match(line) {
                let status = &mut self.status[stream as usize];
                *status = (status.0 + 1, if status.0 == 0 { n } else { status.1 }, n);
                if line.trim_start().starts_with("Finished ") {
                    self.finished = short(line.trim(), 200);
                }
            }
        }
    }

    /// One line for a run that printed only Cargo's status lines, such as a clean
    /// `cargo clippy`, with the range of those lines on each stream and the last
    /// `Finished` line. `None` when the run printed anything else.
    pub fn clean(&self, label: &str) -> Option<String> {
        // A shape can take a line without a finding, as a warning without a
        // location, so every line counts.
        let only_status = (0..2).all(|i| self.lines[i] == self.status[i].0);
        if self.overflow
            || !only_status
            || self.block.is_some()
            || !self.findings.is_empty()
            || !self.outcomes.is_empty()
            || !self.uncompiled.is_empty()
            || self.hidden_outcomes > 0
            || self.status.iter().all(|status| status.0 == 0)
        {
            return None;
        }
        let mut line = format!("{label}: 0 findings");
        for stream in [Stream::Stdout, Stream::Stderr] {
            let (count, first, last) = self.status[stream as usize];
            if count > 0 {
                let name = stream.name();
                line.push_str(&format!(
                    " · {count} cargo status line(s) [{name}:{first}-{last}]"
                ));
            }
        }
        if !self.finished.is_empty() {
            line.push_str(&format!(" · {}", self.finished));
        }
        line.push('\n');
        Some(line)
    }

    /// Record a line and return whether it belonged to a recognized shape.
    fn classify(&mut self, line: &str, n: usize, stream: Stream) -> bool {
        if let Some(block) = self.block.as_mut().filter(|b| b.stream == stream) {
            let ends = RUST_HEADER.is_match(line)
                || CODE_HEADER.is_match(line)
                || OXLINT_HEADER.is_match(line)
                || BIOME_HEADER.is_match(line)
                || TOTAL.is_match(line)
                || CARGO_STATUS.is_match(line);
            if line.is_empty() && !ends {
                return true;
            }
            if !ends {
                block.last = n;
                if block.file.is_empty()
                    && let Some(c) = ARROW.captures(line)
                {
                    let (file, row, col) = match (c.get(1), c.get(2)) {
                        (Some(f), Some(r)) => (f.as_str(), r.as_str(), c.get(3)),
                        _ => (&c[4], &c[5], c.get(6)),
                    };
                    (block.file, block.line, block.column) = position(&self.root, file, row, col);
                }
                if block.message.is_empty()
                    && let Some(c) = BIOME_MSG.captures(line)
                {
                    block.severity = match &c[1] {
                        "×" | "✖" => Severity::Error,
                        _ => Severity::Warning,
                    };
                    block.message = short(c[2].trim(), 240);
                }
                if let Some(rule) = attribute(line) {
                    block.rule.get_or_insert(rule);
                }
                if block.excerpt.len() == EXCERPT_LINES && block.file.is_empty() {
                    // Without a location, the cause of a failure usually comes last.
                    block.excerpt.remove(1);
                }
                if block.excerpt.len() < EXCERPT_LINES {
                    block.excerpt.push(short(line, 240));
                }
                return true;
            }
        }
        if let Some(index) = self.open.take()
            && self.findings[index].stream == stream
            && line.starts_with(char::is_whitespace)
            && !line.trim().is_empty()
            && !ESLINT_ROW.is_match(line)
            && !PYRIGHT.is_match(line)
            && !SHELLCHECK_ROW.is_match(line)
            && !OXLINT_HEADER.is_match(line)
            && !CARGO_STATUS.is_match(line)
        {
            let finding = &mut self.findings[index];
            finding.last = n;
            if finding.rule.is_none()
                && let Some(c) = PYRIGHT_RULE.captures(line)
            {
                finding.rule = Some(c[2].to_owned());
            }
            if finding.excerpt.len() < EXCERPT_LINES {
                finding.excerpt.push(short(line, 240));
            }
            self.open = Some(index);
            return true;
        }
        self.close();
        if let Some(c) = RUST_HEADER.captures(line) {
            let rule = c.get(2).map(|m| m.as_str().to_owned());
            self.block =
                Some(Finding::new(Severity::parse(&c[1]), rule, &c[3], stream).lines(n, line));
            return true;
        }
        if let Some(c) = CODE_HEADER.captures(line) {
            let rule = Some(c[1].to_owned());
            self.block = Some(Finding::new(Severity::Warning, rule, &c[2], stream).lines(n, line));
            return true;
        }
        if let Some(c) = OXLINT_HEADER.captures(line) {
            let severity = match &c[1] {
                "×" | "✖" => Severity::Error,
                _ => Severity::Warning,
            };
            self.block = Some(
                Finding::new(severity, Some(c[2].to_owned()), &c[3], stream).lines(n, line.trim()),
            );
            return true;
        }
        if let Some(c) = BIOME_HEADER.captures(line) {
            let finding = Finding::new(Severity::Error, Some(c[4].to_owned()), "", stream)
                .at(&self.root, &c[1], &c[2], c.get(3))
                .lines(n, line);
            self.block = Some(finding);
            return true;
        }
        if let Some(c) = TSC.captures(line) {
            let finding =
                Finding::new(Severity::parse(&c[4]), Some(c[5].to_owned()), &c[6], stream)
                    .at(&self.root, &c[1], &c[2], c.get(3))
                    .lines(n, line);
            self.push(finding, true);
            return true;
        }
        if let Some(c) = LOCATED.captures(line) {
            if let Some(finding) = located(&c[4], stream) {
                let finding = finding
                    .at(&self.root, &c[1], &c[2], c.get(3))
                    .lines(n, line);
                self.push(finding, true);
            }
            return true;
        }
        if let Some(c) = PYRIGHT.captures(line) {
            let (message, rule) = match PYRIGHT_RULE.captures(&c[5]) {
                Some(r) => (r[1].to_owned(), Some(r[2].to_owned())),
                None => (c[5].to_owned(), None),
            };
            let finding = Finding::new(Severity::parse(&c[4]), rule, &message, stream)
                .at(&self.root, &c[1], &c[2], c.get(3))
                .lines(n, line.trim());
            self.push(finding, true);
            return true;
        }
        if let Some(c) = HADOLINT.captures(line) {
            let finding =
                Finding::new(Severity::parse(&c[5]), Some(c[4].to_owned()), &c[6], stream)
                    .at(&self.root, &c[1], &c[2], c.get(3))
                    .lines(n, line);
            self.push(finding, false);
            return true;
        }
        if let Some(c) = SHELLCHECK_FILE.captures(line) {
            self.shellcheck_line = Some((c[1].to_owned(), c[2].to_owned()));
            return true;
        }
        if let Some(c) = SHELLCHECK_ROW.captures(line)
            && let Some((file, row)) = &self.shellcheck_line
        {
            let finding =
                Finding::new(Severity::parse(&c[2]), Some(c[1].to_owned()), &c[3], stream)
                    .at(&self.root, file, row, None)
                    .lines(n, line.trim());
            self.push(finding, false);
            return true;
        }
        if let Some(c) = ESLINT_ROW.captures(line)
            && let Some(file) = &self.eslint_file
        {
            let rule = c.get(5).map(|m| m.as_str().to_owned());
            let finding = Finding::new(Severity::parse(&c[3]), rule, &c[4], stream)
                .at(&self.root, file, &c[1], c.get(2))
                .lines(n, line.trim());
            self.push(finding, false);
            return true;
        }
        // ESLint rows belong to the file named since the last blank or other line.
        if line.trim().is_empty() {
            self.eslint_file = None;
            return false;
        }
        if !line.starts_with(char::is_whitespace) {
            if ESLINT_FILE.is_match(line) {
                self.eslint_file = Some(line.to_owned());
                return true;
            }
            self.eslint_file = None;
        }
        let text = line.trim();
        let lower = text.to_ascii_lowercase();
        if lower.starts_with("error")
            || lower.starts_with("fatal")
            || text.starts_with('✖')
            || TOTAL.is_match(text)
        {
            self.outcome(text, &[], stream, n);
            return true;
        }
        false
    }

    fn push(&mut self, finding: Finding, extends: bool) {
        self.findings.push(finding);
        self.open = extends.then(|| self.findings.len() - 1);
    }

    /// Keep a general outcome line, such as an error without a source location or a
    /// tool's total. `could not compile` lines collapse into one that names each
    /// failed package and target.
    fn outcome(&mut self, text: &str, detail: &[String], stream: Stream, n: usize) {
        let head = short(text, 240);
        let at = format!(" [{}:{n}]", stream.name());
        if let Some(c) = UNCOMPILED.captures(&head) {
            if self.uncompiled.is_empty() {
                self.uncompiled_line = format!("{head}{at}");
                self.uncompiled_at = at;
            }
            let target = c.get(2).map_or("", |m| m.as_str());
            match self.uncompiled.iter_mut().find(|(p, _)| p == &c[1]) {
                Some((_, targets)) if !targets.iter().any(|t| t == target) => {
                    targets.push(target.to_owned());
                }
                Some(_) => {}
                None => self
                    .uncompiled
                    .push((c[1].to_owned(), vec![target.to_owned()])),
            }
            return;
        }
        if self
            .outcomes
            .iter()
            .any(|(o, _)| o.strip_prefix(&head).is_some_and(|r| r.starts_with(" [")))
        {
            return;
        }
        if self.outcomes.len() >= SHOWN_OUTCOMES {
            self.hidden_outcomes += 1;
            return;
        }
        self.outcomes.push((format!("{head}{at}"), detail.to_vec()));
    }

    fn close(&mut self) {
        let Some(block) = self.block.take() else {
            return;
        };
        if !block.file.is_empty() {
            self.findings.push(block);
        } else if block.severity == Severity::Error {
            // An error without a source location, such as a failed build script or link
            // step, stays visible as a general outcome with its detail lines.
            if !UNCOMPILED.is_match(&block.excerpt[0])
                && !block.excerpt[0].starts_with("error: aborting due to")
            {
                self.unlocated_errors += 1;
            }
            self.outcome(
                &block.excerpt[0],
                &block.excerpt[1..],
                block.stream,
                block.first,
            );
        }
    }

    /// Return the grouped report, fitted to `max_bytes`, and the comparison record,
    /// or `None` when no finding was recognized or the stream was too large to group.
    pub fn finish(
        mut self,
        label: &str,
        max_bytes: usize,
        previous: Option<(&str, &Summary)>,
    ) -> Option<(String, Summary)> {
        self.close();
        if self.overflow || self.findings.is_empty() {
            return None;
        }
        // Clippy and rustc name a lint only on its first occurrence. Learn the rule
        // for a normalized message and apply it to the remaining occurrences.
        let mut learned = BTreeMap::new();
        for finding in &self.findings {
            if let Some(rule) = &finding.rule {
                learned
                    .entry(normalize(&finding.message))
                    .or_insert_with(|| rule.clone());
            }
        }
        let mut streams = [false; 2];
        let mut groups: BTreeMap<(Severity, String), Vec<Finding>> = BTreeMap::new();
        for finding in self.findings {
            streams[finding.stream as usize] = true;
            let normalized = normalize(&finding.message);
            let key = finding
                .rule
                .clone()
                .or_else(|| learned.get(&normalized).cloned())
                .or_else(|| rustc_lint(&finding).map(str::to_owned))
                .unwrap_or(normalized);
            groups
                .entry((finding.severity, key))
                .or_default()
                .push(finding);
        }
        let errors: usize = groups
            .iter()
            .filter(|((s, _), _)| *s == Severity::Error)
            .map(|(_, f)| f.len())
            .sum();
        let total: usize = groups.values().map(Vec::len).sum();
        let files = groups
            .values()
            .flatten()
            .map(|f| f.file.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let mut ordered: Vec<_> = groups.into_iter().collect();
        ordered.sort_by(|((a, ak), af), ((b, bk), bf)| {
            a.cmp(b).then(bf.len().cmp(&af.len())).then(ak.cmp(bk))
        });

        let mut summary = Summary {
            kind: "lint".into(),
            ..Summary::default()
        };
        let mut header = format!(
            "{label}: {total} finding(s), {errors} error(s), {} warning(s) · {} rule(s) · {} file(s)",
            total - errors,
            ordered.len(),
            files
        );
        if self.unlocated_errors > 0 {
            header.push_str(&format!(
                " · {} error(s) without a location",
                self.unlocated_errors
            ));
        }
        header.push('\n');
        let mut sections = Vec::new();
        for ((severity, rule), findings) in &ordered {
            let first = &findings[0];
            let mut item = Item::new(rule, &first.message, first.first);
            item.digest = findings.len().to_string();
            item.last = first.last;
            summary.items.push(item);
            // The comparison record for each item: severity, rule, count and digests of
            // the reported messages per file and of the exact locations.
            let (messages, places) = fingerprints(findings);
            summary.outcomes.push(format!(
                "{} {rule} ×{} {messages}/{places}",
                severity.label(),
                findings.len()
            ));
            // Errors show the detail lines of their first occurrence that has any,
            // introduced by its original line when that is not the group's first.
            let mut detail = String::new();
            if *severity == Severity::Error
                && let Some(example) = findings.iter().find(|f| f.excerpt.len() > 1)
            {
                let skip = usize::from(std::ptr::eq(example, first));
                for line in example.excerpt.iter().skip(skip) {
                    detail.push_str(&format!("    {line}\n"));
                }
            }
            sections.push((
                format!(
                    "{} {rule} ×{}  {}\n",
                    severity.label(),
                    findings.len(),
                    first.message
                ),
                detail,
                format!("  {}\n", locations(findings)),
                findings.len(),
            ));
        }

        let mut tail = String::new();
        if let Some((id, old)) = previous.filter(|(_, s)| s.kind == "lint") {
            tail.push_str(&compare(id, old, &summary));
        }
        // Without a located error, an error without a location, such as a failed build
        // script, explains the failure, so the outcomes come right after the header.
        let mut lead = String::new();
        let outcomes = if errors == 0 && self.unlocated_errors > 0 {
            &mut lead
        } else {
            &mut tail
        };
        let mut detail_room = max_bytes / 2;
        for (head, detail) in &self.outcomes {
            outcomes.push_str(head);
            outcomes.push('\n');
            for line in detail {
                if line.len() + 5 > detail_room {
                    break;
                }
                detail_room -= line.len() + 5;
                outcomes.push_str(&format!("    {line}\n"));
            }
        }
        // One failed target keeps its original line. Several name each package with
        // its failed targets, such as `rt` (lib, lib test).
        match self.uncompiled.as_slice() {
            [] => {}
            [(_, targets)] if targets.len() == 1 => {
                tail.push_str(&self.uncompiled_line);
                tail.push('\n');
            }
            packages => {
                let names: Vec<_> = packages
                    .iter()
                    .take(SHOWN_PACKAGES)
                    .map(|(package, targets)| {
                        let named: Vec<_> = targets.iter().filter(|t| !t.is_empty()).collect();
                        let mut list = named
                            .iter()
                            .take(SHOWN_TARGETS)
                            .map(|t| t.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        if named.len() > SHOWN_TARGETS {
                            list.push_str(&format!(" +{} more", named.len() - SHOWN_TARGETS));
                        }
                        if list.is_empty() {
                            format!("`{package}`")
                        } else {
                            format!("`{package}` ({list})")
                        }
                    })
                    .collect();
                let more = match packages.len().saturating_sub(SHOWN_PACKAGES) {
                    0 => String::new(),
                    more => format!(" +{more} more"),
                };
                let count = match packages.len() {
                    1 => String::new(),
                    count => format!("{count} packages: "),
                };
                tail.push_str(&format!(
                    "error: could not compile {count}{}{more}{}\n",
                    names.join(", "),
                    self.uncompiled_at
                ));
            }
        }
        if self.hidden_outcomes > 0 {
            tail.push_str(&format!(
                "+{} more outcome line(s) in full log\n",
                self.hidden_outcomes
            ));
        }
        for stream in [Stream::Stdout, Stream::Stderr] {
            let other = &self.other[stream as usize];
            if streams[stream as usize] || other.count == 0 {
                continue;
            }
            let name = stream.name();
            tail.push_str(&format!(
                "{name}: {} other line(s) [{name}:{}-{}]\n  {}\n",
                other.count, other.first, other.last, other.head
            ));
            if other.count > 1 + other.tail.len() {
                tail.push_str("  …\n");
            }
            for line in &other.tail {
                tail.push_str(&format!("  {line}\n"));
            }
        }

        // Every group's rule line and locations come before any excerpt, so a long
        // report drops detail lines first and then whole groups with a count.
        let room = max_bytes.saturating_sub(header.len() + lead.len() + tail.len());
        let compact: usize = sections.iter().map(|(h, _, p, _)| h.len() + p.len()).sum();
        let (shown, mut spare) = if compact <= room {
            (sections.len(), room - compact)
        } else {
            let room = room.saturating_sub(64);
            let mut used = 0;
            let shown = sections
                .iter()
                .take_while(|(h, _, p, _)| {
                    used += h.len() + p.len();
                    used <= room
                })
                .count();
            let used: usize = sections[..shown]
                .iter()
                .map(|(h, _, p, _)| h.len() + p.len())
                .sum();
            (shown, room - used)
        };
        let mut report = header + &lead;
        for (head, detail, places, _) in &sections[..shown] {
            report.push_str(head);
            if detail.len() <= spare {
                report.push_str(detail);
                spare -= detail.len();
            }
            report.push_str(places);
        }
        if shown < sections.len() {
            let hidden = &sections[shown..];
            report.push_str(&format!(
                "+{} more rule(s) with {} finding(s) in full log\n",
                hidden.len(),
                hidden.iter().map(|s| s.3).sum::<usize>()
            ));
        }
        report.push_str(&tail);
        Some((report, summary))
    }
}

/// Counts and digests per severity and rule. Records written before severities were
/// stored have no keys, so both sides then compare by rule alone.
type Tally = BTreeMap<(String, String), (usize, String, String)>;

fn tally(summary: &Summary, keyed: bool) -> Tally {
    let mut tally = Tally::new();
    for (i, item) in summary.items.iter().enumerate() {
        let record = summary
            .outcomes
            .get(i)
            .filter(|_| keyed)
            .map_or("", String::as_str);
        let severity = record.split(' ').next().unwrap_or("");
        let (messages, places) = record
            .rsplit_once(' ')
            .and_then(|(_, digests)| digests.split_once('/'))
            .unwrap_or(("", ""));
        let entry = tally
            .entry((severity.to_owned(), item.name.clone()))
            .or_insert_with(|| (0, messages.to_owned(), places.to_owned()));
        entry.0 += item.digest.parse::<usize>().unwrap_or(0);
    }
    tally
}

fn compare(id: &str, old: &Summary, current: &Summary) -> String {
    let keyed = !old.items.is_empty() && old.outcomes.len() == old.items.len();
    let (before, now) = (tally(old, keyed), tally(current, keyed));
    let name = |(severity, rule): &(String, String)| {
        if severity.is_empty() {
            rule.clone()
        } else {
            format!("{severity} {rule}")
        }
    };
    let mut changes = Vec::new();
    let mut moved = false;
    for (key, (count, messages, places)) in &now {
        match before.get(key) {
            None => changes.push(format!("+ {} ×{count} (new)", name(key))),
            Some((old, ..)) if old != count => {
                changes.push(format!("~ {} ×{old} → ×{count}", name(key)))
            }
            Some((_, old, _)) if old != messages => {
                changes.push(format!("~ {} ×{count} with different findings", name(key)))
            }
            Some((_, _, old)) => moved |= old != places,
        }
    }
    for (key, (count, ..)) in &before {
        if !now.contains_key(key) {
            changes.push(format!("- {} ×{count} (no longer reported)", name(key)));
        }
    }
    if changes.is_empty() {
        return if !keyed {
            format!("Same rule counts as {id}.\n")
        } else if moved {
            format!("Same findings as {id}, some on other lines.\n")
        } else {
            format!("Same findings as {id}.\n")
        };
    }
    let mut out = format!("Compared with {id}:\n");
    let more = changes.len().saturating_sub(SHOWN_CHANGES);
    for change in changes.into_iter().take(SHOWN_CHANGES) {
        out.push_str(&change);
        out.push('\n');
    }
    if more > 0 {
        out.push_str(&format!("+{more} more change(s)\n"));
    }
    out
}

/// Short digests of the messages reported per file, which line shifts leave unchanged,
/// and of the exact locations.
fn fingerprints(findings: &[Finding]) -> (String, String) {
    let digest = |parts: BTreeSet<String>| {
        let mut hash = Sha256::new();
        for part in parts {
            hash.update(part.as_bytes());
            hash.update(b"\n");
        }
        format!("{:x}", hash.finalize())[..12].to_owned()
    };
    let messages = findings
        .iter()
        .map(|f| format!("{}\t{}", f.file, f.message))
        .collect();
    let places = findings.iter().map(Finding::place).collect();
    (digest(messages), digest(places))
}

fn attribute(line: &str) -> Option<String> {
    if SNIPPET.is_match(line) {
        return None;
    }
    let c = ATTRIBUTE.captures(line)?;
    if let Some(name) = c.get(1) {
        return Some(name.as_str().to_owned());
    }
    if let Some(name) = c.get(2) {
        return Some(name.as_str().replace('-', "_"));
    }
    Some(format!("clippy::{}", &c[3]))
}

fn located(message: &str, stream: Stream) -> Option<Finding> {
    let (mut severity, mut text) = (Severity::Warning, message.trim());
    let mut explicit = None;
    let mut rule = None;
    if let Some(c) = SEVERITY.captures(text) {
        let level = c.get(1).unwrap().as_str().to_ascii_lowercase();
        severity = Severity::parse(&level);
        rule = c.get(2).map(|m| m.as_str().to_owned());
        text = c.get(3).unwrap().as_str();
        explicit = Some(level);
    }
    if rule.is_some() {
        // rustc's short format names the error code in the level: `error[E0308]:`.
    } else if let Some(c) = RUBOCOP.captures(text) {
        severity = Severity::parse(&c[1]);
        rule = Some(c[2].to_owned());
        text = c.get(3).unwrap().as_str();
    } else if let Some(c) = LEADING_CODE.captures(text) {
        // pylint's `E0602:` and `F0001:` are errors. flake8's `E302` and Ruff's `F401`
        // have no colon and stay warnings.
        let code = &c[1];
        if !c[2].is_empty() && code.len() == 5 && (code.starts_with('E') || code.starts_with('F')) {
            severity = Severity::Error;
        }
        rule = Some(code.to_owned());
        text = c.get(3).unwrap().as_str();
    }
    // rustc's short format appends its help and notes to the message.
    if let Some(end) = text.find(": help: ").or_else(|| text.find(": note: "))
        && end > 0
    {
        text = &text[..end];
    }
    // It also appends the primary label, which can repeat the message:
    // `unreachable statement: unreachable statement`.
    if let Some(head) = text.get(..text.len().saturating_sub(2) / 2)
        && !head.is_empty()
        && text[head.len()..].strip_prefix(": ") == Some(head)
    {
        text = head;
    }
    if rule.is_none() {
        if let Some(c) = TRAILING_BRACKET.captures(text) {
            rule = Some(c[2].to_owned());
            text = c.get(1).unwrap().as_str();
        } else if let Some(c) = TRAILING_PAREN.captures(text) {
            rule = Some(c[2].to_owned());
            text = c.get(1).unwrap().as_str();
        }
    }
    // mypy and compilers print follow-up notes at the same location. Count a note
    // only when it names a rule, as ShellCheck notes do.
    if explicit.as_deref() == Some("note") && rule.is_none() {
        return None;
    }
    Some(Finding::new(severity, rule, text, stream))
}

/// The rustc lint a Rust finding's wording implies. Other languages' compilers, such
/// as javac's `unreachable statement`, keep their message.
fn rustc_lint(finding: &Finding) -> Option<&'static str> {
    if !finding.file.ends_with(".rs") {
        return None;
    }
    RUSTC_LINTS
        .iter()
        .find(|(shape, _)| shape.is_match(&finding.message))
        .map(|(_, lint)| *lint)
}

fn position(
    root: &str,
    file: &str,
    line: &str,
    column: Option<regex::Match>,
) -> (String, usize, Option<usize>) {
    let file = file.trim();
    let file = file.strip_prefix(root).unwrap_or(file);
    let file = file.strip_prefix("./").unwrap_or(file).to_owned();
    let column = column.and_then(|c| c.as_str().parse().ok());
    (file, line.parse().unwrap_or(0), column)
}

fn normalize(message: &str) -> String {
    QUOTED.replace_all(message, "…").into_owned()
}

fn locations(findings: &[Finding]) -> String {
    // Order by file and numeric position and drop repeats so same-file runs collapse.
    let mut places: Vec<&Finding> = findings.iter().collect();
    places.sort_by(|a, b| (&a.file, a.line, a.column).cmp(&(&b.file, b.line, b.column)));
    places.dedup_by(|a, b| (&a.file, a.line, a.column) == (&b.file, b.line, b.column));
    let distinct = places.len();
    let mut out = String::new();
    let mut previous_file = None;
    for finding in places.into_iter().take(SHOWN_LOCATIONS) {
        if !out.is_empty() {
            out.push_str(", ");
        }
        if previous_file == Some(finding.file.as_str()) {
            out.push_str(&finding.position());
        } else {
            out.push_str(&finding.place());
        }
        previous_file = Some(finding.file.as_str());
    }
    if distinct > SHOWN_LOCATIONS {
        out.push_str(&format!(" +{} more", distinct - SHOWN_LOCATIONS));
    }
    let first = &findings[0];
    out.push_str(&format!(
        " [{}:{}-{}]",
        first.stream.name(),
        first.first,
        first.last
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biome_oxlint_and_ty_check_group_findings_by_rule() {
        let biome_out = "\
src/app.ts:12:5 lint/style/useConst ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  × This let declares a variable that is only assigned once.

src/util.ts:4:3 lint/style/useConst ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  × This let declares a variable that is only assigned once.

src/api.ts:9:1 lint/suspicious/noExplicitAny ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  ! Unexpected any. Specify a different type.

Checked 12 files in 8ms.
Found 2 errors.
";
        let mut biome = Lint::new(Path::new("/repo"));
        for (i, line) in biome_out.split_inclusive('\n').enumerate() {
            biome.observe(line, i + 1, Stream::Stderr);
        }
        let (report, _) = biome.finish("biome check", 4096, None).unwrap();
        assert!(report.contains("lint/style/useConst ×2"), "{report}");
        assert!(
            report.contains("lint/suspicious/noExplicitAny ×1"),
            "{report}"
        );
        assert!(report.contains("src/app.ts:12:5"), "{report}");

        let oxlint_out = "\
  × eslint(no-unused-vars): Variable 'x' is declared but never used.
   ╭─[src/app.ts:10:7]
  × eslint(no-unused-vars): Variable 'y' is declared but never used.
   ╭─[src/app.ts:14:7]
  ! eslint(no-console): Unexpected console statement.
   ╭─[src/log.ts:3:1]
Found 1 warning and 2 errors.
";
        let mut oxlint = Lint::new(Path::new("/repo"));
        for (i, line) in oxlint_out.split_inclusive('\n').enumerate() {
            oxlint.observe(line, i + 1, Stream::Stdout);
        }
        let (report, _) = oxlint.finish("oxlint", 4096, None).unwrap();
        assert!(report.contains("eslint(no-unused-vars) ×2"), "{report}");
        assert!(report.contains("eslint(no-console) ×1"), "{report}");
        assert!(report.contains("src/app.ts:10:7"), "{report}");

        let ty_out = "\
error[invalid-assignment]: Object of type `str` is not assignable to `int`
 --> src/models.py:22:9
  |
22 | x: int = \"bad\"
  |          ^^^^^
Found 1 diagnostic
";
        let mut ty = Lint::new(Path::new("/repo"));
        for (i, line) in ty_out.split_inclusive('\n').enumerate() {
            ty.observe(line, i + 1, Stream::Stderr);
        }
        let (report, _) = ty.finish("ty check", 4096, None).unwrap();
        assert!(report.contains("invalid-assignment ×1"), "{report}");
        assert!(report.contains("src/models.py:22:9"), "{report}");
    }
}
