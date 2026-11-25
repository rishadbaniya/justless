use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use clap::{Arg, ArgMatches, Command as Cli};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::polyglot::{Grammar, Parsers};
use crate::syntax_check::{self, Change, Verdict};
use crate::workspace::{Workspace, lock_state};

fn file() -> Arg<'static> {
    Arg::new("file").required(true)
}
fn revision() -> Arg<'static> {
    Arg::new("revision")
        .long("revision")
        .value_name("REV")
        .help("Revision from the outline")
}
fn session_arg() -> Arg<'static> {
    Arg::new("session")
        .long("session")
        .takes_value(true)
        .value_name("NAME")
        .help("Print only what changed since this session read the file")
}
fn fresh_arg() -> Arg<'static> {
    Arg::new("fresh")
        .long("fresh")
        .help("Print full output even when this session already saw it")
}

pub fn commands() -> Vec<Cli<'static>> {
    vec![
        Cli::new("read")
            .about("Read a small file or page through one or more files' outlines")
            .arg(Arg::new("file").required(true).multiple_values(true))
            .arg(Arg::new("outline").long("outline"))
            .arg(
                Arg::new("shell-range")
                    .long("shell-range")
                    .value_name("A-B|+N")
                    .hidden(true)
                    .conflicts_with_all(&["outline", "json", "match", "after"]),
            )
            .arg(
                Arg::new("json")
                    .long("json")
                    .help("Return a structured outline"),
            )
            .arg(
                Arg::new("shell-object")
                    .long("shell-object")
                    .value_name("ID")
                    .hidden(true)
                    .requires("shell-range"),
            )
            .arg(
                Arg::new("shell-numbered")
                    .long("shell-numbered")
                    .value_name("cat|nl")
                    .possible_values(["cat", "nl"])
                    .hidden(true)
                    .requires("shell-range"),
            )
            .arg(
                Arg::new("shell-session")
                    .long("shell-session")
                    .value_name("ID")
                    .hidden(true)
                    .requires("shell-range"),
            )
            .arg(
                Arg::new("match")
                    .long("match")
                    .value_name("TEXT")
                    .help("Keep entries matching TEXT (use a|b for multiple terms)"),
            )
            .arg(
                Arg::new("after")
                    .long("after")
                    .value_name("REF")
                    .help("Continue after this revision-bound entry"),
            )
            .arg(session_arg())
            .arg(fresh_arg())
            .arg(crate::number("limit", "40", "Maximum entries per page"))
            .arg(crate::budget()),
        Cli::new("expand")
            .about("Print the exact source bytes for a symbol, outline reference or line range")
            .arg(
                Arg::new("file")
                    .value_name("FILE [SELECTOR] | FILE:SELECTOR...")
                    .required(true)
                    .multiple_values(true),
            )
            .arg(
                Arg::new("json")
                    .long("json")
                    .help("Include the edit reference, source lines and revision"),
            )
            .arg(revision())
            .arg(session_arg())
            .arg(fresh_arg()),
        Cli::new("edit")
            .about("Replace or insert at verified source locations")
            .arg(file())
            .arg(
                Arg::new("selector")
                    .required_unless_present("batch")
                    .conflicts_with("batch"),
            )
            .arg(revision())
            .arg(
                Arg::new("before")
                    .long("before")
                    .conflicts_with_all(&["after", "batch"]),
            )
            .arg(Arg::new("after").long("after").conflicts_with("batch"))
            .arg(
                Arg::new("batch")
                    .long("batch")
                    .help("Read a JSON array of edits against one snapshot"),
            )
            .arg(
                Arg::new("replacement-file")
                    .long("replacement-file")
                    .value_name("PATH")
                    .help("Read replacement text or batch JSON from a file instead of stdin"),
            )
            .arg(
                Arg::new("dry-run")
                    .long("dry-run")
                    .help("Print a unified diff and leave the source untouched"),
            )
            .arg(
                Arg::new("allow-parse-errors")
                    .long("allow-parse-errors")
                    .help("Write an edit that leaves the file unparseable"),
            ),
    ]
}

struct Entry {
    name: String,
    kind: String,
    bytes: Range<usize>,
    /// Where the outline signature starts, after Rust attributes and docs and
    /// after TypeScript member decorators.
    signature_start: usize,
    header_end: usize,
}

pub struct Snapshot {
    pub text: String,
    revision: String,
    lines: Vec<usize>,
    entries: Vec<Entry>,
    /// Why the language parser produced no entries, such as a Rust syntax error.
    parse_error: Option<String>,
    /// Whether this snapshot was indexed as a Rust source file.
    is_rust: bool,
    /// Whether this snapshot was indexed as a Markdown source file.
    is_markdown: bool,
}

pub(crate) fn revision_for(file: &str, bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"justless-source-v1\0");
    hash.update(file.as_bytes());
    hash.update([0]);
    hash.update(bytes);
    format!("{:x}", hash.finalize())[..24].to_owned()
}

impl Snapshot {
    pub fn open(ws: &Workspace, file: &str) -> Result<Self> {
        let path = ws.source_path(file)?;
        let cap = ws.settings()?.max_file_bytes;
        // Check the size before decoding: the cap can split a multibyte character.
        let mut bytes = Vec::new();
        File::open(path)?
            .take(cap.saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= cap, "file exceeds max_file_bytes");
        let text = String::from_utf8(bytes).context("expected UTF-8 source")?;
        ensure!(!text.contains('\0'), "expected a text file");
        let mut lines = vec![0];
        lines.extend(
            text.match_indices('\n')
                .map(|(i, _)| i + 1)
                .filter(|i| *i < text.len()),
        );
        let revision = revision_for(file, text.as_bytes());
        Ok(Self {
            text,
            revision,
            lines,
            entries: Vec::new(),
            parse_error: None,
            is_rust: false,
            is_markdown: false,
        })
    }

    /// An outline of `text` as if it were `file`, for source that is not on disk,
    /// such as the committed version of a changed file.
    pub fn from_text(file: &str, text: String) -> Self {
        let mut lines = vec![0];
        lines.extend(
            text.match_indices('\n')
                .map(|(i, _)| i + 1)
                .filter(|i| *i < text.len()),
        );
        let revision = revision_for(file, text.as_bytes());
        Self {
            text,
            revision,
            lines,
            entries: Vec::new(),
            parse_error: None,
            is_rust: false,
            is_markdown: false,
        }
    }

    /// Indexed symbols as (name, kind, first line, last line), lines 1-based.
    pub fn symbols(&self) -> Vec<(String, String, usize, usize)> {
        self.entries
            .iter()
            .map(|entry| {
                let last = entry.bytes.end.saturating_sub(1).max(entry.bytes.start);
                (
                    entry.name.clone(),
                    entry.kind.clone(),
                    self.line(entry.bytes.start),
                    self.line(last),
                )
            })
            .collect()
    }

    /// The number of lines, counting a final line without a newline.
    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn line_window(&self, first: usize, last: usize) -> Option<(usize, Range<usize>)> {
        let start = *self.lines.get(first.checked_sub(1)?)?;
        let last = last.min(self.lines.len());
        let end = self.lines.get(last).copied().unwrap_or(self.text.len());
        (start < end).then_some((last, start..end))
    }

    fn line(&self, offset: usize) -> usize {
        self.lines.partition_point(|&start| start <= offset)
    }

    /// Whether `span` covers every non-whitespace byte on its first and last
    /// lines, so recording it by line range never hides a sibling declaration
    /// sharing the same line.
    fn is_full_lines(&self, span: &Range<usize>) -> bool {
        if span.is_empty() {
            return false;
        }
        let first = self.line(span.start);
        let last = self.line(span.end.saturating_sub(1).max(span.start));
        let line_start = self.lines[first - 1];
        let line_end = self.lines.get(last).copied().unwrap_or(self.text.len());
        self.text[line_start..span.start].trim().is_empty()
            && self.text[span.end..line_end].trim().is_empty()
    }

    /// The start of `offset`'s line when only indentation precedes it, so a
    /// symbol insertion goes before the whole line.
    fn line_aligned_start(&self, offset: usize) -> usize {
        let start = self.text[..offset].rfind('\n').map_or(0, |i| i + 1);
        if self.text[start..offset].trim().is_empty() {
            start
        } else {
            offset
        }
    }

    /// Just past the line ending after `offset` when only whitespace follows
    /// it on that line, so a symbol insertion goes after the whole line.
    fn line_aligned_end(&self, offset: usize) -> usize {
        if offset == 0 || self.text.as_bytes()[offset - 1] == b'\n' {
            return offset;
        }
        let rest = &self.text[offset..];
        let end = rest.find('\n').map_or(rest.len(), |i| i + 1);
        if rest[..end].trim().is_empty() {
            offset + end
        } else {
            offset
        }
    }

    pub fn index(&mut self, file: &str) -> Result<()> {
        let mut imports = Vec::new();
        self.is_rust = file.ends_with(".rs");
        if self.is_rust {
            // syn parses whole files, so one syntax error leaves no symbols.
            match crate::rust_syntax::parse(&self.text) {
                Err(error) => {
                    self.parse_error = Some(format!(
                        "Rust parse failed at line {}: {error}",
                        error.span().start().line
                    ));
                }
                Ok(facts) => {
                    imports = facts.use_ranges;
                    for symbol in facts.symbols {
                        let Some(fragment) = self.text.get(symbol.bytes.clone()) else {
                            continue;
                        };
                        let (start, end) = rust_header(fragment);
                        self.entries.push(Entry {
                            name: symbol.path.join("::"),
                            kind: symbol.kind,
                            signature_start: symbol.bytes.start + start,
                            header_end: symbol.bytes.start + end,
                            bytes: symbol.bytes,
                        });
                    }
                }
            }
        } else if let Some(grammar) = Grammar::detect(file, &self.text) {
            let facts = Parsers::default().parse(grammar, &self.text)?;
            imports = facts.import_ranges;
            for def in facts.definitions {
                self.entries.push(Entry {
                    name: def.path,
                    kind: def.kind,
                    signature_start: def.signature_start,
                    bytes: def.bytes,
                    header_end: def.header_end,
                });
            }
        }
        if self.entries.is_empty() {
            self.sections(file);
        }
        let needs_pages = self.entries.is_empty();
        for bytes in imports {
            self.entries.push(Entry {
                name: "import".into(),
                kind: "import".into(),
                signature_start: bytes.start,
                header_end: bytes.end,
                bytes,
            });
        }
        if needs_pages {
            for (page, chunk) in self.lines.chunks(80).enumerate() {
                let begin = chunk[0];
                let end = self
                    .lines
                    .get((page + 1) * 80)
                    .copied()
                    .unwrap_or(self.text.len());
                self.entries.push(Entry {
                    name: format!("lines-{}", page * 80 + 1),
                    kind: "text".into(),
                    signature_start: begin,
                    bytes: begin..end,
                    header_end: self.lines.get(page * 80 + 1).copied().unwrap_or(end),
                });
            }
        }
        self.entries
            .sort_by_key(|e| (e.bytes.start, std::cmp::Reverse(e.bytes.end)));
        Ok(())
    }

