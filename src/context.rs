use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::sync::LazyLock;

use anyhow::{Context, Result, ensure};
use regex::Regex;

use crate::capture;
use crate::index::{Definition, Fingerprint, Index, is_call, nul_paths};
use crate::rust_syntax::Kind;
use crate::workspace::{Location, Workspace, display_path};

static HUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("Git hunk pattern")
});

pub(crate) struct Report {
    output: String,
    cap: usize,
    pub(crate) clipped: bool,
}

impl Report {
    const CLIPPED: &str = "[context budget reached]\n";

    pub(crate) fn new(cap: usize) -> Self {
        Self {
            output: String::new(),
            cap,
            clipped: false,
        }
    }

    pub(crate) fn line(&mut self, line: impl AsRef<str>) {
        let line = line.as_ref();
        let left = self
            .cap
            .saturating_sub(self.output.len() + Self::CLIPPED.len());
        if self.clipped || left == 0 {
            self.clipped = true;
        } else if line.len() < left {
            self.output.push_str(line);
            self.output.push('\n');
        } else {
            let end = line.floor_char_boundary(left.saturating_sub(4));
            if end > 0 {
                self.output.push_str(&line[..end]);
                self.output.push_str("…\n");
            }
            self.clipped = true;
        }
    }

    /// Bytes still available before the budget is reached. A line is kept
    /// whole while it is shorter than this.
    pub(crate) fn left(&self) -> usize {
        self.cap
            .saturating_sub(self.output.len() + Self::CLIPPED.len())
    }

    pub(crate) fn into_string(mut self) -> String {
        if self.clipped {
            self.output.push_str(Self::CLIPPED);
        }
        self.output
    }

    pub(crate) fn finish(self) -> Result<()> {
        io::stdout()
            .lock()
            .write_all(self.into_string().as_bytes())?;
        Ok(())
    }
}

fn describe(definition: &Definition) -> String {
    let kind = if definition.test {
        "test"
    } else {
        &definition.kind
    };
    format!(
        "{} [{kind}] at {}",
        definition.qualified, definition.location
    )
}

pub fn query(index: &Index, name: &str, view: &str, cap: usize) -> Result<()> {
    let id = index.select(name)?;
    let mut report = Report::new(cap);
    report.line(describe(&index.definitions[id]));
    if view == "definition" {
        return report.finish();
    }
    if matches!(view, "references" | "callers" | "all") {
        let mut count = 0;
        for edge in index
            .incoming(id)
            .filter(|edge| view != "callers" || is_call(edge.kind))
        {
            count += 1;
            let owner = edge
                .from
                .map(|id| index.definitions[id].qualified.as_str())
                .unwrap_or("file scope");
            let kind = relationship(edge.kind);
            let confidence = if edge.inferred { "possible " } else { "" };
            report.line(format!(
                "  {confidence}{kind} from {owner} at {}",
                edge.location
            ));
            if report.clipped {
                break;
            }
        }
        if count == 0 {
            report.line("  No matching references found.");
        }
    }
    if matches!(view, "dependencies" | "all") {
        for edge in index.outgoing(id) {
            let confidence = if edge.inferred { "possible " } else { "" };
            report.line(format!(
                "  {confidence}{} → {} (from {})",
                relationship(edge.kind),
                describe(&index.definitions[edge.to]),
                edge.location
            ));
            if report.clipped {
                break;
            }
        }
    }
    if matches!(view, "tests" | "all") {
        let tests = index.related_tests(id, 16);
        for (id, hops, inferred) in &tests {
            let confidence = if *inferred { "possible " } else { "" };
            report.line(format!(
                "  {confidence}related test ({} call hops): {}",
                hops,
                describe(&index.definitions[*id])
            ));
        }
        if tests.is_empty() {
            report.line("  No related tests found in indexed calls.");
        }
    }
    report.finish()
}

