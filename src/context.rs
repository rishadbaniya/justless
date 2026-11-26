use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result, ensure};
use regex::Regex;

use crate::capture;
use crate::index::{Definition, Edge, Index, Removed, is_call, nul_paths};
use crate::rust_syntax::Kind;
use crate::seen::{Memory, Symbol};
use crate::workspace::{Location, Workspace, display_path};

static HUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("Git hunk pattern")
});

/// Cached file text, O(1) 1-based line lookup, and lazily-computed digests for
/// a single `context` / `pack` invocation so multi-hit files are never read or
/// hashed from disk more than once.
pub(crate) struct CachedSource {
    pub(crate) text: String,
    line_starts: Vec<usize>,
    digest: Option<String>,
    revision: Option<String>,
}

impl CachedSource {
    fn new(text: String) -> Self {
        let line_starts = build_line_starts(&text);
        Self {
            text,
            line_starts,
            digest: None,
            revision: None,
        }
    }

    pub(crate) fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// 1-based line slice matching `str::lines()` semantics (strips `\n` and `\r\n`).
    pub(crate) fn line(&self, line_no: usize) -> Option<&str> {
        let idx = line_no.checked_sub(1)?;
        let start = *self.line_starts.get(idx)?;
        let end = self
            .line_starts
            .get(idx + 1)
            .copied()
            .unwrap_or(self.text.len());
        let raw = &self.text[start..end];
        Some(match raw.strip_suffix('\n') {
            Some(stripped) => stripped.strip_suffix('\r').unwrap_or(stripped),
            None => raw,
        })
    }

    pub(crate) fn digest(&mut self) -> &str {
        let text = &self.text;
        self.digest
            .get_or_insert_with(|| capture::source_digest(text.as_bytes()))
            .as_str()
    }

    pub(crate) fn revision(&mut self, file: &str) -> &str {
        let text = &self.text;
        self.revision
            .get_or_insert_with(|| crate::source::revision_for(file, text.as_bytes()))
            .as_str()
    }
}

fn build_line_starts(text: &str) -> Vec<usize> {
    if text.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut starts = Vec::with_capacity(bytes.len() / 40 + 1);
    starts.push(0);
    for (i, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' && i + 1 < bytes.len() {
            starts.push(i + 1);
        }
    }
    starts
}

#[derive(Default)]
pub(crate) struct SourceCache {
    entries: HashMap<String, Option<CachedSource>>,
    reads: usize,
}

impl SourceCache {
    pub(crate) fn get_or_read(
        &mut self,
        workspace: &Workspace,
        file: &str,
    ) -> Result<&mut CachedSource> {
        if !matches!(self.entries.get(file), Some(Some(_))) {
            let path = workspace.source_path(file)?;
            self.reads += 1;
            let text = fs::read_to_string(path)?;
            self.entries
                .insert(file.to_owned(), Some(CachedSource::new(text)));
        }
        Ok(self
            .entries
            .get_mut(file)
            .and_then(Option::as_mut)
            .expect("cached source just inserted"))
    }

    pub(crate) fn get_or_try_read(
        &mut self,
        workspace: &Workspace,
        file: &str,
    ) -> Option<&mut CachedSource> {
        if !self.entries.contains_key(file) {
            let loaded = workspace.source_path(file).ok().and_then(|path| {
                self.reads += 1;
                fs::read_to_string(path).ok().map(CachedSource::new)
            });
            self.entries.insert(file.to_owned(), loaded);
        }
        self.entries.get_mut(file).and_then(Option::as_mut)
    }

    #[cfg(test)]
    pub(crate) fn reads(&self) -> usize {
        self.reads
    }
}

pub(crate) struct Report {
    output: String,
    cap: usize,
    pub(crate) clipped: bool,
}

impl Report {
    const CLIPPED: &str = "[context budget reached]\n";

    pub(crate) fn new(cap: usize) -> Self {
        Self {
            output: String::with_capacity(cap.min(8192)),
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

/// Longest common directory prefix (ending in `/`) shared by all `paths`.
pub(crate) fn common_dir_prefix(paths: &[&str]) -> String {
    fn dir_of(path: &str) -> &str {
        path.rfind('/').map_or("", |i| &path[..=i])
    }
    let Some((first, rest)) = paths.split_first() else {
        return String::new();
    };
    let mut common = dir_of(first);
    for path in rest {
        let dir = dir_of(path);
        let shared = common
            .bytes()
            .zip(dir.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        let cut = common[..common.floor_char_boundary(shared)]
            .rfind('/')
            .map_or(0, |i| i + 1);
        common = &common[..cut];
        if common.is_empty() {
            break;
        }
    }
    common.to_owned()
}

/// Returns a hoistable directory prefix (ending in `/`) when `paths` share a
/// deep directory prefix (`>= 3` segments, or `>= 2` segments across at least
/// 3 path occurrences), no path needs control-char quoting, and emitting
/// `{line_prefix}{prefix}\n` saves at least 8 bytes over repeating `prefix`.
pub(crate) fn hoistable_dir_prefix(paths: &[&str], line_prefix: &str) -> Option<String> {
    if paths.len() < 2 {
        return None;
    }
    if paths.iter().any(|path| display_path(path) != *path) {
        return None;
    }
    let common = common_dir_prefix(paths);
    let depth = common
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .count();
    if depth < 2 || (depth == 2 && paths.len() < 3) {
        return None;
    }
    let base_line_bytes = line_prefix.len() + common.len() + 1;
    let repeated_bytes = paths.len() * common.len();
    (repeated_bytes >= base_line_bytes + 8).then_some(common)
}

pub(crate) fn display_with_base<'a>(
    file: &'a str,
    base: Option<&str>,
) -> std::borrow::Cow<'a, str> {
    if let Some(prefix) = base
        && let Some(rel) = file.strip_prefix(prefix)
    {
        return display_path(rel);
    }
    display_path(file)
}

pub(crate) fn format_location_with_base(location: &Location, base: Option<&str>) -> String {
    let file = display_with_base(&location.file, base);
    if location.start == location.end {
        format!("{file}:{}", location.start)
    } else {
        format!("{file}:{}-{}", location.start, location.end)
    }
}

fn describe(definition: &Definition) -> String {
    describe_with_base(definition, None)
}

fn describe_with_base(definition: &Definition, base: Option<&str>) -> String {
    let kind = if definition.test {
        "test"
    } else {
        &definition.kind
    };
    format!(
        "{} [{kind}] at {}",
        definition.qualified,
        format_location_with_base(&definition.location, base)
    )
}

fn format_span_list(spans: &BTreeSet<(usize, usize)>) -> String {
    let mut out = String::with_capacity(spans.len() * 4);
    for (i, &(start, end)) in spans.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        use std::fmt::Write as _;
        if start == end {
            let _ = write!(out, "{start}");
        } else {
            let _ = write!(out, "{start}-{end}");
        }
    }
    out
}

pub(crate) fn format_line_spans_with_base(
    file: &str,
    spans: &BTreeSet<(usize, usize)>,
    base: Option<&str>,
) -> String {
    format!(
        "{}:{}",
        display_with_base(file, base),
        format_span_list(spans)
    )
}

pub(crate) fn format_same_file_spans(spans: &BTreeSet<(usize, usize)>) -> String {
    format!(":{}", format_span_list(spans))
}

struct QuerySection {
    lines: Vec<String>,
    /// Cumulative byte lengths (including `\n`) for `lines[..shown]` so
    /// `body_bytes(shown)` is O(1) without resumming inside budget loops.
    prefix_bytes: Vec<usize>,
    empty: Option<&'static str>,
    singular: &'static str,
    plural: &'static str,
}

impl QuerySection {
    fn new(
        lines: Vec<String>,
        empty: Option<&'static str>,
        singular: &'static str,
        plural: &'static str,
    ) -> Self {
        let mut prefix_bytes = Vec::with_capacity(lines.len() + 1);
        prefix_bytes.push(0);
        let mut total = 0usize;
        for line in &lines {
            total += line.len() + 1;
            prefix_bytes.push(total);
        }
        Self {
            lines,
            prefix_bytes,
            empty,
            singular,
            plural,
        }
    }

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