    fn sections(&mut self, file: &str) {
        // Where a 0-based line starts, or the end of the text past the last line.
        let offset = |line: usize| self.lines.get(line).copied().unwrap_or(self.text.len());
        let markdown = matches!(
            std::path::Path::new(file)
                .extension()
                .and_then(|x| x.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("md" | "markdown" | "mdx")
        );
        if markdown {
            self.is_markdown = true;
            let lines: Vec<&str> = (0..self.lines.len())
                .map(|line| {
                    let text = &self.text[offset(line)..offset(line + 1)];
                    text.strip_suffix('\n').unwrap_or(text)
                })
                .collect();
            let entries: Vec<Entry> = crate::markdown::outline_blocks(&lines)
                .into_iter()
                .map(|block| Entry {
                    name: block.name,
                    kind: block.kind.as_str().into(),
                    signature_start: offset(block.first),
                    bytes: offset(block.first)..offset(block.end),
                    header_end: offset(block.header_last + 1),
                })
                .collect();
            self.entries.extend(entries);
            return;
        }
        let entries: Vec<Entry> = crate::config_outline::sections(file, &self.text, &self.lines)
            .into_iter()
            .map(|section| Entry {
                name: section.name,
                kind: section.kind.into(),
                signature_start: section.signature_start,
                bytes: section.bytes,
                header_end: section.header_end,
            })
            .collect();
        self.entries.extend(entries);
    }

    fn reference(&self, index: usize) -> String {
        format!("{}#{}", self.revision, index + 1)
    }

    fn resolve_entry_range(&self, first: usize, last: usize) -> Result<Range<usize>> {
        ensure!(first >= 1, "entry IDs start at 1");
        ensure!(
            first <= last && last <= self.entries.len(),
            "outline entry range outside file"
        );
        let start = self.entries[first - 1].bytes.start;
        let end = self.entries[first - 1..last]
            .iter()
            .map(|entry| entry.bytes.end)
            .max()
            .unwrap_or(start);
        Ok(start..end)
    }

    fn resolve(
        &self,
        selector: &str,
        revision: Option<&str>,
        guarded: bool,
    ) -> Result<Range<usize>> {
        let (embedded, target) = split_selector(selector);
        for supplied in [embedded, revision].into_iter().flatten() {
            ensure!(
                supplied == self.revision,
                "stale revision for this file; read a fresh outline"
            );
        }
        ensure!(
            !guarded || embedded.is_some() || revision.is_some(),
            "editing requires a revision-bound reference or --revision"
        );
        if selector.contains('#') {
            if let Ok(id) = target.parse::<usize>() {
                return self
                    .entries
                    .get(id.checked_sub(1).context("entry IDs start at 1")?)
                    .map(|entry| entry.bytes.clone())
                    .context("unknown outline entry");
            }
            if let Some((first, last)) = entry_range(target) {
                return self.resolve_entry_range(first, last);
            }
        }
        if let Some(range) = line_range(target) {
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            let start = first.parse::<usize>()?;
            let end = last.trim_start_matches('L').parse::<usize>()?;
            ensure!(
                start >= 1 && start <= end && end <= self.lines.len(),
                "line range outside file"
            );
            return Ok(
                self.lines[start - 1]..self.lines.get(end).copied().unwrap_or(self.text.len())
            );
        }
        let mut matching = self.entries.iter().enumerate().filter(|(_, entry)| {
            entry.name == target || entry.name.rsplit("::").next() == Some(target)
        });
        let Some((id, entry)) = matching.next() else {
            match &self.parse_error {
                Some(error) => bail!("unknown symbol; {error}, so select a line range instead"),
                None => bail!("unknown symbol or selector"),
            }
        };
        if let Some((other, _)) = matching.next() {
            bail!(
                "ambiguous symbol; select an outline reference such as {} or {}",
                self.reference(id),
                self.reference(other)
            );
        }
        Ok(entry.bytes.clone())
    }

    fn resolve_span(
        &self,
        selector: &str,
        revision: Option<&str>,
        order: usize,
    ) -> Result<ResolvedSpan> {
        let span = self.resolve(selector, revision, false)?;
        let first = self.line(span.start);
        let last = self.line(span.end.saturating_sub(1).max(span.start));
        let (_, target) = split_selector(selector);
        let (id_label, anchor) = if selector.contains('#')
            && let Some((a, b)) = entry_range(target)
        {
            if a == b {
                (Some(format!("#{a}")), self.reference(a - 1))
            } else {
                (
                    Some(format!("#{a}..#{b}")),
                    format!("{}#{a}..#{b}", self.revision),
                )
            }
        } else if let Some(index) = self.entries.iter().position(|entry| entry.bytes == span) {
            (Some(format!("#{}", index + 1)), self.reference(index))
        } else {
            (None, format!("{}#L{first}-{last}", self.revision))
        };
        Ok(ResolvedSpan {
            order,
            span,
            first,
            last,
            id_label,
            anchor,
        })
    }

    fn coalesce_spans(&self, mut resolved: Vec<ResolvedSpan>) -> Vec<CoalescedSpan> {
        resolved.sort_by_key(|item| {
            (
                item.span.start,
                std::cmp::Reverse(item.span.end),
                item.order,
            )
        });
        let mut clusters: Vec<(Range<usize>, Vec<ResolvedSpan>)> = Vec::new();
        for item in resolved {
            if let Some((cluster_span, members)) = clusters.last_mut()
                && (item.span.start < cluster_span.end || item.span == *cluster_span)
            {
                cluster_span.end = cluster_span.end.max(item.span.end);
                members.push(item);
            } else {
                clusters.push((item.span.clone(), vec![item]));
            }
        }
        clusters.sort_by_key(|(_, members)| {
            members.iter().map(|m| m.order).min().unwrap_or(usize::MAX)
        });
        clusters
            .into_iter()
            .map(|(span, mut members)| {
                members.sort_by_key(|m| m.order);
                let first = self.line(span.start);
                let last = self.line(span.end.saturating_sub(1).max(span.start));
                if members.len() == 1 {
                    let item = &members[0];
                    let label = match &item.id_label {
                        Some(id) => format!("{id} L{first}-{last}"),
                        None => format!("L{first}-{last}"),
                    };
                    return CoalescedSpan {
                        span,
                        first,
                        last,
                        anchor: item.anchor.clone(),
                        label,
                    };
                }
                if let Some(primary_idx) = members
                    .iter()
                    .position(|m| m.span == span && m.id_label.is_some())
                {
                    let primary = &members[primary_idx];
                    let primary_id = primary.id_label.as_deref().unwrap();
                    let mut included = Vec::new();
                    for (idx, other) in members.iter().enumerate() {
                        if idx == primary_idx {
                            continue;
                        }
                        let tag = other
                            .id_label
                            .clone()
                            .unwrap_or_else(|| format!("L{}-{}", other.first, other.last));
                        if tag != primary_id && !included.contains(&tag) {
                            included.push(tag);
                        }
                    }
                    let label = if included.is_empty() {
                        format!("{primary_id} L{first}-{last}")
                    } else {
                        format!(
                            "{primary_id} L{first}-{last} (includes {})",
                            included.join(", ")
                        )
                    };
                    CoalescedSpan {
                        span,
                        first,
                        last,
                        anchor: primary.anchor.clone(),
                        label,
                    }
                } else {
                    let mut ids = Vec::new();
                    for member in &members {
                        if let Some(id) = &member.id_label
                            && !ids.contains(id)
                        {
                            ids.push(id.clone());
                        }
                    }
                    let label = if ids.is_empty() {
                        format!("L{first}-{last}")
                    } else {
                        format!("L{first}-{last} (includes {})", ids.join(", "))
                    };
                    CoalescedSpan {
                        span,
                        first,
                        last,
                        anchor: format!("{}#L{first}-{last}", self.revision),
                        label,
                    }
                }
            })
            .collect()
    }

    /// Trims strictly redundant outline signatures: Markdown blocks whose kind
    /// and name already convey their heading level or anchor label, and plain
    /// Rust type declarations where the header repeats only `{kind} {name}`
    /// with no visibility, generics, bounds or tuple fields.
    fn slim_signature(&self, entry: &Entry, signature: String) -> String {
        if self.is_markdown
            && matches!(
                entry.kind.as_str(),
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "frontmatter" | "code" | "table"
            )
        {
            return String::new();
        }
        if self.is_rust && matches!(entry.kind.as_str(), "struct" | "enum" | "trait" | "union") {
            let short = entry.name.rsplit("::").next().unwrap_or(&entry.name);
            if signature == format!("{} {short}", entry.kind) {
                return String::new();
            }
        }
        signature
    }

    pub fn page(
        &self,
        filter: &str,
        cursor: Option<&str>,
        limit: usize,
        budget: usize,
    ) -> Result<Page> {
        let start = match cursor {
            Some(cursor) => {
                let (rev, n) = cursor
                    .split_once('#')
                    .context("use the next cursor from the outline")?;
                ensure!(rev == self.revision, "stale page cursor");
                let index = n.parse::<usize>()?;
                ensure!(index > 0 && index <= self.entries.len(), "invalid cursor");
                index
            }
            None => 0,
        };
        let mut page = Page {
            revision: self.revision.clone(),
            lines: self.lines.len(),
            total_entries: self.entries.len(),
            parse_error: None,
            entries: Vec::new(),
            next: None,
            progress: None,
        };
        if let Some(error) = &self.parse_error {
            // Leave room for the cursor line and one short row.
            let room = budget.saturating_sub(page.display().len() + 42 + 32 + 1);
            page.parse_error = (room >= 32).then(|| clipped(error, room.min(240)));
        }
        let reserve = if budget >= 256 { 80 } else { 42 };
        let mut size = page.display().len() + reserve;
        let mut last = None;
        let filter = filter.to_lowercase();
        let terms: Vec<&str> =
            if !filter.is_empty() && !filter.contains(' ') && filter.contains('|') {
                let parts: Vec<&str> = filter.split('|').collect();
                if parts.iter().all(|part| !part.is_empty()) {
                    parts
                } else {
                    vec![filter.as_str()]
                }
            } else if filter.is_empty() {
                Vec::new()
            } else {
                vec![filter.as_str()]
            };
        let words = |raw: &str| raw.split_whitespace().collect::<Vec<_>>().join(" ");
        fn scope_prefix(entry: &Entry) -> Option<&str> {
            entry.name.rsplit_once("::").map(|(prefix, _)| prefix)
        }
        let fold_group = |entry: &Entry| -> Option<(&'static str, usize)> {
            if entry.kind == "import" {
                Some(("imports", 3))
            } else if entry.kind == "module"
                && self.text[entry.bytes.clone()].trim_end().ends_with(';')
            {
                Some(("modules", 3))
            } else if matches!(
                entry.kind.as_str(),
                "constant" | "const" | "static" | "variable"
            ) && self.line(entry.bytes.start)
                == self.line(entry.bytes.end.saturating_sub(1).max(entry.bytes.start))
            {
                Some(("constants", 5))
            } else {
                None
            }
        };
        let mut index = start;
        while index < self.entries.len() {
            let e = &self.entries[index];
            // Fold runs of 3+ contiguous imports or external `mod name;` declarations,
            // or 5+ contiguous single-line constants/statics/variables, in unfiltered
            // outlines so large preamble blocks don't crowd real declarations off page 1.
            // Individual `#N` entry IDs stay unchanged for `expand` and `edit`, and
            // folded blocks expand directly with `#A..#B`.
            let (row, end_index) = if filter.is_empty()
                && let Some((group, min_run)) = fold_group(e)
            {
                let mut run_end = index + 1;
                while run_end < self.entries.len()
                    && fold_group(&self.entries[run_end]) == Some((group, min_run))
                    && (group != "constants"
                        || scope_prefix(e) == scope_prefix(&self.entries[run_end]))
                {
                    run_end += 1;
                }
                let run_len = run_end - index;
                if run_len >= min_run {
                    let row = Row {
                        id: index + 1,
                        end_id: Some(run_end),
                        name: format!("{run_len} items"),
                        kind: group.into(),
                        first: self.line(e.bytes.start),
                        last: self.line(self.entries[run_end - 1].bytes.end.saturating_sub(1)),
                        signature: String::new(),
                    };
                    (row, run_end - 1)
                } else {
                    let signature =
                        self.slim_signature(e, words(&self.text[e.signature_start..e.header_end]));
                    let row = Row {
                        id: index + 1,
                        end_id: None,
                        name: clipped(&e.name, 100),
                        kind: e.kind.clone(),
                        first: self.line(e.bytes.start),
                        last: self.line(e.bytes.end.saturating_sub(1)),
                        signature: clipped(&signature, 160),
                    };
                    (row, index)
                }
            } else {
                let signature = words(&self.text[e.signature_start..e.header_end]);
                // Matching also covers the docs and attributes the row leaves out.
                if !terms.is_empty() {
                    let name = e.name.to_lowercase();
                    let header = words(&self.text[e.bytes.start..e.header_end]).to_lowercase();
                    if !terms
                        .iter()
                        .any(|term| name.contains(term) || header.contains(term))
                    {
                        index += 1;
                        continue;
                    }
                }
                let signature = self.slim_signature(e, signature);
                let row = Row {
                    id: index + 1,
                    end_id: None,
                    name: clipped(&e.name, 100),
                    kind: e.kind.clone(),
                    first: self.line(e.bytes.start),
                    last: self.line(e.bytes.end.saturating_sub(1)),
                    signature: clipped(&signature, 160),
                };
                (row, index)
            };
            let mut row = row;
            let row_size = row.display().len();
            if let Some(id) = last
                && (page.entries.len() == limit || size + row_size > budget)
            {
                page.next = Some(self.reference(id));
                break;
            }
            if size + row_size > budget {
                row.signature.clear();
                let remaining = budget.saturating_sub(size + row.display().len() - row.name.len());
                row.name = clipped(&row.name, remaining);
            }
            size += row.display().len();
            page.entries.push(row);
            last = Some(end_index);
            index = end_index + 1;
        }
        if let Some(shown) = page.next_index() {
            let remaining = self.entries.len().saturating_sub(shown);
            if remaining > 0 {
                let noun = if remaining == 1 { "entry" } else { "entries" };
                let line = format!(
                    "… {remaining} more {noun} ({shown} of {})\n",
                    self.entries.len()
                );
                if page.display().len() + line.len() <= budget {
                    page.progress = Some(line);
                }
            }
        }
        Ok(page)
    }

    /// The outline rows that overlap source lines `first..=last`, such as the
    /// window of a ranged Read, with their usual IDs. `None` when rows would not
    /// show where to look in the window: the file has no parser rows (a syntax
    /// error, or 80-line pages), or one row is the narrowest around more than half
    /// of the window's lines, such as a long function that holds the window.
    pub fn window(&self, first: usize, last: usize) -> Option<Window> {
        if first == 0
            || first > last
            || self.parse_error.is_some()
            || self.entries.iter().any(|entry| entry.kind == "text")
        {
            return None;
        }
        let words = |raw: &str| raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let rows: Vec<Row> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let lines = (
                    self.line(entry.bytes.start),
                    self.line(entry.bytes.end.saturating_sub(1)),
                );
                (lines.0 <= last && lines.1 >= first).then(|| {
                    let signature = self.slim_signature(
                        entry,
                        words(&self.text[entry.signature_start..entry.header_end]),
                    );
                    Row {
                        id: index + 1,
                        end_id: None,
                        kind: entry.kind.clone(),
                        name: clipped(&entry.name, 100),
                        first: lines.0,
                        last: lines.1,
                        signature: clipped(&signature, 160),
                    }
                })
            })
            .collect();
        let mut held = vec![0; rows.len()];
        for line in first..=last {
            let narrowest = (0..rows.len())
                .filter(|&index| rows[index].first <= line && line <= rows[index].last)
                .min_by_key(|&index| rows[index].last - rows[index].first);
            if let Some(index) = narrowest {
                held[index] += 1;
            }
        }
        let span = last - first + 1;
        (!rows.is_empty() && held.iter().all(|&count| count * 2 <= span)).then(|| Window {
            revision: self.revision.clone(),
            lines: self.lines.len(),
            first,
            last,
            rows,
        })
    }
}