pub(crate) fn relationship(kind: Kind) -> &'static str {
    match kind {
        Kind::Call | Kind::Method => "call",
        Kind::Reference => "reference",
        Kind::Import => "import",
        Kind::Route => "HTTP route",
        Kind::Data => "SQL relation",
    }
}

pub fn path(index: &Index, from: &str, to: &str, cap: usize) -> Result<()> {
    let from = index.select(from)?;
    let to = index.select(to)?;
    let path = index
        .path(from, to)
        .context("no indexed relationship path found")?;
    let mut report = Report::new(cap);
    report.line(describe(&index.definitions[from]));
    for (edge, reverse) in path {
        let direction = if reverse { "←" } else { "→" };
        let other = if reverse { edge.from.unwrap() } else { edge.to };
        let confidence = if edge.inferred { "possible " } else { "" };
        report.line(format!(
            "  {direction} {confidence}{} at {} → {}",
            relationship(edge.kind),
            edge.location,
            describe(&index.definitions[other])
        ));
        if report.clipped {
            break;
        }
    }
    report.finish()
}

pub fn find(
    workspace: &Workspace,
    index: &Index,
    term: &str,
    insensitive: bool,
    limit: usize,
    cap: usize,
) -> Result<()> {
    ensure!(
        !term.is_empty() && !term.contains('\n'),
        "enter a non-empty, single-line search term"
    );
    let term = if insensitive {
        term.to_ascii_lowercase()
    } else {
        term.to_owned()
    };
    let fingerprint = Fingerprint::new(&term);
    let mut report = Report::new(cap);
    let mut matches = 0;
    let mut more = false;
    'files: for (file, candidate) in &index.fingerprints {
        if !candidate.includes(fingerprint) {
            continue;
        }
        let Ok(path) = workspace.source_path(file) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (offset, line) in text.lines().enumerate() {
            let position = if insensitive {
                line.to_ascii_lowercase().find(&term)
            } else {
                line.find(&term)
            };
            if let Some(position) = position {
                if matches == limit {
                    more = true;
                    break 'files;
                }
                let start = line.ceil_char_boundary(position.saturating_sub(80));
                let end = line.floor_char_boundary(
                    (position + term.len()).saturating_add(180).min(line.len()),
                );
                report.line(format!(
                    "{}:{}: {}",
                    display_path(file),
                    offset + 1,
                    &line[start..end]
                ));
                matches += 1;
                if report.clipped {
                    break 'files;
                }
            }
        }
    }
    if more {
        report.line(format!(
            "[limit of {limit} matches reached; more exist. Raise --limit to see them.]"
        ));
    }
    if matches == 0 {
        report.line("No matching indexed text.");
        if let Some(note) = index.skipped_note() {
            report.line(note);
        }
    }
    report.finish()
}

pub fn code(workspace: &Workspace, file: &str, from: usize, to: usize, cap: usize) -> Result<()> {
    ensure!(
        from > 0 && from <= to,
        "expected a positive, ordered line range"
    );
    let lines = ensure_line(workspace, file, from)?;
    let shown = to.min(lines);
    let mut report = Report::new(cap);
    report.line(format!("{}:{from}-{shown}", display_path(file)));
    snippet(workspace, file, from, shown, &mut report)?;
    if shown < to && !report.clipped {
        report.line(format!("  (file ends at line {lines})"));
    }
    report.finish()
}

/// Lines in a workspace file. A range that starts past the end is an error,
/// so a stale line number never looks like an empty location.
fn line_count(workspace: &Workspace, file: &str) -> Result<usize> {
    let text = fs::read_to_string(workspace.source_path(file)?)?;
    Ok(text.lines().count())
}

fn ensure_line(workspace: &Workspace, file: &str, line: usize) -> Result<usize> {
    let lines = line_count(workspace, file)?;
    ensure!(
        line <= lines,
        "{} has {lines} lines; line {line} is past the end",
        display_path(file)
    );
    Ok(lines)
}