    fn clip_note_bytes(&self, shown: usize) -> usize {
        let missing = self.lines.len().saturating_sub(shown);
        if missing == 0 {
            return 0;
        }
        let label = if missing == 1 {
            self.singular
        } else {
            self.plural
        };
        let digits = (missing.ilog10() as usize) + 1;
        // `"  … +"` (7 UTF-8 bytes) + digits + `" more "` (6) + label + `'\n'` (1)
        "  … +".len() + digits + " more ".len() + label.len() + 1
    }

    fn body_bytes(&self, shown: usize) -> usize {
        self.prefix_bytes[shown]
    }

    fn rendered_bytes(&self, shown: usize) -> usize {
        if self.lines.is_empty() {
            return self.empty.map_or(0, |line| line.len() + 1);
        }
        self.body_bytes(shown) + self.clip_note_bytes(shown)
    }
}

fn total_query_bytes(header_bytes: usize, sections: &[QuerySection], shown: &[usize]) -> usize {
    header_bytes
        + sections
            .iter()
            .zip(shown)
            .map(|(section, &count)| section.rendered_bytes(count))
            .sum::<usize>()
}

/// Allocates `cap` across non-empty query sections so no single hub section
/// starves the others: Pass 1 gives each non-empty section up to an equal byte
/// share, and Pass 2 water-fills any leftover bytes across incomplete sections.
fn fit_query_sections(header_bytes: usize, sections: &[QuerySection], cap: usize) -> Vec<usize> {
    let usable = cap.saturating_sub(Report::CLIPPED.len());
    let all: Vec<usize> = sections.iter().map(|section| section.lines.len()).collect();
    if total_query_bytes(header_bytes, sections, &all) <= usable {
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
    let fixed: usize = header_bytes
        + sections
            .iter()
            .filter(|section| section.lines.is_empty())
            .map(|section| section.rendered_bytes(0))
            .sum::<usize>();
    let fair_share = usable.saturating_sub(fixed) / non_empty.len();
    let mut current_total = total_query_bytes(header_bytes, sections, &shown);
    let can_grow =
        |i: usize, shown: &[usize], current: usize, limit: Option<usize>| -> Option<usize> {
            if shown[i] >= sections[i].lines.len() {
                return None;
            }
            let before = sections[i].rendered_bytes(shown[i]);
            let after = sections[i].rendered_bytes(shown[i] + 1);
            if let Some(limit) = limit
                && after > limit
            {
                return None;
            }
            let next_total = current - before + after;
            (next_total <= usable).then_some(next_total)
        };
    // Pass 1: fair byte share per non-empty section.
    loop {
        let mut progress = false;
        for &i in &non_empty {
            if let Some(next_total) = can_grow(i, &shown, current_total, Some(fair_share)) {
                shown[i] += 1;
                current_total = next_total;
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
            .filter_map(|i| {
                can_grow(i, &shown, current_total, None).map(|next_total| (i, next_total))
            })
            .min_by_key(|&(i, _)| (sections[i].body_bytes(shown[i]), shown[i], i));
        let Some((i, next_total)) = next else {
            break;
        };
        shown[i] += 1;
        current_total = next_total;
    }
    shown
}

/// Resolves the [`Workspace`] behind an [`Index`] via its recorded invocation
/// prefix (`index.justless`) so `query` and `path` can record code-graph
/// telemetry without changing their public signatures.
fn workspace_for_index(index: &Index) -> Option<Workspace> {
    let words = shlex::split(&index.justless)?;
    match words.as_slice() {
        [cmd] if cmd == "justless" => Workspace::discover(None).ok(),
        [cmd, flag, root] if cmd == "justless" && flag == "--root" => {
            Workspace::discover(Some(Path::new(root))).ok()
        }
        _ => None,
    }
}

pub fn query(index: &Index, name: &str, view: &str, cap: usize) -> Result<()> {
    let (report, files) = query_report_with_files(index, name, view, cap)?;
    let text = report.into_string();
    io::stdout().lock().write_all(text.as_bytes())?;
    if let Some(workspace) = workspace_for_index(index) {
        crate::history::record_graph_files(
            &workspace,
            "query",
            text.len() as u64,
            files.iter().map(String::as_str),
        );
    }
    Ok(())
}

#[cfg(test)]
fn query_report(index: &Index, name: &str, view: &str, cap: usize) -> Result<Report> {
    query_report_with_files(index, name, view, cap).map(|(report, _)| report)
}

fn query_report_with_files(
    index: &Index,
    name: &str,
    view: &str,
    cap: usize,
) -> Result<(Report, BTreeSet<String>)> {
    let id = index.select(name)?;
    let mut report = Report::new(cap);
    let mut files = BTreeSet::new();
    files.insert(index.definitions[id].location.file.clone());
    let header = describe(&index.definitions[id]);
    if view == "definition" {
        report.line(&header);
        return Ok((report, files));
    }
    type IncomingKey<'a> = (Option<usize>, &'static str, bool, &'a str);
    type OutgoingKey<'a> = (usize, &'static str, bool, &'a str);

    let incoming_groups = if matches!(view, "references" | "callers" | "all") {
        let mut groups: Vec<(IncomingKey<'_>, BTreeSet<(usize, usize)>)> = Vec::new();
        let mut by_key: HashMap<IncomingKey<'_>, usize> = HashMap::new();
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
        Some(groups)
    } else {
        None
    };

    let def_file = index.definitions[id].location.file.as_str();
    let outgoing_groups = if matches!(view, "dependencies" | "all") {
        let mut groups: Vec<(OutgoingKey<'_>, BTreeSet<(usize, usize)>)> = Vec::new();
        let mut by_key: HashMap<OutgoingKey<'_>, usize> = HashMap::new();
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
        Some(groups)
    } else {
        None
    };

    let related_tests = if matches!(view, "tests" | "all") {
        Some(index.related_tests(id, 16))
    } else {
        None
    };

    // Collect every rendered file path occurrence inside the body sections so we
    // can hoist a deep `>= 2`-segment shared directory prefix when strictly smaller.
    let mut section_paths: Vec<&str> = Vec::new();
    if let Some(groups) = &incoming_groups {
        for &((_, _, _, file), _) in groups {
            section_paths.push(file);
        }
    }
    if let Some(groups) = &outgoing_groups {
        for &((to, _, _, file), _) in groups {
            section_paths.push(index.definitions[to].location.file.as_str());
            if file != def_file {
                section_paths.push(file);
            }
        }
    }
    if let Some(tests) = &related_tests {
        for &(test_id, _, _) in tests {
            section_paths.push(index.definitions[test_id].location.file.as_str());
        }
    }

    let build_sections = |base: Option<&str>| -> Vec<QuerySection> {
        let mut sections = Vec::with_capacity(3);
        if let Some(groups) = &incoming_groups {
            let lines = groups
                .iter()
                .map(|((from, kind, inferred, file), spans)| {
                    let owner = from
                        .map(|owner_id| index.definitions[owner_id].qualified.as_str())
                        .unwrap_or("file scope");
                    let confidence = if *inferred { "possible " } else { "" };
                    format!(
                        "  {confidence}{kind} from {owner} at {}",
                        format_line_spans_with_base(file, spans, base)
                    )
                })
                .collect();
            let (singular, plural) = if view == "callers" {
                ("caller", "callers")
            } else {
                ("reference", "references")
            };
            sections.push(QuerySection::new(
                lines,
                Some("  No matching references found."),
                singular,
                plural,
            ));
        }
        if let Some(groups) = &outgoing_groups {
            let lines = groups
                .iter()
                .map(|((to, kind, inferred, file), spans)| {
                    let confidence = if *inferred { "possible " } else { "" };
                    let origin = if *file == def_file {
                        format_same_file_spans(spans)
                    } else {
                        format_line_spans_with_base(file, spans, base)
                    };
                    format!(
                        "  {confidence}{kind} → {} (from {origin})",
                        describe_with_base(&index.definitions[*to], base),
                    )
                })
                .collect();
            sections.push(QuerySection::new(lines, None, "dependency", "dependencies"));
        }
        if let Some(tests) = &related_tests {
            let lines = tests
                .iter()
                .map(|&(test_id, hops, inferred)| {
                    let confidence = if inferred { "possible " } else { "" };
                    format!(
                        "  {confidence}related test ({hops} call hops): {}",
                        describe_with_base(&index.definitions[test_id], base)
                    )
                })
                .collect();
            sections.push(QuerySection::new(
                lines,
                Some("  No related tests found in indexed calls."),
                "related test",
                "related tests",
            ));
        }
        sections
    };

    let mut base_line: Option<String> = None;
    let mut sections = build_sections(None);
    let header_bytes = header.len() + 1;
    let mut shown = fit_query_sections(header_bytes, &sections, cap);

    if let Some(prefix) = hoistable_dir_prefix(&section_paths, "  base: ") {
        let candidate_base = format!("  base: {prefix}");
        let candidate_sections = build_sections(Some(&prefix));
        let candidate_header_bytes = header.len() + 1 + candidate_base.len() + 1;
        let candidate_shown = fit_query_sections(candidate_header_bytes, &candidate_sections, cap);
        let plain_total = total_query_bytes(header_bytes, &sections, &shown);
        let hoisted_total = total_query_bytes(
            candidate_header_bytes,
            &candidate_sections,
            &candidate_shown,
        );
        let plain_items: usize = shown.iter().sum();
        let hoisted_items: usize = candidate_shown.iter().sum();
        if hoisted_items > plain_items
            || (hoisted_items == plain_items && hoisted_total < plain_total)
        {
            base_line = Some(candidate_base);
            sections = candidate_sections;
            shown = candidate_shown;
        }
    }

    report.line(&header);
    if let Some(line) = &base_line {
        report.line(line);
    }
    let mut rendered_counts = Vec::with_capacity(sections.len());
    for (section, &count) in sections.iter().zip(&shown) {
        if report.clipped {
            rendered_counts.push(0);
            break;
        }
        if section.lines.is_empty() {
            if let Some(empty) = section.empty {
                report.line(empty);
            }
            rendered_counts.push(0);
            continue;
        }
        let mut emitted = 0usize;
        for line in &section.lines[..count] {
            report.line(line);
            emitted += 1;
            if report.clipped {
                break;
            }
        }
        rendered_counts.push(emitted);
        if !report.clipped
            && let Some(note) = section.clip_note(count)
        {
            report.line(note);
        }
    }
    let mut sec_idx = 0usize;
    if let Some(groups) = &incoming_groups {
        let emitted = rendered_counts.get(sec_idx).copied().unwrap_or(0);
        for &((from, _, _, file), _) in &groups[..emitted] {
            files.insert(file.to_owned());
            if let Some(owner_id) = from {
                files.insert(index.definitions[owner_id].location.file.clone());
            }
        }
        sec_idx += 1;
    }
    if let Some(groups) = &outgoing_groups {
        let emitted = rendered_counts.get(sec_idx).copied().unwrap_or(0);
        for &((to, _, _, file), _) in &groups[..emitted] {
            files.insert(index.definitions[to].location.file.clone());
            files.insert(file.to_owned());
        }
        sec_idx += 1;
    }
    if let Some(tests) = &related_tests {
        let emitted = rendered_counts.get(sec_idx).copied().unwrap_or(0);
        for &(test_id, _, _) in &tests[..emitted] {
            files.insert(index.definitions[test_id].location.file.clone());
        }
    }
    Ok((report, files))
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
    let (report, files) = path_report_with_files(index, from, to, cap)?;
    let text = report.into_string();
    io::stdout().lock().write_all(text.as_bytes())?;
    if let Some(workspace) = workspace_for_index(index) {
        crate::history::record_graph_files(
            &workspace,
            "path",
            text.len() as u64,
            files.iter().map(String::as_str),
        );
    }
    Ok(())
}

#[cfg(test)]
fn path_report(index: &Index, from: &str, to: &str, cap: usize) -> Result<Report> {
    path_report_with_files(index, from, to, cap).map(|(report, _)| report)
}

fn path_report_with_files(
    index: &Index,
    from: &str,
    to: &str,
    cap: usize,
) -> Result<(Report, BTreeSet<String>)> {
    let from = index.select(from)?;
    let to = index.select(to)?;
    let hops = index
        .path(from, to)
        .context("no indexed relationship path found")?;
    let mut hop_paths = Vec::with_capacity(hops.len() * 2);
    for &(edge, reverse) in &hops {
        let other = if reverse { edge.from.unwrap() } else { edge.to };
        hop_paths.push(edge.location.file.as_str());
        hop_paths.push(index.definitions[other].location.file.as_str());
    }
    let base = hoistable_dir_prefix(&hop_paths, "  base: ");
    let mut report = Report::new(cap);
    let mut files = BTreeSet::new();
    files.insert(index.definitions[from].location.file.clone());
    report.line(describe(&index.definitions[from]));
    if let Some(prefix) = &base {
        report.line(format!("  base: {prefix}"));
    }
    for (edge, reverse) in hops {
        let direction = if reverse { "←" } else { "→" };
        let other = if reverse { edge.from.unwrap() } else { edge.to };
        let confidence = if edge.inferred { "possible " } else { "" };
        files.insert(edge.location.file.clone());
        files.insert(index.definitions[other].location.file.clone());
        report.line(format!(
            "  {direction} {confidence}{} at {} → {}",
            relationship(edge.kind),
            format_location_with_base(&edge.location, base.as_deref()),
            describe_with_base(&index.definitions[other], base.as_deref())
        ));
        if report.clipped {
            break;
        }
    }
    Ok((report, files))
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
    let mut cache = SourceCache::default();
    code_with_indent_cached(workspace, &mut cache, file, from, to, cap, min_indent)?.finish()
}

fn code_with_indent_cached(
    workspace: &Workspace,
    cache: &mut SourceCache,
    file: &str,
    from: usize,
    to: usize,
    cap: usize,
    min_indent: usize,
) -> Result<Report> {
    ensure!(
        from > 0 && from <= to,
        "expected a positive, ordered line range"
    );
    let lines = ensure_line_cached(workspace, cache, file, from)?;
    let shown = to.min(lines);
    let mut report = Report::new(cap);
    report.line(format!("{}:{from}-{shown}", display_path(file)));
    snippets_trimmed_cached(
        workspace,
        cache,
        file,
        &[(from, shown)],
        min_indent,
        &mut report,
    )?;
    if shown < to && !report.clipped {
        report.line(format!("  (file ends at line {lines})"));
    }
    Ok(report)
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
fn ensure_line_cached(
    workspace: &Workspace,
    cache: &mut SourceCache,
    file: &str,
    line: usize,
) -> Result<usize> {
    let lines = cache.get_or_read(workspace, file)?.line_count();
    ensure!(
        line <= lines,
        "{} has {lines} lines; line {line} is past the end",
        display_path(file)
    );
    Ok(lines)
}

#[cfg(test)]
fn snippets_trimmed(
    workspace: &Workspace,
    file: &str,
    windows: &[(usize, usize)],
    min_indent: usize,
    report: &mut Report,
) -> Result<()> {
    let mut cache = SourceCache::default();
    snippets_trimmed_cached(workspace, &mut cache, file, windows, min_indent, report)
}

fn snippet_cached(
    workspace: &Workspace,
    cache: &mut SourceCache,
    file: &str,
    from: usize,
    to: usize,
    report: &mut Report,
) -> Result<()> {
    snippets_trimmed_cached(
        workspace,
        cache,
        file,
        &[(from, to)],
        MIN_TRIM_INDENT,
        report,
    )
}

fn snippets_trimmed_cached(
    workspace: &Workspace,
    cache: &mut SourceCache,
    file: &str,
    windows: &[(usize, usize)],
    min_indent: usize,
    report: &mut Report,
) -> Result<()> {
    if windows.is_empty() {
        return Ok(());
    }
    let source = cache.get_or_read(workspace, file)?;
    let digest = source.digest().to_owned();
    let total_lines = source.line_count();
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
        let first = from.max(1);
        let end = to.min(total_lines);
        let count = end.saturating_sub(first) + usize::from(first <= end);
        let mut window_lines: Vec<(usize, &str)> = Vec::with_capacity(count);
        let mut raw: Vec<&str> = Vec::with_capacity(count);
        if first <= end {
            for line_no in first..=end {
                if let Some(content) = source.line(line_no) {
                    window_lines.push((line_no, content));
                    raw.push(content);
                }
            }
        }
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
    let mut cache = SourceCache::default();
    let mut referenced = BTreeSet::new();
    let text = at_report_cached_with_files(
        workspace,
        &mut cache,
        index,
        input,
        cap,
        memory,
        &mut referenced,
    )?
    .into_string();
    io::stdout().lock().write_all(text.as_bytes())?;
    crate::history::record_graph_files(
        workspace,
        "context",
        text.len() as u64,
        referenced.iter().map(String::as_str),
    );
    Ok(())
}

#[cfg(test)]
fn at_report_cached(
    workspace: &Workspace,
    cache: &mut SourceCache,
    index: &Index,
    input: &str,
    cap: usize,
    memory: Option<&mut Memory>,
) -> Result<Report> {
    let mut referenced = BTreeSet::new();
    at_report_cached_with_files(workspace, cache, index, input, cap, memory, &mut referenced)
}

fn at_report_cached_with_files(
    workspace: &Workspace,
    cache: &mut SourceCache,
    index: &Index,
    input: &str,
    cap: usize,
    memory: Option<&mut Memory>,
    referenced: &mut BTreeSet<String>,
) -> Result<Report> {
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
    let lines = ensure_line_cached(workspace, cache, file, start)?;
    let mut report = Report::new(cap);
    at_location_cached(
        workspace,
        cache,
        index,
        &Location {
            file: file.to_owned(),
            start,
            end: end.min(lines),
        },
        None,
        &mut report,
        memory,
        referenced,
    )?;
    Ok(report)
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
    let mut cache = SourceCache::default();
    let report = match memory {
        None => code_with_indent_cached(
            workspace,
            &mut cache,
            &location.file,
            location.start,
            location.end,
            cap,
            SESSION_MIN_TRIM_INDENT,
        )?,
        Some(memory) => {
            let line_count = cache.get_or_read(workspace, &location.file)?.line_count();
            if location.start == 0 || location.start > line_count {
                code_with_indent_cached(
                    workspace,
                    &mut cache,
                    &location.file,
                    location.start,
                    location.end,
                    cap,
                    SESSION_MIN_TRIM_INDENT,
                )?
            } else {
                let text = &cache.get_or_read(workspace, &location.file)?.text;
                let mut report = Report::new(cap);
                memory.show(
                    workspace,
                    text,
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
                report
            }
        }
    };
    let text = report.into_string();
    io::stdout().lock().write_all(text.as_bytes())?;
    crate::history::record_graph_files(
        workspace,
        "context",
        text.len() as u64,
        [location.file.as_str()],
    );
    Ok(())
}

/// The name a session remembers a definition by: its qualified name, numbered
/// from the second definition of that name in the same file, such as a
/// cfg-gated twin, so one twin's view is never compared with another's.
fn seen_name(index: &Index, id: usize) -> String {
    let definition = &index.definitions[id];
    let earlier = if definition.location.start <= 1 {
        0
    } else {
        let before = Location {
            file: definition.location.file.clone(),
            start: 1,
            end: definition.location.start - 1,
        };
        index
            .at(&before)
            .into_iter()
            .filter(|&other_id| {
                let other = &index.definitions[other_id];
                other.qualified == definition.qualified
                    && other.location.start < definition.location.start
            })
            .count()
    };
    if earlier == 0 {
        definition.qualified.clone()
    } else {
        format!("{}#{}", definition.qualified, earlier + 1)
    }
}

/// The names a session remembers the definitions of `file` by that share any
/// of lines `from..=to`, such as a method and the block around it.
pub(crate) fn seen_names(index: &Index, file: &str, from: usize, to: usize) -> Vec<String> {
    let all_in_file = index.at(&Location {
        file: file.to_owned(),
        start: 1,
        end: usize::MAX,
    });
    if all_in_file.is_empty() {
        return Vec::new();
    }
    let mut starts_by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut overlapping: Vec<usize> = Vec::new();
    for &id in &all_in_file {
        let def = &index.definitions[id];
        starts_by_name
            .entry(def.qualified.as_str())
            .or_default()
            .push(def.location.start);
        if def.location.start <= to && from <= def.location.end {
            overlapping.push(id);
        }
    }
    overlapping.sort_unstable();
    overlapping
        .into_iter()
        .map(|id| {
            let def = &index.definitions[id];
            let earlier = starts_by_name
                .get(def.qualified.as_str())
                .map_or(0, |starts| {
                    starts.iter().filter(|&&s| s < def.location.start).count()
                });
            if earlier == 0 {
                def.qualified.clone()
            } else {
                format!("{}#{}", def.qualified, earlier + 1)
            }
        })
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
    base: Option<&str>,
    report: &mut Report,
) -> Result<()> {
    let shown = diagnostics.len().min(MAX_DIAGNOSTICS_PER_LOCATION);
    let display_loc = format_location_with_base(location, base);
    for diagnostic in &diagnostics[..shown] {
        if report.clipped {
            break;
        }
        report.line(format!(
            "{display_loc} · log {}:{}",
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
    let (directory, text, source, referenced) = from_run_report(workspace, index, id, cap)?;
    io::stdout().lock().write_all(text.as_bytes())?;
    // The log lines and notes retrieve the run's output again. The source shown
    // beside them does not.
    crate::history::recall(
        &directory,
        crate::history::Retrieval::Context,
        Some(text.len().saturating_sub(source) as u64),
    );
    crate::history::record_graph_files(
        workspace,
        "context",
        text.len() as u64,
        referenced.iter().map(String::as_str),
    );
    Ok(())
}

fn from_run_report(
    workspace: &Workspace,
    index: &Index,
    id: &str,
    cap: usize,
) -> Result<(std::path::PathBuf, String, usize, BTreeSet<String>)> {
    let (directory, run) = capture::load(workspace, id)?;
    let mut report = Report::new(cap);
    let mut cache = SourceCache::default();
    let mut referenced = BTreeSet::new();
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
    // Group by source location in O(D) while keeping every distinct log line:
    // progress output and the real error often name the same line, and the
    // error must survive.
    let mut groups: Vec<(&Location, Vec<&capture::Diagnostic>)> =
        Vec::with_capacity(run.diagnostics.len());
    let mut by_loc: HashMap<(&str, usize, usize), usize> =
        HashMap::with_capacity(run.diagnostics.len());
    let mut seen_diag: HashSet<(usize, &str, usize)> =
        HashSet::with_capacity(run.diagnostics.len());
    for diagnostic in &run.diagnostics {
        let location = &diagnostic.location;
        let loc_key = (location.file.as_str(), location.start, location.end);
        let slot = *by_loc.entry(loc_key).or_insert_with(|| {
            let pos = groups.len();
            groups.push((location, Vec::new()));
            pos
        });
        if seen_diag.insert((slot, diagnostic.stream.name(), diagnostic.log_line)) {
            groups[slot].1.push(diagnostic);
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
            overlapping_seen: HashSet<usize>,
            items: Vec<(&'a Location, Vec<&'a capture::Diagnostic>)>,
        },
        Standalone {
            location: &'a Location,
            diagnostics: Vec<&'a capture::Diagnostic>,
            stale: bool,
        },
    }
    let mut entries: Vec<RunEntry<'_>> = Vec::with_capacity(groups.len());
    let mut by_symbol: HashMap<usize, usize> = HashMap::new();
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
                    overlapping_seen,
                    items,
                    ..
                } = &mut entries[slot]
                {
                    if location.end > location.start {
                        for id in overlapping {
                            if overlapping_seen.insert(id) {
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
                let overlapping_seen: HashSet<usize> = keep_overlap.iter().copied().collect();
                entries.push(RunEntry::Symbol {
                    best,
                    overlapping: keep_overlap,
                    overlapping_seen,
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
    let mut run_paths: Vec<&str> = Vec::new();
    for entry in &entries {
        match entry {
            RunEntry::Symbol { best, items, .. } => {
                for (loc, _) in items {
                    run_paths.push(loc.file.as_str());
                }
                run_paths.push(index.definitions[*best].location.file.as_str());
            }
            RunEntry::Standalone { location, .. } => {
                run_paths.push(location.file.as_str());
            }
        }
    }
    let base = hoistable_dir_prefix(&run_paths, "  base: ");
    if let Some(prefix) = &base {
        report.line(format!("  base: {prefix}"));
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
                ..
            } => {
                for (location, diagnostics) in &items {
                    emit_diagnostics(
                        &directory,
                        &justless,
                        id,
                        location,
                        diagnostics,
                        base.as_deref(),
                        &mut report,
                    )?;
                    if report.clipped {
                        break;
                    }
                }
                if !report.clipped {
                    let locations: Vec<&Location> = items.iter().map(|(loc, _)| *loc).collect();
                    let room = report.left();
                    detail_multi_cached(
                        workspace,
                        &mut cache,
                        index,
                        best,
                        &locations,
                        &overlapping,
                        base.as_deref(),
                        &mut report,
                        SnippetView::Report,
                        &mut referenced,
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
                    base.as_deref(),
                    &mut report,
                )?;
                if report.clipped {
                    break;
                }
                if stale {
                    referenced.insert(location.file.clone());
                    report.line("  Source changed or disappeared after this run. Re-run the command for current locations.");
                } else {
                    let room = report.left();
                    at_location_cached(
                        workspace,
                        &mut cache,
                        index,
                        location,
                        base.as_deref(),
                        &mut report,
                        None,
                        &mut referenced,
                    )?;
                    source += room.saturating_sub(report.left());
                }
            }
        }
    }
    Ok((directory, report.into_string(), source, referenced))
}

#[cfg(test)]
fn at_location(
    workspace: &Workspace,
    index: &Index,
    location: &Location,
    report: &mut Report,
    memory: Option<&mut Memory>,
) -> Result<()> {
    let mut cache = SourceCache::default();
    let mut referenced = BTreeSet::new();
    at_location_cached(
        workspace,
        &mut cache,
        index,
        location,
        None,
        report,
        memory,
        &mut referenced,
    )
}

#[allow(clippy::too_many_arguments)]
fn at_location_cached(
    workspace: &Workspace,
    cache: &mut SourceCache,
    index: &Index,
    location: &Location,
    base: Option<&str>,
    report: &mut Report,
    mut memory: Option<&mut Memory>,
    referenced: &mut BTreeSet<String>,
) -> Result<()> {
    referenced.insert(location.file.clone());
    match resolve_location(index, location) {
        Resolution::Single { best, overlapping } => detail_cached(
            workspace,
            cache,
            index,
            best,
            location,
            &overlapping,
            base,
            report,
            memory,
            referenced,
        ),
        Resolution::Multiple { members } => {
            // Find outermost spans in O(M log M) while keeping identical-span twins.
            let mut sorted = members.clone();
            sorted.sort_by_key(|&id| {
                let span = &index.definitions[id].location;
                (span.start, Reverse(span.end), id)
            });
            let mut outermost: Vec<usize> = Vec::with_capacity(sorted.len());
            let mut enclosing: Option<(usize, usize)> = None;
            for &id in &sorted {
                let span = &index.definitions[id].location;
                let pair = (span.start, span.end);
                if let Some((es, ee)) = enclosing
                    && (es, ee) != pair
                    && es <= pair.0
                    && pair.1 <= ee
                {
                    continue;
                }
                outermost.push(id);
                if enclosing.is_none_or(|(_, ee)| pair.1 > ee) {
                    enclosing = Some(pair);
                }
            }
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
                "{} · {} symbols: {}",
                format_location_with_base(location, base),
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
                detail_cached(
                    workspace,
                    cache,
                    index,
                    id,
                    &part,
                    &inner,
                    base,
                    report,
                    memory.as_deref_mut(),
                    referenced,
                )?;
            }
            Ok(())
        }
        Resolution::None => {
            report.line(format!(
                "{} · no enclosing indexed symbol",
                format_location_with_base(location, base)
            ));
            if let Some(error) = index.errors.get(&location.file) {
                report.line(format!("  Parse error: {error}"));
            }
            snippet_cached(
                workspace,
                cache,
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
#[allow(clippy::too_many_arguments)]
fn detail_cached(
    workspace: &Workspace,
    cache: &mut SourceCache,
    index: &Index,
    best: usize,
    location: &Location,
    overlapping: &[usize],
    base: Option<&str>,
    report: &mut Report,
    memory: Option<&mut Memory>,
    referenced: &mut BTreeSet<String>,
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
    let locations = [location];
    detail_multi_cached(
        workspace, cache, index, best, &locations, keep, base, report, view, referenced,
    )
}

/// One symbol around one or more locations: prints its metadata once and merges
/// overlapping ±3-line source windows into disjoint sorted windows.
#[allow(clippy::too_many_arguments)]
fn detail_multi_cached(
    workspace: &Workspace,
    cache: &mut SourceCache,
    index: &Index,
    best: usize,
    locations: &[&Location],
    overlapping: &[usize],
    base: Option<&str>,
    report: &mut Report,
    view: SnippetView<'_>,
    referenced: &mut BTreeSet<String>,
) -> Result<()> {
    let definition = &index.definitions[best];
    referenced.insert(definition.location.file.clone());
    report.line(describe_with_base(definition, base));
    // Sort outgoing call/bridge/import edges once by (inferred, nearest distance, line),
    // then group repeated call sites to the same target into one line so all line
    // numbers survive without duplicating the target summary.
    let mut ranked_edges: Vec<(bool, usize, usize, &Edge)> = index
        .outgoing(best)
        .filter(|edge| {
            is_call(edge.kind) || matches!(edge.kind, Kind::Route | Kind::Data | Kind::Import)
        })
        .map(|edge| {
            let distance = locations
                .iter()
                .map(|location| edge.location.start.abs_diff(location.start))
                .min()
                .unwrap_or(usize::MAX);
            (edge.inferred, distance, edge.location.start, edge)
        })
        .collect();
    ranked_edges.sort_by_key(|&(inferred, distance, start, _)| (inferred, distance, start));

    type DetailEdgeKey<'a> = (usize, Kind, bool, &'a str);
    let mut edge_groups: Vec<(DetailEdgeKey<'_>, BTreeSet<(usize, usize)>)> = Vec::new();
    let mut edge_by_target: HashMap<usize, usize> = HashMap::new();
    for (_, _, _, edge) in ranked_edges {
        if let Some(&slot) = edge_by_target.get(&edge.to) {
            let (key, spans) = &mut edge_groups[slot];
            if key.1 == edge.kind && key.2 == edge.inferred && key.3 == edge.location.file.as_str()
            {
                spans.insert((edge.location.start, edge.location.end));
            }
        } else if edge_groups.len() < 4 {
            let slot = edge_groups.len();
            let mut spans = BTreeSet::new();
            spans.insert((edge.location.start, edge.location.end));
            edge_groups.push((
                (
                    edge.to,
                    edge.kind,
                    edge.inferred,
                    edge.location.file.as_str(),
                ),
                spans,
            ));
            edge_by_target.insert(edge.to, slot);
        }
    }
    for ((to, kind, inferred, origin_file), spans) in edge_groups {
        let description = match kind {
            Kind::Call | Kind::Method => "calls",
            Kind::Import => "imports",
            Kind::Route => "HTTP route to",
            Kind::Data => "SQL relation to",
            Kind::Reference => "references",
        };
        let confidence = if inferred {
            "possible relationship · "
        } else {
            ""
        };
        let origin = if origin_file == definition.location.file {
            format_same_file_spans(&spans)
        } else {
            format_line_spans_with_base(origin_file, &spans, base)
        };
        referenced.insert(index.definitions[to].location.file.clone());
        referenced.insert(origin_file.to_owned());
        report.line(format!(
            "  {confidence}{description} {} (from {origin})",
            describe_with_base(&index.definitions[to], base),
        ));
    }
    if !definition.test {
        for (id, _, inferred) in index.related_tests(best, 3) {
            let confidence = if inferred {
                "possible related test"
            } else {
                "related test"
            };
            referenced.insert(index.definitions[id].location.file.clone());
            report.line(format!(
                "  {confidence}: {}",
                describe_with_base(&index.definitions[id], base)
            ));
        }
    }
    if !overlapping.is_empty() {
        let others: Vec<_> = overlapping
            .iter()
            .filter(|&&id| id != best && index.definitions[id].location != definition.location)
            .collect();
        for &&id in others.iter().take(4) {
            report.line(format!(
                "  overlaps {}",
                describe_with_base(&index.definitions[id], base)
            ));
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
            let text = &cache.get_or_read(workspace, file)?.text;
            memory.show(
                workspace,
                text,
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
        (SnippetView::AtPlain | SnippetView::Session(_), _) => snippets_trimmed_cached(
            workspace,
            cache,
            file,
            &windows,
            SESSION_MIN_TRIM_INDENT,
            report,
        )?,
        (SnippetView::Report, _) => {
            snippets_trimmed_cached(workspace, cache, file, &windows, MIN_TRIM_INDENT, report)?
        }
    }
    if let Some(&(first_from, _)) = windows.first()
        && definition.declaration < first_from
        && !report.clipped
    {
        report.line(format!(
            "  Declaration starts at {}:{}",
            display_with_base(file, base),
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
    let mut cache = SourceCache::default();
    let mut referenced = BTreeSet::new();
    let text = changed_report_cached_with_files(
        workspace,
        &mut cache,
        index,
        since,
        cap,
        &mut referenced,
    )?
    .into_string();
    io::stdout().lock().write_all(text.as_bytes())?;
    crate::history::record_graph_files(
        workspace,
        "context",
        text.len() as u64,
        referenced.iter().map(String::as_str),
    );
    Ok(())
}

#[cfg(test)]
fn changed_report(
    workspace: &Workspace,
    index: &Index,
    since: &Since,
    cap: usize,
) -> Result<Report> {
    let mut cache = SourceCache::default();
    let mut referenced = BTreeSet::new();
    changed_report_cached_with_files(workspace, &mut cache, index, since, cap, &mut referenced)
}

fn changed_report_cached_with_files(
    workspace: &Workspace,
    cache: &mut SourceCache,
    index: &Index,
    since: &Since,
    cap: usize,
    referenced: &mut BTreeSet<String>,
) -> Result<Report> {
    let mut report = Report::new(cap);
    let changes = changes_with_cache(workspace, index, since, cache)?;
    let mut unique_files: Vec<&str> = Vec::new();
    let mut seen_files: HashSet<&str> = HashSet::new();
    for (file, _) in &changes {
        if seen_files.insert(file.as_str()) {
            unique_files.push(file.as_str());
        }
    }
    let base = hoistable_dir_prefix(&unique_files, "base: ");
    if let Some(prefix) = &base {
        report.line(format!("base: {prefix}"));
    }
    let mut deferred: HashMap<(String, usize), Vec<Location>> = HashMap::new();
    for (pos, (file, change)) in changes.iter().enumerate() {
        referenced.insert(file.clone());
        let name = display_with_base(file, base.as_deref());
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
                let next_range_symbols: HashSet<usize> = changes
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
                    cache,
                    index,
                    file,
                    deletions,
                    &next_range_symbols,
                    &mut deferred,
                    base.as_deref(),
                    &mut report,
                    referenced,
                )?;
            }
            Change::Ranges(locations) => {
                report.line(format!("{name} · {} changed ranges", locations.len()));
                emit_changed_ranges(
                    workspace,
                    cache,
                    index,
                    file,
                    locations,
                    &mut deferred,
                    base.as_deref(),
                    &mut report,
                    referenced,
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

fn emit_deletion_header(file: &str, deletion: &Deletion, base: Option<&str>, report: &mut Report) {
    const SHOWN: usize = 8;
    let name = display_with_base(file, base);
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
#[allow(clippy::too_many_arguments)]
fn emit_deletions(
    workspace: &Workspace,
    cache: &mut SourceCache,
    index: &Index,
    file: &str,
    deletions: &[Deletion],
    deferred_symbols: &HashSet<usize>,
    deferred: &mut HashMap<(String, usize), Vec<Location>>,
    base: Option<&str>,
    report: &mut Report,
    referenced: &mut BTreeSet<String>,
) -> Result<()> {
    enum Entry<'a> {
        HeaderOnly(&'a Deletion),
        Symbol {
            id: usize,
            items: Vec<(&'a Deletion, Location)>,
        },
    }
    let mut entries: Vec<Entry<'_>> = Vec::with_capacity(deletions.len());
    let mut by_symbol: HashMap<usize, usize> = HashMap::new();
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
            Entry::HeaderOnly(deletion) => emit_deletion_header(file, deletion, base, report),
            Entry::Symbol { id, items } => {
                for (deletion, _) in &items {
                    emit_deletion_header(file, deletion, base, report);
                    if report.clipped {
                        break;
                    }
                }
                if !report.clipped {
                    let locations: Vec<&Location> = items.iter().map(|(_, loc)| loc).collect();
                    detail_multi_cached(
                        workspace,
                        cache,
                        index,
                        id,
                        &locations,
                        &[],
                        base,
                        report,
                        SnippetView::Report,
                        referenced,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Changed ranges in one file for `context --changes`, grouped by enclosing
/// symbol so each symbol's metadata prints once and overlapping snippets merge.
#[allow(clippy::too_many_arguments)]
fn emit_changed_ranges(
    workspace: &Workspace,
    cache: &mut SourceCache,
    index: &Index,
    file: &str,
    locations: &[(usize, usize)],
    deferred: &mut HashMap<(String, usize), Vec<Location>>,
    base: Option<&str>,
    report: &mut Report,
    referenced: &mut BTreeSet<String>,
) -> Result<()> {
    enum RangeEntry {
        Symbol {
            best: usize,
            ranges: Vec<(usize, usize)>,
            locations: Vec<Location>,
            overlapping: Vec<usize>,
            overlapping_seen: HashSet<usize>,
        },
        Standalone(Location),
    }
    let name = display_with_base(file, base);
    let mut entries: Vec<RangeEntry> = Vec::with_capacity(locations.len());
    let mut by_symbol: HashMap<usize, usize> = HashMap::new();
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
                        overlapping_seen,
                        ..
                    } = &mut entries[slot]
                    {
                        ranges.push((start, end));
                        group_locs.push(location);
                        if end > start {
                            for id in overlapping {
                                if overlapping_seen.insert(id) {
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
                    let overlapping_seen: HashSet<usize> = keep_overlap.iter().copied().collect();
                    entries.push(RangeEntry::Symbol {
                        best,
                        ranges: vec![(start, end)],
                        locations: group_locs,
                        overlapping: keep_overlap,
                        overlapping_seen,
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
                ..
            } => {
                let list: Vec<String> = ranges
                    .iter()
                    .map(|&(start, end)| format!("{start}-{end}"))
                    .collect();
                report.line(format!("Changed {name}:{}", list.join(", ")));
                if !report.clipped {
                    let loc_refs: Vec<&Location> = locations.iter().collect();
                    detail_multi_cached(
                        workspace,
                        cache,
                        index,
                        best,
                        &loc_refs,
                        &overlapping,
                        base,
                        report,
                        SnippetView::Report,
                        referenced,
                    )?;
                }
            }
            RangeEntry::Standalone(location) => {
                report.line(format!(
                    "Changed {name}:{}-{}",
                    location.start, location.end
                ));
                if !report.clipped {
                    at_location_cached(
                        workspace, cache, index, &location, base, report, None, referenced,
                    )?;
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
    let mut cache = SourceCache::default();
    changes_with_cache(workspace, index, since, &mut cache)
}

fn changes_with_cache(
    workspace: &Workspace,
    index: &Index,
    since: &Since,
    cache: &mut SourceCache,
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
        let file_lines = cache.get_or_read(workspace, file)?.line_count();
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

        let (_, out, source_bytes, _) =
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

    #[test]
    fn source_cache_reads_multi_symbol_file_once_and_hoists_deep_query_prefix() {
        // 1. Multi-symbol `context --at` across one file reads disk exactly once.
        let mut py = String::new();
        for i in 0..8 {
            py.push_str(&format!("def step_{i}(x):\n    return x + {i}\n\n"));
        }
        let (_dir1, ws1) = temp_workspace(&[("multi.py", &py)]);
        let idx1 = Index::open(&ws1).unwrap();
        let mut cache = SourceCache::default();
        let first = at_report_cached(&ws1, &mut cache, &idx1, "multi.py:1-24", 8000, None)
            .unwrap()
            .into_string();
        assert_eq!(
            cache.reads(),
            1,
            "multi-symbol context --at must read file once"
        );
        let second = at_report_cached(&ws1, &mut cache, &idx1, "multi.py:1-24", 8000, None)
            .unwrap()
            .into_string();
        assert_eq!(cache.reads(), 1, "repeat lookup must stay cached");
        assert_eq!(first, second);

        // 2. Deep `>= 2`-segment paths in `query` and `path` hoist `  base: <dir>/`
        // while preserving every filename, line number, and edge kind.
        let (_dir2, ws2) = temp_workspace(&[
            (
                "packages/billing/src/core.py",
                "def compute_fee(amount):\n    return amount * 2\n",
            ),
            (
                "packages/billing/src/api.py",
                "from core import compute_fee\n\ndef handle_invoice(amount):\n    return compute_fee(amount)\n",
            ),
            (
                "packages/billing/src/cli.py",
                "from core import compute_fee\n\ndef run_billing(amount):\n    return compute_fee(amount)\n",
            ),
            (
                "packages/billing/src/batch.py",
                "from api import handle_invoice\n\ndef run_batch(amount):\n    return handle_invoice(amount)\n",
            ),
        ]);
        let idx2 = Index::open(&ws2).unwrap();
        let refs = query_report(&idx2, "compute_fee", "callers", 8000)
            .unwrap()
            .into_string();
        assert!(
            refs.contains("  base: packages/billing/src/\n"),
            "expected hoisted base prefix:\n{refs}"
        );
        assert!(refs.contains("at api.py:4\n"), "{refs}");
        assert!(refs.contains("at cli.py:4\n"), "{refs}");
        assert!(
            !refs.contains("at packages/billing/src/api.py:4"),
            "hoisted location must not repeat prefix:\n{refs}"
        );

        let route = path_report(&idx2, "run_batch", "compute_fee", 8000)
            .unwrap()
            .into_string();
        assert!(
            route.contains("  base: packages/billing/src/\n"),
            "expected hoisted base prefix in path:\n{route}"
        );
        assert!(route.contains("at batch.py:4 →"), "{route}");
        assert!(route.contains("at api.py:4 →"), "{route}");
        assert!(route.contains("at core.py:1-2\n"), "{route}");
    }

    #[test]
    fn detail_multi_groups_repeated_call_sites_without_losing_lines() {
        let manifest = "[package]\nname = \"callsites\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let src = concat!(
            "pub fn helper(v: u32) -> u32 { v + 1 }\n\n",
            "pub fn pipeline(x: u32) -> u32 {\n",
            "    let a = helper(x);\n",
            "    let b = helper(a);\n",
            "    helper(b)\n",
            "}\n",
        );
        let (_dir, workspace) = temp_workspace(&[("Cargo.toml", manifest), ("src/lib.rs", src)]);
        let index = Index::open(&workspace).unwrap();
        let mut cache = SourceCache::default();
        let out = at_report_cached(&workspace, &mut cache, &index, "src/lib.rs:5", 8000, None)
            .unwrap()
            .into_string();
        assert_eq!(cache.reads(), 1);
        assert_eq!(out.matches("  calls callsites::helper").count(), 1, "{out}");
        assert!(
            out.contains("(from :4,5,6)\n"),
            "all repeated call-site line numbers must survive on one line:\n{out}"
        );
    }

    #[test]
    fn query_path_context_and_pack_record_graph_telemetry_only_with_state() {
        let manifest = "[package]\nname = \"gstat\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let lib =
            "pub mod caller;\npub mod entry;\n\npub fn core_fn(x: u32) -> u32 {\n    x + 1\n}\n";
        let caller =
            "use crate::core_fn;\n\npub fn middle_fn(v: u32) -> u32 {\n    core_fn(v) * 2\n}\n";
        let entry = "use crate::caller::middle_fn;\n\npub fn entry_fn(n: u32) -> u32 {\n    middle_fn(n)\n}\n";
        let (_dir, workspace) = temp_workspace(&[
            ("Cargo.toml", manifest),
            ("src/lib.rs", lib),
            ("src/caller.rs", caller),
            ("src/entry.rs", entry),
        ]);
        init_git(&workspace);
        let index = Index::open(&workspace).unwrap();

        query(&index, "core_fn", "references", 4096).unwrap();
        let after_query = crate::history::graph_usage(&workspace);
        assert_eq!(after_query.calls, 1, "{after_query:?}");
        assert_eq!(after_query.by_command.get("query"), Some(&1));
        assert_eq!(
            after_query.referenced_file_bytes,
            (lib.len() + caller.len()) as u64,
            "{after_query:?}"
        );
        assert!(after_query.shown_bytes > 0);

        path(&index, "entry_fn", "core_fn", 4096).unwrap();
        let after_path = crate::history::graph_usage(&workspace);
        assert_eq!(after_path.calls, 2, "{after_path:?}");
        assert_eq!(after_path.by_command.get("path"), Some(&1));
        assert_eq!(
            after_path.referenced_file_bytes - after_query.referenced_file_bytes,
            (entry.len() + caller.len() + lib.len()) as u64,
            "{after_path:?}"
        );

        at(&workspace, &index, "src/caller.rs:4", 4096, None).unwrap();
        symbol(&workspace, &index, "core_fn", 4096, None).unwrap();
        changed(&workspace, &index, &Since::Head, 4096).unwrap();
        crate::pack::pack(&workspace, &index, "core_fn", 4096, None).unwrap();

        let total = crate::history::graph_usage(&workspace);
        assert_eq!(total.calls, 6, "{total:?}");
        assert_eq!(total.by_command.get("query"), Some(&1));
        assert_eq!(total.by_command.get("path"), Some(&1));
        assert_eq!(total.by_command.get("context"), Some(&3));
        assert_eq!(total.by_command.get("pack"), Some(&1));
        assert!(total.shown_bytes > after_path.shown_bytes);
        assert!(total.referenced_file_bytes > after_path.referenced_file_bytes);

        // Removing `.justless/` keeps `query`, `path`, and `pack` fail-silent with no state created.
        fs::remove_dir_all(&workspace.state).unwrap();
        assert!(!workspace.state.exists());
        query(&index, "core_fn", "references", 4096).unwrap();
        path(&index, "entry_fn", "core_fn", 4096).unwrap();
        crate::pack::pack(&workspace, &index, "core_fn", 4096, None).unwrap();
        assert!(
            !workspace.state.exists(),
            "query, path, and pack must stay silent and not create .justless/ when absent"
        );
    }
}