/// The outline rows around a window of a file, from `Snapshot::window`.
pub struct Window {
    revision: String,
    lines: usize,
    first: usize,
    last: usize,
    rows: Vec<Row>,
}

impl Window {
    /// The revision, the window and one line per row, with signatures unless
    /// `signatures` is false.
    pub fn display(&self, signatures: bool) -> String {
        let mut out = format!(
            "revision {} · {} lines · window L{}-{}\n",
            self.revision, self.lines, self.first, self.last
        );
        for row in &self.rows {
            if signatures {
                out.push_str(&row.display());
            } else {
                out.push_str(&format!(
                    "#{} L{}-{} {} {}\n",
                    row.id, row.first, row.last, row.kind, row.name
                ));
            }
        }
        out
    }
}

/// Offsets within a Rust declaration where its signature starts, after outer
/// attributes and doc comments, and where its body begins. Token groups
/// distinguish a declaration's body from braces inside attributes, types,
/// patterns, strings and comments.
fn rust_header(fragment: &str) -> (usize, usize) {
    use proc_macro2::{Delimiter, TokenTree};
    let Ok(tokens) = fragment.parse::<proc_macro2::TokenStream>() else {
        return (0, fragment.len());
    };
    let tokens: Vec<_> = tokens.into_iter().collect();
    let mut first = 0;
    // `#[…]`, and `///` comments, which lex as `#[doc = "…"]`.
    while let Some(TokenTree::Punct(hash)) = tokens.get(first)
        && hash.as_char() == '#'
        && let Some(TokenTree::Group(group)) = tokens.get(first + 1)
        && group.delimiter() == Delimiter::Bracket
    {
        first += 2;
    }
    let start = tokens
        .get(first)
        .map_or(0, |token| token.span().byte_range().start);
    let body = &fragment[start..];
    // For multi-line or long `const`/`static` items, stop before the top-level `=`
    // so outlines keep the name and type without dumping 160 bytes of regex/array/
    // query initializer noise. Short single-line scalars keep their `= value`.
    let trim_initializer =
        is_const_or_static(&tokens[first..]) && (body.contains('\n') || body.len() > 80);
    let end = tokens[first..]
        .iter()
        .find_map(|token| match token {
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                Some(group.span_open().byte_range().start)
            }
            TokenTree::Punct(punct) if trim_initializer && punct.as_char() == '=' => {
                Some(punct.span().byte_range().start)
            }
            _ => None,
        })
        .unwrap_or(fragment.len());
    (start, end.max(start))
}

