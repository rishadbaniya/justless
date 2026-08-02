use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::sync::LazyLock;

use anyhow::{Context, Result, ensure};
use regex::Regex;

use crate::capture;
use crate::index::{Definition, Fingerprint, Index, is_call, nul_paths};
use crate::rust_syntax::Kind;
use crate::workspace::{Location, Workspace};

static HUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@").expect("Git hunk pattern")
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

    pub(crate) fn finish(mut self) -> Result<()> {
        if self.clipped {
            self.output.push_str(Self::CLIPPED);
        }
        io::stdout().lock().write_all(self.output.as_bytes())?;
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
                let start = line.ceil_char_boundary(position.saturating_sub(80));
                let end = line.floor_char_boundary(
                    (position + term.len()).saturating_add(180).min(line.len()),
                );
                report.line(format!("{file}:{}: {}", offset + 1, &line[start..end]));
                matches += 1;
                if matches == limit || report.clipped {
                    break 'files;
                }
            }
        }
    }
    if matches == 0 {
        report.line("No matching indexed text.");
    }
    report.finish()
}

pub fn code(workspace: &Workspace, file: &str, from: usize, to: usize, cap: usize) -> Result<()> {
    ensure!(
        from > 0 && from <= to,
        "expected a positive, ordered line range"
    );
    let mut report = Report::new(cap);
    report.line(format!("{file}:{from}-{to}"));
    snippet(workspace, file, from, to, &mut report)?;
    report.finish()
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
    workspace.source_path(file)?;
    let mut report = Report::new(cap);
    at_location(
        workspace,
        index,
        &Location {
            file: file.to_owned(),
            start,
            end,
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
    let mut seen = BTreeSet::new();
    for diagnostic in &run.diagnostics {
        let location = &diagnostic.location;
        if !seen.insert(location) {
            continue;
        }
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
        for &id in definitions.iter().skip(1).take(4) {
            if index.definitions[id].location != definition.location {
                report.line(format!("  overlaps {}", describe(&index.definitions[id])));
            }
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
            location.file, definition.declaration
        ));
    }
    Ok(())
}

pub fn changed(workspace: &Workspace, index: &Index, staged: bool, cap: usize) -> Result<()> {
    let mut report = Report::new(cap);
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
        String::from_utf8_lossy(&output.stderr).trim()
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
            String::from_utf8_lossy(&output.stderr).trim()
        );
        new_files.extend(nul_paths(&output.stdout).map(str::to_owned));
        if !head.status.success() {
            let output = workspace.git(&["diff", "--cached", "--name-only", "-z", "--", "."])?;
            ensure!(output.status.success(), "could not list staged changes");
            new_files.extend(nul_paths(&output.stdout).map(str::to_owned));
        }
        tracked.retain(|file| !new_files.contains(file));
    }
    let mut files = 0;
    for (file, untracked) in tracked
        .iter()
        .map(|file| (file, false))
        .chain(new_files.iter().map(|file| (file, true)))
    {
        files += 1;
        if !index.files.contains_key(file) {
            report.line(format!("{file} · no current indexed source"));
            continue;
        }
        if staged {
            let difference = workspace.git(&["diff", "--quiet", "--", file])?;
            if !difference.status.success() {
                report.line(format!("{file} · working tree differs from staged source. Use justless context --changes to inspect the current file."));
                continue;
            }
        }
        let text = fs::read_to_string(workspace.source_path(file)?)?;
        let file_lines = text.lines().count();
        if file_lines == 0 {
            report.line(format!("{file} · empty after changes"));
            continue;
        }
        let locations = if untracked {
            vec![(1, file_lines)]
        } else {
            let mut hunk = args.clone();
            hunk.extend(["--unified=0", "--", file]);
            let output = workspace.git(&hunk)?;
            ensure!(output.status.success(), "could not read diff for {file}");
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| {
                    let matches = HUNK.captures(line)?;
                    let start: usize = matches[1].parse().ok()?;
                    let count = matches
                        .get(2)
                        .and_then(|value| value.as_str().parse::<usize>().ok())
                        .unwrap_or(1);
                    let start = start.max(1).min(file_lines);
                    Some((
                        start,
                        start
                            .saturating_add(count.saturating_sub(1))
                            .min(file_lines),
                    ))
                })
                .collect()
        };
        report.line(format!("{file} · {} changed ranges", locations.len()));
        for (start, end) in locations {
            report.line(format!("Changed {file}:{start}-{end}"));
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
        if report.clipped {
            break;
        }
    }
    if files == 0 {
        report.line("No changed files.");
    }
    report.finish()
}
