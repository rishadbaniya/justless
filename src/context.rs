use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::sync::LazyLock;

use anyhow::{Context, Result, ensure};
use regex::Regex;

use crate::capture;
use crate::index::{Definition, Index, Removed, is_call, nul_paths};
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
    let mut report = Report::new(cap);
    let justless = workspace.justless();
    // Bytes of source shown around the locations.
    let mut source = 0;
    report.line(format!(
        "Run {id} · {} source locations",
        run.diagnostics.len()
    ));
    if run.diagnostics.is_empty() {
        report.line(format!("No indexed source location found. Inspect {justless} search {id} <text> or {justless} log {id}."));
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
                "  Replay: {justless} log {id} --stream {} --from {} --to {}",
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
                "  +{} more log line(s) at this location. {justless} search {id} {}",
                diagnostics.len() - shown,
                crate::workspace::shell_text(&location.file)
            ));
        }
        if let Some(digest) = run.source_digests.get(&location.file)
            && index.files.get(&location.file) != Some(digest)
        {
            report.line("  Source changed or disappeared after this run. Re-run the command for current locations.");
        } else {
            let room = report.left();
            at_location(workspace, index, location, &mut report)?;
            source += room.saturating_sub(report.left());
        }
        if report.clipped {
            break;
        }
    }
    let text = report.into_string();
    io::stdout().lock().write_all(text.as_bytes())?;
    // The log lines and notes retrieve the run's output again. The source shown
    // beside them does not.
    crate::history::recall(
        &directory,
        crate::history::Retrieval::Context,
        Some(text.len().saturating_sub(source) as u64),
    );
    Ok(())
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
            // Members such as a class's methods are named under their owner.
            let mut inner: Vec<usize> = members
                .iter()
                .copied()
                .filter(|&other| {
                    let member = &index.definitions[other].location;
                    other != id && span.start <= member.start && member.end <= span.end
                })
                .collect();
            inner.sort_by_key(|&other| (index.definitions[other].location.start, other));
            detail(workspace, index, id, &part, &inner, report)?;
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

/// What `--changes` compares the working tree with.
#[derive(Clone, Debug)]
pub(crate) enum Since {
    /// `HEAD`, untracked files included.
    Head,
    /// The index against `HEAD`.
    Staged,
    /// The merge base of a ref and `HEAD`: a branch's commits and the
    /// working tree on top of them, untracked files included.
    Base {
        name: String,
        commit: String,
        short: String,
    },
}

impl Since {
    /// Resolves `--staged` or `--base REF`. A ref that names no commit, or
    /// shares no history with `HEAD`, is an error rather than an empty diff.
    pub(crate) fn resolve(
        workspace: &Workspace,
        staged: bool,
        base: Option<&str>,
    ) -> Result<Since> {
        let Some(name) = base else {
            return Ok(if staged { Since::Staged } else { Since::Head });
        };
        let inside = workspace.git(&["rev-parse", "--is-inside-work-tree"])?;
        ensure!(
            inside.status.success() && inside.stdout.starts_with(b"true"),
            "--base needs a Git repository: {}",
            first_line(&inside.stderr)
        );
        let target = workspace.git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{name}^{{commit}}"),
        ])?;
        ensure!(
            target.status.success(),
            "--base {name}: no such commit in this repository"
        );
        let target = first_line(&target.stdout);
        let merge_base = workspace.git(&["merge-base", &target, "HEAD"])?;
        ensure!(
            merge_base.status.success(),
            "--base {name}: no common history with HEAD{}",
            if workspace
                .git(&["rev-parse", "--is-shallow-repository"])
                .is_ok_and(|output| output.stdout.starts_with(b"true"))
            {
                " in this shallow clone; fetch more history"
            } else {
                ""
            }
        );
        let commit = first_line(&merge_base.stdout);
        let short = workspace
            .git(&["rev-parse", "--short", &commit])
            .map(|output| first_line(&output.stdout))
            .unwrap_or_else(|_| commit.chars().take(7).collect());
        Ok(Since::Base {
            name: name.to_owned(),
            commit,
            short,
        })
    }

    /// The commit whose files are the version before the change.
    pub(crate) fn rev(&self) -> &str {
        match self {
            Since::Base { commit, .. } => commit,
            _ => "HEAD",
        }
    }

    /// What a clean comparison says, with how to review a branch's commits.
    pub(crate) fn nothing_changed(&self, workspace: &Workspace) -> String {
        match self {
            Since::Head => format!("No changed files. {}", base_hint(workspace, "review")),
            Since::Staged => "No changed files.".into(),
            Since::Base { name, short, .. } => {
                format!("No changed files against {name} (merge-base {short}).")
            }
        }
    }
}