/// Whether `tokens` (after outer attributes) begin a `const` or `static` value
/// declaration rather than a `const fn` / `const unsafe fn` / `const extern fn`.
fn is_const_or_static(tokens: &[proc_macro2::TokenTree]) -> bool {
    use proc_macro2::{Delimiter, TokenTree};
    let mut i = 0;
    if matches!(tokens.get(i), Some(TokenTree::Ident(id)) if id == "pub") {
        i += 1;
        if matches!(tokens.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
        {
            i += 1;
        }
    }
    if matches!(tokens.get(i), Some(TokenTree::Ident(id)) if id == "unsafe") {
        i += 1;
    }
    match tokens.get(i) {
        Some(TokenTree::Ident(id)) if id == "static" => true,
        Some(TokenTree::Ident(id)) if id == "const" => !matches!(
            tokens.get(i + 1),
            Some(TokenTree::Ident(next)) if next == "fn" || next == "unsafe" || next == "extern"
        ),
        _ => false,
    }
}

struct ResolvedSpan {
    order: usize,
    span: Range<usize>,
    first: usize,
    last: usize,
    id_label: Option<String>,
    anchor: String,
}

struct CoalescedSpan {
    span: Range<usize>,
    first: usize,
    last: usize,
    anchor: String,
    label: String,
}

/// The inclusive 1-based `(first, last)` outline-entry IDs of an `A..#B` or
/// `A..B` target (after the leading `#` has been stripped by `split_selector`).
fn entry_range(target: &str) -> Option<(usize, usize)> {
    let (start, end) = target.split_once("..")?;
    let end = end.strip_prefix('#').unwrap_or(end);
    let first = start.parse::<usize>().ok()?;
    let last = end.parse::<usize>().ok()?;
    Some((first, last))
}

/// A selector's embedded revision, if any, and its target.
fn split_selector(selector: &str) -> (Option<&str>, &str) {
    selector
        .split_once('#')
        .filter(|(r, t)| {
            (r.is_empty() && (t.parse::<usize>().is_ok() || entry_range(t).is_some()))
                || (r.len() == 24 && r.bytes().all(|b| b.is_ascii_hexdigit()))
        })
        .map_or((None, selector), |(r, t)| ((!r.is_empty()).then_some(r), t))
}

fn has_windows_drive_prefix(spec: &str) -> bool {
    let bytes = spec.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

/// Splits a `FILE:SELECTOR` batch argument while preserving Windows drive
/// prefixes (`C:\path\file.rs:L1-10`), `::` symbol paths (`file.rs:Foo::bar`),
/// and Unix filenames that contain colons.
fn split_file_selector<'a>(
    ws: &Workspace,
    spec: &'a str,
    root_given: bool,
) -> Option<(&'a str, &'a str)> {
    let start = if has_windows_drive_prefix(spec) { 3 } else { 0 };
    let bytes = spec.as_bytes();
    let mut fallback = None;
    for i in start..bytes.len() {
        if bytes[i] != b':' {
            continue;
        }
        let left = &spec[..i];
        let right = &spec[i + 1..];
        if left.is_empty() || right.is_empty() {
            continue;
        }
        if workspace_file(ws, left, root_given)
            .and_then(|file| ws.source_path(&file))
            .is_ok()
        {
            return Some((left, right));
        }
        let prev_colon = i > 0 && bytes[i - 1] == b':';
        let next_colon = bytes.get(i + 1) == Some(&b':');
        if fallback.is_none() && !prev_colon && !next_colon {
            fallback = Some((left, right));
        }
    }
    fallback
}

/// Parses positional arguments to `expand` into `(file, selector)` pairs and
/// whether cross-file batch formatting (`=== FILE ===`) applies.
fn parse_expand_targets<'a>(
    ws: &Workspace,
    raw: &[&'a str],
    root_given: bool,
) -> Result<(Vec<(&'a str, &'a str)>, bool)> {
    match raw {
        [] => bail!("missing file and selector"),
        [only] => {
            if workspace_file(ws, only, root_given)
                .and_then(|file| ws.source_path(&file))
                .is_err()
                && let Some(pair) = split_file_selector(ws, only, root_given)
            {
                return Ok((vec![pair], false));
            }
            bail!(
                "missing selector; use `expand FILE SELECTOR` or `expand FILE1:SEL1 FILE2:SEL2 ...`"
            );
        }
        [first, second] => {
            // Preserve standard 2-arg `expand FILE SEL` whenever `first` is an
            // existing workspace file or cannot be split cleanly with `second`.
            if workspace_file(ws, first, root_given)
                .and_then(|file| ws.source_path(&file))
                .is_ok()
            {
                return Ok((vec![(first, second)], false));
            }
            if let (Some(a), Some(b)) = (
                split_file_selector(ws, first, root_given),
                split_file_selector(ws, second, root_given),
            ) {
                return Ok((vec![a, b], true));
            }
            Ok((vec![(first, second)], false))
        }
        many => {
            let mut pairs = Vec::with_capacity(many.len());
            for &spec in many {
                let pair = split_file_selector(ws, spec, root_given).with_context(|| {
                    format!("expected FILE:SELECTOR in batch expand, got `{spec}`")
                })?;
                pairs.push(pair);
            }
            Ok((pairs, true))
        }
    }
}

/// The `A-B` of an `LA-B` or `LA-LB` target.
fn line_range(target: &str) -> Option<&str> {
    target.strip_prefix('L').filter(|range| {
        range.starts_with(|c: char| c.is_ascii_digit())
            && range
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'L'))
    })
}

/// Split a comma-separated list of selectors (`REV#10,#11,#20`, `L1-5,L20-25`,
/// `foo,bar`) while leaving prose selectors such as Markdown headings containing
/// commas (`"Setup, usage, and tips"`) intact.
fn split_expand_selectors<'a>(source: &Snapshot, selector: &'a str) -> Vec<&'a str> {
    if !selector.contains(',') {
        return vec![selector];
    }
    let parts: Vec<&str> = selector.split(',').map(str::trim).collect();
    if parts.len() < 2
        || parts
            .iter()
            .any(|part| part.is_empty() || part.contains(char::is_whitespace))
    {
        return vec![selector];
    }
    let has_ref_syntax = parts
        .iter()
        .any(|part| part.contains('#') || line_range(split_selector(part).1).is_some());
    if !has_ref_syntax && source.entries.iter().any(|entry| entry.name == selector) {
        return vec![selector];
    }
    parts
}

/// Line ranges resolve without the outline, so they skip the parser.
fn needs_outline(selector: &str) -> bool {
    if selector.contains(',') {
        let parts: Vec<&str> = selector.split(',').map(str::trim).collect();
        if parts.len() >= 2
            && parts
                .iter()
                .all(|part| !part.is_empty() && !part.contains(char::is_whitespace))
        {
            return parts
                .iter()
                .any(|part| line_range(split_selector(part).1).is_none());
        }
    }
    line_range(split_selector(selector).1).is_none()
}

fn clipped(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    if max < 3 {
        return String::new();
    }
    let mut end = max - 3;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[derive(Serialize)]
pub struct Page {
    revision: String,
    lines: usize,
    total_entries: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_error: Option<String>,
    entries: Vec<Row>,
    next: Option<String>,
    #[serde(skip)]
    progress: Option<String>,
}
#[derive(Serialize)]
struct Row {
    id: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_id: Option<usize>,
    kind: String,
    name: String,
    first: usize,
    last: usize,
    signature: String,
}
impl Row {
    fn display(&self) -> String {
        if let Some(end_id) = self.end_id {
            format!(
                "#{}..#{} L{}-{} {} ({})\n",
                self.id, end_id, self.first, self.last, self.kind, self.name
            )
        } else if self.signature.is_empty() {
            format!(
                "#{} L{}-{} {} {}\n",
                self.id, self.first, self.last, self.kind, self.name
            )
        } else {
            format!(
                "#{} L{}-{} {} {}  {}\n",
                self.id, self.first, self.last, self.kind, self.name, self.signature
            )
        }
    }
}
impl Page {
    pub(crate) fn next_index(&self) -> Option<usize> {
        self.next
            .as_deref()?
            .split_once('#')?
            .1
            .parse::<usize>()
            .ok()
    }

    pub fn display(&self) -> String {
        let mut out = format!("revision {} · {} lines\n", self.revision, self.lines);
        if let Some(error) = &self.parse_error {
            out.push_str(error);
            out.push('\n');
        }
        for row in &self.entries {
            out.push_str(&row.display());
        }
        if let Some(next) = &self.next {
            if let Some(progress) = &self.progress {
                out.push_str(progress);
            }
            out.push_str(&format!("next --after {next}\n"));
        }
        out
    }
}

/// FILE as a workspace-relative name. A bare path comes from the current
/// directory like a shell path and falls back to the workspace root. With
/// `--root`, as hooks pass it, a bare path is root-relative first. Paths that
/// start with `./` or `../`, and absolute paths, always resolve from the
/// current directory.
fn workspace_file(ws: &Workspace, file: &str, root_given: bool) -> Result<String> {
    let path = Path::new(file);
    let bare = path
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    if bare && ws.cwd == ws.root {
        return Ok(file.to_owned());
    }
    let mut local = PathBuf::new();
    for part in ws.cwd.join(path).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                local.pop();
            }
            part => local.push(part),
        }
    }
    // Resolve the directory, such as macOS's /tmp, but not a linked file.
    if let (Some(parent), Some(name)) = (local.parent(), local.file_name())
        && let Ok(parent) = parent.canonicalize()
    {
        local = parent.join(name);
    }
    let relative = local
        .strip_prefix(&ws.root)
        .ok()
        .and_then(Path::to_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    if !bare {
        return relative.with_context(|| format!("{file} is outside the workspace"));
    }
    let Some(relative) = relative else {
        return Ok(file.to_owned());
    };
    let up = "../".repeat(
        ws.cwd
            .strip_prefix(&ws.root)
            .map_or(0, |dir| dir.iter().count()),
    );
    Ok(
        match (
            ws.source_path(&relative).is_ok(),
            ws.source_path(file).is_ok(),
            root_given,
        ) {
            (true, false, _) | (false, false, false) => relative,
            (false, true, _) => file.to_owned(),
            (true, true, false) => {
                eprintln!(
                    "justless: {file} is {relative}; use {up}{file} for the workspace root's file"
                );
                relative
            }
            (true, true, true) => {
                eprintln!(
                    "justless: {file} is the workspace root's file; use ./{file} for {relative}"
                );
                file.to_owned()
            }
            (false, false, true) => file.to_owned(),
        },
    )
}

