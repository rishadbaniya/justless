//! Group compiler and linter findings by rule across both output streams.
//!
//! Recognized shapes:
//! - rustc, Clippy and Ruff blocks: a `warning:`/`error[E0308]:`/`F401` header followed
//!   by a ` --> path:line:col` location and indented detail lines.
//! - ShellCheck's default `In path line N:` blocks with `^-- SC2086 (info): message` rows.
//! - One finding per line: `path:line[:col]: message` (Ruff and flake8 concise output,
//!   pylint, mypy, ShellCheck `-f gcc`, golangci-lint, go vet, RuboCop, gcc and clang)
//!   and TypeScript's `path(line,col): error TS2322: message`.
//! - ESLint's default formatter: a file name line followed by `line:col  severity  message  rule`.
//!
//! Lines that match none of these stay in the full log. Findings are only reported
//! when at least one was recognized, so other output keeps the ordinary preview.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::capture::Stream;
use crate::summary::{Item, Summary, short};

const MAX_FINDINGS: usize = 20_000;
const EXCERPT_LINES: usize = 10;
const SHOWN_LOCATIONS: usize = 4;

static RUST_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(error|warning)(?:\[([A-Za-z0-9_]+)\])?: (.+)$").unwrap());
static CODE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Z]{1,4}[0-9]{2,5}) (?:\[\*\] )?(.+)$").unwrap());
static ARROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:-->|╭▸|┌─)\s*(.+?):(\d+)(?::(\d+))?\s*$").unwrap());
static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"#\[(?:warn|deny|forbid)\(([A-Za-z0-9_:]+)\)\]|index\.html#([a-z0-9_]+)").unwrap()
});
static TSC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\S[^()]*?)\((\d+),(\d+)\): (error|warning) (TS\d+): (.+)$").unwrap()
});
static LOCATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s:][^:\s]*\.[A-Za-z0-9]+):(\d+)(?::(\d+))?:\s+(.+)$").unwrap()
});
static SEVERITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?i)(fatal error|error|warning|note|info|style|hint|refactor|convention):\s*(.+)$",
    )
    .unwrap()
});
static RUBOCOP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([CWEFR]): (?:\[Correctable\] )?([A-Z][A-Za-z]+/[A-Za-z0-9]+): (.+)$").unwrap()
});
static LEADING_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Z]{1,4}[0-9]{2,5}):? (?:\[\*\] )?(.+)$").unwrap());
static TRAILING_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?)\s+\[(-?[A-Za-z][\w.:/-]*)\]$").unwrap());
static TRAILING_PAREN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?)\s+\(([a-z][a-z0-9_-]*)\)$").unwrap());
static ESLINT_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:/|[A-Za-z]:\\|\.{0,2}/?)?[^\s:]+\.[A-Za-z0-9]+$").unwrap());
static ESLINT_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.+?)(?:\s{2,}(@?[\w./-]+))?$").unwrap()
});
static SHELLCHECK_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^In (.+) line (\d+):$").unwrap());
static SHELLCHECK_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\^-*\^?\s*(SC\d+) \((\w+)\): (.+)$").unwrap());
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"`[^`]*`|'[^']*'|"[^"]*""#).unwrap());

#[derive(Clone)]
struct Finding {
    severity: Severity,
    rule: Option<String>,
    message: String,
    location: String,
    stream: Stream,
    first: usize,
    last: usize,
    excerpt: Vec<String>,
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

pub struct Lint {
    root: String,
    findings: Vec<Finding>,
    block: Option<Finding>,
    eslint_file: Option<String>,
    shellcheck_line: Option<(String, String)>,
    loose: Vec<String>,
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
            eslint_file: None,
            shellcheck_line: None,
            loose: Vec::new(),
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
        if let Some(block) = self.block.as_mut().filter(|b| b.stream == stream) {
            let header = RUST_HEADER.is_match(line) || CODE_HEADER.is_match(line);
            if !header && !line.is_empty() {
                block.last = n;
                if block.location.is_empty()
                    && let Some(c) = ARROW.captures(line)
                {
                    block.location = location(&self.root, &c[1], &c[2], c.get(3));
                }
                if let Some(c) = ATTRIBUTE.captures(line) {
                    let name = c
                        .get(1)
                        .map_or_else(|| format!("clippy::{}", &c[2]), |m| m.as_str().to_owned());
                    block.rule.get_or_insert(name);
                }
                if block.excerpt.len() < EXCERPT_LINES {
                    block.excerpt.push(short(line, 240));
                }
                return;
            }
            if !header {
                return;
            }
        }
        self.close();
        if let Some(c) = RUST_HEADER.captures(line) {
            self.block = Some(Finding {
                severity: Severity::parse(&c[1]),
                rule: c.get(2).map(|m| m.as_str().to_owned()),
                message: short(&c[3], 300),
                location: String::new(),
                stream,
                first: n,
                last: n,
                excerpt: vec![short(line, 240)],
            });
            return;
        }
        if let Some(c) = CODE_HEADER.captures(line) {
            self.block = Some(Finding {
                severity: Severity::Warning,
                rule: Some(c[1].to_owned()),
                message: short(&c[2], 300),
                location: String::new(),
                stream,
                first: n,
                last: n,
                excerpt: vec![short(line, 240)],
            });
            return;
        }
        if let Some(c) = TSC.captures(line) {
            self.push(Finding {
                severity: Severity::parse(&c[4]),
                rule: Some(c[5].to_owned()),
                message: short(&c[6], 300),
                location: location(&self.root, &c[1], &c[2], c.get(3)),
                stream,
                first: n,
                last: n,
                excerpt: vec![short(line, 240)],
            });
            return;
        }
        if let Some(c) = LOCATED.captures(line) {
            let place = location(&self.root, &c[1], &c[2], c.get(3));
            if let Some(finding) = located(&c[4], place, stream, n, line) {
                self.push(finding);
            }
            return;
        }
        if let Some(c) = SHELLCHECK_FILE.captures(line) {
            self.shellcheck_line = Some((c[1].to_owned(), c[2].to_owned()));
            return;
        }
        if let Some(c) = SHELLCHECK_ROW.captures(line)
            && let Some((file, row)) = &self.shellcheck_line
        {
            let place = location(&self.root, file, row, None);
            self.push(Finding {
                severity: Severity::parse(&c[2]),
                rule: Some(c[1].to_owned()),
                message: short(&c[3], 300),
                location: place,
                stream,
                first: n,
                last: n,
                excerpt: vec![short(line.trim(), 240)],
            });
            return;
        }
        if let Some(c) = ESLINT_ROW.captures(line)
            && let Some(file) = &self.eslint_file
        {
            let place = location(&self.root, file, &c[1], c.get(2));
            self.push(Finding {
                severity: Severity::parse(&c[3]),
                rule: c.get(5).map(|m| m.as_str().to_owned()),
                message: short(&c[4], 300),
                location: place,
                stream,
                first: n,
                last: n,
                excerpt: vec![short(line.trim(), 240)],
            });
            return;
        }
        if !line.starts_with(char::is_whitespace) && ESLINT_FILE.is_match(line) {
            self.eslint_file = Some(line.to_owned());
            return;
        }
        let lower = line.trim_start().to_ascii_lowercase();
        let seen = self
            .loose
            .iter()
            .any(|l| l.starts_with(&format!("{} [", short(line.trim(), 240))));
        if (lower.starts_with("error") || lower.starts_with("fatal") || lower.starts_with("✖"))
            && self.loose.len() < 4
            && !seen
        {
            self.loose.push(format!(
                "{} [{}:{n}]",
                short(line.trim(), 240),
                stream.name()
            ));
        }
    }

    fn push(&mut self, finding: Finding) {
        self.findings.push(finding);
    }

    fn close(&mut self) {
        let Some(block) = self.block.take() else {
            return;
        };
        if !block.location.is_empty() {
            self.findings.push(block);
        } else if block.severity == Severity::Error && self.loose.len() < 4 {
            // An error without a source location, such as a failed link step or the
            // final "could not compile" line, stays visible as a general outcome.
            self.loose.push(format!(
                "{} [{}:{}]",
                block.excerpt[0],
                block.stream.name(),
                block.first
            ));
        }
    }

    /// Return the grouped report and the comparison record, or `None` when no
    /// finding was recognized or the stream was too large to group reliably.
    pub fn finish(
        mut self,
        label: &str,
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
        let mut groups: BTreeMap<(Severity, String), Vec<Finding>> = BTreeMap::new();
        for finding in self.findings {
            let key = finding
                .rule
                .clone()
                .or_else(|| learned.get(&normalize(&finding.message)).cloned())
                .unwrap_or_else(|| normalize(&finding.message));
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
            .map(|f| f.location.split(':').next().unwrap_or(""))
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let mut ordered: Vec<_> = groups.into_iter().collect();
        ordered.sort_by(|((a, ak), af), ((b, bk), bf)| {
            a.cmp(b).then(bf.len().cmp(&af.len())).then(ak.cmp(bk))
        });

        let mut summary = Summary {
            kind: "lint".into(),
            items: Vec::new(),
            outcomes: Vec::new(),
        };
        let mut report = format!(
            "{label}: {total} finding(s), {errors} error(s), {} warning(s) · {} rule(s) · {} file(s)\n",
            total - errors,
            ordered.len(),
            files
        );
        let previous = previous.filter(|(_, s)| s.kind == "lint");
        let mut current = BTreeMap::new();
        for ((severity, rule), findings) in &ordered {
            let first = &findings[0];
            let mut item = Item::new(rule, &first.message, first.first);
            item.digest = findings.len().to_string();
            item.last = first.last;
            current.insert(rule.clone(), findings.len());
            summary.items.push(item);
            report.push_str(&format!(
                "{} {rule} ×{}  {}\n",
                severity.label(),
                findings.len(),
                first.message
            ));
            if *severity == Severity::Error {
                for line in first.excerpt.iter().skip(1) {
                    report.push_str(&format!("    {line}\n"));
                }
            }
            report.push_str(&format!("  {}\n", locations(findings)));
        }
        if let Some((id, old)) = previous {
            let before: BTreeMap<_, _> = old
                .items
                .iter()
                .map(|i| (i.name.clone(), i.digest.parse::<usize>().unwrap_or(0)))
                .collect();
            let mut changes = Vec::new();
            for (rule, count) in &current {
                match before.get(rule) {
                    None => changes.push(format!("+ {rule} ×{count} (new)")),
                    Some(old) if old != count => {
                        changes.push(format!("~ {rule} ×{old} → ×{count}"))
                    }
                    _ => {}
                }
            }
            for (rule, count) in &before {
                if !current.contains_key(rule) {
                    changes.push(format!("- {rule} ×{count} (no longer reported)"));
                }
            }
            if changes.is_empty() {
                report.push_str(&format!("Same findings as {id}.\n"));
            } else {
                report.push_str(&format!("Compared with {id}:\n"));
                for change in changes {
                    report.push_str(&change);
                    report.push('\n');
                }
            }
        }
        for line in &self.loose {
            report.push_str(line);
            report.push('\n');
        }
        Some((report, summary))
    }
}

fn located(message: &str, place: String, stream: Stream, n: usize, line: &str) -> Option<Finding> {
    let (mut severity, mut text) = (Severity::Warning, message.trim());
    let mut explicit = None;
    if let Some(c) = SEVERITY.captures(text) {
        let level = c.get(1).unwrap().as_str().to_ascii_lowercase();
        severity = Severity::parse(&level);
        text = c.get(2).unwrap().as_str();
        explicit = Some(level);
    }
    let mut rule = None;
    if let Some(c) = RUBOCOP.captures(text) {
        severity = Severity::parse(&c[1]);
        rule = Some(c[2].to_owned());
        text = c.get(3).unwrap().as_str();
    } else if let Some(c) = LEADING_CODE.captures(text) {
        rule = Some(c[1].to_owned());
        text = c.get(2).unwrap().as_str();
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
    Some(Finding {
        severity,
        rule,
        message: short(text.trim(), 300),
        location: place,
        stream,
        first: n,
        last: n,
        excerpt: vec![short(line, 240)],
    })
}

fn location(root: &str, file: &str, line: &str, column: Option<regex::Match>) -> String {
    let file = file.trim();
    let file = file.strip_prefix(root).unwrap_or(file);
    let file = file.strip_prefix("./").unwrap_or(file);
    match column {
        Some(column) => format!("{file}:{line}:{}", column.as_str()),
        None => format!("{file}:{line}"),
    }
}

fn normalize(message: &str) -> String {
    QUOTED.replace_all(message, "…").into_owned()
}

fn locations(findings: &[Finding]) -> String {
    // Order by file and numeric position and drop repeats so same-file runs collapse.
    let mut places: Vec<(&str, Vec<usize>, &str)> = findings
        .iter()
        .map(|f| {
            let (file, rest) = f.location.split_once(':').unwrap_or((&f.location, ""));
            let numbers = rest.split(':').filter_map(|n| n.parse().ok()).collect();
            (file, numbers, f.location.as_str())
        })
        .collect();
    places.sort();
    places.dedup();
    let distinct = places.len();
    let mut out = String::new();
    let mut previous_file = "";
    for (_, _, location) in places.into_iter().take(SHOWN_LOCATIONS) {
        let (file, rest) = location.split_once(':').unwrap_or((location, ""));
        if !out.is_empty() {
            out.push_str(", ");
        }
        if file == previous_file {
            out.push_str(rest);
        } else {
            out.push_str(location);
        }
        previous_file = file;
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