/// How to `verb` a branch's commits when nothing changed against `HEAD`,
/// naming a ref to pass when one fits: never one that names no commit here.
pub(crate) fn base_hint(workspace: &Workspace, verb: &str) -> String {
    match suggested_base(workspace) {
        Some(name) => {
            format!("To {verb} a branch's commits, add --base REF, such as --base {name}.")
        }
        None => format!("To {verb} a branch's commits, add --base REF."),
    }
}

/// The remote's default branch, or a usual local one, that names a commit
/// other than `HEAD`.
fn suggested_base(workspace: &Workspace) -> Option<String> {
    let commit = |name: &str| {
        let output = workspace
            .git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{name}^{{commit}}"),
            ])
            .ok()?;
        output.status.success().then(|| first_line(&output.stdout))
    };
    let head = commit("HEAD")?;
    let remote = workspace
        .git(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| first_line(&output.stdout))
        .filter(|name| !name.is_empty());
    remote
        .into_iter()
        .chain(["main", "master", "trunk", "develop"].map(str::to_owned))
        .find(|name| commit(name).is_some_and(|at| at != head))
}

pub(crate) enum Change {
    Unindexed,
    StagedDiffers,
    Empty,
    Ranges(Vec<(usize, usize)>),
    /// Lines removed without a replacement.
    Deleted(Vec<Deletion>),
    /// A change to the file as a whole that no line range shows, such as
    /// `mode 100755 -> 100644`, a binary body, a symlink target or a
    /// submodule commit. Nothing can trace what depends on it.
    FileLevel(String),
    /// Where an added file came from: `renamed from …` or `copied from …`.
    /// The change itself is still the deleted and the added path.
    Origin(String),
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
    /// The definitions removed with `symbols`, nested ones included, as the
    /// committed version declared them, each with the position in `symbols`
    /// of the outermost symbol that held it.
    pub removed: Vec<(usize, crate::index::Removed)>,
}