fn snippet(
    workspace: &Workspace,
    file: &str,
    from: usize,
    to: usize,
    report: &mut Report,
) -> Result<()> {
    let text = fs::read_to_string(workspace.source_path(file)?)?;
    let mut last = None;
    for (line, content) in text.lines().enumerate().skip(from - 1).take(to - from + 1) {
        if report.clipped {
            break;
        }
        report.line(format!("  {} | {content}", line + 1));
        last = Some(line + 1);
    }
    if let Some(last) = last {
        crate::history::saw_source(
            workspace,
            file,
            from,
            last,
            capture::source_digest(text.as_bytes()),
        );
    }
    Ok(())
}

pub fn at(workspace: &Workspace, index: &Index, input: &str, cap: usize) -> Result<()> {
    let (file, range) = input
        .rsplit_once(':')
        .context("use a workspace-relative path and line, e.g. src/lib.rs:10")?;
    let (start, end) = range.split_once('-').unwrap_or((range, range));
    let start = start.parse::<usize>().context("invalid source line")?;
    let end = end.parse::<usize>().context("invalid source line")?;
    ensure!(
        start > 0 && start <= end,
        "expected a positive, ordered line range"
    );
    let lines = ensure_line(workspace, file, start)?;
    let mut report = Report::new(cap);
    at_location(
        workspace,
        index,
        &Location {
            file: file.to_owned(),
            start,
            end: end.min(lines),
        },
        &mut report,
    )?;
    report.finish()
}

pub fn symbol(workspace: &Workspace, index: &Index, name: &str, cap: usize) -> Result<()> {
    let symbol = &index.definitions[index.select(name)?];
    code(
        workspace,
        &symbol.location.file,
        symbol.location.start,
        symbol.location.end,
        cap,
    )
}

/// Distinct log lines listed for one source location in `context --run`.
const MAX_DIAGNOSTICS_PER_LOCATION: usize = 4;

pub fn from_run(workspace: &Workspace, index: &Index, id: &str, cap: usize) -> Result<()> {
    let (directory, run) = capture::load(workspace, id)?;
    crate::history::recall(&directory, "context");
    let mut report = Report::new(cap);
    report.line(format!(
        "Run {id} · {} source locations",
        run.diagnostics.len()
    ));
    if run.diagnostics.is_empty() {
        report.line(format!("No indexed source location found. Inspect justless search {id} <text> or justless log {id}."));
    }
    // Group by source location but keep every distinct log line: progress output and
    // the real error often name the same line, and the error must survive.
    let mut groups: Vec<(&Location, Vec<&capture::Diagnostic>)> = Vec::new();
    for diagnostic in &run.diagnostics {
        let location = &diagnostic.location;
        match groups.iter_mut().find(|(seen, _)| *seen == location) {
            Some((_, list)) => {
                if !list.iter().any(|d| {
                    d.stream.name() == diagnostic.stream.name() && d.log_line == diagnostic.log_line
                }) {
                    list.push(diagnostic);
                }
            }
            None => groups.push((location, vec![diagnostic])),
        }
    }
    // A failed command usually reports its error on stderr; list those first, both
    // within a location and across locations, so a tight budget keeps them.
    let on_stderr = |d: &&capture::Diagnostic| d.stream.name() == "stderr";
    if run.status != 0 {
        for (_, list) in &mut groups {
            list.sort_by_key(|d| !on_stderr(d));
        }
        groups.sort_by_key(|(_, list)| !list.iter().any(on_stderr));
    }
    for (location, diagnostics) in groups {
        let shown = diagnostics.len().min(MAX_DIAGNOSTICS_PER_LOCATION);
        for diagnostic in &diagnostics[..shown] {
            report.line(format!(
                "{} · log {}:{}",
                location,
                diagnostic.stream.name(),
                diagnostic.log_line
            ));
            report.line(format!(
                "  Replay: justless log {id} --stream {} --from {} --to {}",
                diagnostic.stream.name(),
                diagnostic.log_line.saturating_sub(2).max(1),
                diagnostic.log_line.saturating_add(2)
            ));
            for line in capture::diagnostic_excerpt(&directory, diagnostic)? {
                report.line(line);
            }
        }
        if diagnostics.len() > shown {
            report.line(format!(
                "  +{} more log line(s) at this location. justless search {id} {}",
                diagnostics.len() - shown,
                location.file
            ));
        }
        if let Some(digest) = run.source_digests.get(&location.file)
            && index.files.get(&location.file) != Some(digest)
        {
            report.line("  Source changed or disappeared after this run. Re-run the command for current locations.");
        } else {
            at_location(workspace, index, location, &mut report)?;
        }
        if report.clipped {
            break;
        }
    }
    report.finish()
}