fn read_many(ws: &Workspace, files: &[&str], args: &ArgMatches, root_given: bool) -> Result<()> {
    ensure!(
        args.value_of("shell-range").is_none(),
        "--shell-range applies to a single file"
    );
    ensure!(
        args.value_of("after").is_none(),
        "--after applies to a single file"
    );
    let total_budget = crate::context_budget(ws, args)?;
    let outline_forced = ["outline", "json", "match", "limit"]
        .iter()
        .any(|id| args.occurrences_of(id) > 0);
    let filter = args.value_of("match").unwrap_or("");
    let limit = crate::read_num(args, "limit", 1)?;
    let json_mode = args.is_present("json");
    let fresh = args.is_present("fresh");
    let mut memory = crate::seen::Memory::open(ws, args.value_of("session"), fresh);
    let can_dedup_outline = !json_mode
        && filter.is_empty()
        && args.occurrences_of("limit") == 0
        && args.occurrences_of("budget") == 0;

    let mut remaining = total_budget;
    let mut json_files = Vec::new();
    let mut text_out = String::new();

    for (idx, raw) in files.iter().enumerate() {
        let left = files.len() - idx;
        let share = (remaining / left).max(256);
        let file = workspace_file(ws, raw, root_given)?;
        let mut source = Snapshot::open(ws, &file)?;
        let digest = crate::capture::source_digest(source.text.as_bytes());
        if !json_mode && !outline_forced && source.text.len() <= share {
            let header = format!("=== {file} ===\n");
            let full_hint = format!(
                "full: {} read {} --fresh",
                ws.invocation().files,
                crate::workspace::shell_text(&file)
            );
            let deduped = if let Some(mem) = memory.as_mut()
                && !source.text.is_empty()
            {
                mem.show_range(
                    &file,
                    &source.revision,
                    &source.text,
                    1,
                    source.line_count(),
                    &source.text,
                    fresh,
                    &full_hint,
                )
            } else {
                None
            };
            text_out.push_str(&header);
            if let Some(short) = deduped {
                text_out.push_str(&short);
                remaining = remaining.saturating_sub(header.len() + short.len());
            } else {
                text_out.push_str(&source.text);
                if !source.text.ends_with('\n') {
                    text_out.push('\n');
                }
                let lines = source.text.lines().count();
                if lines > 0 {
                    crate::history::saw_expand(
                        ws,
                        &file,
                        1,
                        lines,
                        digest,
                        source.text.len() as u64,
                    );
                }
                remaining = remaining.saturating_sub(header.len() + source.text.len());
            }
            continue;
        }
        source.index(&file)?;
        let page = source.page(filter, None, limit, share)?;
        if json_mode {
            let mut value = serde_json::to_value(&page)?;
            if let Some(obj) = value.as_object_mut() {
                obj.insert("file".into(), serde_json::Value::String(file.clone()));
            }
            let shown_bytes = serde_json::to_vec(&value)?.len() as u64;
            crate::history::saw_outline_measured(
                ws,
                &file,
                digest,
                source.text.len() as u64,
                shown_bytes,
            );
            remaining = remaining.saturating_sub(shown_bytes as usize);
            json_files.push(value);
        } else {
            let header = format!("=== {file} ===\n");
            let rendered = page.display();
            let full_hint = format!(
                "full outline: {} read {} --outline --fresh",
                ws.invocation().files,
                crate::workspace::shell_text(&file)
            );
            let deduped = if can_dedup_outline && let Some(mem) = memory.as_mut() {
                mem.saw_outline(&file, &source.revision, &rendered, fresh, &full_hint)
            } else {
                None
            };
            text_out.push_str(&header);
            if let Some(notice) = deduped {
                text_out.push_str(&notice);
                remaining = remaining.saturating_sub(header.len() + notice.len());
            } else {
                let used = header.len() + rendered.len();
                text_out.push_str(&rendered);
                crate::history::saw_outline_measured(
                    ws,
                    &file,
                    digest,
                    source.text.len() as u64,
                    rendered.len() as u64,
                );
                remaining = remaining.saturating_sub(used);
            }
        }
    }

    if json_mode {
        let encoded = serde_json::to_vec(&serde_json::json!({ "files": json_files }))?;
        io::stdout().lock().write_all(&encoded)?;
    } else {
        print!("{text_out}");
    }
    if let Some(mem) = memory {
        mem.save(ws);
    }
    Ok(())
}

struct PreparedExpand {
    file: String,
    source: Snapshot,
    digest: String,
    selector: String,
    multi: bool,
    spans: Vec<CoalescedSpan>,
}

fn prepare_expand(
    ws: &Workspace,
    raw_file: &str,
    selector: &str,
    explicit_revision: Option<&str>,
    root_given: bool,
) -> Result<PreparedExpand> {
    let file = workspace_file(ws, raw_file, root_given)?;
    let mut source = Snapshot::open(ws, &file)?;
    if needs_outline(selector) {
        source.index(&file)?;
    }
    let parts = split_expand_selectors(&source, selector);
    let digest = crate::capture::source_digest(source.text.as_bytes());
    let multi = parts.len() > 1;
    let spans = if !multi {
        let resolved = source.resolve_span(parts[0], explicit_revision, 0)?;
        let label = match &resolved.id_label {
            Some(id) => format!("{id} L{}-{}", resolved.first, resolved.last),
            None => format!("L{}-{}", resolved.first, resolved.last),
        };
        vec![CoalescedSpan {
            span: resolved.span,
            first: resolved.first,
            last: resolved.last,
            anchor: resolved.anchor,
            label,
        }]
    } else {
        // Inherit an embedded revision from the first selector so callers can
        // write `REV#10,#11,#20` without repeating the 24-hex revision.
        let inherited = split_selector(parts[0]).0;
        let mut resolved = Vec::with_capacity(parts.len());
        for (order, part) in parts.into_iter().enumerate() {
            let rev = explicit_revision.or_else(|| split_selector(part).0.or(inherited));
            resolved.push(source.resolve_span(part, rev, order)?);
        }
        source.coalesce_spans(resolved)
    };
    Ok(PreparedExpand {
        file,
        source,
        digest,
        selector: selector.to_owned(),
        multi,
        spans,
    })
}

fn expand_json_value(prepared: &PreparedExpand) -> serde_json::Value {
    if !prepared.multi {
        let span = &prepared.spans[0];
        serde_json::json!({
            "file": prepared.file,
            "revision": prepared.source.revision,
            "selector": span.anchor,
            "first": span.first,
            "last": span.last,
            "content": &prepared.source.text[span.span.clone()],
        })
    } else {
        let spans: Vec<_> = prepared
            .spans
            .iter()
            .map(|span| {
                serde_json::json!({
                    "selector": span.anchor,
                    "first": span.first,
                    "last": span.last,
                    "content": &prepared.source.text[span.span.clone()],
                })
            })
            .collect();
        serde_json::json!({
            "file": prepared.file,
            "revision": prepared.source.revision,
            "spans": spans,
        })
    }
}

fn run_expand(ws: &Workspace, raw: &[&str], args: &ArgMatches, root_given: bool) -> Result<()> {
    let (targets, is_batch) = parse_expand_targets(ws, raw, root_given)?;
    let explicit_revision = args.value_of("revision");
    if is_batch {
        ensure!(
            explicit_revision.is_none(),
            "--revision applies to a single file; embed REV#... in each FILE:SELECTOR"
        );
    }
    // Resolve and verify every file and revision strictly before printing anything.
    let mut prepared = Vec::with_capacity(targets.len());
    for (raw_file, selector) in targets {
        prepared.push(prepare_expand(
            ws,
            raw_file,
            selector,
            explicit_revision,
            root_given,
        )?);
    }
    let json_mode = args.is_present("json");
    let fresh = args.is_present("fresh");
    let mut memory = crate::seen::Memory::open(ws, args.value_of("session"), fresh);

    if json_mode {
        if is_batch {
            let files: Vec<_> = prepared.iter().map(expand_json_value).collect();
            let encoded = serde_json::to_vec(&serde_json::json!({ "files": files }))?;
            io::stdout().lock().write_all(&encoded)?;
            for item in &prepared {
                let item_bytes = serde_json::to_vec(&expand_json_value(item))?.len() as u64;
                for (index, span) in item.spans.iter().enumerate() {
                    if let Some(mem) = memory.as_mut()
                        && item.source.is_full_lines(&span.span)
                    {
                        mem.record_range(
                            &item.file,
                            &item.source.revision,
                            &item.source.text,
                            span.first,
                            span.last,
                        );
                    }
                    let charged = if index == 0 { item_bytes } else { 0 };
                    crate::history::saw_expand(
                        ws,
                        &item.file,
                        span.first,
                        span.last,
                        item.digest.clone(),
                        charged,
                    );
                }
            }
        } else {
            let item = &prepared[0];
            let encoded = serde_json::to_vec(&expand_json_value(item))?;
            io::stdout().lock().write_all(&encoded)?;
            let shown_bytes = encoded.len() as u64;
            for (index, span) in item.spans.iter().enumerate() {
                if let Some(mem) = memory.as_mut()
                    && item.source.is_full_lines(&span.span)
                {
                    mem.record_range(
                        &item.file,
                        &item.source.revision,
                        &item.source.text,
                        span.first,
                        span.last,
                    );
                }
                let charged = if index == 0 { shown_bytes } else { 0 };
                crate::history::saw_expand(
                    ws,
                    &item.file,
                    span.first,
                    span.last,
                    item.digest.clone(),
                    charged,
                );
            }
        }
        if let Some(mem) = memory {
            mem.save(ws);
        }
        return Ok(());
    }

    if !is_batch && !prepared[0].multi {
        // Single-selector `expand FILE SEL`: 100% raw exact source bytes (zero headers)
        // unless `--session` returns an unchanged notice or line-level delta.
        let item = &prepared[0];
        let span = &item.spans[0];
        let content = &item.source.text[span.span.clone()];
        if let Some(mem) = memory.as_mut()
            && item.source.is_full_lines(&span.span)
        {
            let full_hint = format!(
                "full: {} expand {} {} --fresh",
                ws.invocation().files,
                crate::workspace::shell_text(&item.file),
                crate::workspace::shell_text(&item.selector)
            );
            if let Some(short) = mem.show_range(
                &item.file,
                &item.source.revision,
                &item.source.text,
                span.first,
                span.last,
                content,
                fresh,
                &full_hint,
            ) {
                io::stdout().lock().write_all(short.as_bytes())?;
                if let Some(mem) = memory {
                    mem.save(ws);
                }
                return Ok(());
            }
        }
        io::stdout().lock().write_all(content.as_bytes())?;
        crate::history::saw_expand(
            ws,
            &item.file,
            span.first,
            span.last,
            item.digest.clone(),
            content.len() as u64,
        );
        if let Some(mem) = memory {
            mem.save(ws);
        }
        return Ok(());
    }

    let mut out = String::new();
    let mut history_records: Vec<(String, usize, usize, String, u64)> = Vec::new();
    for item in &prepared {
        if is_batch {
            out.push_str("=== ");
            out.push_str(&item.file);
            out.push_str(" ===\n");
        }
        let item_start = out.len();
        let mut printed_whole = Vec::new();
        for span in &item.spans {
            let content = &item.source.text[span.span.clone()];
            let deduped = if let Some(mem) = memory.as_mut()
                && item.source.is_full_lines(&span.span)
            {
                let hint_sel = if item.multi {
                    span.anchor.as_str()
                } else {
                    item.selector.as_str()
                };
                let full_hint = format!(
                    "full: {} expand {} {} --fresh",
                    ws.invocation().files,
                    crate::workspace::shell_text(&item.file),
                    crate::workspace::shell_text(hint_sel)
                );
                mem.show_range(
                    &item.file,
                    &item.source.revision,
                    &item.source.text,
                    span.first,
                    span.last,
                    content,
                    fresh,
                    &full_hint,
                )
            } else {
                None
            };
            if let Some(short) = deduped {
                out.push_str(&short);
                continue;
            }
            if item.multi {
                out.push_str("--- ");
                out.push_str(&span.label);
                out.push_str(" ---\n");
            }
            out.push_str(content);
            if !content.ends_with('\n') {
                out.push('\n');
            }
            printed_whole.push((span.first, span.last));
        }
        let item_bytes = (out.len() - item_start) as u64;
        for (index, (first, last)) in printed_whole.into_iter().enumerate() {
            let charged = if index == 0 { item_bytes } else { 0 };
            history_records.push((item.file.clone(), first, last, item.digest.clone(), charged));
        }
    }
    io::stdout().lock().write_all(out.as_bytes())?;
    for (file, first, last, digest, charged) in history_records {
        crate::history::saw_expand(ws, &file, first, last, digest, charged);
    }
    if let Some(mem) = memory {
        mem.save(ws);
    }
    Ok(())
}