/// Name the symbols each deletion removed, from the committed version of `file`
/// at `rev`. Leaves the list empty when that version cannot be read or parsed.
fn deleted_symbols(workspace: &Workspace, file: &str, rev: &str, deletions: &mut [Deletion]) {
    let Ok(output) = workspace.git(&["show", &format!("{rev}:./{file}")]) else {
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
    let mut spans = Vec::new();
    for deletion in deletions.iter_mut() {
        let (first, last) = deletion.old;
        let inside = |start: usize, end: usize| first <= start && end <= last;
        let mut outermost = Vec::new();
        for (name, kind, start, end) in &symbols {
            // Keep the outermost removed symbol; its members went with it.
            let nested = symbols.iter().any(|(_, _, s, e)| {
                (s, e) != (start, end) && inside(*s, *e) && s <= start && end <= e
            });
            if inside(*start, *end) && !nested {
                deletion.symbols.push((name.clone(), kind.clone()));
                outermost.push((*start, *end));
            }
        }
        spans.push(outermost);
    }
    // The same definitions as the index parses them, to restore while resolving
    // the references that remain.
    let removed: Vec<(usize, usize, Removed)> = if file.ends_with(".rs") {
        crate::rust_syntax::parse(&snapshot.text)
            .map(|syntax| {
                syntax
                    .symbols
                    .into_iter()
                    .map(|symbol| (symbol.start, symbol.end, Removed::Rust(symbol)))
                    .collect()
            })
            .unwrap_or_default()
    } else if let Some(grammar) = crate::polyglot::Grammar::detect(file, &snapshot.text) {
        crate::polyglot::Parsers::default()
            .parse(grammar, &snapshot.text)
            .map(|facts| {
                facts
                    .definitions
                    .into_iter()
                    .map(|tag| (tag.start, tag.end, Removed::Foreign(tag)))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    for (start, end, definition) in removed {
        let holder = deletions
            .iter_mut()
            .zip(&spans)
            .find_map(|(deletion, spans)| {
                let (first, last) = deletion.old;
                if start < first || last < end {
                    return None;
                }
                let owner = spans.iter().position(|&(s, e)| s <= start && end <= e)?;
                Some((deletion, owner))
            });
        if let Some((deletion, owner)) = holder {
            deletion.removed.push((owner, definition));
        }
    }
}

pub fn changed(workspace: &Workspace, index: &Index, since: &Since, cap: usize) -> Result<()> {
    let mut report = Report::new(cap);
    let changes = changes(workspace, index, since)?;
    for (file, change) in &changes {
        let name = display_path(file);
        match change {
            Change::Unindexed if fs::symlink_metadata(workspace.root.join(file)).is_err() => {
                report.line(format!("{name} · deleted"))
            }
            Change::Unindexed => report.line(format!("{name} · no current indexed source")),
            Change::StagedDiffers => report.line(format!("{name} · working tree differs from staged source. Use {} context --changes to inspect the current file.", workspace.justless())),
            Change::Empty => report.line(format!("{name} · empty after changes")),
            Change::FileLevel(what) | Change::Origin(what) => {
                report.line(format!("{name} · {what}"))
            }
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
        report.line(since.nothing_changed(workspace));
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

/// Changed line ranges per file against `HEAD`, the index with
/// [`Since::Staged`], or a merge base with [`Since::Base`]. Untracked files
/// count as changed throughout. Lines removed without a replacement are
/// reported apart, as deletions. Every path Git lists is kept: a change no
/// line range shows, such as a mode, a binary body, a symlink or a
/// submodule, is a [`Change::FileLevel`].
pub(crate) fn changes(
    workspace: &Workspace,
    index: &Index,
    since: &Since,
) -> Result<Vec<(String, Change)>> {
    let staged = matches!(since, Since::Staged);
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
        // A submodule's own settings must not hide a changed pointer.
        "--ignore-submodules=none",
        "--no-renames",
    ];
    let head = workspace.git(&["rev-parse", "--verify", "HEAD"])?;
    match since {
        Since::Staged => args.push("--cached"),
        // One commit and no dots: its tree against the working tree, so a
        // branch's commits and uncommitted edits both count.
        Since::Base { commit, .. } => args.push(commit),
        Since::Head if head.status.success() => args.push("HEAD"),
        Since::Head => {}
    }
    // Git's file-level listing, not the line hunks, decides what changed.
    let mut listing = args.clone();
    listing.extend(["--raw", "-z", "--no-abbrev", "--", "."]);
    let output = workspace.git(&listing)?;
    ensure!(
        output.status.success(),
        "git diff failed: {}",
        first_line(&output.stderr)
    );
    let mut tracked: BTreeMap<String, Raw> = raw_entries(&output.stdout)
        .into_iter()
        .filter_map(|(raw, mut paths)| Some((paths.pop()?, raw)))
        .collect();
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
        tracked.retain(|file, _| !new_files.contains(file));
    }
    let origins = origins(workspace, &args, &tracked);
    let mut changes = Vec::new();
    for (file, raw) in tracked
        .iter()
        .map(|(file, raw)| (file, Some(raw)))
        .chain(new_files.iter().map(|file| (file, None)))
    {
        let untracked = raw.is_none();
        if let Some(origin) = origins.get(file) {
            changes.push((file.clone(), Change::Origin(origin.clone())));
        }
        let level = match raw {
            Some(raw) => raw.file_level(workspace, file),
            None => untracked_level(workspace, file),
        };
        if let Some((what, lines)) = &level {
            changes.push((file.clone(), Change::FileLevel(what.clone())));
            if !lines {
                continue;
            }
        }
        if !index.files.contains_key(file) {
            // The index skips binaries, so a tracked file's own patch says
            // whether its body changed, and whether in lines.
            let body = match raw {
                Some(raw) if raw.new_mode != ABSENT => body(&patch(workspace, &args, file)?),
                _ => Body::Lines,
            };
            match (body, raw) {
                (Body::Opaque, Some(raw)) => {
                    changes.push((file.clone(), Change::FileLevel(raw.opaque())))
                }
                // Only the mode changed, and that is listed above.
                (Body::Unchanged, _) if level.is_some() => {}
                _ => changes.push((file.clone(), Change::Unindexed)),
            }
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
        let mut opaque = false;
        let locations = if untracked {
            vec![(1, file_lines)]
        } else {
            let output = patch(workspace, &args, file)?;
            opaque = body(&output) == Body::Opaque;
            let mut ranges = Vec::new();
            // Whether the removed lines that follow belong to the last deletion.
            let mut removing = false;
            for line in output.lines() {
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
                        removed: Vec::new(),
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
        let lined = !locations.is_empty() || !deletions.is_empty();
        if !deletions.is_empty() {
            deleted_symbols(workspace, file, since.rev(), &mut deletions);
            changes.push((
                file.clone(),
                Change::Deleted(std::mem::take(&mut deletions)),
            ));
        }
        if !locations.is_empty() || untracked {
            changes.push((file.clone(), Change::Ranges(locations)));
        }
        // Git lists the file, but no line shows the change: a binary body, a
        // file marked `-diff`, or anything else without a line diff.
        if let Some(raw) = raw
            && !lined
            && (opaque || level.is_none())
        {
            changes.push((file.clone(), Change::FileLevel(raw.opaque())));
        }
    }
    Ok(changes)
}

/// Git's mode for a path that does not exist on that side.
const ABSENT: &str = "000000";
const SYMLINK: &str = "120000";
const SUBMODULE: &str = "160000";

/// One path in `git diff --raw`: its modes before and after, its object id
/// after, and Git's status letter, with a score for a rename or copy.
struct Raw {
    old_mode: String,
    new_mode: String,
    new_id: String,
    status: String,
}

impl Raw {
    /// A change to the path as a whole, if any, and whether its lines can
    /// still be read, as a file's can after its mode changed. A symlink, a
    /// submodule or a change of type has no lines of its own.
    fn file_level(&self, workspace: &Workspace, file: &str) -> Option<(String, bool)> {
        let (old, new) = (self.old_mode.as_str(), self.new_mode.as_str());
        let what = if self.status.starts_with('T') {
            format!("type changed from {} to {}", kind(old), kind(new))
        } else if old == SUBMODULE || new == SUBMODULE {
            match (old, new) {
                (ABSENT, _) => "new submodule",
                (_, ABSENT) => "submodule removed",
                _ => "submodule changed",
            }
            .to_owned()
        } else if old == SYMLINK || new == SYMLINK {
            match (old, new) {
                (_, ABSENT) => "symlink deleted".to_owned(),
                (ABSENT, _) => format!("new symlink{}", self.target(workspace, file)),
                _ => format!("symlink target changed{}", self.target(workspace, file)),
            }
        } else if old != new && old != ABSENT && new != ABSENT {
            return Some((format!("mode {old} -> {new}"), true));
        } else {
            return None;
        };
        Some((what, false))
    }

    /// ` to TARGET` for a symlink: the staged blob, or the link on disk when
    /// Git compared the working tree.
    fn target(&self, workspace: &Workspace, file: &str) -> String {
        let target = if self.new_id.bytes().all(|byte| byte == b'0') {
            fs::read_link(workspace.root.join(file))
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        } else {
            workspace
                .git(&["cat-file", "blob", &self.new_id])
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        };
        target.map_or_else(String::new, |target| format!(" to {target}"))
    }

    /// A body Git shows without a line diff.
    fn opaque(&self) -> String {
        if self.status.starts_with('A') {
            "added, binary or no line diff".into()
        } else {
            "binary or no line diff".into()
        }
    }
}

/// What a mode in `git diff --raw` holds.
fn kind(mode: &str) -> String {
    match mode {
        "100644" => "file".into(),
        "100755" => "executable file".into(),
        SYMLINK => "symlink".into(),
        SUBMODULE => "submodule".into(),
        _ => format!("mode {mode}"),
    }
}

/// An untracked path with no lines of its own: a symlink, or a nested
/// repository, which `git ls-files` lists as `DIR/`.
fn untracked_level(workspace: &Workspace, file: &str) -> Option<(String, bool)> {
    if file.ends_with('/') {
        return Some(("new nested Git repository".into(), false));
    }
    let path = workspace.root.join(file);
    if !fs::symlink_metadata(&path).ok()?.file_type().is_symlink() {
        return None;
    }
    let target = fs::read_link(&path)
        .map(|target| format!(" to {}", target.to_string_lossy()))
        .unwrap_or_default();
    Some((format!("new symlink{target}"), false))
}

/// Parses `git diff --raw -z`: a status record, then one path, or two for a
/// rename or copy. Paths that are not UTF-8 are skipped, as elsewhere.
fn raw_entries(bytes: &[u8]) -> Vec<(Raw, Vec<String>)> {
    let mut fields = bytes.split(|&byte| byte == 0);
    let mut entries = Vec::new();
    while let Some(record) = fields.next() {
        let Some(record) = std::str::from_utf8(record)
            .ok()
            .and_then(|record| record.strip_prefix(':'))
        else {
            continue;
        };
        let [old_mode, new_mode, _, new_id, status] = record.split(' ').collect::<Vec<_>>()[..]
        else {
            continue;
        };
        let count = if status.starts_with(['R', 'C']) { 2 } else { 1 };
        let paths: Vec<String> = fields
            .by_ref()
            .take(count)
            .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_owned))
            .collect();
        if paths.len() == count {
            entries.push((
                Raw {
                    old_mode: old_mode.to_owned(),
                    new_mode: new_mode.to_owned(),
                    new_id: new_id.to_owned(),
                    status: status.to_owned(),
                },
                paths,
            ));
        }
    }
    entries
}

/// Where each added path came from, by Git's rename and copy detection. The
/// change set itself keeps the deleted path and the added one apart, so a
/// rename reaches the users of both.
fn origins(
    workspace: &Workspace,
    args: &[&str],
    tracked: &BTreeMap<String, Raw>,
) -> BTreeMap<String, String> {
    let mut origins = BTreeMap::new();
    if !tracked.values().any(|raw| raw.status.starts_with('A')) {
        return origins;
    }
    let mut detect: Vec<&str> = args
        .iter()
        .copied()
        .filter(|arg| *arg != "--no-renames")
        .collect();
    detect.extend(["-M", "-C", "--raw", "-z", "--no-abbrev", "--", "."]);
    let Ok(output) = workspace.git(&detect) else {
        return origins;
    };
    if !output.status.success() {
        return origins;
    }
    for (raw, paths) in raw_entries(&output.stdout) {
        let how = match raw.status.chars().next() {
            Some('R') => "renamed",
            Some('C') => "copied",
            _ => continue,
        };
        let [from, to] = &paths[..] else {
            continue;
        };
        let similar = raw.status[1..]
            .parse::<u32>()
            .ok()
            .filter(|score| *score < 100)
            .map(|score| format!(", {score}% similar"))
            .unwrap_or_default();
        origins.insert(
            to.clone(),
            format!("{how} from {}{similar}", display_path(from)),
        );
    }
    origins
}

/// What a file's patch shows of its body.
#[derive(PartialEq)]
enum Body {
    Lines,
    /// Git said `Binary files … differ`: a binary, or a file marked `-diff`.
    Opaque,
    /// No hunk at all, as when only the mode changed.
    Unchanged,
}

fn body(patch: &str) -> Body {
    for line in patch.lines() {
        if HUNK.is_match(line) {
            return Body::Lines;
        }
        if (line.starts_with("Binary files ") && line.ends_with(" differ"))
            || line == "GIT binary patch"
        {
            return Body::Opaque;
        }
    }
    Body::Unchanged
}

/// `git diff` for one file with no context lines.
fn patch(workspace: &Workspace, args: &[&str], file: &str) -> Result<String> {
    let mut hunk = args.to_vec();
    hunk.extend(["--unified=0", "--", file]);
    let output = workspace.git(&hunk)?;
    ensure!(output.status.success(), "could not read diff for {file}");
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_listing_keeps_modes_and_both_paths_of_a_rename() {
        let zero = "0".repeat(40);
        let id = "a".repeat(40);
        let listing = format!(
            ":100755 100644 {id} {zero} M\0run.sh\0:100644 100644 {id} {id} R096\0old.rs\0new.rs\0:160000 160000 {id} {zero} M\0sub\0"
        );
        let entries = raw_entries(listing.as_bytes());
        let summary: Vec<(&str, &str, &str, Vec<&str>)> = entries
            .iter()
            .map(|(raw, paths)| {
                (
                    raw.old_mode.as_str(),
                    raw.new_mode.as_str(),
                    raw.status.as_str(),
                    paths.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("100755", "100644", "M", vec!["run.sh"]),
                ("100644", "100644", "R096", vec!["old.rs", "new.rs"]),
                ("160000", "160000", "M", vec!["sub"]),
            ]
        );
    }

    #[test]
    fn a_patch_without_hunks_is_binary_or_only_a_mode() {
        assert!(body("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n") == Body::Lines);
        assert!(
            body("diff --git a/g b/g\nindex 1..2 100644\nBinary files a/g and b/g differ\n")
                == Body::Opaque
        );
        assert!(body("diff --git a/x b/x\nold mode 100755\nnew mode 100644\n") == Body::Unchanged);
        assert!(body("") == Body::Unchanged);
    }
}