fn at_location(
    workspace: &Workspace,
    index: &Index,
    location: &Location,
    report: &mut Report,
) -> Result<()> {
    let definitions = index.at(location);
    // A file or module contains everything in it, so it only stands for a range
    // when no symbol inside it overlaps. A range across several symbols, such as
    // a new file, shows each of them in source order.
    let members: Vec<usize> = definitions
        .iter()
        .copied()
        .filter(|&id| !matches!(index.definitions[id].kind.as_str(), "file" | "module"))
        .collect();
    let containing = members.iter().copied().find(|&id| {
        let span = &index.definitions[id].location;
        span.start <= location.start && location.end <= span.end
    });
    if let Some(best) = containing {
        return detail(workspace, index, best, location, &members, report);
    }
    if !members.is_empty() {
        let mut outermost: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&id| {
                let span = &index.definitions[id].location;
                !members.iter().any(|&other| {
                    let outer = &index.definitions[other].location;
                    outer != span && outer.start <= span.start && span.end <= outer.end
                })
            })
            .collect();
        outermost.sort_by_key(|&id| (index.definitions[id].location.start, id));
        let mut names: Vec<_> = outermost
            .iter()
            .take(24)
            .map(|&id| {
                let qualified = &index.definitions[id].qualified;
                qualified.rsplit("::").next().unwrap_or(qualified)
            })
            .collect();
        let more = outermost.len().saturating_sub(names.len());
        let rest = format!("+{more} more");
        if more > 0 {
            names.push(&rest);
        }
        report.line(format!(
            "{location} · {} symbols: {}",
            outermost.len(),
            names.join(", ")
        ));
        for id in outermost {
            if report.clipped {
                break;
            }
            let span = &index.definitions[id].location;
            let part = Location {
                file: location.file.clone(),
                start: span.start.max(location.start),
                end: span.end.min(location.end),
            };
            detail(workspace, index, id, &part, &[], report)?;
        }
        return Ok(());
    }
    let Some(best) = definitions.first().copied() else {
        report.line(format!("{} · no enclosing indexed symbol", location));
        if let Some(error) = index.errors.get(&location.file) {
            report.line(format!("  Parse error: {error}"));
        }
        return snippet(
            workspace,
            &location.file,
            location.start.saturating_sub(2).max(1),
            location.end.min(location.start.saturating_add(8)),
            report,
        );
    };
    detail(workspace, index, best, location, &definitions, report)
}