pub fn run(ws: &Workspace, name: &str, args: &ArgMatches, root_given: bool) -> Result<()> {
    if name == "read" {
        let files: Vec<&str> = args.values_of("file").unwrap().collect();
        if files.len() > 1 {
            return read_many(ws, &files, args, root_given);
        }
    }
    if name == "expand" {
        let raw: Vec<&str> = args.values_of("file").unwrap().collect();
        return run_expand(ws, &raw, args, root_given);
    }
    let argument = args.value_of("file").unwrap();
    if name == "read"
        && let Some(range) = args.value_of("shell-range")
    {
        // The hook passes the already resolved, workspace-relative file, or the
        // `git show` object name with the blob it resolved to.
        return crate::shellread::read(
            ws,
            argument,
            args.value_of("shell-object"),
            (
                range,
                args.value_of("shell-numbered")
                    .map(|numbering| match numbering {
                        "cat" => crate::shellread::Numbering::Cat,
                        _ => crate::shellread::Numbering::Nl,
                    }),
            ),
            args.value_of("shell-session").unwrap_or("terminal"),
        );
    }
    let file = &workspace_file(ws, argument, root_given)?;
    let mut source = Snapshot::open(ws, file)?;
    if name == "read" {
        let budget = crate::context_budget(ws, args)?;
        let outline = ["outline", "json", "match", "after", "limit"]
            .iter()
            .any(|id| args.occurrences_of(id) > 0);
        let digest = crate::capture::source_digest(source.text.as_bytes());
        let fresh = args.is_present("fresh");
        let mut memory = crate::seen::Memory::open(ws, args.value_of("session"), fresh);
        if !outline && source.text.len() <= budget {
            if let Some(mem) = memory.as_mut()
                && !source.text.is_empty()
            {
                let full_hint = format!(
                    "full: {} read {} --fresh",
                    ws.invocation().files,
                    crate::workspace::shell_text(file)
                );
                if let Some(short) = mem.show_range(
                    file,
                    &source.revision,
                    &source.text,
                    1,
                    source.line_count(),
                    &source.text,
                    fresh,
                    &full_hint,
                ) {
                    io::stdout().lock().write_all(short.as_bytes())?;
                    if let Some(mem) = memory {
                        mem.save(ws);
                    }
                    return Ok(());
                }
            }
            io::stdout().lock().write_all(source.text.as_bytes())?;
            // The whole file, recorded as `expand FILE L1-N` would record it.
            let lines = source.text.lines().count();
            if lines > 0 {
                crate::history::saw_expand(ws, file, 1, lines, digest, source.text.len() as u64);
            }
            if let Some(mem) = memory {
                mem.save(ws);
            }
            return Ok(());
        }
        source.index(file)?;
        let page = source.page(
            args.value_of("match").unwrap_or(""),
            args.value_of("after"),
            crate::read_num(args, "limit", 1)?,
            budget,
        )?;
        let can_dedup_outline = !args.is_present("json")
            && args.value_of("match").is_none()
            && args.value_of("after").is_none()
            && args.occurrences_of("limit") == 0
            && args.occurrences_of("budget") == 0;
        if args.is_present("json") {
            let encoded = serde_json::to_vec(&page)?;
            io::stdout().lock().write_all(&encoded)?;
            crate::history::saw_outline_measured(
                ws,
                file,
                digest,
                source.text.len() as u64,
                encoded.len() as u64,
            );
        } else {
            let rendered = page.display();
            if can_dedup_outline && let Some(mem) = memory.as_mut() {
                let full_hint = format!(
                    "full outline: {} read {} --outline --fresh",
                    ws.invocation().files,
                    crate::workspace::shell_text(file)
                );
                if let Some(notice) =
                    mem.saw_outline(file, &source.revision, &rendered, fresh, &full_hint)
                {
                    print!("{notice}");
                    if let Some(mem) = memory {
                        mem.save(ws);
                    }
                    return Ok(());
                }
            }
            print!("{rendered}");
            crate::history::saw_outline_measured(
                ws,
                file,
                digest,
                source.text.len() as u64,
                rendered.len() as u64,
            );
        }
        if let Some(mem) = memory {
            mem.save(ws);
        }
    } else {
        return edit(ws, file, source, args);
    }
    Ok(())
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    #[default]
    Replace,
    Before,
    After,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    selector: String,
    replacement: String,
    #[serde(default)]
    mode: Mode,
}

fn edit(ws: &Workspace, file: &str, mut source: Snapshot, args: &ArgMatches) -> Result<()> {
    let mut input = String::new();
    match args.value_of("replacement-file") {
        Some(path) => input = fs::read_to_string(path)?,
        None => {
            io::stdin().read_to_string(&mut input)?;
        }
    }
    let edits: Vec<Edit> = if args.is_present("batch") {
        serde_json::from_str(&input)?
    } else {
        vec![Edit {
            selector: args.value_of("selector").unwrap().into(),
            replacement: input,
            mode: if args.is_present("before") {
                Mode::Before
            } else if args.is_present("after") {
                Mode::After
            } else {
                Mode::Replace
            },
        }]
    };
    ensure!(!edits.is_empty(), "empty batch");
    if edits.iter().any(|edit| needs_outline(&edit.selector)) {
        source.index(file)?;
    }
    let mut resolved = Vec::new();
    for edit in edits {
        let span = source.resolve(&edit.selector, args.value_of("revision"), true)?;
        let span = match edit.mode {
            Mode::Replace => span,
            Mode::Before => {
                let at = source.line_aligned_start(span.start);
                at..at
            }
            Mode::After => {
                let at = source.line_aligned_end(span.end);
                at..at
            }
        };
        ensure!(!edit.replacement.contains('\0'), "replacement must be text");
        resolved.push((span, edit.replacement));
    }
    resolved.sort_by_key(|(range, _)| (range.start, range.end));
    for pair in resolved.windows(2) {
        ensure!(
            pair[0].0.end <= pair[1].0.start && pair[0].0.start != pair[1].0.start,
            "overlapping edits; batch rejected"
        );
    }
    let mut edited = String::new();
    let mut offset = 0;
    for (range, replacement) in &resolved {
        edited.push_str(&source.text[offset..range.start]);
        edited.push_str(replacement);
        offset = range.end;
    }
    edited.push_str(&source.text[offset..]);
    ensure!(
        edited.len() as u64 <= ws.settings()?.max_file_bytes,
        "result exceeds max_file_bytes"
    );
    let mut shift = 0isize;
    let changes: Vec<Change> = resolved
        .iter()
        .map(|(range, replacement)| {
            let start = range.start.saturating_add_signed(shift);
            shift += replacement.len() as isize - range.len() as isize;
            Change {
                before: range.clone(),
                after: start..start + replacement.len(),
            }
        })
        .collect();
    let verdict = if edited == source.text {
        None
    } else {
        syntax_check::judge(&ws.root, file, Some(&source.text), &edited, &changes, true)
    };
    // Only a certain problem in a file that parsed blocks the edit. Others are
    // reported on stderr after it is written.
    let rejected = matches!(verdict, Some(Verdict::Introduced { certain: true, .. }))
        && !args.is_present("allow-parse-errors");
    let dry_run = args.is_present("dry-run");
    if rejected && !dry_run {
        bail!(rejection(ws, file, &edited, &changes, verdict.as_ref()));
    }
    ws.prepare_state()?;
    let _lock = lock_state(&ws.state.join("edit.lock"))?;
    let path = ws.source_path(file)?;
    if dry_run {
        ensure!(
            fs::read(&path)? == source.text.as_bytes(),
            "stale revision; file changed during edit"
        );
        preview(file, &path, &source.text, &edited)?;
        if rejected {
            bail!(rejection(ws, file, &edited, &changes, verdict.as_ref()));
        }
        report(file, &source.text, &edited, verdict.as_ref());
        return Ok(());
    }
    // Opening for writing applies the same permission check as a shell write.
    let mut target = File::options()
        .write(true)
        .open(&path)
        .with_context(|| format!("{file} is not writable"))?;
    let replacement = path
        .parent()
        .and_then(|dir| NamedTempFile::new_in(dir).ok())
        .filter(|temp| replaceable(&path, temp));
    let unchanged = || -> Result<()> {
        ensure!(
            fs::read(ws.source_path(file)?)? == source.text.as_bytes(),
            "stale revision; file changed during edit"
        );
        Ok(())
    };
    match replacement {
        Some(mut temp) if edited != source.text => {
            temp.write_all(edited.as_bytes())?;
            temp.as_file()
                .set_permissions(fs::metadata(&path)?.permissions())?;
            unchanged()?;
            // A bind-mounted file, for one, cannot be renamed over.
            if temp.persist(&path).is_err() {
                write_in_place(&mut target, edited.as_bytes())?;
            }
        }
        _ => {
            unchanged()?;
            if edited != source.text {
                write_in_place(&mut target, edited.as_bytes())?;
            }
        }
    }
    println!(
        "{} edits · revision {}",
        resolved.len(),
        revision_for(file, edited.as_bytes())
    );
    report(file, &source.text, &edited, verdict.as_ref());
    Ok(())
}

