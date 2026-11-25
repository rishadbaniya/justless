use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::sync::LazyLock;

use anyhow::{Context, Result, ensure};
use regex::Regex;

use crate::capture;
use crate::index::{Definition, Index, Removed, is_call, nul_paths};
use crate::rust_syntax::Kind;
use crate::seen::{Memory, Symbol};
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

fn format_span_list(spans: &BTreeSet<(usize, usize)>) -> String {
    spans
        .iter()
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn format_line_spans(file: &str, spans: &BTreeSet<(usize, usize)>) -> String {
    format!("{}:{}", display_path(file), format_span_list(spans))
}

fn format_same_file_spans(spans: &BTreeSet<(usize, usize)>) -> String {
    format!(":{}", format_span_list(spans))
}

struct QuerySection {
    lines: Vec<String>,
    empty: Option<&'static str>,
    singular: &'static str,
    plural: &'static str,
}

impl QuerySection {
    fn clip_note(&self, shown: usize) -> Option<String> {
        let missing = self.lines.len().saturating_sub(shown);
        (missing > 0).then(|| {
            let label = if missing == 1 {
                self.singular
            } else {
                self.plural
            };
            format!("  … +{missing} more {label}")
        })
    }

    fn body_bytes(&self, shown: usize) -> usize {
        self.lines[..shown].iter().map(|line| line.len() + 1).sum()
    }

    fn rendered_bytes(&self, shown: usize) -> usize {
        if self.lines.is_empty() {
            return self.empty.map_or(0, |line| line.len() + 1);
        }
        let note = self.clip_note(shown).map_or(0, |line| line.len() + 1);
        self.body_bytes(shown) + note
    }
}

fn total_query_bytes(header: &str, sections: &[QuerySection], shown: &[usize]) -> usize {
    header.len()
        + 1
        + sections
            .iter()
            .zip(shown)
            .map(|(section, &count)| section.rendered_bytes(count))
            .sum::<usize>()
}

/// Allocates `cap` across non-empty query sections so no single hub section
/// starves the others: Pass 1 gives each non-empty section up to an equal byte
/// share, and Pass 2 water-fills any leftover bytes across incomplete sections.
fn fit_query_sections(header: &str, sections: &[QuerySection], cap: usize) -> Vec<usize> {
    let usable = cap.saturating_sub(Report::CLIPPED.len());
    let all: Vec<usize> = sections.iter().map(|section| section.lines.len()).collect();
    if total_query_bytes(header, sections, &all) <= usable {
        return all;
    }
    let mut shown = vec![0usize; sections.len()];
    let non_empty: Vec<usize> = sections
        .iter()
        .enumerate()
        .filter_map(|(i, section)| (!section.lines.is_empty()).then_some(i))
        .collect();
    if non_empty.is_empty() {
        return shown;
    }
    let fixed: usize = header.len()
        + 1
        + sections
            .iter()
            .filter(|section| section.lines.is_empty())
            .map(|section| section.rendered_bytes(0))
            .sum::<usize>();
    let fair_share = usable.saturating_sub(fixed) / non_empty.len();
    let can_grow = |i: usize, shown: &[usize], limit: Option<usize>| -> bool {
        if shown[i] >= sections[i].lines.len() {
            return false;
        }
        let before = sections[i].rendered_bytes(shown[i]);
        let after = sections[i].rendered_bytes(shown[i] + 1);
        if let Some(limit) = limit
            && after > limit
        {
            return false;
        }
        let next_total = total_query_bytes(header, sections, shown) - before + after;
        next_total <= usable
    };
    // Pass 1: fair byte share per non-empty section.
    loop {
        let mut progress = false;
        for &i in &non_empty {
            if can_grow(i, &shown, Some(fair_share)) {
                shown[i] += 1;
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    // Pass 2: surplus water-fill, preferring whichever incomplete section has
    // shown the fewest body bytes so far.
    loop {
        let next = non_empty
            .iter()
            .copied()
            .filter(|&i| can_grow(i, &shown, None))
            .min_by_key(|&i| (sections[i].body_bytes(shown[i]), shown[i], i));
        let Some(i) = next else {
            break;
        };
        shown[i] += 1;
    }
    shown
}

pub fn query(index: &Index, name: &str, view: &str, cap: usize) -> Result<()> {
    query_report(index, name, view, cap)?.finish()
}

fn query_report(index: &Index, name: &str, view: &str, cap: usize) -> Result<Report> {
    let id = index.select(name)?;
    let mut report = Report::new(cap);
    let header = describe(&index.definitions[id]);
    if view == "definition" {
        report.line(&header);
        return Ok(report);
    }
    let mut sections = Vec::new();
    if matches!(view, "references" | "callers" | "all") {
        type IncomingKey<'a> = (Option<usize>, &'static str, bool, &'a str);
        let mut groups: Vec<(IncomingKey<'_>, BTreeSet<(usize, usize)>)> = Vec::new();
        let mut by_key: BTreeMap<IncomingKey<'_>, usize> = BTreeMap::new();
        for edge in index
            .incoming(id)
            .filter(|edge| view != "callers" || is_call(edge.kind))
        {
            let key = (
                edge.from,
                relationship(edge.kind),
                edge.inferred,
                edge.location.file.as_str(),
            );
            let slot = *by_key.entry(key).or_insert_with(|| {
                let pos = groups.len();
                groups.push((key, BTreeSet::new()));
                pos
            });
            groups[slot]
                .1
                .insert((edge.location.start, edge.location.end));
        }
        let lines = groups
            .into_iter()
            .map(|((from, kind, inferred, file), spans)| {
                let owner = from
                    .map(|owner_id| index.definitions[owner_id].qualified.as_str())
                    .unwrap_or("file scope");
                let confidence = if inferred { "possible " } else { "" };
                format!(
                    "  {confidence}{kind} from {owner} at {}",
                    format_line_spans(file, &spans)
                )
            })
            .collect();
        let (singular, plural) = if view == "callers" {
            ("caller", "callers")
        } else {
            ("reference", "references")
        };
        sections.push(QuerySection {
            lines,
            empty: Some("  No matching references found."),
            singular,
            plural,
        });
    }
    if matches!(view, "dependencies" | "all") {
        type OutgoingKey<'a> = (usize, &'static str, bool, &'a str);
        let mut groups: Vec<(OutgoingKey<'_>, BTreeSet<(usize, usize)>)> = Vec::new();
        let mut by_key: BTreeMap<OutgoingKey<'_>, usize> = BTreeMap::new();
        for edge in index.outgoing(id) {
            let key = (
                edge.to,
                relationship(edge.kind),
                edge.inferred,
                edge.location.file.as_str(),
            );
            let slot = *by_key.entry(key).or_insert_with(|| {
                let pos = groups.len();
                groups.push((key, BTreeSet::new()));
                pos
            });
            groups[slot]
                .1
                .insert((edge.location.start, edge.location.end));
        }
        let def_file = index.definitions[id].location.file.as_str();
        let lines = groups
            .into_iter()
            .map(|((to, kind, inferred, file), spans)| {
                let confidence = if inferred { "possible " } else { "" };
                let origin = if file == def_file {
                    format_same_file_spans(&spans)
                } else {
                    format_line_spans(file, &spans)
                };
                format!(
                    "  {confidence}{kind} → {} (from {origin})",
                    describe(&index.definitions[to]),
                )
            })
            .collect();
        sections.push(QuerySection {
            lines,
            empty: None,
            singular: "dependency",
            plural: "dependencies",
        });
    }
    if matches!(view, "tests" | "all") {
        let lines = index
            .related_tests(id, 16)
            .into_iter()
            .map(|(test_id, hops, inferred)| {
                let confidence = if inferred { "possible " } else { "" };
                format!(
                    "  {confidence}related test ({hops} call hops): {}",
                    describe(&index.definitions[test_id])
                )
            })
            .collect();
        sections.push(QuerySection {
            lines,
            empty: Some("  No related tests found in indexed calls."),
            singular: "related test",
            plural: "related tests",
        });
    }
    let shown = fit_query_sections(&header, &sections, cap);
    report.line(&header);
    for (section, &count) in sections.iter().zip(&shown) {
        if report.clipped {
            break;
        }
        if section.lines.is_empty() {
            if let Some(empty) = section.empty {
                report.line(empty);
            }
            continue;
        }
        for line in &section.lines[..count] {
            report.line(line);
            if report.clipped {
                break;
            }
        }
        if !report.clipped
            && let Some(note) = section.clip_note(count)
        {
            report.line(note);
        }
    }
    Ok(report)
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
    code_with_indent(workspace, file, from, to, cap, MIN_TRIM_INDENT)
}

fn code_with_indent(
    workspace: &Workspace,
    file: &str,
    from: usize,
    to: usize,
    cap: usize,
    min_indent: usize,
) -> Result<()> {
    ensure!(
        from > 0 && from <= to,
        "expected a positive, ordered line range"
    );
    let lines = ensure_line(workspace, file, from)?;
    let shown = to.min(lines);
    let mut report = Report::new(cap);
    report.line(format!("{}:{from}-{shown}", display_path(file)));
    snippets_trimmed(workspace, file, &[(from, shown)], min_indent, &mut report)?;
    if shown < to && !report.clipped {
        report.line(format!("  (file ends at line {lines})"));
    }
    report.finish()
}

/// Minimum shared leading indentation width (in columns: `' '` = 1, `'\t'` = 4)
/// trimmed from displayed code snippets in `context` / `code` reports.
const MIN_TRIM_INDENT: usize = 4;
/// Minimum shared leading indentation width for plain `context --at`/`--symbol`
/// views that pair with `seen::Memory::show` (preserves parity on 1-level 4-space
/// function bodies while still trimming deeply nested blocks).
const SESSION_MIN_TRIM_INDENT: usize = 8;

/// Leading ASCII indentation slice (`' '` and `'\t'`) of `line`.
fn leading_indent(line: &str) -> &str {
    let end = line
        .bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    &line[..end]
}

/// Visual column width of an ASCII indentation slice (`' '` = 1, `'\t'` = 4).
fn indent_width(ws: &str) -> usize {
    ws.bytes()
        .map(|byte| if byte == b'\t' { 4 } else { 1 })
        .sum()
}

/// Longest common leading ASCII indentation byte length across all non-empty
/// lines when its visual width is at least `min_width`, or `0`.
fn shared_indent_strip(lines: &[&str], min_width: usize) -> usize {
    let mut shared: Option<&[u8]> = None;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let ws = leading_indent(line).as_bytes();
        shared = Some(match shared {
            None => ws,
            Some(current) => {
                let common = current
                    .iter()
                    .zip(ws)
                    .take_while(|(left, right)| left == right)
                    .count();
                &current[..common]
            }
        });
    }
    let Some(prefix) = shared else {
        return 0;
    };
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    if indent_width(prefix_str) >= min_width {
        prefix.len()
    } else {
        0
    }
}

fn strip_leading_indent(line: &str, strip: usize) -> &str {
    if strip == 0 {
        return line;
    }
    let drop = strip.min(leading_indent(line).len());
    &line[drop..]
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
    snippets_trimmed(workspace, file, &[(from, to)], MIN_TRIM_INDENT, report)
}

fn snippets_trimmed(
    workspace: &Workspace,
    file: &str,
    windows: &[(usize, usize)],
    min_indent: usize,
    report: &mut Report,
) -> Result<()> {
    if windows.is_empty() {
        return Ok(());
    }
    let text = fs::read_to_string(workspace.source_path(file)?)?;
    let lines: Vec<&str> = text.lines().collect();
    let digest = capture::source_digest(text.as_bytes());
    for (index, &(from, to)) in windows.iter().enumerate() {
        if report.clipped {
            break;
        }
        if index > 0 {
            report.line("  …");
            if report.clipped {
                break;
            }
        }
        let before = report.left();
        let mut last = None;
        let window_lines: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .skip(from.saturating_sub(1))
            .take(to.saturating_sub(from) + 1)
            .map(|(offset, &content)| (offset + 1, content))
            .collect();
        let raw: Vec<&str> = window_lines.iter().map(|&(_, content)| content).collect();
        let strip = shared_indent_strip(&raw, min_indent);
        for (line_no, content) in window_lines {
            if report.clipped {
                break;
            }
            let shown = strip_leading_indent(content, strip);
            report.line(format!("  {line_no} | {shown}"));
            last = Some(line_no);
        }
        if let Some(last) = last {
            let printed = before.saturating_sub(report.left()) as u64;
            crate::history::saw_expand(workspace, file, from, last, digest.clone(), printed);
        }
    }
    Ok(())
}

pub fn at(
    workspace: &Workspace,
    index: &Index,
    input: &str,
    cap: usize,
    memory: Option<&mut Memory>,
) -> Result<()> {
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
        memory,
    )?;
    report.finish()
}

/// The symbol's source. In a session, a symbol read before prints only the
/// lines that changed since, or one line when none did.
pub fn symbol(
    workspace: &Workspace,
    index: &Index,
    name: &str,
    cap: usize,
    memory: Option<&mut Memory>,
) -> Result<()> {
    let id = index.select(name)?;
    let definition = &index.definitions[id];
    let location = &definition.location;
    let Some(memory) = memory else {
        return code_with_indent(
            workspace,
            &location.file,
            location.start,
            location.end,
            cap,
            SESSION_MIN_TRIM_INDENT,
        );
    };
    let text = fs::read_to_string(workspace.source_path(&location.file)?)?;
    if location.start == 0 || location.start > text.lines().count() {
        return code_with_indent(
            workspace,
            &location.file,
            location.start,
            location.end,
            cap,
            SESSION_MIN_TRIM_INDENT,
        );
    }
    let mut report = Report::new(cap);
    memory.show(
        workspace,
        &text,
        &Symbol {
            file: &location.file,
            name: &seen_name(index, id),
            start: location.start,
            end: location.end,
        },
        &overlapping_names(index, id),
        (location.start, location.end),
        true,
        &mut report,
    );
    report.finish()
}

/// The name a session remembers a definition by: its qualified name, numbered
/// from the second definition of that name in the same file, such as a
/// cfg-gated twin, so one twin's view is never compared with another's.
fn seen_name(index: &Index, id: usize) -> String {
    let definition = &index.definitions[id];
    let earlier = index
        .definitions
        .iter()
        .filter(|other| {
            other.qualified == definition.qualified
                && other.location.file == definition.location.file
                && other.location.start < definition.location.start
        })
        .count();
    if earlier == 0 {
        definition.qualified.clone()
    } else {
        format!("{}#{}", definition.qualified, earlier + 1)
    }
}

/// The names a session remembers the definitions of `file` by that share any
/// of lines `from..=to`, such as a method and the block around it.
pub(crate) fn seen_names(index: &Index, file: &str, from: usize, to: usize) -> Vec<String> {
    index
        .definitions
        .iter()
        .enumerate()
        .filter(|(_, other)| {
            other.location.file == file && other.location.start <= to && from <= other.location.end
        })
        .map(|(id, _)| seen_name(index, id))
        .collect()
}

/// The other definitions that share lines with definition `id`.
fn overlapping_names(index: &Index, id: usize) -> Vec<String> {
    let location = &index.definitions[id].location;
    let own = seen_name(index, id);
    let mut names = seen_names(index, &location.file, location.start, location.end);
    names.retain(|name| *name != own);
    names
}

/// Distinct log lines listed for one source location in `context --run`.
const MAX_DIAGNOSTICS_PER_LOCATION: usize = 4;

enum Resolution {
    /// One enclosing symbol: the innermost non-file/non-module member containing
    /// the location, or the first file/module definition when no members overlap.
    Single {
        best: usize,
        overlapping: Vec<usize>,
    },
    /// A range spanning multiple non-file/non-module symbols.
    Multiple { members: Vec<usize> },
    /// No indexed symbol overlaps the location.
    None,
}

fn resolve_location(index: &Index, location: &Location) -> Resolution {
    let definitions = index.at(location);
    let members: Vec<usize> = definitions
        .iter()
        .copied()
        .filter(|&id| !matches!(index.definitions[id].kind.as_str(), "file" | "module"))
        .collect();
    if let Some(best) = members.iter().copied().find(|&id| {
        let span = &index.definitions[id].location;
        span.start <= location.start && location.end <= span.end
    }) {
        return Resolution::Single {
            best,
            overlapping: members,
        };
    }
    if !members.is_empty() {
        return Resolution::Multiple { members };
    }
    match definitions.first().copied() {
        Some(best) => Resolution::Single {
            best,
            overlapping: definitions,
        },
        None => Resolution::None,
    }
}

fn snippet_window(definition: &Definition, location: &Location) -> (usize, usize) {
    let from = location
        .start
        .saturating_sub(3)
        .max(definition.location.start);
    let to = location
        .end
        .max(location.start.saturating_add(3))
        .min(definition.location.end)
        .min(from.saturating_add(14));
    (from, to.max(from))
}

fn merge_windows(mut windows: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    windows.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (from, to) in windows {
        match merged.last_mut() {
            Some((_, last_to)) if from <= last_to.saturating_add(1) => {
                *last_to = (*last_to).max(to);
            }
            _ => merged.push((from, to)),
        }
    }
    merged
}

fn emit_diagnostics(
    directory: &std::path::Path,
    justless: &str,
    id: &str,
    location: &Location,
    diagnostics: &[&capture::Diagnostic],
    report: &mut Report,
) -> Result<()> {
    let shown = diagnostics.len().min(MAX_DIAGNOSTICS_PER_LOCATION);
    for diagnostic in &diagnostics[..shown] {
        if report.clipped {
            break;
        }
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
        for line in capture::diagnostic_excerpt(directory, diagnostic)? {
            report.line(line);
        }
    }
    if diagnostics.len() > shown && !report.clipped {
        report.line(format!(
            "  +{} more log line(s) at this location. {justless} search {id} {}",
            diagnostics.len() - shown,
            crate::workspace::shell_text(&location.file)
        ));
    }
    Ok(())
}

pub fn from_run(workspace: &Workspace, index: &Index, id: &str, cap: usize) -> Result<()> {
    let (directory, text, source) = from_run_report(workspace, index, id, cap)?;
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

fn from_run_report(
    workspace: &Workspace,
    index: &Index,
    id: &str,
    cap: usize,
) -> Result<(std::path::PathBuf, String, usize)> {
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
    // Group non-stale locations sharing one enclosing symbol so the symbol's
    // metadata prints once and overlapping ±3-line snippets merge cleanly.
    enum RunEntry<'a> {
        Symbol {
            best: usize,
            overlapping: Vec<usize>,
            items: Vec<(&'a Location, Vec<&'a capture::Diagnostic>)>,
        },
        Standalone {
            location: &'a Location,
            diagnostics: Vec<&'a capture::Diagnostic>,
            stale: bool,
        },
    }
    let mut entries: Vec<RunEntry<'_>> = Vec::new();
    let mut by_symbol: BTreeMap<usize, usize> = BTreeMap::new();
    for (location, diagnostics) in groups {
        let stale = run
            .source_digests
            .get(&location.file)
            .is_some_and(|digest| index.files.get(&location.file) != Some(digest));
        if !stale
            && let Resolution::Single { best, overlapping } = resolve_location(index, location)
        {
            if let Some(&slot) = by_symbol.get(&best) {
                if let RunEntry::Symbol {
                    overlapping: existing,
                    items,
                    ..
                } = &mut entries[slot]
                {
                    if location.end > location.start {
                        for id in overlapping {
                            if !existing.contains(&id) {
                                existing.push(id);
                            }
                        }
                    }
                    items.push((location, diagnostics));
                }
            } else {
                let slot = entries.len();
                let keep_overlap = if location.end > location.start {
                    overlapping
                } else {
                    Vec::new()
                };
                entries.push(RunEntry::Symbol {
                    best,
                    overlapping: keep_overlap,
                    items: vec![(location, diagnostics)],
                });
                by_symbol.insert(best, slot);
            }
        } else {
            entries.push(RunEntry::Standalone {
                location,
                diagnostics,
                stale,
            });
        }
    }
    for entry in entries {
        if report.clipped {
            break;
        }
        match entry {
            RunEntry::Symbol {
                best,
                overlapping,
                items,
            } => {
                for (location, diagnostics) in &items {
                    emit_diagnostics(
                        &directory,
                        &justless,
                        id,
                        location,
                        diagnostics,
                        &mut report,
                    )?;
                    if report.clipped {
                        break;
                    }
                }
                if !report.clipped {
                    let locations: Vec<Location> =
                        items.iter().map(|(loc, _)| (*loc).clone()).collect();
                    let room = report.left();
                    detail_multi(
                        workspace,
                        index,
                        best,
                        &locations,
                        &overlapping,
                        &mut report,
                        SnippetView::Report,
                    )?;
                    source += room.saturating_sub(report.left());
                }
            }
            RunEntry::Standalone {
                location,
                diagnostics,
                stale,
            } => {
                emit_diagnostics(
                    &directory,
                    &justless,
                    id,
                    location,
                    &diagnostics,
                    &mut report,
                )?;
                if report.clipped {
                    break;
                }
                if stale {
                    report.line("  Source changed or disappeared after this run. Re-run the command for current locations.");
                } else {
                    let room = report.left();
                    at_location(workspace, index, location, &mut report, None)?;
                    source += room.saturating_sub(report.left());
                }
            }
        }
    }
    Ok((directory, report.into_string(), source))
}

fn at_location(
    workspace: &Workspace,
    index: &Index,
    location: &Location,
    report: &mut Report,
    mut memory: Option<&mut Memory>,
) -> Result<()> {
    match resolve_location(index, location) {
        Resolution::Single { best, overlapping } => detail(
            workspace,
            index,
            best,
            location,
            &overlapping,
            report,
            memory,
        ),
        Resolution::Multiple { members } => {
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
                detail(
                    workspace,
                    index,
                    id,
                    &part,
                    &inner,
                    report,
                    memory.as_deref_mut(),
                )?;
            }
            Ok(())
        }
        Resolution::None => {
            report.line(format!("{} · no enclosing indexed symbol", location));
            if let Some(error) = index.errors.get(&location.file) {
                report.line(format!("  Parse error: {error}"));
            }
            snippet(
                workspace,
                &location.file,
                location.start.saturating_sub(2).max(1),
                location.end.min(location.start.saturating_add(8)),
                report,
            )
        }
    }
}

enum SnippetView<'a> {
    /// `context --at` with an active session (`seen::Memory::show` renders deltas).
    Session(&'a mut Memory),
    /// `context --at` without a session: trims deep shared indent (`>= 8` cols)
    /// so single-level 4-space windows match `seen::Memory::show`.
    AtPlain,
    /// `context --run` and `context --changes`: trims shared indent (`>= 4` cols).
    Report,
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
    memory: Option<&mut Memory>,
) -> Result<()> {
    let keep = if location.end > location.start {
        overlapping
    } else {
        &[]
    };
    let view = match memory {
        Some(mem) => SnippetView::Session(mem),
        None => SnippetView::AtPlain,
    };
    detail_multi(
        workspace,
        index,
        best,
        std::slice::from_ref(location),
        keep,
        report,
        view,
    )
}

/// One symbol around one or more locations: prints its metadata once and merges
/// overlapping ±3-line source windows into disjoint sorted windows.
fn detail_multi(
    workspace: &Workspace,
    index: &Index,
    best: usize,
    locations: &[Location],
    overlapping: &[usize],
    report: &mut Report,
    view: SnippetView<'_>,
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
        let distance = locations
            .iter()
            .map(|location| edge.location.start.abs_diff(location.start))
            .min()
            .unwrap_or(usize::MAX);
        (edge.inferred, distance, edge.location.start)
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
            let origin = if edge.location.file == definition.location.file {
                if edge.location.start == edge.location.end {
                    format!(":{}", edge.location.start)
                } else {
                    format!(":{}-{}", edge.location.start, edge.location.end)
                }
            } else {
                edge.location.to_string()
            };
            report.line(format!(
                "  {confidence}{description} {} (from {origin})",
                describe(&index.definitions[edge.to]),
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
    if !overlapping.is_empty() {
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
    let windows = merge_windows(
        locations
            .iter()
            .map(|location| snippet_window(definition, location))
            .collect(),
    );
    let file = locations
        .first()
        .map_or(definition.location.file.as_str(), |loc| loc.file.as_str());
    match (view, windows.as_slice()) {
        (SnippetView::Session(memory), &[(from, to)]) => {
            let text = fs::read_to_string(workspace.source_path(file)?)?;
            memory.show(
                workspace,
                &text,
                &Symbol {
                    file,
                    name: &seen_name(index, best),
                    start: definition.location.start,
                    end: definition.location.end,
                },
                &overlapping_names(index, best),
                (from, to),
                false,
                report,
            );
        }
        (SnippetView::AtPlain | SnippetView::Session(_), _) => {
            snippets_trimmed(workspace, file, &windows, SESSION_MIN_TRIM_INDENT, report)?
        }
        (SnippetView::Report, _) => {
            snippets_trimmed(workspace, file, &windows, MIN_TRIM_INDENT, report)?
        }
    }
    if let Some(&(first_from, _)) = windows.first()
        && definition.declaration < first_from
        && !report.clipped
    {
        report.line(format!(
            "  Declaration starts at {}:{}",
            display_path(file),
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
    changed_report(workspace, index, since, cap)?.finish()
}

fn changed_report(
    workspace: &Workspace,
    index: &Index,
    since: &Since,
    cap: usize,
) -> Result<Report> {
    let mut report = Report::new(cap);
    let changes = changes(workspace, index, since)?;
    let mut deferred: BTreeMap<(String, usize), Vec<Location>> = BTreeMap::new();
    for (pos, (file, change)) in changes.iter().enumerate() {
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
                // If this file also has changed ranges right after, defer symbol
                // detail for any surviving symbol touched by both so its metadata
                // prints once and its snippets merge.
                let next_range_symbols: BTreeSet<usize> = changes
                    .get(pos + 1)
                    .and_then(|(next_file, next_change)| {
                        (next_file == file)
                            .then_some(next_change)
                            .and_then(|change| match change {
                                Change::Ranges(locations) => Some(locations),
                                _ => None,
                            })
                    })
                    .into_iter()
                    .flatten()
                    .filter_map(|&(start, end)| {
                        let location = Location {
                            file: file.clone(),
                            start,
                            end,
                        };
                        match resolve_location(index, &location) {
                            Resolution::Single { best, .. } => Some(best),
                            _ => None,
                        }
                    })
                    .collect();
                emit_deletions(
                    workspace,
                    index,
                    file,
                    deletions,
                    &next_range_symbols,
                    &mut deferred,
                    &mut report,
                )?;
            }
            Change::Ranges(locations) => {
                report.line(format!("{name} · {} changed ranges", locations.len()));
                emit_changed_ranges(
                    workspace,
                    index,
                    file,
                    locations,
                    &mut deferred,
                    &mut report,
                )?;
            }
        }
        if report.clipped {
            break;
        }
    }
    if changes.is_empty() {
        report.line(since.nothing_changed(workspace));
    }
    Ok(report)
}

fn surviving_symbol(index: &Index, file: &str, deletion: &Deletion) -> Option<(usize, Location)> {
    if deletion.after == 0 {
        return None;
    }
    // The lines on both sides of the removal belong to one symbol: it changed.
    let location = Location {
        file: file.to_owned(),
        start: deletion.after,
        end: deletion.after + 1,
    };
    let id = index
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
        })?;
    Some((id, location))
}

fn emit_deletion_header(file: &str, deletion: &Deletion, report: &mut Report) {
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
    let shown: Vec<&str> = deletion
        .lines
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect();
    let strip = shared_indent_strip(&shown, MIN_TRIM_INDENT);
    for (offset, line) in shown.into_iter().enumerate() {
        let trimmed = strip_leading_indent(line, strip);
        report.line(format!("  -{} | {trimmed}", first + offset));
    }
    if deletion.lines.len() > SHOWN {
        report.line(format!(
            "  … and {} more deleted lines",
            deletion.lines.len() - SHOWN
        ));
    }
}

/// Deletions in one file for `context --changes`, grouped by surviving enclosing
/// symbol so its metadata prints once and overlapping snippets merge.
fn emit_deletions(
    workspace: &Workspace,
    index: &Index,
    file: &str,
    deletions: &[Deletion],
    deferred_symbols: &BTreeSet<usize>,
    deferred: &mut BTreeMap<(String, usize), Vec<Location>>,
    report: &mut Report,
) -> Result<()> {
    enum Entry<'a> {
        HeaderOnly(&'a Deletion),
        Symbol {
            id: usize,
            items: Vec<(&'a Deletion, Location)>,
        },
    }
    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut by_symbol: BTreeMap<usize, usize> = BTreeMap::new();
    for deletion in deletions {
        match surviving_symbol(index, file, deletion) {
            Some((id, location)) if deferred_symbols.contains(&id) => {
                deferred
                    .entry((file.to_owned(), id))
                    .or_default()
                    .push(location);
                entries.push(Entry::HeaderOnly(deletion));
            }
            Some((id, location)) => {
                if let Some(&slot) = by_symbol.get(&id) {
                    if let Entry::Symbol { items, .. } = &mut entries[slot] {
                        items.push((deletion, location));
                    }
                } else {
                    let slot = entries.len();
                    entries.push(Entry::Symbol {
                        id,
                        items: vec![(deletion, location)],
                    });
                    by_symbol.insert(id, slot);
                }
            }
            None => entries.push(Entry::HeaderOnly(deletion)),
        }
    }
    for entry in entries {
        if report.clipped {
            break;
        }
        match entry {
            Entry::HeaderOnly(deletion) => emit_deletion_header(file, deletion, report),
            Entry::Symbol { id, items } => {
                for (deletion, _) in &items {
                    emit_deletion_header(file, deletion, report);
                    if report.clipped {
                        break;
                    }
                }
                if !report.clipped {
                    let locations: Vec<Location> =
                        items.into_iter().map(|(_, location)| location).collect();
                    detail_multi(
                        workspace,
                        index,
                        id,
                        &locations,
                        &[],
                        report,
                        SnippetView::Report,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Changed ranges in one file for `context --changes`, grouped by enclosing
/// symbol so each symbol's metadata prints once and overlapping snippets merge.
fn emit_changed_ranges(
    workspace: &Workspace,
    index: &Index,
    file: &str,
    locations: &[(usize, usize)],
    deferred: &mut BTreeMap<(String, usize), Vec<Location>>,
    report: &mut Report,
) -> Result<()> {
    enum RangeEntry {
        Symbol {
            best: usize,
            ranges: Vec<(usize, usize)>,
            locations: Vec<Location>,
            overlapping: Vec<usize>,
        },
        Standalone(Location),
    }
    let name = display_path(file);
    let mut entries: Vec<RangeEntry> = Vec::new();
    let mut by_symbol: BTreeMap<usize, usize> = BTreeMap::new();
    for &(start, end) in locations {
        let location = Location {
            file: file.to_owned(),
            start,
            end,
        };
        match resolve_location(index, &location) {
            Resolution::Single { best, overlapping } => {
                if let Some(&slot) = by_symbol.get(&best) {
                    if let RangeEntry::Symbol {
                        ranges,
                        locations: group_locs,
                        overlapping: existing,
                        ..
                    } = &mut entries[slot]
                    {
                        ranges.push((start, end));
                        group_locs.push(location);
                        if end > start {
                            for id in overlapping {
                                if !existing.contains(&id) {
                                    existing.push(id);
                                }
                            }
                        }
                    }
                } else {
                    let slot = entries.len();
                    let mut group_locs = deferred
                        .remove(&(file.to_owned(), best))
                        .unwrap_or_default();
                    group_locs.push(location);
                    let keep_overlap = if end > start { overlapping } else { Vec::new() };
                    entries.push(RangeEntry::Symbol {
                        best,
                        ranges: vec![(start, end)],
                        locations: group_locs,
                        overlapping: keep_overlap,
                    });
                    by_symbol.insert(best, slot);
                }
            }
            Resolution::Multiple { .. } | Resolution::None => {
                entries.push(RangeEntry::Standalone(location));
            }
        }
    }
    for entry in entries {
        if report.clipped {
            break;
        }
        match entry {
            RangeEntry::Symbol {
                best,
                ranges,
                locations,
                overlapping,
            } => {
                let list: Vec<String> = ranges
                    .iter()
                    .map(|&(start, end)| format!("{start}-{end}"))
                    .collect();
                report.line(format!("Changed {name}:{}", list.join(", ")));
                if !report.clipped {
                    detail_multi(
                        workspace,
                        index,
                        best,
                        &locations,
                        &overlapping,
                        report,
                        SnippetView::Report,
                    )?;
                }
            }
            RangeEntry::Standalone(location) => {
                report.line(format!(
                    "Changed {name}:{}-{}",
                    location.start, location.end
                ));
                if !report.clipped {
                    at_location(workspace, index, &location, report, None)?;
                }
            }
        }
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

    fn temp_workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        for (rel, body) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, body).unwrap();
        }
        let workspace = Workspace::discover(Some(root)).unwrap();
        (dir, workspace)
    }

    fn init_git(workspace: &Workspace) {
        for args in [
            &["init", "-q"][..],
            &["config", "user.name", "Test"][..],
            &["config", "user.email", "test@example.com"][..],
            &["add", "."][..],
            &["commit", "-qm", "init"][..],
        ] {
            let out = workspace.git(args).unwrap();
            assert!(out.status.success(), "{args:?}: {:?}", out.stderr);
        }
    }

    #[test]
    fn query_groups_duplicate_edges_and_saves_bytes() {
        let manifest = "[package]\nname = \"hubdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let lib =
            "pub mod callers;\npub mod single;\n\npub fn core_fn(x: u32) -> u32 {\n    x + 1\n}\n";
        let mut callers = String::from(
            "use crate::core_fn;\n\npub fn heavy_caller() -> u32 {\n    let mut total = 0;\n",
        );
        for i in 0..8 {
            callers.push_str(&format!("    total += core_fn({i});\n"));
        }
        callers.push_str("    total\n}\n");
        let single = "use crate::core_fn;\n\npub fn single_caller() -> u32 {\n    core_fn(42)\n}\n";
        let (_dir, workspace) = temp_workspace(&[
            ("Cargo.toml", manifest),
            ("src/lib.rs", lib),
            ("src/callers.rs", &callers),
            ("src/single.rs", single),
        ]);
        let index = Index::open(&workspace).unwrap();

        let refs = query_report(&index, "core_fn", "references", 8000)
            .unwrap()
            .into_string();
        assert!(
            refs.contains(
                "  call from hubdemo::callers::heavy_caller at src/callers.rs:5,6,7,8,9,10,11,12\n"
            ),
            "{refs}"
        );
        assert_eq!(
            refs.matches("hubdemo::callers::heavy_caller").count(),
            1,
            "{refs}"
        );
        // Single-hit caller preserves the natural `file:line` format.
        assert!(
            refs.contains("  call from hubdemo::single::single_caller at src/single.rs:4\n"),
            "{refs}"
        );

        let deps = query_report(&index, "heavy_caller", "dependencies", 8000)
            .unwrap()
            .into_string();
        assert!(
            deps.contains("  call → hubdemo::core_fn [function] at src/lib.rs:4-6 (from :5,6,7,8,9,10,11,12)\n"),
            "{deps}"
        );
        assert!(
            !deps.contains("(from src/callers.rs:"),
            "same-file origin should omit redundant defining path:\n{deps}"
        );
        assert_eq!(
            deps.matches("hubdemo::core_fn [function]").count(),
            1,
            "{deps}"
        );

        // Measure byte reduction against ungrouped lines for the 8 repeated call sites.
        let ungrouped_refs: usize = (5..=12)
            .map(|line| {
                format!("  call from hubdemo::callers::heavy_caller at src/callers.rs:{line}\n")
                    .len()
            })
            .sum();
        let grouped_ref_line = refs
            .lines()
            .find(|line| line.contains("heavy_caller"))
            .unwrap()
            .len()
            + 1;
        assert!(
            grouped_ref_line * 4 < ungrouped_refs,
            "expected >75% reduction on 8-site caller: grouped={grouped_ref_line} vs ungrouped={ungrouped_refs}"
        );
    }

    #[test]
    fn query_all_view_balances_hub_references_dependencies_and_tests() {
        let manifest = "[package]\nname = \"balance\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let mut src = String::from(
            "pub fn dep_alpha(x: u32) -> u32 { x + 1 }\n\
             pub fn dep_beta(x: u32) -> u32 { x + 2 }\n\
             pub fn dep_gamma(x: u32) -> u32 { x + 3 }\n\
             pub fn hub_symbol(x: u32) -> u32 {\n\
                 dep_alpha(x) + dep_beta(x) + dep_gamma(x)\n\
             }\n",
        );
        for i in 0..30 {
            src.push_str(&format!(
                "pub fn consumer_{i:02}(x: u32) -> u32 {{ hub_symbol(x + {i}) }}\n"
            ));
        }
        src.push_str(
            "#[cfg(test)]\nmod tests {\n\
                 use super::*;\n\
                 #[test]\nfn test_hub_first() { assert_eq!(hub_symbol(1), 9); }\n\
                 #[test]\nfn test_hub_second() { assert_eq!(hub_symbol(2), 12); }\n\
             }\n",
        );
        let (_dir, workspace) = temp_workspace(&[("Cargo.toml", manifest), ("src/lib.rs", &src)]);
        let index = Index::open(&workspace).unwrap();

        let cap = 1200;
        let out = query_report(&index, "hub_symbol", "all", cap)
            .unwrap()
            .into_string();
        assert!(out.len() <= cap, "len={} > {cap}\n{out}", out.len());
        assert!(out.contains("dep_alpha"), "dependencies starved:\n{out}");
        assert!(out.contains("dep_beta"), "dependencies starved:\n{out}");
        assert!(out.contains("dep_gamma"), "dependencies starved:\n{out}");
        assert!(out.contains("test_hub_first"), "tests starved:\n{out}");
        assert!(out.contains("test_hub_second"), "tests starved:\n{out}");

        // Exact clipped count for references: shown + omitted == total incoming groups.
        let shown_refs = out
            .lines()
            .filter(|line| line.starts_with("  call from "))
            .count();
        let clip_line = out
            .lines()
            .find(|line| line.contains("… +") && line.contains("more reference"))
            .unwrap_or_else(|| panic!("expected clipped references note:\n{out}"));
        let omitted: usize = clip_line
            .split("… +")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        // 30 consumers + 2 tests call hub_symbol directly.
        assert_eq!(shown_refs + omitted, 32, "{out}");

        // Single view `--view callers` also reports exact `… +N more callers`.
        let callers = query_report(&index, "hub_symbol", "callers", 500)
            .unwrap()
            .into_string();
        assert!(callers.len() <= 500, "{callers}");
        assert!(callers.contains("more caller"), "{callers}");
    }

    #[test]
    fn changed_deduplicates_enclosing_symbol_and_merges_snippets() {
        let manifest = "[package]\nname = \"chdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let mut lines = vec![
            "pub fn helper(v: u32) -> u32 { v * 2 }".to_owned(),
            String::new(),
            "pub fn process_order(seed: u32) -> u32 {".to_owned(),
            "    let mut acc = helper(seed);".to_owned(),
        ];
        for i in 1..=36 {
            lines.push(format!("    acc += {i}; // step_{i:02}"));
        }
        lines.push("    acc".to_owned());
        lines.push("}".to_owned());
        lines.push("#[cfg(test)]".to_owned());
        lines.push("mod tests {".to_owned());
        lines.push("    #[test]".to_owned());
        lines.push(
            "    fn test_process_order() { assert!(super::process_order(1) > 0); }".to_owned(),
        );
        lines.push("}".to_owned());
        let original = format!("{}\n", lines.join("\n"));

        let (_dir, workspace) = temp_workspace(&[
            ("Cargo.toml", manifest),
            (".gitignore", "/.justless/\n"),
            ("src/lib.rs", &original),
        ]);
        init_git(&workspace);

        // Make 3 edits inside `process_order`: two overlapping ±3 windows (step_10, step_13)
        // and one distant window (step_34), plus a pure deletion at step_22.
        let edited = original
            .replace("acc += 10; // step_10", "acc += 100; // step_10")
            .replace("acc += 13; // step_13", "acc += 130; // step_13")
            .replace("    acc += 22; // step_22\n", "")
            .replace("acc += 34; // step_34", "acc += 340; // step_34");
        fs::write(workspace.root.join("src/lib.rs"), &edited).unwrap();

        let index = Index::open(&workspace).unwrap();
        let out = changed_report(&workspace, &index, &Since::Head, 12_000)
            .unwrap()
            .into_string();

        // Symbol header, calls, related test, and Declaration line appear ONCE.
        assert_eq!(
            out.matches("chdemo::process_order [function]").count(),
            1,
            "{out}"
        );
        assert_eq!(out.matches("  calls chdemo::helper").count(), 1, "{out}");
        assert_eq!(
            out.matches("  related test: chdemo::tests::test_process_order")
                .count(),
            1,
            "{out}"
        );
        assert_eq!(
            out.matches("  Declaration starts at src/lib.rs:3").count(),
            1,
            "{out}"
        );
        // Overlapping hunks merge cleanly without repeating lines in the overlap.
        assert_eq!(out.matches("step_11").count(), 1, "{out}");
        assert_eq!(out.matches("step_12").count(), 1, "{out}");
        assert!(out.contains("step_10"), "{out}");
        assert!(out.contains("step_13"), "{out}");
        assert!(out.contains("step_34"), "{out}");
        assert!(out.contains("\n  …\n"), "expected window separator:\n{out}");
        // Compare against ungrouped baseline (calling detail once per hunk + deletion).
        let mut ungrouped = Report::new(12_000);
        for line in [14usize, 17, 25, 37] {
            let loc = Location {
                file: "src/lib.rs".into(),
                start: line,
                end: line,
            };
            at_location(&workspace, &index, &loc, &mut ungrouped, None).unwrap();
        }
        let ungrouped_len = ungrouped.into_string().len();
        assert!(
            out.len() * 10 < ungrouped_len * 7,
            "expected >30% byte reduction: grouped={} vs ungrouped={ungrouped_len}\n{out}",
            out.len()
        );
    }

    #[test]
    fn from_run_deduplicates_enclosing_symbol_and_merges_snippets() {
        let manifest = "[package]\nname = \"rundemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let mut lines = vec![
            "pub fn helper(v: u32) -> u32 { v * 2 }".to_owned(),
            String::new(),
            "pub fn compute(seed: u32) -> u32 {".to_owned(),
            "    let mut acc = helper(seed);".to_owned(),
        ];
        for i in 1..=36 {
            lines.push(format!("    acc += {i}; // line_{i:02}"));
        }
        lines.push("    acc".to_owned());
        lines.push("}".to_owned());
        lines.push("#[cfg(test)]".to_owned());
        lines.push("mod tests {".to_owned());
        lines.push("    #[test]".to_owned());
        lines.push("    fn test_compute() { assert!(super::compute(1) > 0); }".to_owned());
        lines.push("}".to_owned());
        let src = format!("{}\n", lines.join("\n"));

        let (_dir, workspace) = temp_workspace(&[("Cargo.toml", manifest), ("src/lib.rs", &src)]);
        let index = Index::open(&workspace).unwrap();

        // Create a real archived run with 3 diagnostics inside `compute`:
        // lines 14 & 17 (overlapping ±3 windows) and line 38 (disjoint window).
        let run_dir = workspace.state.join("runs/run-multi01");
        fs::create_dir_all(&run_dir).unwrap();
        let stderr = "src/lib.rs:14:5: error[E0308]: mismatch A\n\
                      src/lib.rs:17:5: error[E0308]: mismatch B\n\
                      src/lib.rs:38:5: error[E0308]: mismatch C\n";
        fs::write(run_dir.join("stdout"), "").unwrap();
        fs::write(run_dir.join("stderr"), stderr).unwrap();
        let digest = index.files["src/lib.rs"].clone();
        let record = serde_json::json!({
            "id": "run-multi01",
            "status": 1,
            "raw_bytes": stderr.len(),
            "preview_bytes": stderr.len(),
            "diagnostics": [
                {"location": {"file": "src/lib.rs", "start": 14, "end": 14}, "stream": "stderr", "log_line": 1},
                {"location": {"file": "src/lib.rs", "start": 17, "end": 17}, "stream": "stderr", "log_line": 2},
                {"location": {"file": "src/lib.rs", "start": 38, "end": 38}, "stream": "stderr", "log_line": 3}
            ],
            "source_digests": {"src/lib.rs": digest}
        });
        fs::write(run_dir.join("record.json"), record.to_string()).unwrap();

        let (_, out, source_bytes) =
            from_run_report(&workspace, &index, "run-multi01", 12_000).unwrap();
        // Every diagnostic header & Replay command is preserved.
        assert!(out.contains("src/lib.rs:14 · log stderr:1"), "{out}");
        assert!(out.contains("src/lib.rs:17 · log stderr:2"), "{out}");
        assert!(out.contains("src/lib.rs:38 · log stderr:3"), "{out}");
        // Symbol metadata prints ONCE.
        assert_eq!(
            out.matches("rundemo::compute [function]").count(),
            1,
            "{out}"
        );
        assert_eq!(out.matches("  calls rundemo::helper").count(), 1, "{out}");
        assert_eq!(
            out.matches("  related test: rundemo::tests::test_compute")
                .count(),
            1,
            "{out}"
        );
        assert_eq!(
            out.matches("  Declaration starts at src/lib.rs:3").count(),
            1,
            "{out}"
        );
        // Overlapping ±3 lines (14..=17) print ONCE; disjoint window is separated by `  …`.
        assert_eq!(out.matches("line_11").count(), 1, "{out}");
        assert_eq!(out.matches("line_12").count(), 1, "{out}");
        assert!(out.contains("\n  …\n"), "{out}");
        assert!(
            source_bytes > 0 && source_bytes < out.len(),
            "source={source_bytes}, len={}",
            out.len()
        );

        // Compare source section size against 3 ungrouped `at_location` calls.
        let mut ungrouped = Report::new(12_000);
        for line in [14usize, 17, 38] {
            let loc = Location {
                file: "src/lib.rs".into(),
                start: line,
                end: line,
            };
            at_location(&workspace, &index, &loc, &mut ungrouped, None).unwrap();
        }
        let ungrouped_source = ungrouped.into_string().len();
        assert!(
            source_bytes * 10 < ungrouped_source * 6,
            "expected >40% source byte reduction: grouped={source_bytes} vs ungrouped={ungrouped_source}\n{out}"
        );
    }

    #[test]
    fn shared_indent_trimming_preserves_line_numbers_and_relative_indent() {
        let lines_4 = [
            "    if cond {",
            "        work();",
            "",
            "        done();",
            "    }",
        ];
        assert_eq!(shared_indent_strip(&lines_4, MIN_TRIM_INDENT), 4);
        assert_eq!(strip_leading_indent(lines_4[0], 4), "if cond {");
        assert_eq!(strip_leading_indent(lines_4[1], 4), "    work();");
        assert_eq!(strip_leading_indent(lines_4[2], 4), "");

        // Fewer than 4 leading spaces stay untouched.
        let lines_2 = ["  let a = 1;", "    let b = 2;"];
        assert_eq!(shared_indent_strip(&lines_2, MIN_TRIM_INDENT), 0);

        // Tab indentation (1 tab = 4 visual columns) trims cleanly while keeping relative tabs.
        let lines_tab = ["\t\tif ok {", "\t\t\treturn nil", "\t\t}"];
        assert_eq!(shared_indent_strip(&lines_tab, MIN_TRIM_INDENT), 2);
        assert_eq!(strip_leading_indent(lines_tab[0], 2), "if ok {");
        assert_eq!(strip_leading_indent(lines_tab[1], 2), "\treturn nil");

        // End-to-end `snippets_trimmed` trims shared indent on deeply nested code
        // while keeping exact line numbers and relative indentation.
        let src = concat!(
            "pub mod outer {\n",
            "    pub mod inner {\n",
            "        pub fn run(x: i32) -> i32 {\n",
            "            if x > 0 {\n",
            "                let y = x * 2;\n",
            "\n",
            "                y + 1\n",
            "            } else {\n",
            "                0\n",
            "            }\n",
            "        }\n",
            "    }\n",
            "}\n",
        );
        let (_dir, workspace) = temp_workspace(&[
            (
                "Cargo.toml",
                "[package]\nname = \"indentctx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs", src),
        ]);
        let mut trimmed = Report::new(8000);
        snippets_trimmed(
            &workspace,
            "src/lib.rs",
            &[(4, 10)],
            MIN_TRIM_INDENT,
            &mut trimmed,
        )
        .unwrap();
        let shown = trimmed.into_string();
        assert!(shown.contains("  4 | if x > 0 {\n"), "{shown}");
        assert!(shown.contains("  5 |     let y = x * 2;\n"), "{shown}");
        assert!(shown.contains("  6 | \n"), "{shown}");
        assert!(shown.contains("  7 |     y + 1\n"), "{shown}");
        assert!(shown.contains("  8 | } else {\n"), "{shown}");
        assert!(shown.contains("  9 |     0\n"), "{shown}");
        assert!(shown.contains("  10 | }\n"), "{shown}");
        assert!(!shown.contains("  4 |             if"), "{shown}");

        // `context --at` also trims deeply nested (8+ space) blocks while keeping
        // single-level 4-space bodies aligned with `seen::Memory::show`.
        let index = Index::open(&workspace).unwrap();
        let mut at_report = Report::new(8000);
        at_location(
            &workspace,
            &index,
            &Location {
                file: "src/lib.rs".into(),
                start: 7,
                end: 7,
            },
            &mut at_report,
            None,
        )
        .unwrap();
        let at_out = at_report.into_string();
        assert!(at_out.contains("  4 | if x > 0 {\n"), "{at_out}");
        assert!(at_out.contains("  5 |     let y = x * 2;\n"), "{at_out}");
    }

    #[test]
    fn query_report_dedups_same_file_origin_and_keeps_cross_file_origin() {
        let manifest =
            "[package]\nname = \"origindemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let lib = concat!(
            "pub mod sub;\n\n",
            "pub fn leaf(x: u32) -> u32 {\n",
            "    x + 1\n",
            "}\n",
        );
        let sub = concat!(
            "use crate::leaf;\n\n",
            "pub fn caller(x: u32) -> u32 {\n",
            "    let a = leaf(x);\n",
            "    let b = leaf(a);\n",
            "    b\n",
            "}\n",
        );
        let (_dir, workspace) = temp_workspace(&[
            ("Cargo.toml", manifest),
            ("src/lib.rs", lib),
            ("src/deep/nested/sub.rs", sub),
        ]);
        // Wire `#[path = "deep/nested/sub.rs"] pub mod sub;` to test long path savings.
        fs::write(
            workspace.root.join("src/lib.rs"),
            format!("#[path = \"deep/nested/sub.rs\"]\n{lib}"),
        )
        .unwrap();
        let index = Index::open(&workspace).unwrap();
        let deps = query_report(&index, "caller", "dependencies", 8000)
            .unwrap()
            .into_string();
        assert!(deps.contains("at src/deep/nested/sub.rs:3-7\n"), "{deps}");
        assert!(
            deps.contains("  call → origindemo::sub::leaf")
                || deps.contains(
                    "  call → origindemo::leaf [function] at src/lib.rs:4-6 (from :4,5)\n"
                ),
            "{deps}"
        );
        assert!(
            !deps.contains("(from src/deep/nested/sub.rs:"),
            "same-file origin must not repeat long path:\n{deps}"
        );
    }
}