/// One symbol around a location: its calls, related tests, the other symbols
/// the range overlaps, and source from the location.
fn detail(
    workspace: &Workspace,
    index: &Index,
    best: usize,
    location: &Location,
    overlapping: &[usize],
    report: &mut Report,
) -> Result<()> {
    let definition = &index.definitions[best];
    report.line(describe(definition));
    let mut referenced = BTreeSet::new();
    let mut edges: Vec<_> = index
        .outgoing(best)
        .filter(|edge| {
            is_call(edge.kind) || matches!(edge.kind, Kind::Route | Kind::Data | Kind::Import)
        })
        .collect();
    edges.sort_by_key(|edge| {
        (
            edge.inferred,
            edge.location.start.abs_diff(location.start),
            edge.location.start,
        )
    });
    for edge in edges {
        if referenced.insert(edge.to) {
            let description = match edge.kind {
                Kind::Call | Kind::Method => "calls",
                Kind::Import => "imports",
                Kind::Route => "HTTP route to",
                Kind::Data => "SQL relation to",
                Kind::Reference => "references",
            };
            let confidence = if edge.inferred {
                "possible relationship · "
            } else {
                ""
            };
            report.line(format!(
                "  {confidence}{description} {} (from {})",
                describe(&index.definitions[edge.to]),
                edge.location
            ));
            if referenced.len() >= 4 {
                break;
            }
        }
    }
    if !definition.test {
        for (id, _, inferred) in index.related_tests(best, 3) {
            let confidence = if inferred {
                "possible related test"
            } else {
                "related test"
            };
            report.line(format!(
                "  {confidence}: {}",
                describe(&index.definitions[id])
            ));
        }
    }
    if location.end > location.start {
        let others: Vec<_> = overlapping
            .iter()
            .filter(|&&id| id != best && index.definitions[id].location != definition.location)
            .collect();
        for &&id in others.iter().take(4) {
            report.line(format!("  overlaps {}", describe(&index.definitions[id])));
        }
        if others.len() > 4 {
            report.line(format!(
                "  … and {} more overlapping symbols",
                others.len() - 4
            ));
        }
    }
    let from = location
        .start
        .saturating_sub(3)
        .max(definition.location.start);
    let to = location
        .end
        .max(location.start.saturating_add(3))
        .min(definition.location.end)
        .min(from.saturating_add(14));
    snippet(workspace, &location.file, from, to.max(from), report)?;
    if definition.declaration < from && !report.clipped {
        report.line(format!(
            "  Declaration starts at {}:{}",
            display_path(&location.file),
            definition.declaration
        ));
    }
    Ok(())
}

pub(crate) enum Change {
    Unindexed,
    StagedDiffers,
    Empty,
    Ranges(Vec<(usize, usize)>),
    /// Lines removed without a replacement.
    Deleted(Vec<Deletion>),
}

/// Lines a change removed from a file without adding any in their place.
pub(crate) struct Deletion {
    /// The current line the removed lines followed (0 for the top of the file).
    pub after: usize,
    /// The removed lines in the committed version, inclusive.
    pub old: (usize, usize),
    /// Symbols that lay entirely inside the removed lines, as (name, kind).
    pub symbols: Vec<(String, String)>,
    /// The text of the removed lines, as the diff showed them.
    pub lines: Vec<String>,
}

/// Name the symbols each deletion removed, from the committed version of `file`.
/// Leaves the list empty when that version cannot be read or parsed.
fn deleted_symbols(workspace: &Workspace, file: &str, deletions: &mut [Deletion]) {
    let Ok(output) = workspace.git(&["show", &format!("HEAD:./{file}")]) else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return;
    };
    let mut snapshot = crate::source::Snapshot::from_text(file, text);
    if snapshot.index(file).is_err() {
        return;
    }
    let symbols = snapshot.symbols();
    for deletion in deletions {
        let (first, last) = deletion.old;
        let inside = |start: usize, end: usize| first <= start && end <= last;
        for (name, kind, start, end) in &symbols {
            // Keep the outermost removed symbol; its members went with it.
            let nested = symbols.iter().any(|(_, _, s, e)| {
                (s, e) != (start, end) && inside(*s, *e) && s <= start && end <= e
            });
            if inside(*start, *end) && !nested {
                deletion.symbols.push((name.clone(), kind.clone()));
            }
        }
    }
}