/// Why an edit that leaves the file unparseable was rejected: the parser's
/// location and message, a few lines around it, the lines the edit wrote and
/// the flag that writes it anyway. Secrets in the quoted lines are masked.
fn rejection(
    ws: &Workspace,
    file: &str,
    edited: &str,
    changes: &[Change],
    verdict: Option<&Verdict>,
) -> String {
    let Some(Verdict::Introduced {
        problem,
        unbalanced,
        ..
    }) = verdict
    else {
        return "edit rejected".into();
    };
    let mut out = format!(
        "edit rejected: it leaves {file} unparseable, and the file is unchanged\n{}\n",
        syntax_check::located(file, edited, problem)
    );
    let (line, column) = syntax_check::line_column(edited, problem.bytes.start);
    let mut lines: Vec<&str> = edited.split('\n').collect();
    // The empty piece after a final newline is no line, unless the error is there.
    if lines.len() > line && lines.last() == Some(&"") {
        lines.pop();
    }
    let first = line.saturating_sub(2).max(1);
    let last = (line + 2).min(lines.len());
    let width = last.to_string().len();
    for number in first..=last {
        let text = lines[number - 1].trim_end_matches('\r');
        let text = clipped(text, 160);
        out.push_str(&format!("{number:>width$} | {text}\n"));
        if number == line {
            // Keep tabs so the caret lines up under the column.
            let pad: String = text
                .chars()
                .take(column - 1)
                .map(|c| if c == '\t' { '\t' } else { ' ' })
                .collect();
            out.push_str(&format!("{:>width$} | {pad}^\n", ""));
        }
    }
    let mut written: Vec<String> = changes
        .iter()
        .map(|change| {
            let start = syntax_check::line_column(edited, change.after.start).0;
            let end = syntax_check::line_column(edited, change.after.end.saturating_sub(1))
                .0
                .max(start);
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect();
    written.dedup();
    let more = written.len().saturating_sub(4);
    written.truncate(4);
    let mut lines = written.join(", ");
    if more > 0 {
        lines.push_str(&format!(" and {more} more"));
    }
    out.push_str(&format!(
        "The edit wrote line{} {lines}. The file parsed before it{}. Fix the replacement, or pass --allow-parse-errors to write it anyway.",
        if lines.contains(['-', ',', ' ']) { "s" } else { "" },
        if *unbalanced {
            ", and its brackets no longer pair up"
        } else {
            ""
        }
    ));
    match crate::redact::for_workspace(ws) {
        Some(redactor) => redactor.text(&out).into_owned(),
        None => out,
    }
}

/// Tells stderr how an edit changed a file's parse state when there is news: it
/// broke the file, or the file did not parse before the edit.
fn report(file: &str, before: &str, after: &str, verdict: Option<&Verdict>) {
    let at = |text: &str, problem: &syntax_check::Problem| {
        let (line, column) = syntax_check::line_column(text, problem.bytes.start);
        format!("{line}:{column}")
    };
    let message = match verdict {
        Some(Verdict::Introduced {
            problem,
            parsed_before: true,
            certain: true,
            ..
        }) => format!(
            "{file} no longer parses: {}",
            syntax_check::located(file, after, problem)
        ),
        Some(Verdict::Introduced {
            problem,
            parsed_before: true,
            ..
        }) => format!(
            "warning: {file} may no longer parse: {}. The edit was written, because this parser may not know newer syntax or the file's macros. Check the lines it wrote.",
            syntax_check::located(file, after, problem)
        ),
        Some(Verdict::Introduced { problem, .. }) => format!(
            "{file} did not parse before this edit and now also fails in the text it wrote: {}",
            syntax_check::located(file, after, problem)
        ),
        Some(Verdict::Fixed { was }) => format!(
            "{file} parses after this edit; before it, it failed at {}",
            at(before, was)
        ),
        Some(Verdict::Unchanged { problem }) => format!(
            "{file} did not parse before this edit and still fails where it did: {}",
            syntax_check::located(file, after, problem)
        ),
        Some(Verdict::Moved { was, now }) => format!(
            "{file} did not parse before this edit (it failed at {}) and now fails at {}",
            at(before, was),
            syntax_check::located(file, after, now)
        ),
        Some(Verdict::GaveUp) => {
            format!("warning: {file} was not parse-checked, because parsing it took too long")
        }
        Some(Verdict::Clean) | None => return,
    };
    eprintln!("justless: {message}");
}

/// Whether renaming `temp` over `path` keeps the file's hard links, owner,
/// group and extended attributes. Otherwise the edit is written in place.
#[cfg(unix)]
fn replaceable(path: &Path, temp: &NamedTempFile) -> bool {
    use std::os::unix::fs::{MetadataExt, fchown};
    let Ok(old) = fs::metadata(path) else {
        return false;
    };
    // Only root can keep another owner, but a member can keep the group.
    if fchown(temp.as_file(), Some(old.uid()), Some(old.gid())).is_err() {
        let _ = fchown(temp.as_file(), None, Some(old.gid()));
    }
    let Ok(new) = temp.as_file().metadata() else {
        return false;
    };
    old.nlink() == 1
        && (old.uid(), old.gid()) == (new.uid(), new.gid())
        && keeps_attributes(path, temp.path())
}

#[cfg(not(unix))]
fn replaceable(_: &Path, _: &NamedTempFile) -> bool {
    true
}

/// Whether the new file already has every extended attribute name the old
/// one has, such as those the system adds to every new file.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn keeps_attributes(old: &Path, new: &Path) -> bool {
    let names = |path: &Path| {
        let mut list = vec![0; rustix::fs::listxattr_query(path).unwrap_or(0)];
        let len = rustix::fs::listxattr(path, &mut list[..]).unwrap_or(0);
        list.truncate(len);
        list.split(|&byte| byte == 0)
            .filter(|name| !name.is_empty())
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>()
    };
    let kept = names(new);
    names(old).iter().all(|name| kept.contains(name))
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
fn keeps_attributes(_: &Path, _: &Path) -> bool {
    true
}

/// Overwrites the file's bytes, keeping its inode and metadata, at the cost
/// of the atomic rename.
fn write_in_place(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()
}

/// Prints the edit as a Git patch whose headers name the workspace-relative
/// file, so `git apply` can apply it from the workspace root.
fn preview(file: &str, path: &Path, before: &str, after: &str) -> Result<()> {
    let sides = tempfile::tempdir()?;
    let permissions = fs::metadata(path)?.permissions();
    for (side, text) in [("a", before), ("b", after)] {
        let copy = sides.path().join(side).join(file);
        fs::create_dir_all(copy.parent().context("expected source directory")?)?;
        fs::write(&copy, text)?;
        fs::set_permissions(&copy, permissions.clone())?;
    }
    let result = Command::new("git")
        .current_dir(sides.path())
        .args([
            "diff",
            "--no-index",
            "--no-prefix",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--",
        ])
        .arg(Path::new("a").join(file))
        .arg(Path::new("b").join(file))
        .output()
        .context("run git diff")?;
    ensure!(
        matches!(result.status.code(), Some(0 | 1)),
        "git diff failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(io::stdout().lock().write_all(&result.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_selector_and_entry_range_parse_inclusive_ranges() {
        assert_eq!(entry_range("1..#12"), Some((1, 12)));
        assert_eq!(entry_range("3..7"), Some((3, 7)));
        assert_eq!(entry_range("12"), None);
        assert_eq!(entry_range("a..#b"), None);

        assert_eq!(split_selector("#1..#12"), (None, "1..#12"));
        assert_eq!(split_selector("#1..12"), (None, "1..12"));
        let rev = "0123456789abcdef01234567";
        let full = format!("{rev}#1..#12");
        assert_eq!(split_selector(&full), (Some(rev), "1..#12"));
        assert_eq!(split_selector("#5"), (None, "5"));
    }

    #[test]
    fn entry_range_resolution_and_validation() {
        let text = concat!(
            "use std::fs;\n",
            "use std::io;\n",
            "use std::path::Path;\n",
            "use std::fmt;\n\n",
            "pub fn alpha() -> usize { 1 }\n",
            "pub fn beta() -> usize { 2 }\n",
        );
        let mut snap = Snapshot::from_text("src/lib.rs", text.to_owned());
        snap.index("src/lib.rs").unwrap();
        let rev = snap.revision.clone();

        let imports = snap.resolve(&format!("{rev}#1..#4"), None, false).unwrap();
        assert_eq!(
            &snap.text[imports],
            "use std::fs;\nuse std::io;\nuse std::path::Path;\nuse std::fmt;"
        );

        let single = snap.resolve("#5..#5", None, false).unwrap();
        assert_eq!(&snap.text[single], "pub fn alpha() -> usize { 1 }");

        assert!(snap.resolve("#0..#2", None, false).is_err());
        assert!(snap.resolve("#4..#2", None, false).is_err());
        assert!(snap.resolve("#1..#99", None, false).is_err());
        assert!(
            snap.resolve("000000000000000000000000#1..#4", None, false)
                .is_err()
        );
    }

    #[test]
    fn coalesce_spans_merges_containment_and_overlaps() {
        let text = concat!(
            "mod outer {\n",
            "    pub fn inside() -> usize {\n",
            "        1\n",
            "    }\n",
            "}\n",
            "pub fn after() -> usize {\n",
            "    2\n",
            "}\n",
        );
        let mut snap = Snapshot::from_text("src/lib.rs", text.to_owned());
        snap.index("src/lib.rs").unwrap();
        let rev = snap.revision.clone();

        // Parent `#1` (mod outer) contains `#2` (outer::inside).
        let r1 = snap.resolve_span(&format!("{rev}#1"), None, 0).unwrap();
        let r2 = snap.resolve_span("#2", Some(&rev), 1).unwrap();
        let merged = snap.coalesce_spans(vec![r1, r2]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].label, "#1 L1-5 (includes #2)");
        assert_eq!(
            &snap.text[merged[0].span.clone()],
            "mod outer {\n    pub fn inside() -> usize {\n        1\n    }\n}"
        );

        // Overlapping line ranges `L1-4,L3-7` merge into `L1-7`.
        let l1 = snap.resolve_span("L1-4", None, 0).unwrap();
        let l2 = snap.resolve_span("L3-7", None, 1).unwrap();
        let merged_lines = snap.coalesce_spans(vec![l1, l2]);
        assert_eq!(merged_lines.len(), 1);
        assert_eq!(merged_lines[0].label, "L1-7");
        assert_eq!(merged_lines[0].first, 1);
        assert_eq!(merged_lines[0].last, 7);
    }

    #[test]
    fn windows_drive_prefix_detection_avoids_splitting_drive_colon() {
        assert!(has_windows_drive_prefix(r"C:\repo\src\lib.rs:L1-10"));
        assert!(has_windows_drive_prefix("D:/repo/src/lib.rs:Foo::bar"));
        assert!(!has_windows_drive_prefix("a:L1-10"));
        assert!(!has_windows_drive_prefix("src/lib.rs:L1-10"));
    }

    #[test]
    fn outline_folds_five_plus_single_line_constants_and_slims_plain_rust_types() {
        let mut text = String::new();
        for i in 1..=6 {
            text.push_str(&format!("const K_{i}: usize = {i};\n"));
        }
        text.push_str("struct Local {\n    x: usize,\n}\n");
        text.push_str("pub struct Exported {\n    pub x: usize,\n}\n");
        let mut snap = Snapshot::from_text("src/cfg.rs", text);
        snap.index("src/cfg.rs").unwrap();

        let page = snap.page("", None, 40, 4096).unwrap().display();
        assert!(page.contains("#1..#6 L1-6 constants (6 items)\n"), "{page}");
        assert!(page.contains("#7 L7-9 struct Local\n"), "{page}");
        assert!(
            page.contains("#8 L10-12 struct Exported  pub struct Exported\n"),
            "{page}"
        );

        // `--match` bypasses folding and still finds individual constants and slimmed structs.
        let matched = snap.page("K_4", None, 40, 4096).unwrap().display();
        assert!(matched.contains("#4 L4-4 constant K_4"), "{matched}");
        assert!(!matched.contains("constants ("), "{matched}");

        let matched_struct = snap.page("struct Local", None, 40, 4096).unwrap().display();
        assert!(
            matched_struct.contains("#7 L7-9 struct Local\n"),
            "{matched_struct}"
        );
    }

    #[test]
    fn markdown_outline_eliminates_heading_duplication_on_docs_reference() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/reference.md");
        let text = fs::read_to_string(&path).unwrap();
        let mut snap = Snapshot::from_text("docs/reference.md", text.clone());
        snap.index("docs/reference.md").unwrap();

        // Compute the legacy redundant heading-only outline bytes for apples-to-apples comparison.
        let lines: Vec<&str> = text.lines().collect();
        let legacy_sections = crate::markdown::sections(&lines);
        let mut legacy_heading_bytes = 0usize;
        let mut legacy_page1_rows = 0usize;
        let mut legacy_page1_bytes =
            format!("revision {} · {} lines\n", snap.revision, snap.lines.len()).len();
        let reserve = 80;
        for (idx, sec) in legacy_sections.iter().enumerate() {
            let raw = lines[sec.heading.first..=sec.heading.last]
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let row = format!(
                "#{} L{}-{} heading {}  {}\n",
                idx + 1,
                sec.heading.first + 1,
                sec.end,
                clipped(&sec.heading.title, 100),
                clipped(&raw, 160)
            );
            legacy_heading_bytes += row.len();
            if legacy_page1_rows < 40 && legacy_page1_bytes + reserve + row.len() <= 2400 {
                legacy_page1_bytes += row.len();
                legacy_page1_rows += 1;
            }
        }

        let full_page = snap.page("", None, 500, 100_000).unwrap();
        let full_display = full_page.display();
        let new_heading_bytes: usize = full_page
            .entries
            .iter()
            .filter(|row| matches!(row.kind.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6"))
            .map(|row| {
                assert!(
                    row.signature.is_empty(),
                    "redundant signature on {}",
                    row.display()
                );
                row.display().len()
            })
            .sum();
        // Eliminating `heading ...  ## ...` cuts heading bytes by >40% (~45%).
        assert!(
            new_heading_bytes * 100 <= legacy_heading_bytes * 60,
            "expected >=40% heading byte reduction: before={legacy_heading_bytes}, after={new_heading_bytes}"
        );
        // Even with new table and code anchors included, the full outline is smaller than legacy headings alone.
        assert!(
            full_display.len() < legacy_heading_bytes,
            "expected full outline ({}) < legacy headings ({legacy_heading_bytes})",
            full_display.len()
        );
        assert!(!full_display.contains(" heading "), "{full_display}");

        // Default page 1 (limit=40, budget=2400) fits more entries in fewer bytes than legacy.
        let page1 = snap.page("", None, 40, 2400).unwrap();
        assert!(
            page1.entries.len() > legacy_page1_rows,
            "expected page 1 to fit more entries: new={} vs legacy={legacy_page1_rows}",
            page1.entries.len()
        );
        assert!(
            page1.display().len() < legacy_page1_bytes,
            "expected page 1 bytes ({}) < legacy ({legacy_page1_bytes})",
            page1.display().len()
        );

        // Verify real `table` and `code` anchors exist in `docs/reference.md` and expand cleanly.
        let rev = snap.revision.clone();
        let tables: Vec<_> = full_page
            .entries
            .iter()
            .filter(|row| row.kind == "table")
            .collect();
        let codes: Vec<_> = full_page
            .entries
            .iter()
            .filter(|row| row.kind == "code")
            .collect();
        assert!(
            !tables.is_empty(),
            "expected large tables in docs/reference.md"
        );
        assert!(
            !codes.is_empty(),
            "expected large fenced code blocks in docs/reference.md"
        );
        for row in tables {
            let span = snap
                .resolve(&format!("{rev}#{}", row.id), None, false)
                .unwrap();
            let expanded = &snap.text[span];
            assert!(expanded.trim_start().starts_with('|'), "{expanded}");
            assert!(row.signature.is_empty());
        }
        for row in codes {
            let span = snap
                .resolve(&format!("{rev}#{}", row.id), None, false)
                .unwrap();
            let expanded = &snap.text[span];
            let trimmed = expanded.trim_start();
            assert!(
                trimmed.starts_with("```") || trimmed.starts_with("~~~"),
                "{expanded}"
            );
            assert!(row.signature.is_empty());
        }
    }

    #[test]
    fn markdown_frontmatter_setext_code_and_table_anchors_expand_and_edit_exact_bytes() {
        let mut fence = String::from("```yaml title=\"pipeline\"\n");
        for i in 1..=13 {
            fence.push_str(&format!("step_{i}: run_{i}\n"));
        }
        fence.push_str("```\n");

        let mut table = String::from("| Flag | Effect |\n| --- | --- |\n");
        for i in 1..=8 {
            table.push_str(&format!("| --opt-{i} | sets {i} |\n"));
        }

        let frontmatter = "---\ntitle: Spec\ndescription: Exact bytes\ntags:\n  - md\n---\n";
        let setext = "Overview\n========\n\nIntro paragraph.\n\n";
        let section2 = "## Details\n\n";
        let doc = format!("{frontmatter}{setext}{section2}{fence}\n{table}");

        let mut snap = Snapshot::from_text("guide.md", doc.clone());
        snap.index("guide.md").unwrap();
        let rev = snap.revision.clone();
        let page = snap.page("", None, 40, 4096).unwrap().display();

        assert!(
            page.contains("#1 L1-6 frontmatter keys: title, description, tags\n"),
            "{page}"
        );
        assert!(page.contains("#2 L7-39 h1 Overview\n"), "{page}");
        assert!(page.contains("#3 L12-39 h2 Details\n"), "{page}");
        assert!(page.contains("#4 L14-28 code yaml\n"), "{page}");
        assert!(
            page.contains("#5 L30-39 table Flag | Effect (8 rows)\n"),
            "{page}"
        );
        // Setext underline and redundant fence/table/heading headers must never leak.
        assert!(!page.contains("========"), "{page}");
        assert!(!page.contains("## Details  ##"), "{page}");
        assert!(!page.contains("```yaml"), "{page}");

        // `--match` still finds blocks via their raw header text.
        assert!(
            snap.page("Exact bytes", None, 40, 4096)
                .unwrap()
                .display()
                .contains("frontmatter")
        );
        assert!(
            snap.page("pipeline", None, 40, 4096)
                .unwrap()
                .display()
                .contains("code yaml")
        );
        assert!(
            snap.page("## Details", None, 40, 4096)
                .unwrap()
                .display()
                .contains("h2 Details")
        );
        assert!(
            snap.page("Effect", None, 40, 4096)
                .unwrap()
                .display()
                .contains("table Flag | Effect")
        );

        // Exact byte expand round-trips for frontmatter, code block, and table.
        let fm_span = snap.resolve(&format!("{rev}#1"), None, true).unwrap();
        assert_eq!(&snap.text[fm_span.clone()], frontmatter);
        let code_span = snap.resolve(&format!("{rev}#4"), None, true).unwrap();
        assert_eq!(&snap.text[code_span.clone()], fence);
        let table_span = snap.resolve(&format!("{rev}#5"), None, true).unwrap();
        assert_eq!(&snap.text[table_span.clone()], table);

        // Simulated verified edit on frontmatter and table anchors replaces exact bytes.
        let new_fm = "---\ntitle: Updated\n---\n";
        let edited_fm = format!("{}{new_fm}{}", &doc[..fm_span.start], &doc[fm_span.end..]);
        assert!(edited_fm.starts_with("---\ntitle: Updated\n---\nOverview\n========\n"));

        let new_table = "| Flag | Effect |\n| --- | --- |\n| --all | done |\n";
        let edited_table = format!(
            "{}{new_table}{}",
            &doc[..table_span.start],
            &doc[table_span.end..]
        );
        assert!(edited_table.ends_with(&format!("{fence}\n{new_table}")));
    }

    #[test]
    fn window_applies_slim_signature_to_markdown_and_rust() {
        // Markdown ranged window: headings, code, and table have no duplicate signatures.
        let mut md = String::new();
        for i in 1..=6 {
            md.push_str(&format!("## Section {i}\n\n"));
            for j in 1..=10 {
                md.push_str(&format!("Paragraph {i}.{j}.\n"));
            }
            md.push('\n');
        }
        let mut md_snap = Snapshot::from_text("doc.md", md);
        md_snap.index("doc.md").unwrap();
        let md_window = md_snap
            .window(1, 70)
            .expect("markdown window")
            .display(true);
        assert!(md_window.contains("#1 L1-13 h2 Section 1\n"), "{md_window}");
        assert!(
            md_window.contains("#2 L14-26 h2 Section 2\n"),
            "{md_window}"
        );
        assert!(!md_window.contains("  ## Section"), "{md_window}");

        // Rust ranged window: plain types slimmed, `pub` types keep their signature.
        let mut rs = String::new();
        for i in 1..=6 {
            rs.push_str(&format!("struct Local{i} {{\n"));
            for f in 1..=4 {
                rs.push_str(&format!("    f{f}: usize,\n"));
            }
            rs.push_str("}\n");
            rs.push_str(&format!("pub struct Exported{i} {{\n"));
            for f in 1..=4 {
                rs.push_str(&format!("    pub f{f}: usize,\n"));
            }
            rs.push_str("}\n");
        }
        let mut rs_snap = Snapshot::from_text("src/types.rs", rs);
        rs_snap.index("src/types.rs").unwrap();
        let rs_window = rs_snap.window(1, 70).expect("rust window").display(true);
        assert!(rs_window.contains("#1 L1-6 struct Local1\n"), "{rs_window}");
        assert!(
            rs_window.contains("#2 L7-12 struct Exported1  pub struct Exported1\n"),
            "{rs_window}"
        );
        assert!(!rs_window.contains("  struct Local1"), "{rs_window}");
    }
}