pub fn changed(workspace: &Workspace, index: &Index, staged: bool, cap: usize) -> Result<()> {
    let mut report = Report::new(cap);
    let changes = changes(workspace, index, staged)?;
    for (file, change) in &changes {
        let name = display_path(file);
        match change {
            Change::Unindexed if fs::symlink_metadata(workspace.root.join(file)).is_err() => {
                report.line(format!("{name} · deleted"))
            }
            Change::Unindexed => report.line(format!("{name} · no current indexed source")),
            Change::StagedDiffers => report.line(format!("{name} · working tree differs from staged source. Use justless context --changes to inspect the current file.")),
            Change::Empty => report.line(format!("{name} · empty after changes")),
            Change::Deleted(deletions) => {
                for deletion in deletions {
                    deleted(workspace, index, file, deletion, &mut report)?;
                    if report.clipped {
                        break;
                    }
                }
            }
            Change::Ranges(locations) => {
                report.line(format!("{name} · {} changed ranges", locations.len()));
                for &(start, end) in locations {
                    report.line(format!("Changed {name}:{start}-{end}"));
                    at_location(
                        workspace,
                        index,
                        &Location {
                            file: file.clone(),
                            start,
                            end,
                        },
                        &mut report,
                    )?;
                    if report.clipped {
                        break;
                    }
                }
            }
        }
        if report.clipped {
            break;
        }
    }
    if changes.is_empty() {
        report.line("No changed files.");
    }
    report.finish()
}

/// One deletion for `context --changes`: where the lines were, what they said,
/// the symbols removed whole, and the surviving symbol that lost lines, if any.
/// A deletion between symbols is not pinned on the line before it.
fn deleted(
    workspace: &Workspace,
    index: &Index,
    file: &str,
    deletion: &Deletion,
    report: &mut Report,
) -> Result<()> {
    const SHOWN: usize = 8;
    let name = display_path(file);
    let (first, last) = deletion.old;
    let (lines, previously) = if first == last {
        ("1 line".to_owned(), format!("previously line {first}"))
    } else {
        (
            format!("{} lines", last + 1 - first),
            format!("previously lines {first}-{last}"),
        )
    };
    let place = if deletion.after == 0 {
        format!("at the start of {name}")
    } else {
        format!("after {name}:{}", deletion.after)
    };
    let removed = if deletion.symbols.is_empty() {
        String::new()
    } else {
        let symbols: Vec<_> = deletion
            .symbols
            .iter()
            .map(|(name, kind)| format!("{name} [{kind}]"))
            .collect();
        format!(" · removed {}", symbols.join(", "))
    };
    report.line(format!("Deleted {lines} {place} ({previously}){removed}"));
    for (offset, line) in deletion.lines.iter().take(SHOWN).enumerate() {
        report.line(format!("  -{} | {line}", first + offset));
    }
    if deletion.lines.len() > SHOWN {
        report.line(format!(
            "  … and {} more deleted lines",
            deletion.lines.len() - SHOWN
        ));
    }
    if deletion.after == 0 {
        return Ok(());
    }
    // The lines on both sides of the removal belong to one symbol: it changed.
    let location = Location {
        file: file.to_owned(),
        start: deletion.after,
        end: deletion.after + 1,
    };
    let around = index
        .at(&location)
        .into_iter()
        .filter(|&id| {
            let definition = &index.definitions[id];
            let span = &definition.location;
            !matches!(definition.kind.as_str(), "file" | "module")
                && span.start <= deletion.after
                && deletion.after < span.end
        })
        .min_by_key(|&id| {
            let span = &index.definitions[id].location;
            span.end - span.start
        });
    if let Some(id) = around {
        detail(workspace, index, id, &location, &[], report)?;
    }
    Ok(())
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Changed line ranges per file against `HEAD`, or the index with `staged`.
/// Untracked files count as changed throughout. Lines removed without a
/// replacement are reported apart, as deletions.
pub(crate) fn changes(
    workspace: &Workspace,
    index: &Index,
    staged: bool,
) -> Result<Vec<(String, Change)>> {
    let inside = workspace.git(&["rev-parse", "--is-inside-work-tree"])?;
    ensure!(
        inside.status.success() && inside.stdout.starts_with(b"true"),
        "--changes needs a Git repository: {}",
        first_line(&inside.stderr)
    );
    let mut args = vec![
        "diff",
        "--relative",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
    ];
    let head = workspace.git(&["rev-parse", "--verify", "HEAD"])?;
    if staged {
        args.push("--cached");
    } else if head.status.success() {
        args.push("HEAD");
    }
    let mut names = args.clone();
    names.extend(["--name-only", "-z", "--", "."]);
    let output = workspace.git(&names)?;
    ensure!(
        output.status.success(),
        "git diff failed: {}",
        first_line(&output.stderr)
    );
    let mut tracked: BTreeSet<_> = nul_paths(&output.stdout).map(str::to_owned).collect();
    let mut new_files = BTreeSet::new();
    if !staged {
        let output = workspace.git(&[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ])?;
        ensure!(
            output.status.success(),
            "git ls-files failed: {}",
            first_line(&output.stderr)
        );
        new_files.extend(nul_paths(&output.stdout).map(str::to_owned));
        if !head.status.success() {
            let output = workspace.git(&["diff", "--cached", "--name-only", "-z", "--", "."])?;
            ensure!(output.status.success(), "could not list staged changes");
            new_files.extend(nul_paths(&output.stdout).map(str::to_owned));
        }
        tracked.retain(|file| !new_files.contains(file));
    }
    let mut changes = Vec::new();
    for (file, untracked) in tracked
        .iter()
        .map(|file| (file, false))
        .chain(new_files.iter().map(|file| (file, true)))
    {
        if !index.files.contains_key(file) {
            changes.push((file.clone(), Change::Unindexed));
            continue;
        }
        if staged {
            let difference = workspace.git(&["diff", "--quiet", "--", file])?;
            if !difference.status.success() {
                changes.push((file.clone(), Change::StagedDiffers));
                continue;
            }
        }
        let text = fs::read_to_string(workspace.source_path(file)?)?;
        let file_lines = text.lines().count();
        if file_lines == 0 {
            changes.push((file.clone(), Change::Empty));
            continue;
        }
        let mut deletions = Vec::new();
        let locations = if untracked {
            vec![(1, file_lines)]
        } else {
            let mut hunk = args.clone();
            hunk.extend(["--unified=0", "--", file]);
            let output = workspace.git(&hunk)?;
            ensure!(output.status.success(), "could not read diff for {file}");
            let mut ranges = Vec::new();
            // Whether the removed lines that follow belong to the last deletion.
            let mut removing = false;
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let Some(matches) = HUNK.captures(line) else {
                    if removing && let Some(text) = line.strip_prefix('-') {
                        let deletion: &mut Deletion =
                            deletions.last_mut().expect("deletion in progress");
                        deletion.lines.push(text.to_owned());
                    }
                    continue;
                };
                let number = |i: usize, default: usize| {
                    matches
                        .get(i)
                        .and_then(|value| value.as_str().parse::<usize>().ok())
                        .unwrap_or(default)
                };
                let (old_start, old_count) = (number(1, 0), number(2, 1));
                let (start, count) = (number(3, 0), number(4, 1));
                removing = count == 0;
                if count == 0 {
                    // A hunk with no new lines removed lines after line `start` of the
                    // current file. It changes no surviving line, so it must not be
                    // pinned on the line before it.
                    deletions.push(Deletion {
                        after: start,
                        old: (old_start, old_start + old_count.saturating_sub(1)),
                        symbols: Vec::new(),
                        lines: Vec::new(),
                    });
                    continue;
                }
                let start = start.max(1).min(file_lines);
                ranges.push((
                    start,
                    start
                        .saturating_add(count.saturating_sub(1))
                        .min(file_lines),
                ));
            }
            ranges
        };
        if !deletions.is_empty() {
            deleted_symbols(workspace, file, &mut deletions);
            changes.push((
                file.clone(),
                Change::Deleted(std::mem::take(&mut deletions)),
            ));
        }
        if !locations.is_empty() || untracked {
            changes.push((file.clone(), Change::Ranges(locations)));
        }
    }
    Ok(changes)
}
