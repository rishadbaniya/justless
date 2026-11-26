use std::collections::BTreeSet;
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
            .about("Read a small file, or outline files or directories")
            .arg(Arg::new("file").required(true).multiple_values(true))
            .arg(Arg::new("outline").long("outline"))
            .arg(
                Arg::new("shell-range")
                    .long("shell-range")
                    .value_name("A-B|+N")
                    .hidden(true)
                    .conflicts_with_all(&["outline", "json", "match", "after", "depth"]),
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
            .arg(
                Arg::new("depth")
                    .long("depth")
                    .takes_value(true)
                    .value_name("N")
                    .help("Maximum directory recursion depth"),
            )
            .arg(session_arg())
            .arg(fresh_arg())
            .arg(crate::number("limit", "40", "Maximum entries per page"))
            .arg(crate::budget().alias("max-bytes")),
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
    /// Whether this snapshot was indexed as a Jupyter notebook (`.ipynb`).
    is_notebook: bool,
    /// Tree-sitter grammar used when indexed as a polyglot source file.
    grammar: Option<Grammar>,
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
            is_notebook: false,
            grammar: None,
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
            is_notebook: false,
            grammar: None,
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
        let mut tried_sections = false;
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
        } else if prefers_hcl_config_outline(file) {
            self.sections(file);
            tried_sections = true;
        } else if let Some(grammar) = Grammar::detect(file, &self.text) {
            self.grammar = Some(grammar);
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
        if self.entries.is_empty() && !tried_sections {
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
        let ext = Path::new(file)
            .extension()
            .and_then(|x| x.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(ext.as_deref(), Some("md" | "markdown" | "mdx")) {
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
        if ext.as_deref() == Some("ipynb") {
            let entries = notebook_entries(&self.text);
            if !entries.is_empty() {
                self.is_notebook = true;
                self.entries.extend(entries);
                return;
            }
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

    /// Trims strictly redundant outline signatures while preserving all semantic
    /// details (visibility, modifiers, generics, parameters, extends/implements,
    /// return types, and receivers):
    /// - Imports strip a redundant leading `import ` / `use ` / `from ` keyword
    ///   (preserving modifiers such as `pub use` and distinct directives like
    ///   `alias`, `#import`, `#include`, or `using`).
    /// - Markdown blocks drop signatures already conveyed by their kind and name.
    /// - Plain Rust types drop `{kind} {name}` when no modifiers or generics follow.
    /// - Polyglot declarations strip only the redundant leading `{kw} {name}`
    ///   (preserving `export`, `export default`, `async`, and all other modifiers).
    fn slim_signature(&self, entry: &Entry, signature: String) -> String {
        if entry.kind == "import" {
            for kw in ["import ", "use ", "from "] {
                if let Some(rest) = signature.strip_prefix(kw) {
                    let trimmed = rest.trim_start();
                    if !trimmed.is_empty() {
                        return trimmed.to_owned();
                    }
                }
            }
            return signature;
        }
        if self.is_markdown
            && matches!(
                entry.kind.as_str(),
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "frontmatter" | "code" | "table"
            )
        {
            return String::new();
        }
        let short = entry.name.rsplit("::").next().unwrap_or(&entry.name);
        if self.is_rust && matches!(entry.kind.as_str(), "struct" | "enum" | "trait" | "union") {
            if signature == format!("{} {short}", entry.kind) {
                return String::new();
            }
            return signature;
        }
        if let Some(grammar) = self.grammar
            && !short.is_empty()
        {
            return slim_polyglot_signature(grammar, &entry.kind, short, signature);
        }
        signature
    }

    fn fold_group(&self, entry: &Entry) -> Option<(&'static str, usize)> {
        if entry.kind == "import" {
            Some(("imports", 3))
        } else if entry.kind == "module" && self.text[entry.bytes.clone()].trim_end().ends_with(';')
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
    }

    fn row_for(&self, index: usize, entry: &Entry, raw_signature: String) -> Row {
        let signature = self.slim_signature(entry, raw_signature);
        let max_name = if self.is_notebook { 160 } else { 100 };
        let name = if entry.kind == "import" && entry.name == "import" {
            String::new()
        } else {
            clipped(&entry.name, max_name)
        };
        Row {
            id: index + 1,
            end_id: None,
            name,
            kind: entry.kind.clone(),
            first: self.line(entry.bytes.start),
            last: self.line(entry.bytes.end.saturating_sub(1)),
            signature: clipped(&signature, 160),
        }
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
        let terms = parse_match_terms(&filter);
        let words = |raw: &str| raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut index = start;
        while index < self.entries.len() {
            let e = &self.entries[index];
            // Fold runs of 3+ contiguous imports or external `mod name;` declarations,
            // or 5+ contiguous single-line constants/statics/variables, in unfiltered
            // outlines so large preamble blocks don't crowd real declarations off page 1.
            // Individual `#N` entry IDs stay unchanged for `expand` and `edit`, and
            // folded blocks expand directly with `#A..#B`.
            let (row, end_index) = if filter.is_empty()
                && let Some((group, min_run)) = self.fold_group(e)
            {
                let mut run_end = index + 1;
                while run_end < self.entries.len()
                    && self.fold_group(&self.entries[run_end]) == Some((group, min_run))
                    && (group != "constants"
                        || entry_scope_prefix(e) == entry_scope_prefix(&self.entries[run_end]))
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
                    let row =
                        self.row_for(index, e, words(&self.text[e.signature_start..e.header_end]));
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
                let row = self.row_for(index, e, signature);
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

    /// Slimmed, folded outline rows for a directory skeleton.
    ///
    /// Preserves every `#N` / `#A..#B` ID and `L<first>-<last>` range for direct
    /// `justless expand FILE REV#N` follow-up, folds 3+ imports/modules and 5+
    /// single-line constants, and (when unfiltered) collapses inline Rust test
    /// modules into a single module row so production API signatures stay first.
    fn skeleton_rows(&self, filter: &str) -> Vec<Row> {
        let filter_lower = filter.to_lowercase();
        let terms = parse_match_terms(&filter_lower);
        let words = |raw: &str| raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut rows = Vec::new();
        let mut index = 0usize;
        while index < self.entries.len() {
            let e = &self.entries[index];
            let (row, end_index) = if terms.is_empty() {
                if self.is_rust && self.is_test_module(e) {
                    let mut run_end = index + 1;
                    while run_end < self.entries.len()
                        && self.entries[run_end].bytes.start >= e.bytes.start
                        && self.entries[run_end].bytes.end <= e.bytes.end
                    {
                        run_end += 1;
                    }
                    let inner = run_end - (index + 1);
                    if inner > 0 {
                        let row = Row {
                            id: index + 1,
                            end_id: None,
                            name: clipped(&e.name, 100),
                            kind: e.kind.clone(),
                            first: self.line(e.bytes.start),
                            last: self.line(e.bytes.end.saturating_sub(1)),
                            signature: format!("({inner} inner)"),
                        };
                        (row, run_end - 1)
                    } else if let Some((group, min_run)) = self.fold_group(e) {
                        self.folded_or_single_row(index, e, group, min_run, &words)
                    } else {
                        let row = self.row_for(
                            index,
                            e,
                            words(&self.text[e.signature_start..e.header_end]),
                        );
                        (row, index)
                    }
                } else if let Some((group, min_run)) = self.fold_group(e) {
                    self.folded_or_single_row(index, e, group, min_run, &words)
                } else {
                    let row =
                        self.row_for(index, e, words(&self.text[e.signature_start..e.header_end]));
                    (row, index)
                }
            } else {
                let signature = words(&self.text[e.signature_start..e.header_end]);
                let name = e.name.to_lowercase();
                let header = words(&self.text[e.bytes.start..e.header_end]).to_lowercase();
                if !terms
                    .iter()
                    .any(|term| name.contains(term) || header.contains(term))
                {
                    index += 1;
                    continue;
                }
                (self.row_for(index, e, signature), index)
            };
            rows.push(row);
            index = end_index + 1;
        }
        rows
    }

    fn is_test_module(&self, entry: &Entry) -> bool {
        entry.kind == "module"
            && (entry.name == "tests"
                || entry.name.ends_with("::tests")
                || self
                    .text
                    .get(entry.bytes.start..entry.signature_start)
                    .is_some_and(|prefix| prefix.contains("cfg(test)")))
    }

    fn folded_or_single_row(
        &self,
        index: usize,
        e: &Entry,
        group: &'static str,
        min_run: usize,
        words: &impl Fn(&str) -> String,
    ) -> (Row, usize) {
        let mut run_end = index + 1;
        while run_end < self.entries.len()
            && self.fold_group(&self.entries[run_end]) == Some((group, min_run))
            && (group != "constants"
                || entry_scope_prefix(e) == entry_scope_prefix(&self.entries[run_end]))
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
            let row = self.row_for(index, e, words(&self.text[e.signature_start..e.header_end]));
            (row, index)
        }
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
        let overlapping: Vec<(usize, usize, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let lines = (
                    self.line(entry.bytes.start),
                    self.line(entry.bytes.end.saturating_sub(1)),
                );
                (lines.0 <= last && lines.1 >= first).then_some((index, lines.0, lines.1))
            })
            .collect();
        if overlapping.is_empty() {
            return None;
        }
        // Judge whether one individual declaration holds more than half the
        // window BEFORE folding runs of imports/modules/constants so a large
        // import preamble folds cleanly instead of falling back to raw text.
        let mut held = vec![0; overlapping.len()];
        for line in first..=last {
            let narrowest = (0..overlapping.len())
                .filter(|&pos| overlapping[pos].1 <= line && line <= overlapping[pos].2)
                .min_by_key(|&pos| overlapping[pos].2 - overlapping[pos].1);
            if let Some(pos) = narrowest {
                held[pos] += 1;
            }
        }
        let span = last - first + 1;
        if !held.iter().all(|&count| count * 2 <= span) {
            return None;
        }
        let words = |raw: &str| raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut rows = Vec::with_capacity(overlapping.len());
        let mut pos = 0;
        while pos < overlapping.len() {
            let (entry_index, first_line, _) = overlapping[pos];
            let entry = &self.entries[entry_index];
            if let Some((group, min_run)) = self.fold_group(entry) {
                let mut run_end = pos + 1;
                while run_end < overlapping.len()
                    && overlapping[run_end].0 == overlapping[run_end - 1].0 + 1
                    && self.fold_group(&self.entries[overlapping[run_end].0])
                        == Some((group, min_run))
                    && (group != "constants"
                        || entry_scope_prefix(entry)
                            == entry_scope_prefix(&self.entries[overlapping[run_end].0]))
                {
                    run_end += 1;
                }
                let run_len = run_end - pos;
                if run_len >= min_run {
                    let (last_entry_index, _, last_line) = overlapping[run_end - 1];
                    rows.push(Row {
                        id: entry_index + 1,
                        end_id: Some(last_entry_index + 1),
                        name: format!("{run_len} items"),
                        kind: group.into(),
                        first: first_line,
                        last: last_line,
                        signature: String::new(),
                    });
                    pos = run_end;
                    continue;
                }
            }
            rows.push(self.row_for(
                entry_index,
                entry,
                words(&self.text[entry.signature_start..entry.header_end]),
            ));
            pos += 1;
        }
        Some(Window {
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
            out.push_str(&row.display_with_signatures(signatures));
        }
        out
    }
}

/// Zero-dependency structural outline entries for Jupyter notebooks (`.ipynb`).
/// Strips base64/HTML cell outputs from the outline while surfacing markdown headings,
/// code signatures, execution errors, and stripped media notes with exact cell byte spans.
fn notebook_entries(text: &str) -> Vec<Entry> {
    use serde_json::Value;
    let Ok(Value::Object(root)) =
        serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(text))
    else {
        return Vec::new();
    };
    let Some(cells) = root
        .get("cells")
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty() && c.iter().all(Value::is_object))
    else {
        return Vec::new();
    };
    let Some(spans) = notebook_cell_spans(text).filter(|s| s.len() == cells.len()) else {
        return Vec::new();
    };
    cells
        .iter()
        .zip(spans)
        .map(|(cell, span)| {
            let (kind, name) = notebook_cell_summary(cell);
            Entry {
                name,
                kind: kind.into(),
                signature_start: span.start,
                header_end: span.start,
                bytes: span,
            }
        })
        .collect()
}

/// Exact `[start..end]` byte spans of each element in the top-level `"cells"` array.
fn notebook_cell_spans(text: &str) -> Option<Vec<Range<usize>>> {
    let b = text.as_bytes();
    let mut pos = if text.starts_with('\u{feff}') { 3 } else { 0 };
    let skip_ws = |p: &mut usize| {
        while b.get(*p).is_some_and(u8::is_ascii_whitespace) {
            *p += 1;
        }
    };
    let skip_str = |p: &mut usize| -> Option<Range<usize>> {
        (b.get(*p) == Some(&b'"')).then_some(())?;
        let start = *p;
        *p += 1;
        while *p < b.len() {
            match b[*p] {
                b'\\' => *p = (*p + 2 <= b.len()).then_some(*p + 2)?,
                b'"' => {
                    *p += 1;
                    return Some(start..*p);
                }
                _ => *p += 1,
            }
        }
        None
    };
    let skip_val = |p: &mut usize| -> Option<()> {
        skip_ws(p);
        match *b.get(*p)? {
            b'"' => skip_str(p).map(|_| ()),
            b'{' | b'[' => {
                let mut depth = 0usize;
                while *p < b.len() {
                    match b[*p] {
                        b'"' => {
                            skip_str(p)?;
                        }
                        b'{' | b'[' => {
                            depth += 1;
                            *p += 1;
                        }
                        b'}' | b']' => {
                            depth = depth.checked_sub(1)?;
                            *p += 1;
                            if depth == 0 {
                                return Some(());
                            }
                        }
                        _ => *p += 1,
                    }
                }
                None
            }
            _ => {
                let start = *p;
                while b
                    .get(*p)
                    .is_some_and(|c| !c.is_ascii_whitespace() && !matches!(c, b',' | b'}' | b']'))
                {
                    *p += 1;
                }
                (*p > start).then_some(())
            }
        }
    };

    skip_ws(&mut pos);
    (b.get(pos) == Some(&b'{')).then_some(())?;
    pos += 1;
    let mut found = None;
    loop {
        skip_ws(&mut pos);
        if b.get(pos) == Some(&b'}') {
            return found;
        }
        let key: String = serde_json::from_str(&text[skip_str(&mut pos)?]).ok()?;
        skip_ws(&mut pos);
        (b.get(pos) == Some(&b':')).then_some(())?;
        pos += 1;
        skip_ws(&mut pos);
        if key == "cells" {
            (b.get(pos) == Some(&b'[')).then_some(())?;
            pos += 1;
            let mut spans = Vec::new();
            loop {
                skip_ws(&mut pos);
                if b.get(pos) == Some(&b']') {
                    pos += 1;
                    break;
                }
                let start = pos;
                skip_val(&mut pos)?;
                spans.push(start..pos);
                skip_ws(&mut pos);
                match b.get(pos) {
                    Some(b',') => pos += 1,
                    Some(b']') => {
                        pos += 1;
                        break;
                    }
                    _ => return None,
                }
            }
            found = Some(spans);
        } else {
            skip_val(&mut pos)?;
        }
        skip_ws(&mut pos);
        match b.get(pos) {
            Some(b',') => pos += 1,
            Some(b'}') => return found,
            _ => return None,
        }
    }
}

fn notebook_cell_summary(cell: &serde_json::Value) -> (&'static str, String) {
    use serde_json::Value;
    let source = match cell.get("source") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    };
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    match cell.get("cell_type").and_then(Value::as_str) {
        Some("markdown") => {
            let (mut in_fence, mut first) = (false, None);
            for line in source.lines().map(str::trim).filter(|l| !l.is_empty()) {
                first.get_or_insert_with(|| words(line));
                if line.starts_with("```") || line.starts_with("~~~") {
                    in_fence = !in_fence;
                } else if !in_fence {
                    let h = line.bytes().take_while(|&b| b == b'#').count();
                    if (1..=6).contains(&h)
                        && line[h..].starts_with([' ', '\t'])
                        && !line[h..].trim().is_empty()
                    {
                        return ("md", clipped(&words(line), 80));
                    }
                }
            }
            (
                "md",
                clipped(&first.unwrap_or_else(|| "(empty)".into()), 80),
            )
        }
        Some("code") => {
            let exec = cell
                .get("execution_count")
                .and_then(Value::as_i64)
                .map_or_else(|| "[ ]".into(), |n| format!("[{n}]"));
            let (mut sig, mut first_code, mut first_any) = (None, None, None);
            for line in source.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let c = words(line);
                first_any.get_or_insert_with(|| c.clone());
                if !line.starts_with('#') {
                    if ["def ", "async def ", "class "]
                        .iter()
                        .any(|p| line.starts_with(p))
                    {
                        sig = Some(c);
                        break;
                    }
                    first_code.get_or_insert(c);
                }
            }
            let base = format!(
                "{exec} {}",
                sig.or(first_code)
                    .or(first_any)
                    .unwrap_or_else(|| "(empty)".into())
            );
            let suffix = notebook_outputs_suffix(cell.get("outputs"));
            let cap = 160usize.saturating_sub(suffix.len()).min(80);
            ("cell", format!("{}{suffix}", clipped(&base, cap)))
        }
        _ => {
            let first = source
                .lines()
                .map(words)
                .find(|l| !l.is_empty())
                .unwrap_or_else(|| "(empty)".into());
            ("raw", clipped(&first, 80))
        }
    }
}

fn notebook_outputs_suffix(outputs: Option<&serde_json::Value>) -> String {
    use serde_json::Value;
    let Some(items) = outputs.and_then(Value::as_array) else {
        return String::new();
    };
    let (mut err_msg, mut media_bytes, mut text_bytes) = (None::<String>, 0usize, 0usize);
    let (mut best_media, mut best_text) = (None::<(u8, usize, &str)>, None::<(usize, &str)>);

    for out in items {
        match out.get("output_type").and_then(Value::as_str).unwrap_or("") {
            "error" if err_msg.is_none() => {
                let field = |k| {
                    out.get(k)
                        .and_then(Value::as_str)
                        .map(strip_ansi_ws)
                        .unwrap_or_default()
                };
                let (en, ev) = (field("ename"), field("evalue"));
                err_msg = Some(match (en.is_empty(), ev.is_empty()) {
                    (false, false) => format!("{en}: {ev}"),
                    (false, true) => en,
                    (true, false) => ev,
                    (true, true) => "error".into(),
                });
            }
            "display_data" | "execute_result" => {
                let Some(data) = out.get("data").and_then(Value::as_object) else {
                    continue;
                };
                let (mut best, mut rich_sum, mut plain_sum) = (None, 0usize, 0usize);
                for (mime, val) in data {
                    let size = json_payload_bytes(val);
                    if size == 0 {
                        continue;
                    }
                    let prio = if mime.starts_with("image/") {
                        3
                    } else if mime == "text/html"
                        || ["application/", "audio/", "video/"]
                            .iter()
                            .any(|p| mime.starts_with(p))
                    {
                        2
                    } else {
                        0
                    };
                    if prio > 0 {
                        rich_sum += size;
                        if best.is_none_or(|(p, s, _)| (prio, size) > (p, s)) {
                            best = Some((prio, size, mime.as_str()));
                        }
                    } else {
                        plain_sum += size;
                    }
                }
                if let Some((p, s, m)) = best {
                    media_bytes += rich_sum;
                    if best_media.is_none_or(|(bp, bs, _)| (p, s) > (bp, bs)) {
                        best_media = Some((p, s, m));
                    }
                } else if plain_sum > 0 {
                    text_bytes += plain_sum;
                    if best_text.is_none_or(|(bs, _)| plain_sum > bs) {
                        best_text = Some((plain_sum, "text/plain"));
                    }
                }
            }
            "stream" => {
                let size = out.get("text").map_or(0, json_payload_bytes);
                if size > 0 {
                    text_bytes += size;
                    let name = out.get("name").and_then(Value::as_str).unwrap_or("stdout");
                    if best_text.is_none_or(|(bs, _)| size > bs) {
                        best_text = Some((size, name));
                    }
                }
            }
            _ => {}
        }
    }

    let mut suffix = String::new();
    if let Some(err) = err_msg {
        suffix.push_str(&format!(" [error: {}]", clipped(&err, 56)));
    }
    if let Some((_, _, mime)) = best_media {
        let total = crate::history::human_bytes((media_bytes + text_bytes) as u64);
        suffix.push_str(&format!(" (+output: {total} {})", clipped(mime, 24)));
    } else if text_bytes >= 512
        && let Some((_, label)) = best_text
    {
        let total = crate::history::human_bytes(text_bytes as u64);
        suffix.push_str(&format!(" (+output: {total} {})", clipped(label, 24)));
    }
    suffix
}

fn json_payload_bytes(val: &serde_json::Value) -> usize {
    match val {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(arr) => arr.iter().map(json_payload_bytes).sum(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| k.len() + json_payload_bytes(v))
            .sum(),
        _ => 0,
    }
}

fn strip_ansi_ws(s: &str) -> String {
    let (mut out, b, mut i) = (String::with_capacity(s.len()), s.as_bytes(), 0);
    while i < b.len() {
        if b[i] == 0x1b && b.get(i + 1) == Some(&b'[') {
            i += 2;
            while b.get(i).is_some_and(|c| (0x20..=0x3f).contains(c)) {
                i += 1;
            }
            if b.get(i).is_some_and(|c| (0x40..=0x7e).contains(c)) {
                i += 1;
            }
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_match_terms(filter_lower: &str) -> Vec<&str> {
    if !filter_lower.is_empty() && !filter_lower.contains(' ') && filter_lower.contains('|') {
        let parts: Vec<&str> = filter_lower.split('|').collect();
        if parts.iter().all(|part| !part.is_empty()) {
            parts
        } else {
            vec![filter_lower]
        }
    } else if filter_lower.is_empty() {
        Vec::new()
    } else {
        vec![filter_lower]
    }
}

fn entry_scope_prefix(entry: &Entry) -> Option<&str> {
    entry.name.rsplit_once("::").map(|(prefix, _)| prefix)
}

fn is_signature_ident_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | '?' | '!')
}

fn polyglot_keywords(kind: &str) -> &'static [&'static str] {
    match kind {
        "class" => &["class"],
        "interface" => &["interface"],
        "struct" => &["struct"],
        "enum" => &["enum"],
        "trait" => &["trait"],
        "protocol" => &["protocol", "defprotocol"],
        "module" => &["module", "defmodule"],
        "object" => &["object"],
        "mixin" => &["mixin"],
        "extension" => &["extension"],
        "actor" => &["actor"],
        "function" | "method" => &["function", "def", "func", "fun", "fn"],
        "macro" => &["macro", "defmacro"],
        "type" => &["type", "typealias"],
        _ => &[],
    }
}

/// Strips strictly redundant leading `{kw} {short}` from Tree-sitter outline
/// signatures while keeping visibility/modifiers (`export`, `export default`,
/// `async`, `pub`, `final`, `abstract`, `case`, `defp`, `local`, `static`, …),
/// generics, parameters, extends/implements, receivers, and return types intact.
fn slim_polyglot_signature(grammar: Grammar, kind: &str, short: &str, signature: String) -> String {
    // Go `type_spec` headers start with the type name (`Server struct`, `Handler interface`,
    // `Cache[K comparable, V any] struct`, `UserID string`). Strip only the repeated name,
    // preserving `struct`/`interface`/generics/underlying type.
    if grammar == Grammar::Go
        && kind == "type"
        && let Some(rest) = signature.strip_prefix(short)
        && rest
            .chars()
            .next()
            .is_none_or(|c| !is_signature_ident_char(c))
    {
        return rest.trim_start().to_owned();
    }
    if matches!(kind, "struct" | "interface") && signature == format!("type {short} {kind}") {
        return String::new();
    }
    if kind == "test" && signature == format!("test {short}") {
        return String::new();
    }
    let keywords = polyglot_keywords(kind);
    if keywords.is_empty() {
        return signature;
    }
    const PRESERVED_PREFIXES: &[&str] = &[
        "export default async ",
        "export default ",
        "export async ",
        "export ",
        "async ",
        "",
    ];
    for &prefix in PRESERVED_PREFIXES {
        let Some(after_prefix) = signature.strip_prefix(prefix) else {
            continue;
        };
        for &kw in keywords {
            let Some(after_kw) = after_prefix
                .strip_prefix(kw)
                .and_then(|s| s.strip_prefix(' '))
            else {
                continue;
            };
            let Some(after_name) = after_kw.strip_prefix(short) else {
                continue;
            };
            if !after_name
                .chars()
                .next()
                .is_none_or(|c| !is_signature_ident_char(c))
            {
                continue;
            }
            let tail = after_name.trim_start();
            // A bare Python block colon (`class Foo:`) adds no info once `class Foo` is shown.
            let tail = if tail == ":" { "" } else { tail };
            if prefix.is_empty() {
                return tail.to_owned();
            }
            // Never turn `export type X = Y` into `export = Y` (distinct CommonJS syntax).
            if tail.starts_with('=') {
                return signature;
            }
            let kept = prefix.trim_end();
            return if tail.is_empty() {
                kept.to_owned()
            } else {
                format!("{kept} {tail}")
            };
        }
    }
    signature
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
#[derive(Clone, Serialize)]
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
        self.display_with_signatures(true)
    }

    fn display_with_signatures(&self, signatures: bool) -> String {
        if let Some(end_id) = self.end_id {
            format!(
                "#{}..#{} L{}-{} {} ({})\n",
                self.id, end_id, self.first, self.last, self.kind, self.name
            )
        } else if self.name.is_empty() {
            if !signatures || self.signature.is_empty() {
                format!("#{} L{}-{} {}\n", self.id, self.first, self.last, self.kind)
            } else {
                format!(
                    "#{} L{}-{} {} {}\n",
                    self.id, self.first, self.last, self.kind, self.signature
                )
            }
        } else if !signatures || self.signature.is_empty() {
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

fn common_dir_prefix(paths: &[&str]) -> String {
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

/// Returns a hoistable `base: <dir>/` prefix for multi-file `read_many` output
/// when >= 2 files share a >= 2-segment directory prefix and hoisting strictly
/// saves bytes over repeating the prefix in every `=== FILE ===` header.
fn hoistable_read_prefix(paths: &[String]) -> Option<String> {
    if paths.len() < 2 {
        return None;
    }
    if paths
        .iter()
        .any(|path| crate::workspace::display_path(path) != *path)
    {
        return None;
    }
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let common = common_dir_prefix(&refs);
    let depth = common
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .count();
    if depth < 2 {
        return None;
    }
    let base_line_bytes = "base: ".len() + common.len() + 1;
    let repeated_bytes = paths.len() * common.len();
    (base_line_bytes < repeated_bytes).then_some(common)
}

fn read_many_resolved(ws: &Workspace, resolved: &[String], args: &ArgMatches) -> Result<()> {
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

    let base_prefix = if json_mode {
        None
    } else {
        hoistable_read_prefix(resolved)
    };

    let mut remaining = total_budget;
    let mut json_files = Vec::new();
    let mut text_out = String::new();
    if let Some(base) = &base_prefix {
        let line = format!("base: {base}\n");
        remaining = remaining.saturating_sub(line.len());
        text_out.push_str(&line);
    }

    for (idx, file) in resolved.iter().enumerate() {
        let left = resolved.len() - idx;
        let share = (remaining / left).max(256);
        let header_name = base_prefix
            .as_deref()
            .and_then(|base| file.strip_prefix(base))
            .unwrap_or(file.as_str());
        let mut source = Snapshot::open(ws, file)?;
        let digest = crate::capture::source_digest(source.text.as_bytes());
        if !json_mode && !outline_forced && source.text.len() <= share {
            let header = format!("=== {header_name} ===\n");
            let full_hint = format!(
                "full: {} read {} --fresh",
                ws.invocation().files,
                crate::workspace::shell_text(file)
            );
            let deduped = if let Some(mem) = memory.as_mut()
                && !source.text.is_empty()
            {
                mem.show_range(
                    file,
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
                        file,
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
        source.index(file)?;
        let page = source.page(filter, None, limit, share)?;
        if json_mode {
            let mut value = serde_json::to_value(&page)?;
            if let Some(obj) = value.as_object_mut() {
                obj.insert("file".into(), serde_json::Value::String(file.clone()));
            }
            let shown_bytes = serde_json::to_vec(&value)?.len() as u64;
            crate::history::saw_outline_measured(
                ws,
                file,
                digest,
                source.text.len() as u64,
                shown_bytes,
            );
            remaining = remaining.saturating_sub(shown_bytes as usize);
            json_files.push(value);
        } else {
            let header = format!("=== {header_name} ===\n");
            let rendered = page.display();
            let full_hint = format!(
                "full outline: {} read {} --outline --fresh",
                ws.invocation().files,
                crate::workspace::shell_text(file)
            );
            let deduped = if can_dedup_outline && let Some(mem) = memory.as_mut() {
                mem.saw_outline(file, &source.revision, &rendered, fresh, &full_hint)
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
                    file,
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

/// Target resolved for `justless read`: either a workspace file or a workspace
/// directory to render as a budgeted code skeleton.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadTarget {
    File(String),
    Dir {
        /// Workspace-relative directory using `/`; empty string for workspace root.
        relative: String,
        /// Canonical on-disk path strictly inside `ws.root`.
        canonical: PathBuf,
    },
}

/// Minimum per-file floor (header + ~2–3 declarations + truncation line) used when
/// choosing how many files fit fairly into a directory skeleton budget.
const SKELETON_MIN_PER_FILE: usize = 220;

/// Cap on files parsed eagerly during an unfiltered directory walk of a very
/// large tree; remaining files are still counted in totals and summarized.
const SKELETON_MAX_EAGER_PARSE: usize = 512;

fn lexical_path(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            part => clean.push(part),
        }
    }
    clean
}

fn path_to_slash_string(path: &Path) -> Option<String> {
    path.components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

/// Checks whether `raw`, resolved from `base`, names an existing directory
/// strictly inside `ws.root` without crossing symlinks.
///
/// Returns:
/// - `Ok(Some((relative, canonical)))` when it is a valid workspace directory
/// - `Ok(None)` when it is a regular file or does not exist
/// - `Err(_)` when it escapes the workspace or traverses a symlink
fn workspace_dir_at(
    ws: &Workspace,
    base: &Path,
    raw: &str,
    bare: bool,
) -> Result<Option<(String, PathBuf)>> {
    ensure!(!raw.is_empty(), "expected a workspace file or directory");
    let mut local = lexical_path(&base.join(raw));
    if let (Some(parent), Some(name)) = (local.parent(), local.file_name())
        && let Ok(parent) = parent.canonicalize()
    {
        local = parent.join(name);
    }
    let Ok(rel) = local.strip_prefix(&ws.root) else {
        if !bare {
            bail!("{raw} is outside the workspace");
        }
        return Ok(None);
    };
    let rel_buf = rel.to_path_buf();
    if fs::symlink_metadata(&local).is_err() {
        return Ok(None);
    }
    // Reject any symlink component below `ws.root`, preserving the same boundary
    // contract as `Workspace::source_path`.
    let mut cur = ws.root.clone();
    for part in rel_buf.components() {
        cur.push(part);
        if fs::symlink_metadata(&cur).is_ok_and(|meta| meta.file_type().is_symlink()) {
            if cur.canonicalize().is_ok_and(|c| !c.starts_with(&ws.root)) {
                bail!("{raw} is outside the workspace");
            }
            bail!("expected a workspace file or directory: {raw}");
        }
    }
    let canonical = local
        .canonicalize()
        .with_context(|| format!("open {raw}"))?;
    ensure!(
        canonical.starts_with(&ws.root),
        "{raw} is outside the workspace"
    );
    if canonical.is_dir() {
        let rel_str =
            path_to_slash_string(&rel_buf).with_context(|| format!("{raw} is not valid UTF-8"))?;
        return Ok(Some((rel_str, canonical)));
    }
    Ok(None)
}

fn resolve_read_target(ws: &Workspace, raw: &str, root_given: bool) -> Result<ReadTarget> {
    let path = Path::new(raw);
    let bare = !raw.is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    if !bare {
        if let Some((relative, canonical)) = workspace_dir_at(ws, &ws.cwd, raw, false)? {
            return Ok(ReadTarget::Dir {
                relative,
                canonical,
            });
        }
        return Ok(ReadTarget::File(workspace_file(ws, raw, root_given)?));
    }
    if ws.cwd == ws.root {
        if let Some((relative, canonical)) = workspace_dir_at(ws, &ws.root, raw, true)? {
            return Ok(ReadTarget::Dir {
                relative,
                canonical,
            });
        }
        return Ok(ReadTarget::File(raw.to_owned()));
    }
    // Check cwd and root in the same precedence order as `workspace_file`.
    let cwd_rel = ws
        .cwd
        .strip_prefix(&ws.root)
        .ok()
        .map(|sub| sub.join(path))
        .and_then(|p| path_to_slash_string(&p));
    let cwd_is_file = cwd_rel
        .as_deref()
        .is_some_and(|rel| ws.source_path(rel).is_ok());
    let root_is_file = ws.source_path(raw).is_ok();
    if cwd_is_file && root_is_file {
        return Ok(ReadTarget::File(workspace_file(ws, raw, root_given)?));
    }
    let bases: [(&Path, bool); 2] = if root_given {
        [(&ws.root, root_is_file), (&ws.cwd, cwd_is_file)]
    } else {
        [(&ws.cwd, cwd_is_file), (&ws.root, root_is_file)]
    };
    for (base, is_file) in bases {
        if is_file {
            return Ok(ReadTarget::File(workspace_file(ws, raw, root_given)?));
        }
        if let Some((relative, canonical)) = workspace_dir_at(ws, base, raw, true)? {
            return Ok(ReadTarget::Dir {
                relative,
                canonical,
            });
        }
    }
    Ok(ReadTarget::File(workspace_file(ws, raw, root_given)?))
}

fn prefers_hcl_config_outline(file: &str) -> bool {
    Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "terragrunt.hcl" | ".terraform.lock.hcl" | "terraform.lock.hcl"
            )
        })
}

fn is_excluded_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | ".hg" | ".svn" | ".justless" | "target" | "node_modules" | ".venv" | "__pycache__"
    )
}

fn is_lockfile(name: &str) -> bool {
    matches!(
        name,
        "Cargo.lock"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "bun.lock"
            | "bun.lockb"
            | "poetry.lock"
            | "Pipfile.lock"
            | "uv.lock"
            | "pdm.lock"
            | "composer.lock"
            | "Gemfile.lock"
            | "go.sum"
            | "flake.lock"
            | "mix.lock"
            | "pubspec.lock"
            | "Podfile.lock"
            | "Package.resolved"
            | "gradle.lockfile"
            | "packages.lock.json"
            | "paket.lock"
            | "deno.lock"
            | "shrinkwrap.yaml"
            | "terraform.lock.hcl"
            | ".terraform.lock.hcl"
    )
}

fn looks_binary_file(path: &Path) -> bool {
    let mut head = [0u8; 8192];
    File::open(path)
        .and_then(|mut file| file.read(&mut head))
        .map_or(true, |n| head[..n].contains(&0))
}

/// Builds a `Gitignore` matcher covering ignore files from `ws.root` down to
/// `dir_canonical.parent()` (strictly within `ws.root`, so ancestor ignore
/// rules outside the workspace never leak in). `dir_canonical` itself and its
/// subdirectories are handled natively by `ignore::WalkBuilder`.
fn build_ancestor_ignores(
    ws: &Workspace,
    dir_canonical: &Path,
) -> Result<ignore::gitignore::Gitignore> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(&ws.root);
    let git_exclude = ws.root.join(".git").join("info").join("exclude");
    if git_exclude.is_file() {
        let _ = builder.add(&git_exclude);
    }
    let Ok(rel) = dir_canonical.strip_prefix(&ws.root) else {
        return Ok(builder.build()?);
    };
    let components: Vec<_> = rel.components().collect();
    if components.is_empty() {
        // `dir_canonical == ws.root`; WalkBuilder handles `ws.root` itself.
        return Ok(builder.build()?);
    }
    let mut current = ws.root.clone();
    for (idx, part) in components.iter().enumerate() {
        for name in [".gitignore", ".justlessignore"] {
            let candidate = current.join(name);
            if candidate.is_file()
                && let Some(err) = builder.add(&candidate)
            {
                return Err(err).with_context(|| format!("read {}", candidate.display()));
            }
        }
        if idx + 1 < components.len() {
            current.push(part);
        }
    }
    Ok(builder.build()?)
}

/// Discovers all eligible non-binary, non-lockfile, non-ignored workspace files
/// under `dir_canonical`, sorted deterministically by `/`-normalized path.
fn collect_dir_files(
    ws: &Workspace,
    dir_rel: &str,
    dir_canonical: &Path,
    max_depth: Option<usize>,
) -> Result<Vec<String>> {
    if !dir_rel.is_empty() && dir_rel.split('/').any(is_excluded_dir) {
        return Ok(Vec::new());
    }
    let cap = ws.settings()?.max_file_bytes;
    let ancestor_ignores = build_ancestor_ignores(ws, dir_canonical)?;
    let mut walker = ignore::WalkBuilder::new(dir_canonical);
    walker
        .hidden(false)
        .require_git(false)
        .parents(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".justlessignore")
        .follow_links(false);
    if let Some(depth) = max_depth {
        walker.max_depth(Some(depth));
    }
    walker.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        if is_dir && is_excluded_dir(&entry.file_name().to_string_lossy()) {
            return false;
        }
        !ancestor_ignores
            .matched_path_or_any_parents(entry.path(), is_dir)
            .is_ignore()
    });

    let mut files = Vec::new();
    for entry in walker.build() {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        if is_lockfile(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(rel_path) = entry.path().strip_prefix(&ws.root) else {
            continue;
        };
        let Some(rel) = path_to_slash_string(rel_path) else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        let Ok(real) = ws.source_path(&rel) else {
            continue;
        };
        let Ok(meta) = real.metadata() else {
            continue;
        };
        if meta.len() > cap || looks_binary_file(&real) {
            continue;
        }
        files.push(rel);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn is_entrypoint_file(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    matches!(
        base,
        "lib.rs"
            | "mod.rs"
            | "main.rs"
            | "index.ts"
            | "index.tsx"
            | "index.js"
            | "index.mjs"
            | "__init__.py"
            | "main.go"
            | "README.md"
    )
}

fn path_structural_hint(path: &str) -> u8 {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if is_entrypoint_file(path) {
        return 4;
    }
    match ext.as_deref() {
        Some(
            "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "go" | "java" | "kt"
            | "kts" | "swift" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "cs" | "rb" | "php"
            | "scala" | "ex" | "exs" | "dart" | "zig" | "lua" | "jl" | "groovy" | "sql" | "tf"
            | "hcl" | "sh" | "bash" | "ps1",
        ) => 3,
        Some("md" | "markdown" | "mdx" | "toml" | "yaml" | "yml" | "json" | "ipynb") => 2,
        _ => 1,
    }
}

fn structural_priority(rel_path: &str, snapshot: &Snapshot, rows: &[Row]) -> (u8, usize, usize) {
    let real_decls = rows
        .iter()
        .filter(|row| {
            !matches!(
                row.kind.as_str(),
                "import" | "imports" | "modules" | "constants" | "text"
            ) && !(row.kind == "module" && row.signature.ends_with(" inner)"))
        })
        .count();
    let non_text = rows.iter().filter(|row| row.kind != "text").count();
    let tier = if (snapshot.is_rust || snapshot.grammar.is_some()) && real_decls > 0 {
        3
    } else if (snapshot.is_markdown || non_text > 0) && !rows.is_empty() {
        2
    } else if !rows.is_empty() {
        1
    } else {
        0
    };
    let entry_boost = usize::from(is_entrypoint_file(rel_path)) * 100;
    (tier, real_decls + entry_boost, snapshot.line_count())
}

struct SkeletonCandidate {
    /// Workspace-relative file path (with `/`).
    file: String,
    /// Display path relative to hoisted directory/base prefix.
    rel_path: String,
    revision: String,
    digest: String,
    lines: usize,
    raw_bytes: usize,
    parse_error: Option<String>,
    /// Slimmed, folded rows (up to `--limit`).
    rows: Vec<Row>,
    /// Total matching/folded rows before `--limit`.
    total_rows: usize,
    /// Structural priority `(tier, decl_score, lines)` for fair selection.
    priority: (u8, usize, usize),
}

impl SkeletonCandidate {
    fn header_line(&self) -> String {
        format!(
            "=== {} ({} lines) · rev {} ===\n",
            self.rel_path, self.lines, self.revision
        )
    }

    fn parse_error_line(&self) -> Option<String> {
        self.parse_error
            .as_deref()
            .map(|err| format!("{}\n", clipped(err, 160)))
    }

    fn full_bytes(&self) -> usize {
        let mut size = self.header_line().len();
        if let Some(err) = self.parse_error_line() {
            size += err.len();
        }
        for row in &self.rows {
            size += row.display().len();
        }
        size += skeleton_trunc_line(self.rows.len(), self.total_rows).len();
        size
    }

    fn min_useful_bytes(&self) -> usize {
        let mut size = self.header_line().len();
        if let Some(err) = self.parse_error_line() {
            size += err.len();
        }
        if let Some(first) = self.rows.first() {
            size += first.display().len();
            size += skeleton_trunc_line(1, self.total_rows).len();
        } else {
            size += skeleton_trunc_line(0, self.total_rows).len();
        }
        size
    }
}

struct OmittedSummaryEntry {
    file: String,
    rel_path: String,
    lines: usize,
    raw_bytes: usize,
    total_entries: usize,
    priority: (u8, usize, usize),
}

fn skeleton_row_priority(row: &Row) -> u8 {
    let is_public = row.signature.starts_with("pub ")
        || row.signature.starts_with("pub(")
        || row.signature.starts_with("export ")
        || row.signature == "export"
        || row.signature.starts_with("public ");
    match row.kind.as_str() {
        "import" | "imports" | "modules" => 0,
        "module" if row.signature.ends_with(" inner)") => 1,
        "constants" | "text" => 1,
        "struct" | "enum" | "trait" | "interface" | "class" | "union" | "type" | "table"
        | "protocol" | "record" | "object" => {
            if is_public {
                6
            } else {
                5
            }
        }
        "h1" | "h2" => 5,
        "h3" | "section" => 4,
        "cell" if row.name.contains("[error:") => 6,
        "md" | "cell" => 4,
        "function" | "method" => {
            if is_public {
                5
            } else if !row.name.contains("::") {
                4
            } else {
                3
            }
        }
        "module" | "namespace" => 3,
        "constant" | "const" | "static" | "variable" if is_public => 3,
        _ => 2,
    }
}

struct SelectedSkeletonFile {
    candidate_idx: usize,
    shown_rows: Vec<Row>,
    rendered_rows: Vec<String>,
    /// True when `shown_rows` is an unbroken prefix `0..shown_rows.len()` of `candidate.rows`.
    contiguous_prefix: bool,
}

struct SkeletonAllocation {
    header: String,
    base_line: String,
    shown: Vec<SelectedSkeletonFile>,
    omitted: Vec<OmittedSummaryEntry>,
    omitted_summary_line: Option<String>,
    next_line: Option<String>,
}

fn skeleton_header_line(
    target_label: &str,
    shown: usize,
    total_files: usize,
    total_lines: usize,
    total_bytes: u64,
    filter: &str,
) -> String {
    let raw = crate::history::human_bytes(total_bytes);
    if filter.is_empty() {
        format!(
            "skeleton {target_label} · {shown}/{total_files} files · {total_lines} lines ({raw} raw)\n"
        )
    } else {
        format!(
            "skeleton {target_label} · {shown}/{total_files} files · {total_lines} lines ({raw} raw) · match \"{filter}\"\n"
        )
    }
}

fn skeleton_trunc_line(shown: usize, total: usize) -> String {
    if shown >= total {
        String::new()
    } else {
        format!("… +{} more ({shown}/{total})\n", total - shown)
    }
}

fn format_omitted_footer(
    omitted: &[OmittedSummaryEntry],
    filter: &str,
    inv_files: &str,
    max_bytes: usize,
) -> (Option<String>, Option<String>) {
    if omitted.is_empty() || max_bytes < 24 {
        return (None, None);
    }
    let count = omitted.len();
    let rem_lines: usize = omitted.iter().map(|e| e.lines).sum();
    let rem_bytes: u64 = omitted.iter().map(|e| e.raw_bytes as u64).sum();
    let noun = match (filter.is_empty(), count == 1) {
        (true, true) => "file",
        (true, false) => "files",
        (false, true) => "matching file",
        (false, false) => "matching files",
    };
    let mut ranked: Vec<usize> = (0..omitted.len()).collect();
    ranked.sort_by(|&a, &b| {
        omitted[b]
            .priority
            .cmp(&omitted[a].priority)
            .then_with(|| omitted[a].file.cmp(&omitted[b].file))
    });
    let top_file = &omitted[ranked[0]].file;
    let next_candidate = format!(
        "Next: {inv_files} read {} --outline\n",
        crate::workspace::shell_text(top_file)
    );
    let base_summary = if rem_lines > 0 {
        format!(
            "… +{count} more {noun} ({rem_lines} lines, {} raw)",
            crate::history::human_bytes(rem_bytes)
        )
    } else {
        format!(
            "… +{count} more {noun} ({} raw)",
            crate::history::human_bytes(rem_bytes)
        )
    };
    let next_line =
        (next_candidate.len() + base_summary.len() < max_bytes).then_some(next_candidate.clone());
    let summary_cap = max_bytes.saturating_sub(next_line.as_ref().map_or(0, String::len));
    if base_summary.len() >= summary_cap {
        let minimal = format!("… +{count} more {noun}\n");
        let summary = (minimal.len() <= summary_cap).then_some(minimal);
        return (summary, next_line);
    }
    // Append highest-priority omitted filenames while staying within `summary_cap`.
    let mut line = base_summary;
    let mut listed = 0usize;
    for (pos, &idx) in ranked.iter().enumerate() {
        let entry = &omitted[idx];
        let item = if entry.lines > 0 {
            format!("{} ({}L)", entry.rel_path, entry.lines)
        } else {
            entry.rel_path.clone()
        };
        let sep = if pos == 0 { ": " } else { ", " };
        let tail_count = omitted.len() - (pos + 1);
        let tail = if tail_count > 0 {
            format!(", +{tail_count} more")
        } else {
            String::new()
        };
        if line.len() + sep.len() + item.len() + tail.len() < summary_cap {
            line.push_str(sep);
            line.push_str(&item);
            listed += 1;
        } else {
            break;
        }
    }
    let unlisted = omitted.len().saturating_sub(listed);
    if listed > 0 && unlisted > 0 {
        let tail = format!(", +{unlisted} more");
        if line.len() + tail.len() < summary_cap {
            line.push_str(&tail);
        }
    }
    line.push('\n');
    (Some(line), next_line)
}

fn clip_row_to_fit(row: &Row, max_bytes: usize) -> Option<(Row, String)> {
    let mut slim = row.clone();
    slim.signature.clear();
    let rendered = slim.display();
    if rendered.len() <= max_bytes {
        return Some((slim, rendered));
    }
    let overhead = rendered.len().saturating_sub(slim.name.len());
    if overhead >= max_bytes {
        return None;
    }
    let allowed_name = max_bytes - overhead;
    slim.name = clipped(&slim.name, allowed_name);
    let rendered = slim.display();
    (rendered.len() <= max_bytes).then_some((slim, rendered))
}

#[allow(clippy::too_many_arguments)]
fn allocate_skeleton(
    candidates: &[SkeletonCandidate],
    unparsed_omitted: &[OmittedSummaryEntry],
    target_label: &str,
    base_line: String,
    total_files: usize,
    total_lines: usize,
    total_bytes: u64,
    filter: &str,
    per_file_limit: Option<usize>,
    budget: usize,
    inv_files: &str,
) -> SkeletonAllocation {
    if candidates.is_empty() {
        let header = skeleton_header_line(
            target_label,
            0,
            total_files,
            total_lines,
            total_bytes,
            filter,
        );
        let omitted: Vec<OmittedSummaryEntry> = unparsed_omitted
            .iter()
            .map(|e| OmittedSummaryEntry {
                file: e.file.clone(),
                rel_path: e.rel_path.clone(),
                lines: e.lines,
                raw_bytes: e.raw_bytes,
                total_entries: e.total_entries,
                priority: e.priority,
            })
            .collect();
        let footer_cap = budget.saturating_sub(header.len() + base_line.len());
        let (omitted_summary_line, next_line) =
            format_omitted_footer(&omitted, filter, inv_files, footer_cap);
        return SkeletonAllocation {
            header,
            base_line,
            shown: Vec::new(),
            omitted,
            omitted_summary_line,
            next_line,
        };
    }

    // Fast path: every candidate fits with all its rows (respecting `--limit`) and no unparsed files remain.
    if unparsed_omitted.is_empty()
        && per_file_limit.is_none_or(|lim| candidates.iter().all(|c| c.rows.len() <= lim))
    {
        let header = skeleton_header_line(
            target_label,
            candidates.len(),
            total_files,
            total_lines,
            total_bytes,
            filter,
        );
        let full_total = header.len()
            + base_line.len()
            + candidates
                .iter()
                .map(SkeletonCandidate::full_bytes)
                .sum::<usize>();
        if full_total <= budget {
            let shown = candidates
                .iter()
                .enumerate()
                .map(|(idx, c)| SelectedSkeletonFile {
                    candidate_idx: idx,
                    rendered_rows: c.rows.iter().map(Row::display).collect(),
                    shown_rows: c.rows.clone(),
                    contiguous_prefix: true,
                })
                .collect();
            return SkeletonAllocation {
                header,
                base_line,
                shown,
                omitted: Vec::new(),
                omitted_summary_line: None,
                next_line: None,
            };
        }
    }

    // Budgeted path: select the most structural files fairly, then fill rows round-robin.
    let mut ranked: Vec<usize> = (0..candidates.len()).collect();
    ranked.sort_by(|&a, &b| {
        candidates[b]
            .priority
            .cmp(&candidates[a].priority)
            .then_with(|| candidates[a].file.cmp(&candidates[b].file))
    });

    let max_header_len = skeleton_header_line(
        target_label,
        candidates.len(),
        total_files,
        total_lines,
        total_bytes,
        filter,
    )
    .len();
    let overhead = max_header_len + base_line.len();
    let avail_all = budget.saturating_sub(overhead);

    // Reserve footer space if we already have unparsed files or won't fit every file's floor.
    let all_floor: usize = candidates
        .iter()
        .map(|c| {
            c.min_useful_bytes()
                .max(SKELETON_MIN_PER_FILE.min(c.full_bytes()))
        })
        .sum();
    let will_omit = !unparsed_omitted.is_empty() || all_floor > avail_all;
    let footer_reserve = if will_omit {
        (budget / 4).clamp(64, 300).min(avail_all / 2)
    } else if budget >= 256 {
        96.min(avail_all / 4)
    } else {
        0
    };
    let select_budget = avail_all.saturating_sub(footer_reserve);

    // Scale the per-file admission floor down under very tight budgets so one
    // large file cannot monopolize a 512 B skeleton while dozens of siblings exist.
    let target_floor = SKELETON_MIN_PER_FILE.min(select_budget / 2).max(120);
    let mut admitted_ranked: Vec<usize> = Vec::new();
    let mut floor_used = 0usize;
    for &idx in &ranked {
        let c = &candidates[idx];
        let floor = c.min_useful_bytes().max(target_floor.min(c.full_bytes()));
        if floor_used + floor <= select_budget {
            floor_used += floor;
            admitted_ranked.push(idx);
        } else {
            break;
        }
    }
    // Under very tight budgets, fall back to fitting at least header + 1 row (or header).
    if admitted_ranked.is_empty() {
        for &idx in &ranked {
            let need = candidates[idx].min_useful_bytes();
            if floor_used + need <= select_budget {
                floor_used += need;
                admitted_ranked.push(idx);
            } else {
                break;
            }
        }
    }
    if admitted_ranked.is_empty() {
        let first = ranked[0];
        let base_need = candidates[first].header_line().len()
            + candidates[first]
                .parse_error_line()
                .as_deref()
                .map_or(0, str::len)
            + skeleton_trunc_line(0, candidates[first].total_rows).len();
        if base_need <= avail_all {
            admitted_ranked.push(first);
        }
    }

    // Build omitted list and ensure headers + zero-row trunc lines + footer fit strictly.
    let build_omitted = |admitted: &[usize]| -> Vec<OmittedSummaryEntry> {
        let admitted_set: BTreeSet<usize> = admitted.iter().copied().collect();
        let mut omitted: Vec<OmittedSummaryEntry> = candidates
            .iter()
            .enumerate()
            .filter(|(idx, _)| !admitted_set.contains(idx))
            .map(|(_, c)| OmittedSummaryEntry {
                file: c.file.clone(),
                rel_path: c.rel_path.clone(),
                lines: c.lines,
                raw_bytes: c.raw_bytes,
                total_entries: c.total_rows,
                priority: c.priority,
            })
            .collect();
        for extra in unparsed_omitted {
            omitted.push(OmittedSummaryEntry {
                file: extra.file.clone(),
                rel_path: extra.rel_path.clone(),
                lines: extra.lines,
                raw_bytes: extra.raw_bytes,
                total_entries: extra.total_entries,
                priority: extra.priority,
            });
        }
        omitted.sort_by(|a, b| a.file.cmp(&b.file));
        omitted
    };

    let mut omitted = build_omitted(&admitted_ranked);
    let mut header = skeleton_header_line(
        target_label,
        admitted_ranked.len(),
        total_files,
        total_lines,
        total_bytes,
        filter,
    );
    let mut footer_cap = if !omitted.is_empty() {
        (budget / 4)
            .clamp(48, 300)
            .min(budget.saturating_sub(header.len() + base_line.len()))
    } else {
        0
    };
    let (mut omitted_summary_line, mut next_line) =
        format_omitted_footer(&omitted, filter, inv_files, footer_cap);

    loop {
        let footer_bytes = omitted_summary_line.as_deref().map_or(0, str::len)
            + next_line.as_deref().map_or(0, str::len);
        let headers_and_trunc: usize = admitted_ranked
            .iter()
            .map(|&idx| {
                let c = &candidates[idx];
                c.header_line().len()
                    + c.parse_error_line().as_deref().map_or(0, str::len)
                    + skeleton_trunc_line(0, c.total_rows).len()
            })
            .sum();
        if header.len() + base_line.len() + headers_and_trunc + footer_bytes <= budget
            || admitted_ranked.is_empty()
        {
            break;
        }
        // Drop the lowest-priority admitted file to omitted and recompute.
        admitted_ranked.pop();
        omitted = build_omitted(&admitted_ranked);
        header = skeleton_header_line(
            target_label,
            admitted_ranked.len(),
            total_files,
            total_lines,
            total_bytes,
            filter,
        );
        footer_cap = (budget / 4)
            .clamp(48, 300)
            .min(budget.saturating_sub(header.len() + base_line.len()));
        (omitted_summary_line, next_line) =
            format_omitted_footer(&omitted, filter, inv_files, footer_cap);
    }

    // Re-sort admitted files into deterministic path order for readable tree output.
    admitted_ranked.sort_unstable();

    let fixed_footer = omitted_summary_line.as_deref().map_or(0, str::len)
        + next_line.as_deref().map_or(0, str::len);
    // When all files are shown, reserve space for a single `Next:` hint if any file truncates.
    let trunc_next_reserve = if omitted.is_empty() && budget >= 256 {
        let longest_next = admitted_ranked
            .iter()
            .map(|&idx| {
                format!(
                    "Next: {inv_files} read {} --outline\n",
                    crate::workspace::shell_text(&candidates[idx].file)
                )
                .len()
            })
            .max()
            .unwrap_or(0);
        longest_next.min(budget.saturating_sub(header.len() + base_line.len()) / 4)
    } else {
        0
    };
    let body_budget =
        budget.saturating_sub(header.len() + base_line.len() + fixed_footer + trunc_next_reserve);

    // For each admitted file, order rows by structural priority (when unfiltered)
    // so tight budgets surface public types, traits, classes and functions before
    // imports and private constants; selected rows are re-sorted back into source
    // line order after allocation.
    struct FillSlot {
        candidate_idx: usize,
        row_order: Vec<usize>,
        cursor: usize,
        picked: Vec<(usize, Row, String)>,
    }

    let mut slots: Vec<FillSlot> = admitted_ranked
        .iter()
        .map(|&idx| {
            let c = &candidates[idx];
            let mut order: Vec<usize> = (0..c.rows.len()).collect();
            if filter.is_empty() {
                order.sort_by(|&a, &b| {
                    skeleton_row_priority(&c.rows[b])
                        .cmp(&skeleton_row_priority(&c.rows[a]))
                        .then_with(|| a.cmp(&b))
                });
            }
            FillSlot {
                candidate_idx: idx,
                row_order: order,
                cursor: 0,
                picked: Vec::new(),
            }
        })
        .collect();
    let mut total_used: usize = slots
        .iter()
        .map(|slot| {
            let c = &candidates[slot.candidate_idx];
            c.header_line().len()
                + c.parse_error_line().as_deref().map_or(0, str::len)
                + skeleton_trunc_line(0, c.total_rows).len()
        })
        .sum();

    // Fair round-robin row filling: every file gets 1 row per pass until full, limit, or budget ends.
    loop {
        let mut progressed = false;
        for slot in &mut slots {
            if per_file_limit.is_some_and(|lim| slot.picked.len() >= lim) {
                continue;
            }
            let c = &candidates[slot.candidate_idx];
            let mut skipped_structural = false;
            while slot.cursor < slot.row_order.len() {
                let row_idx = slot.row_order[slot.cursor];
                let row = &c.rows[row_idx];
                if filter.is_empty() && skipped_structural && skeleton_row_priority(row) < 2 {
                    slot.cursor = slot.row_order.len();
                    break;
                }
                let rendered = row.display();
                let k = slot.picked.len();
                let old_trunc = skeleton_trunc_line(k, c.total_rows).len();
                let new_trunc = skeleton_trunc_line(k + 1, c.total_rows).len();
                let new_total = total_used + rendered.len() + new_trunc - old_trunc;
                slot.cursor += 1;
                if new_total <= body_budget {
                    slot.picked.push((row_idx, row.clone(), rendered));
                    total_used = new_total;
                    progressed = true;
                    break;
                } else if k == 0 {
                    let max_row_bytes =
                        (body_budget + old_trunc).saturating_sub(total_used + new_trunc);
                    if let Some((slim, slim_str)) = clip_row_to_fit(row, max_row_bytes) {
                        let slim_total = total_used + slim_str.len() + new_trunc - old_trunc;
                        if slim_total <= body_budget {
                            slot.picked.push((row_idx, slim, slim_str));
                            total_used = slim_total;
                            progressed = true;
                            break;
                        }
                    }
                } else if skeleton_row_priority(row) >= 2 {
                    skipped_structural = true;
                }
            }
        }
        if !progressed {
            break;
        }
    }

    let shown: Vec<SelectedSkeletonFile> = slots
        .into_iter()
        .map(|mut slot| {
            slot.picked.sort_by_key(|(row_idx, _, _)| *row_idx);
            let contiguous_prefix = slot
                .picked
                .iter()
                .enumerate()
                .all(|(pos, (row_idx, _, _))| pos == *row_idx);
            let mut shown_rows = Vec::with_capacity(slot.picked.len());
            let mut rendered_rows = Vec::with_capacity(slot.picked.len());
            for (_, row, rendered) in slot.picked {
                shown_rows.push(row);
                rendered_rows.push(rendered);
            }
            SelectedSkeletonFile {
                candidate_idx: slot.candidate_idx,
                shown_rows,
                rendered_rows,
                contiguous_prefix,
            }
        })
        .collect();

    // If all files were shown but at least one was truncated, suggest expanding the most-truncated file.
    if omitted.is_empty()
        && let Some(most_truncated) = shown
            .iter()
            .filter_map(|slot| {
                let c = &candidates[slot.candidate_idx];
                let rem = c.total_rows.saturating_sub(slot.shown_rows.len());
                (rem > 0).then_some((rem, &c.file))
            })
            .max_by(|(rem_a, file_a), (rem_b, file_b)| {
                rem_a.cmp(rem_b).then_with(|| file_b.cmp(file_a))
            })
    {
        let hint = format!(
            "Next: {inv_files} read {} --outline\n",
            crate::workspace::shell_text(most_truncated.1)
        );
        if header.len() + base_line.len() + total_used + hint.len() <= budget {
            next_line = Some(hint);
        }
    }

    SkeletonAllocation {
        header,
        base_line,
        shown,
        omitted,
        omitted_summary_line,
        next_line,
    }
}

fn format_target_label(targets: &[ReadTarget]) -> String {
    let label_one = |target: &ReadTarget| match target {
        ReadTarget::Dir { relative, .. } => {
            if relative.is_empty() {
                "./".to_owned()
            } else {
                format!("{relative}/")
            }
        }
        ReadTarget::File(file) => file.clone(),
    };
    if targets.len() <= 3 {
        targets.iter().map(label_one).collect::<Vec<_>>().join(", ")
    } else {
        format!(
            "{}, {}, +{} more",
            label_one(&targets[0]),
            label_one(&targets[1]),
            targets.len() - 2
        )
    }
}

struct SkeletonOptions<'a> {
    budget: usize,
    filter: &'a str,
    limit: Option<usize>,
    max_depth: Option<usize>,
    json_mode: bool,
    fresh: bool,
    can_dedup: bool,
}

struct SkeletonOutput {
    bytes: Vec<u8>,
}

fn build_skeleton_output(
    ws: &Workspace,
    targets: &[ReadTarget],
    opts: &SkeletonOptions<'_>,
    mut memory: Option<&mut crate::seen::Memory>,
) -> Result<SkeletonOutput> {
    // Collect explicit files (which must open cleanly) and discovered dir files (skip unreadable/binary).
    let mut explicit_files = BTreeSet::new();
    let mut all_files = BTreeSet::new();
    for target in targets {
        match target {
            ReadTarget::File(file) => {
                explicit_files.insert(file.clone());
                all_files.insert(file.clone());
            }
            ReadTarget::Dir {
                relative,
                canonical,
            } => {
                for file in collect_dir_files(ws, relative, canonical, opts.max_depth)? {
                    all_files.insert(file);
                }
            }
        }
    }

    let ordered_files: Vec<String> = all_files.into_iter().collect();
    let target_label = format_target_label(targets);
    let (strip_prefix, emit_base_line) = match targets {
        [ReadTarget::Dir { relative, .. }] => {
            let prefix = (!relative.is_empty()).then(|| format!("{relative}/"));
            (prefix, false)
        }
        _ => {
            let hoisted = hoistable_read_prefix(&ordered_files);
            let emit = !opts.json_mode && hoisted.is_some();
            (hoisted, emit)
        }
    };
    let display_rel = |file: &str| -> String {
        strip_prefix
            .as_deref()
            .and_then(|base| file.strip_prefix(base))
            .unwrap_or(file)
            .to_owned()
    };

    // For very large unfiltered trees, prioritize structural paths for eager parsing
    // while still counting total files and raw bytes across the entire directory.
    let parse_set: BTreeSet<usize> =
        if ordered_files.len() <= SKELETON_MAX_EAGER_PARSE || !opts.filter.is_empty() {
            (0..ordered_files.len()).collect()
        } else {
            let mut by_hint: Vec<usize> = (0..ordered_files.len()).collect();
            by_hint.sort_by(|&a, &b| {
                let exp_a = explicit_files.contains(&ordered_files[a]);
                let exp_b = explicit_files.contains(&ordered_files[b]);
                exp_b
                    .cmp(&exp_a)
                    .then_with(|| {
                        path_structural_hint(&ordered_files[b])
                            .cmp(&path_structural_hint(&ordered_files[a]))
                    })
                    .then_with(|| ordered_files[a].cmp(&ordered_files[b]))
            });
            by_hint.into_iter().take(SKELETON_MAX_EAGER_PARSE).collect()
        };

    let mut total_files = 0usize;
    let mut total_lines = 0usize;
    let mut total_bytes = 0u64;
    let mut candidates = Vec::new();
    let mut unparsed_omitted = Vec::new();

    for (idx, file) in ordered_files.iter().enumerate() {
        if !parse_set.contains(&idx) {
            if let Ok(path) = ws.source_path(file)
                && let Ok(meta) = path.metadata()
            {
                let raw_bytes = meta.len() as usize;
                total_files += 1;
                total_bytes += meta.len();
                unparsed_omitted.push(OmittedSummaryEntry {
                    file: file.clone(),
                    rel_path: display_rel(file),
                    lines: 0,
                    raw_bytes,
                    total_entries: 0,
                    priority: (path_structural_hint(file), 0, 0),
                });
            }
            continue;
        }
        let mut source = match Snapshot::open(ws, file) {
            Ok(snap) => snap,
            Err(err) if explicit_files.contains(file) => return Err(err),
            Err(_) => continue,
        };
        let raw_bytes = source.text.len();
        let lines = source.line_count();
        let digest = crate::capture::source_digest(source.text.as_bytes());
        total_files += 1;
        total_lines += lines;
        total_bytes += raw_bytes as u64;
        source.index(file)?;
        let rows = source.skeleton_rows(opts.filter);
        let total_rows = rows.len();
        if !opts.filter.is_empty() && rows.is_empty() {
            continue;
        }
        let rel_path = display_rel(file);
        let priority = structural_priority(&rel_path, &source, &rows);
        candidates.push(SkeletonCandidate {
            file: file.clone(),
            rel_path,
            revision: source.revision,
            digest,
            lines,
            raw_bytes,
            parse_error: source.parse_error,
            rows,
            total_rows,
            priority,
        });
    }

    let inv_files = ws.invocation().files;
    let base_line = if emit_base_line {
        strip_prefix
            .as_deref()
            .map_or_else(String::new, |base| format!("base: {base}\n"))
    } else {
        String::new()
    };
    let allocation = allocate_skeleton(
        &candidates,
        &unparsed_omitted,
        &target_label,
        base_line,
        total_files,
        total_lines,
        total_bytes,
        opts.filter,
        opts.limit,
        opts.budget,
        &inv_files,
    );

    if opts.json_mode {
        let mut json_files = Vec::with_capacity(allocation.shown.len());
        for slot in &allocation.shown {
            let c = &candidates[slot.candidate_idx];
            let omitted_entries = c.total_rows.saturating_sub(slot.shown_rows.len());
            let truncated = omitted_entries > 0;
            let next = if truncated && slot.contiguous_prefix {
                slot.shown_rows
                    .last()
                    .map(|row| format!("{}#{}", c.revision, row.end_id.unwrap_or(row.id)))
            } else {
                None
            };
            let mut obj = serde_json::Map::new();
            obj.insert("file".into(), serde_json::Value::String(c.file.clone()));
            obj.insert(
                "rel_path".into(),
                serde_json::Value::String(c.rel_path.clone()),
            );
            obj.insert(
                "revision".into(),
                serde_json::Value::String(c.revision.clone()),
            );
            obj.insert("lines".into(), serde_json::json!(c.lines));
            obj.insert("bytes".into(), serde_json::json!(c.raw_bytes));
            obj.insert("total_entries".into(), serde_json::json!(c.total_rows));
            if let Some(err) = &c.parse_error {
                obj.insert(
                    "parse_error".into(),
                    serde_json::Value::String(clipped(err, 240)),
                );
            }
            obj.insert("entries".into(), serde_json::to_value(&slot.shown_rows)?);
            obj.insert("truncated".into(), serde_json::json!(truncated));
            obj.insert("omitted_entries".into(), serde_json::json!(omitted_entries));
            obj.insert("next".into(), serde_json::to_value(next)?);
            let value = serde_json::Value::Object(obj);
            let shown_bytes = serde_json::to_vec(&value)?.len() as u64;
            crate::history::saw_outline_measured(
                ws,
                &c.file,
                c.digest.clone(),
                c.raw_bytes as u64,
                shown_bytes,
            );
            json_files.push(value);
        }
        let json_omitted: Vec<serde_json::Value> = allocation
            .omitted
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "file": entry.file,
                    "rel_path": entry.rel_path,
                    "lines": entry.lines,
                    "bytes": entry.raw_bytes,
                    "total_entries": entry.total_entries,
                })
            })
            .collect();
        let target_names: Vec<String> = targets
            .iter()
            .map(|t| match t {
                ReadTarget::Dir { relative, .. } => {
                    if relative.is_empty() {
                        "./".to_owned()
                    } else {
                        format!("{relative}/")
                    }
                }
                ReadTarget::File(file) => file.clone(),
            })
            .collect();
        let mut root = serde_json::Map::new();
        root.insert("mode".into(), serde_json::json!("skeleton"));
        root.insert("targets".into(), serde_json::json!(target_names));
        if let Some(base) = &strip_prefix {
            root.insert("base".into(), serde_json::json!(base));
        }
        root.insert("budget".into(), serde_json::json!(opts.budget));
        root.insert("shown_files".into(), serde_json::json!(json_files.len()));
        root.insert("total_files".into(), serde_json::json!(total_files));
        root.insert("total_lines".into(), serde_json::json!(total_lines));
        root.insert("total_bytes".into(), serde_json::json!(total_bytes));
        if !opts.filter.is_empty() {
            root.insert("match".into(), serde_json::json!(opts.filter));
        }
        if let Some(depth) = opts.max_depth {
            root.insert("depth".into(), serde_json::json!(depth));
        }
        root.insert("files".into(), serde_json::Value::Array(json_files));
        root.insert(
            "omitted_files".into(),
            serde_json::Value::Array(json_omitted),
        );
        let bytes = serde_json::to_vec(&serde_json::Value::Object(root))?;
        return Ok(SkeletonOutput { bytes });
    }

    // Text rendering with strict `<= budget` enforcement and optional session dedup.
    let mut out = String::new();
    if allocation.header.len() <= opts.budget {
        out.push_str(&allocation.header);
    } else {
        out.push_str(&clipped(
            allocation.header.trim_end_matches('\n'),
            opts.budget.saturating_sub(1),
        ));
        if out.len() < opts.budget {
            out.push('\n');
        }
    }
    if !allocation.base_line.is_empty() && out.len() + allocation.base_line.len() <= opts.budget {
        out.push_str(&allocation.base_line);
    }
    if candidates.is_empty() && !opts.filter.is_empty() && total_files > 0 {
        let none = "no matching entries\n";
        if out.len() + none.len() <= opts.budget {
            out.push_str(none);
        }
    }

    for slot in &allocation.shown {
        let c = &candidates[slot.candidate_idx];
        let mut section = c.header_line();
        if let Some(err) = c.parse_error_line() {
            section.push_str(&err);
        }
        for row_str in &slot.rendered_rows {
            section.push_str(row_str);
        }
        let complete = slot.shown_rows.len() == c.total_rows && c.parse_error.is_none();
        if !complete {
            section.push_str(&skeleton_trunc_line(slot.shown_rows.len(), c.total_rows));
        }
        let deduped = if opts.can_dedup
            && complete
            && let Some(mem) = memory.as_deref_mut()
        {
            let full_hint = format!(
                "full outline: {inv_files} read {} --outline --fresh",
                crate::workspace::shell_text(&c.file)
            );
            mem.saw_outline(&c.file, &c.revision, &section, opts.fresh, &full_hint)
        } else {
            None
        };
        let emitted = match deduped {
            Some(notice) if notice.len() <= section.len() => notice,
            _ => {
                crate::history::saw_outline_measured(
                    ws,
                    &c.file,
                    c.digest.clone(),
                    c.raw_bytes as u64,
                    section.len() as u64,
                );
                section
            }
        };
        if out.len() + emitted.len() <= opts.budget {
            out.push_str(&emitted);
        }
    }
    if let Some(summary) = &allocation.omitted_summary_line
        && out.len() + summary.len() <= opts.budget
    {
        out.push_str(summary);
    }
    if let Some(next) = &allocation.next_line
        && out.len() + next.len() <= opts.budget
    {
        out.push_str(next);
    }

    Ok(SkeletonOutput {
        bytes: out.into_bytes(),
    })
}

fn read_skeleton(ws: &Workspace, targets: &[ReadTarget], args: &ArgMatches) -> Result<()> {
    ensure!(
        args.value_of("shell-range").is_none(),
        "--shell-range applies to a single file"
    );
    ensure!(
        args.value_of("after").is_none(),
        "--after applies to a single file"
    );
    let budget = if args.is_present("budget") {
        crate::context_budget(ws, args)?
    } else {
        let settings = ws.settings()?;
        settings.max_output_bytes.max(settings.context_budget)
    };
    let filter = args.value_of("match").unwrap_or("");
    let limit = if args.occurrences_of("limit") > 0 {
        Some(crate::read_num(args, "limit", 1)?)
    } else {
        None
    };
    let max_depth = if args.is_present("depth") {
        Some(crate::read_num(args, "depth", 1)?)
    } else {
        None
    };
    let json_mode = args.is_present("json");
    let fresh = args.is_present("fresh");
    let mut memory = crate::seen::Memory::open(ws, args.value_of("session"), fresh);
    let can_dedup = !json_mode
        && filter.is_empty()
        && args.occurrences_of("limit") == 0
        && args.occurrences_of("budget") == 0;
    let opts = SkeletonOptions {
        budget,
        filter,
        limit,
        max_depth,
        json_mode,
        fresh,
        can_dedup,
    };
    let output = build_skeleton_output(ws, targets, &opts, memory.as_mut())?;
    io::stdout().lock().write_all(&output.bytes)?;
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
    if name == "expand" {
        let raw: Vec<&str> = args.values_of("file").unwrap().collect();
        return run_expand(ws, &raw, args, root_given);
    }
    let resolved_single_file = if name == "read" {
        if let Some(range) = args.value_of("shell-range") {
            let files: Vec<&str> = args.values_of("file").unwrap().collect();
            ensure!(files.len() == 1, "--shell-range applies to a single file");
            // The hook passes the already resolved, workspace-relative file, or the
            // `git show` object name with the blob it resolved to.
            return crate::shellread::read(
                ws,
                files[0],
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
        let raw_paths: Vec<&str> = args.values_of("file").unwrap().collect();
        let targets: Vec<ReadTarget> = raw_paths
            .iter()
            .map(|raw| resolve_read_target(ws, raw, root_given))
            .collect::<Result<_>>()?;
        if targets.iter().any(|t| matches!(t, ReadTarget::Dir { .. })) {
            return read_skeleton(ws, &targets, args);
        }
        let files: Vec<String> = targets
            .into_iter()
            .map(|t| match t {
                ReadTarget::File(file) => file,
                ReadTarget::Dir { .. } => unreachable!(),
            })
            .collect();
        if files.len() > 1 {
            return read_many_resolved(ws, &files, args);
        }
        files.into_iter().next().unwrap()
    } else {
        let argument = args.value_of("file").unwrap();
        workspace_file(ws, argument, root_given)?
    };
    let file = resolved_single_file.as_str();
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

    #[test]
    fn window_folds_imports_modules_and_constants_with_exact_expand() {
        let mut src = String::new();
        for i in 1..=6 {
            src.push_str(&format!("use std::collections::Type{i};\n"));
        }
        src.push('\n');
        for i in 1..=4 {
            src.push_str(&format!("mod part_{i};\n"));
        }
        src.push('\n');
        for i in 1..=6 {
            src.push_str(&format!("const LIMIT_{i}: usize = {i};\n"));
        }
        src.push('\n');
        for i in 1..=6 {
            src.push_str(&format!(
                "pub fn worker_{i}(v: usize) -> usize {{\n    let a = v + {i};\n    a * 2\n}}\n\n"
            ));
        }

        let mut snap = Snapshot::from_text("src/lib.rs", src);
        snap.index("src/lib.rs").unwrap();
        let rev = snap.revision.clone();
        let win = snap.window(1, 50).expect("window should succeed");
        let with_sigs = win.display(true);
        let without_sigs = win.display(false);

        for rendered in [&with_sigs, &without_sigs] {
            assert!(
                rendered.contains("#1..#6 L1-6 imports (6 items)\n"),
                "{rendered}"
            );
            assert!(
                rendered.contains("#7..#10 L8-11 modules (4 items)\n"),
                "{rendered}"
            );
            assert!(
                rendered.contains("#11..#16 L13-18 constants (6 items)\n"),
                "{rendered}"
            );
            assert!(!rendered.contains("#2 L2-2"), "{rendered}");
            assert!(!rendered.contains("#12 L14-14"), "{rendered}");
        }
        assert!(
            with_sigs
                .contains("#17 L20-23 function worker_1  pub fn worker_1(v: usize) -> usize\n"),
            "{with_sigs}"
        );
        assert!(
            without_sigs.contains("#17 L20-23 function worker_1\n"),
            "{without_sigs}"
        );

        // `#A..#B` from the folded window resolves exact source bytes.
        let import_span = snap.resolve(&format!("{rev}#1..#6"), None, true).unwrap();
        assert!(snap.text[import_span.clone()].starts_with("use std::collections::Type1;"));
        assert!(snap.text[import_span].ends_with("use std::collections::Type6;"));
        let const_span = snap.resolve(&format!("{rev}#11..#16"), None, true).unwrap();
        assert!(snap.text[const_span.clone()].starts_with("const LIMIT_1: usize = 1;"));
        assert!(snap.text[const_span].ends_with("const LIMIT_6: usize = 6;"));

        // A large import preamble covering >50% of the window still folds cleanly
        // instead of tripping the single-long-declaration guard.
        let mut heavy_imports = String::new();
        for i in 1..=30 {
            heavy_imports.push_str(&format!("use crate::mod_{i}::Item{i};\n"));
        }
        heavy_imports.push_str("\npub fn run() {\n    let x = 1;\n    let y = x + 1;\n}\n");
        let mut heavy = Snapshot::from_text("src/heavy.rs", heavy_imports);
        heavy.index("src/heavy.rs").unwrap();
        let heavy_win = heavy
            .window(1, 35)
            .expect("folded preamble should not reject window")
            .display(true);
        assert!(
            heavy_win.contains("#1..#30 L1-30 imports (30 items)\n"),
            "{heavy_win}"
        );
    }

    #[test]
    fn unfolded_imports_drop_doubled_label_and_preserve_modifiers() {
        // Rust: 2 imports (below fold threshold of 3).
        let rs = "use std::fs::File;\npub use crate::model::Order;\n\npub fn run() {}\n";
        let mut rs_snap = Snapshot::from_text("src/lib.rs", rs.into());
        rs_snap.index("src/lib.rs").unwrap();
        let rs_page = rs_snap.page("", None, 40, 4096).unwrap().display();
        assert!(
            rs_page.contains("#1 L1-1 import std::fs::File;\n"),
            "{rs_page}"
        );
        assert!(
            rs_page.contains("#2 L2-2 import pub use crate::model::Order;\n"),
            "{rs_page}"
        );
        assert!(!rs_page.contains("import import"), "{rs_page}");

        // TypeScript: 2 imports.
        let ts = "import { tax } from './tax';\nimport type { Order } from './types';\n\nexport function run() {}\n";
        let mut ts_snap = Snapshot::from_text("src/app.ts", ts.into());
        ts_snap.index("src/app.ts").unwrap();
        let ts_page = ts_snap.page("", None, 40, 4096).unwrap().display();
        assert!(
            ts_page.contains("#1 L1-1 import { tax } from './tax';\n"),
            "{ts_page}"
        );
        assert!(
            ts_page.contains("#2 L2-2 import type { Order } from './types';\n"),
            "{ts_page}"
        );
        assert!(!ts_page.contains("import import"), "{ts_page}");

        // Python: `from ...` and `import ...`.
        let py = "import os\nfrom pathlib import Path\n\ndef run():\n    return os.name\n";
        let mut py_snap = Snapshot::from_text("app.py", py.into());
        py_snap.index("app.py").unwrap();
        let py_page = py_snap.page("", None, 40, 4096).unwrap().display();
        assert!(py_page.contains("#1 L1-1 import os\n"), "{py_page}");
        assert!(
            py_page.contains("#2 L2-2 import pathlib import Path\n"),
            "{py_page}"
        );
        assert!(!py_page.contains("import import"), "{py_page}");

        // Window without signatures renders `#1 L1-1 import` instead of `import import`.
        let mut win_src = String::from("use std::io;\n\n");
        for i in 1..=6 {
            win_src.push_str(&format!("pub fn f{i}() {{\n    let _ = {i};\n}}\n"));
        }
        let mut win_snap = Snapshot::from_text("src/w.rs", win_src);
        win_snap.index("src/w.rs").unwrap();
        let win_nosig = win_snap.window(1, 20).expect("window").display(false);
        assert!(win_nosig.contains("#1 L1-1 import\n"), "{win_nosig}");
        assert!(!win_nosig.contains("import import"), "{win_nosig}");
    }

    #[test]
    fn polyglot_slim_signature_trims_redundant_keywords_without_losing_semantics() {
        // TypeScript: class, interface, enum, function, export, async, type alias.
        let ts = concat!(
            "class Foo extends Bar {\n  run(): void {}\n}\n",
            "export class Service<T> implements Runner {\n  go(): void {}\n}\n",
            "interface Baz {\n  id: string;\n}\n",
            "export interface Config extends Base {\n  url: string;\n}\n",
            "enum Mode {\n  Fast,\n}\n",
            "export enum State {\n  Open,\n}\n",
            "function qux(a: number, b: string): boolean {\n  return a > b.length;\n}\n",
            "export async function fetchItem(id: string): Promise<void> {}\n",
            "export type UserId = string;\n",
            "export type ComplexConfig = {\n  a: string;\n  b: string;\n  c: string;\n};\n",
        );
        let mut ts_snap = Snapshot::from_text("src/app.ts", ts.into());
        ts_snap.index("src/app.ts").unwrap();
        let ts_rev = ts_snap.revision.clone();
        let ts_out = ts_snap.page("", None, 40, 4096).unwrap().display();
        assert!(ts_out.contains("class Foo  extends Bar\n"), "{ts_out}");
        assert!(
            ts_out.contains("class Service  export <T> implements Runner\n"),
            "{ts_out}"
        );
        assert!(ts_out.contains("interface Baz\n"), "{ts_out}");
        assert!(
            ts_out.contains("interface Config  export extends Base\n"),
            "{ts_out}"
        );
        assert!(ts_out.contains("enum Mode\n"), "{ts_out}");
        assert!(ts_out.contains("enum State  export\n"), "{ts_out}");
        assert!(
            ts_out.contains("function qux  (a: number, b: string): boolean\n"),
            "{ts_out}"
        );
        assert!(
            ts_out.contains("function fetchItem  export async (id: string): Promise<void>\n"),
            "{ts_out}"
        );
        // Never turn `export type UserId = string;` into `export = string;`.
        assert!(
            ts_out.contains("type UserId  export type UserId = string;\n"),
            "{ts_out}"
        );
        assert!(ts_out.contains("type ComplexConfig  export\n"), "{ts_out}");
        // `expand` still returns exact source bytes.
        let qux_span = ts_snap.resolve(&format!("{ts_rev}#1"), None, true).unwrap();
        assert!(ts_snap.text[qux_span].starts_with("class Foo extends Bar {"));

        // Python: class, subclass, def, async def.
        let py = concat!(
            "class Plain:\n    pass\n\n",
            "class Child(Plain):\n    def calc(self, x: int) -> int:\n        return x\n\n",
            "async def load(url: str) -> bytes:\n    return b''\n",
        );
        let mut py_snap = Snapshot::from_text("app.py", py.into());
        py_snap.index("app.py").unwrap();
        let py_out = py_snap.page("", None, 40, 4096).unwrap().display();
        assert!(py_out.contains("#1 L1-2 class Plain\n"), "{py_out}");
        assert!(py_out.contains("class Child  (Plain):\n"), "{py_out}");
        assert!(
            py_out.contains("function Child::calc  (self, x: int) -> int:\n"),
            "{py_out}"
        );
        assert!(
            py_out.contains("function load  async (url: str) -> bytes:\n"),
            "{py_out}"
        );

        // Go: struct, interface, generic struct, free function, receiver method.
        let go = concat!(
            "package demo\n\n",
            "type Item struct {\n\tID string\n}\n\n",
            "type Store interface {\n\tGet(id string) Item\n}\n\n",
            "type Cache[K comparable, V any] struct {\n\tm map[K]V\n}\n\n",
            "func run(id string) error {\n\treturn nil\n}\n\n",
            "func (i *Item) Save() error {\n\treturn nil\n}\n",
        );
        let mut go_snap = Snapshot::from_text("demo.go", go.into());
        go_snap.index("demo.go").unwrap();
        let go_out = go_snap.page("", None, 40, 4096).unwrap().display();
        assert!(go_out.contains("type Item  struct\n"), "{go_out}");
        assert!(go_out.contains("type Store  interface\n"), "{go_out}");
        assert!(
            go_out.contains("type Cache  [K comparable, V any] struct\n"),
            "{go_out}"
        );
        assert!(
            go_out.contains("function run  (id string) error\n"),
            "{go_out}"
        );
        // Receiver `(i *Item)` is preserved untouched.
        assert!(
            go_out.contains("method Save  func (i *Item) Save() error\n"),
            "{go_out}"
        );
    }

    #[test]
    fn read_many_hoists_common_prefix_only_when_deep_and_strictly_smaller() {
        assert_eq!(
            hoistable_read_prefix(&["packages/billing/src/a.ts".into()]),
            None
        );
        assert_eq!(
            hoistable_read_prefix(&["src/a.rs".into(), "src/b.rs".into()]),
            None,
            "1-segment prefix should not hoist"
        );
        assert_eq!(
            hoistable_read_prefix(&["a/b/x.rs".into(), "a/b/y.rs".into()]),
            None,
            "2 files with 4-byte prefix: `base: a/b/\\n` (11 B) is not smaller than 8 B"
        );
        assert_eq!(
            hoistable_read_prefix(&["a/b/x.rs".into(), "a/b/y.rs".into(), "a/b/z.rs".into()]),
            Some("a/b/".into()),
            "3 files with 4-byte prefix: 11 B < 12 B"
        );
        assert_eq!(
            hoistable_read_prefix(&[
                "packages/billing/src/a.ts".into(),
                "packages/billing/src/b.ts".into(),
            ]),
            Some("packages/billing/src/".into())
        );
        assert_eq!(
            hoistable_read_prefix(&[
                "packages/billing/src/a.ts".into(),
                "packages/auth/src/b.ts".into(),
            ]),
            None,
            "only 1 common directory segment (`packages/`)"
        );
    }

    fn test_workspace(root: &Path) -> Workspace {
        Workspace::discover(Some(root)).expect("discover temp workspace")
    }

    fn default_skeleton_opts(budget: usize) -> SkeletonOptions<'static> {
        SkeletonOptions {
            budget,
            filter: "",
            limit: None,
            max_depth: None,
            json_mode: false,
            fresh: false,
            can_dedup: false,
        }
    }

    fn extract_file_revision(skeleton: &str, rel_path: &str) -> String {
        let marker = format!("=== {rel_path} (");
        let line = skeleton
            .lines()
            .find(|line| line.starts_with(&marker))
            .unwrap_or_else(|| panic!("missing header for {rel_path} in:\n{skeleton}"));
        line.split(" · rev ")
            .nth(1)
            .and_then(|rest| rest.strip_suffix(" ==="))
            .unwrap_or_else(|| panic!("missing rev in header: {line}"))
            .to_owned()
    }

    fn extract_row_id(skeleton: &str, rel_path: &str, needle: &str) -> usize {
        let marker = format!("=== {rel_path} (");
        let mut in_section = false;
        for line in skeleton.lines() {
            if line.starts_with("=== ") {
                in_section = line.starts_with(&marker);
                continue;
            }
            if in_section && line.contains(needle) {
                let rest = line.strip_prefix('#').expect("row starts with #");
                let digits = rest
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .expect("row digits");
                return digits.parse::<usize>().expect("numeric row id");
            }
        }
        panic!("missing row `{needle}` under `{rel_path}` in:\n{skeleton}");
    }

    #[test]
    fn directory_skeleton_multilang_compression_and_expand_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        fs::create_dir_all(pkg.join("rust")).unwrap();
        fs::create_dir_all(pkg.join("web")).unwrap();
        fs::create_dir_all(pkg.join("docs")).unwrap();

        // Rich Rust module (~9 KB): imports, constants, public types/traits/fns, and inline test module.
        let mut rust_src = String::from(concat!(
            "use std::collections::BTreeMap;\n",
            "use std::collections::HashSet;\n",
            "use std::fmt;\n",
            "use std::path::PathBuf;\n\n",
            "const C1: usize = 1;\n",
            "const C2: usize = 2;\n",
            "const C3: usize = 3;\n",
            "const C4: usize = 4;\n",
            "const C5: usize = 5;\n\n",
            "pub struct BillingLedger {\n",
            "    entries: BTreeMap<String, i64>,\n",
            "    seen: HashSet<String>,\n",
            "    root: PathBuf,\n",
            "}\n\n",
            "pub trait Pricer {\n",
            "    fn quote(&self, sku: &str, qty: u32) -> i64;\n",
            "}\n\n",
            "pub enum InvoiceStatus {\n",
            "    Draft,\n",
            "    Issued,\n",
            "    Paid,\n",
            "}\n\n",
            "impl BillingLedger {\n",
            "    pub fn record_charge(&mut self, account: &str, cents: i64) -> Result<i64, fmt::Error> {\n",
        ));
        for i in 0..40 {
            rust_src.push_str(&format!(
                "        let step_{i} = cents.saturating_add({i} * 17) ^ 0x55;\n"
            ));
        }
        rust_src.push_str(concat!(
            "        let total = self.entries.entry(account.to_owned()).or_insert(0);\n",
            "        *total += cents + step_39 - step_39;\n",
            "        self.seen.insert(account.to_owned());\n",
            "        Ok(*total)\n",
            "    }\n\n",
            "    pub fn balance(&self, account: &str) -> i64 {\n",
        ));
        for i in 0..35 {
            rust_src.push_str(&format!(
                "        let _check_{i} = account.len().saturating_add({i});\n"
            ));
        }
        rust_src.push_str(concat!(
            "        self.entries.get(account).copied().unwrap_or_default()\n",
            "    }\n",
            "}\n\n",
            "pub fn reconcile_batch(items: &[(&str, i64)], root: PathBuf) -> BillingLedger {\n",
        ));
        for i in 0..35 {
            rust_src.push_str(&format!(
                "    let _audit_{i} = items.len().wrapping_mul({i} + 1);\n"
            ));
        }
        rust_src.push_str(concat!(
            "    let mut ledger = BillingLedger { entries: BTreeMap::new(), seen: HashSet::new(), root };\n",
            "    for &(acct, cents) in items {\n",
            "        let _ = ledger.record_charge(acct, cents);\n",
            "    }\n",
            "    ledger\n",
            "}\n\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    use super::*;\n",
            "    #[test]\n",
            "    fn test_reconcile() {\n",
            "        let _ = reconcile_batch(&[(\"a\", 100)], PathBuf::new());\n",
            "    }\n",
            "    #[test]\n",
            "    fn test_balance() {\n",
            "        let _ = reconcile_batch(&[], PathBuf::new()).balance(\"a\");\n",
            "    }\n",
            "}\n",
        ));

        // Rich TypeScript module (~7 KB): imports, exported interface/class/async function.
        let mut ts_src = String::from(concat!(
            "import { formatCurrency } from './currency';\n",
            "import { fetchJson } from './http';\n",
            "import { AuditLogger } from './audit';\n",
            "import type { TenantId } from './tenant';\n\n",
            "export interface CheckoutQuote {\n",
            "  subtotalCents: number;\n",
            "  taxCents: number;\n",
            "  totalCents: number;\n",
            "}\n\n",
            "export class CheckoutClient {\n",
            "  async submitOrder(tenant: TenantId, sku: string, qty: number): Promise<CheckoutQuote> {\n",
        ));
        for i in 0..45 {
            ts_src.push_str(&format!(
                "    const traceStep{i} = `${{tenant}}:${{sku}}:${{qty + {i}}}`;\n"
            ));
        }
        ts_src.push_str(concat!(
            "    return { subtotalCents: qty * 100, taxCents: qty * 8, totalCents: qty * 108 };\n",
            "  }\n",
            "}\n\n",
            "export async function fetchInvoiceSummary(id: string): Promise<string> {\n",
        ));
        for i in 0..45 {
            ts_src.push_str(&format!(
                "  const payloadChunk{i} = `chunk-{i}-${{id}}`.trim().toLowerCase();\n"
            ));
        }
        ts_src.push_str("  return `invoice:${id}`;\n}\n");

        // Rich Markdown runbook (~4 KB): headings + long prose + fenced code blocks.
        let mut md_src = String::from("# Billing Architecture\n\n");
        for i in 0..20 {
            md_src.push_str(&format!(
                "Paragraph {i}: Every billing write flows through idempotent ledger batches with strict currency validation and replay safety.\n\n"
            ));
        }
        md_src.push_str("## Reconciliation Pipeline\n\n");
        for i in 0..18 {
            md_src.push_str(&format!(
                "Stage {i}: Verify settled charges against checkout quotes and emit structured audit records to cold storage.\n\n"
            ));
        }
        md_src.push_str("## Failure Recovery\n\nReplay dead-letter batches via the CLI.\n");

        fs::write(pkg.join("rust/ledger.rs"), &rust_src).unwrap();
        fs::write(pkg.join("web/checkout.ts"), &ts_src).unwrap();
        fs::write(pkg.join("docs/guide.md"), &md_src).unwrap();
        let raw_total = rust_src.len() + ts_src.len() + md_src.len();

        let ws = test_workspace(dir.path());
        let target = resolve_read_target(&ws, "pkg", false).unwrap();
        assert!(matches!(target, ReadTarget::Dir { ref relative, .. } if relative == "pkg"));

        let output =
            build_skeleton_output(&ws, &[target], &default_skeleton_opts(4096), None).unwrap();
        let skeleton = String::from_utf8(output.bytes).unwrap();

        // Header + 75-95% byte reduction.
        assert!(
            skeleton.starts_with("skeleton pkg/ · 3/3 files · "),
            "{skeleton}"
        );
        let reduction = 1.0 - (skeleton.len() as f64 / raw_total as f64);
        assert!(
            reduction >= 0.75,
            "expected >= 75% reduction, got {:.1}% ({} B vs {} B raw):\n{skeleton}",
            reduction * 100.0,
            skeleton.len(),
            raw_total
        );
        assert!(skeleton.len() <= 4096);

        // 100% of exported types, traits, classes, interfaces, function signatures & headings preserved.
        for expected in [
            "=== docs/guide.md (",
            "h1 Billing Architecture",
            "h2 Reconciliation Pipeline",
            "h2 Failure Recovery",
            "=== rust/ledger.rs (",
            "imports (4 items)",
            "constants (5 items)",
            "struct BillingLedger  pub struct BillingLedger",
            "trait Pricer  pub trait Pricer",
            "enum InvoiceStatus  pub enum InvoiceStatus",
            "method BillingLedger::record_charge  pub fn record_charge(&mut self, account: &str, cents: i64) -> Result<i64, fmt::Error>",
            "method BillingLedger::balance  pub fn balance(&self, account: &str) -> i64",
            "function reconcile_batch  pub fn reconcile_batch(items: &[(&str, i64)], root: PathBuf) -> BillingLedger",
            "module tests  (3 inner)",
            "=== web/checkout.ts (",
            "interface CheckoutQuote  export",
            "class CheckoutClient  export",
            "method CheckoutClient::submitOrder  async submitOrder(tenant: TenantId, sku: string, qty: number): Promise<CheckoutQuote>",
            "function fetchInvoiceSummary  export async (id: string): Promise<string>",
        ] {
            assert!(
                skeleton.contains(expected),
                "missing `{expected}` in:\n{skeleton}"
            );
        }
        // Function bodies must NOT appear in the skeleton.
        assert!(!skeleton.contains("step_39"), "{skeleton}");
        assert!(!skeleton.contains("payloadChunk44"), "{skeleton}");

        // Exact `REV#N` round-trip expandability on both Rust and TypeScript files.
        let rust_rev = extract_file_revision(&skeleton, "rust/ledger.rs");
        let rust_fn_id = extract_row_id(&skeleton, "rust/ledger.rs", "function reconcile_batch");
        let mut rust_snap = Snapshot::open(&ws, "pkg/rust/ledger.rs").unwrap();
        rust_snap.index("pkg/rust/ledger.rs").unwrap();
        let rust_span = rust_snap
            .resolve(&format!("{rust_rev}#{rust_fn_id}"), None, true)
            .unwrap();
        assert!(
            rust_snap.text[rust_span.clone()].starts_with("pub fn reconcile_batch("),
            "unexpected expanded Rust span: {}",
            &rust_snap.text[rust_span]
        );
        assert!(rust_snap.text[rust_span].contains("_audit_34"));

        let ts_rev = extract_file_revision(&skeleton, "web/checkout.ts");
        let ts_fn_id = extract_row_id(&skeleton, "web/checkout.ts", "function fetchInvoiceSummary");
        let mut ts_snap = Snapshot::open(&ws, "pkg/web/checkout.ts").unwrap();
        ts_snap.index("pkg/web/checkout.ts").unwrap();
        let ts_span = ts_snap
            .resolve(&format!("{ts_rev}#{ts_fn_id}"), None, true)
            .unwrap();
        assert!(
            ts_snap.text[ts_span.clone()].starts_with("export async function fetchInvoiceSummary("),
            "unexpected expanded TS span: {}",
            &ts_snap.text[ts_span]
        );
        assert!(ts_snap.text[ts_span].contains("payloadChunk44"));
    }

    #[test]
    fn directory_skeleton_match_filters_across_files_and_supports_or_terms() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("billing.rs"),
            concat!(
                "pub struct Invoice {\n    pub total: u64,\n}\n",
                "pub fn issue_invoice() -> Invoice {\n    Invoice { total: 0 }\n}\n",
                "#[cfg(test)]\nmod tests {\n    fn test_billing_edge() {}\n}\n",
            ),
        )
        .unwrap();
        fs::write(
            app.join("schema.sql"),
            "CREATE TABLE purchase_orders (\n  order_id INTEGER PRIMARY KEY\n);\n",
        )
        .unwrap();
        fs::write(
            app.join("unrelated.ts"),
            "export function greet(name: string): string {\n  return name;\n}\n",
        )
        .unwrap();

        let ws = test_workspace(dir.path());
        let target = resolve_read_target(&ws, "app", false).unwrap();

        let opts = SkeletonOptions {
            filter: "Invoice|purchase_orders",
            ..default_skeleton_opts(4096)
        };
        let out = String::from_utf8(
            build_skeleton_output(&ws, std::slice::from_ref(&target), &opts, None)
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(
            out.starts_with("skeleton app/ · 2/3 files · ")
                && out.contains("match \"Invoice|purchase_orders\""),
            "{out}"
        );
        assert!(out.contains("=== billing.rs ("), "{out}");
        assert!(out.contains("struct Invoice"), "{out}");
        assert!(out.contains("function issue_invoice"), "{out}");
        assert!(out.contains("=== schema.sql ("), "{out}");
        assert!(out.contains("table purchase_orders"), "{out}");
        assert!(
            !out.contains("unrelated.ts"),
            "non-matching file should be omitted:\n{out}"
        );

        // `--match` does not fold `mod tests`, so matching test functions are still discoverable.
        let test_opts = SkeletonOptions {
            filter: "test_billing_edge",
            ..default_skeleton_opts(4096)
        };
        let test_out = String::from_utf8(
            build_skeleton_output(&ws, std::slice::from_ref(&target), &test_opts, None)
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(
            test_out.contains("function tests::test_billing_edge"),
            "{test_out}"
        );

        // Zero matches yields a clean empty skeleton message.
        let miss_opts = SkeletonOptions {
            filter: "definitely_missing_xyz",
            ..default_skeleton_opts(4096)
        };
        let miss_out = String::from_utf8(
            build_skeleton_output(&ws, &[target], &miss_opts, None)
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(
            miss_out.starts_with("skeleton app/ · 0/3 files"),
            "{miss_out}"
        );
        assert!(miss_out.contains("no matching entries"), "{miss_out}");
    }

    #[test]
    fn directory_skeleton_strict_budget_fair_allocation_and_remaining_summary() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("modules");
        fs::create_dir_all(&repo).unwrap();

        // 1 huge file with 45 public structs/functions.
        let mut giant = String::from("use std::io;\nuse std::fs;\nuse std::path::Path;\n\n");
        for i in 0..45 {
            giant.push_str(&format!(
                "pub struct GiantRecord{i} {{\n    pub id: u64,\n}}\npub fn process_giant_{i}(x: u64) -> u64 {{\n    x + {i}\n}}\n\n"
            ));
        }
        fs::write(repo.join("00_giant.rs"), &giant).unwrap();

        // 24 structured Rust/TS modules.
        for i in 1..=14 {
            fs::write(
                repo.join(format!("mod_{i:02}.rs")),
                format!(
                    "pub struct Service{i} {{\n    pub port: u16,\n}}\npub fn start_service_{i}(port: u16) -> Service{i} {{\n    Service{i} {{ port }}\n}}\npub fn stop_service_{i}(svc: Service{i}) {{\n    let _ = svc.port;\n}}\n"
                ),
            )
            .unwrap();
        }
        for i in 15..=24 {
            fs::write(
                repo.join(format!("view_{i:02}.ts")),
                format!(
                    "export interface Panel{i} {{\n  title: string;\n}}\nexport function renderPanel{i}(p: Panel{i}): string {{\n  return p.title;\n}}\n"
                ),
            )
            .unwrap();
        }
        // 5 unstructured plain text files (lowest structural priority).
        for i in 1..=5 {
            fs::write(
                repo.join(format!("notes_{i:02}.txt")),
                format!("plain note line {i}\nanother line\n"),
            )
            .unwrap();
        }

        let ws = test_workspace(dir.path());
        let target = resolve_read_target(&ws, "modules", false).unwrap();

        // Strict budget enforcement across multiple ceilings.
        for budget in [4096usize, 2048, 1024, 512] {
            let out = String::from_utf8(
                build_skeleton_output(
                    &ws,
                    std::slice::from_ref(&target),
                    &default_skeleton_opts(budget),
                    None,
                )
                .unwrap()
                .bytes,
            )
            .unwrap();
            assert!(
                out.len() <= budget,
                "budget {budget} exceeded: got {} bytes:\n{out}",
                out.len()
            );
            let shown_headers = out.lines().filter(|l| l.starts_with("=== ")).count();
            assert!(
                shown_headers >= 2,
                "budget {budget}: giant file must not starve other files (only {shown_headers} shown):\n{out}"
            );
            assert!(
                out.contains("more file"),
                "budget {budget}: expected remaining-files summary:\n{out}"
            );
            if budget >= 1024 {
                assert!(
                    out.contains("Next: "),
                    "budget {budget}: expected Next hint:\n{out}"
                );
                assert!(
                    out.contains("read ") && out.contains("--outline"),
                    "budget {budget}: expected `read <file> --outline` hint:\n{out}"
                );
            }
            // At 4 KB, plain text notes are ranked below all 25 code modules so they stay omitted.
            if budget == 4096 {
                assert!(
                    !out.contains("=== notes_01.txt"),
                    "code modules should take priority over plain .txt files:\n{out}"
                );
                assert!(
                    shown_headers >= 10,
                    "expected fair coverage of >= 10 files at 4 KB, got {shown_headers}:\n{out}"
                );
            }
        }
    }

    #[test]
    fn directory_skeleton_rejects_outside_workspace_and_skips_ignored_binary_lockfiles() {
        let outer = tempfile::tempdir().unwrap();
        let outside_dir = outer.path().join("outside");
        let ws_root = outer.path().join("repo");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("secret.rs"), "pub fn secret() {}\n").unwrap();

        fs::create_dir_all(ws_root.join("src/nested/deep")).unwrap();
        fs::create_dir_all(ws_root.join("src/.git")).unwrap();
        fs::create_dir_all(ws_root.join("src/.justless")).unwrap();
        fs::create_dir_all(ws_root.join("src/target")).unwrap();
        fs::create_dir_all(ws_root.join("src/node_modules/pkg")).unwrap();

        fs::write(
            ws_root.join(".gitignore"),
            "ignored_by_git.rs\ngenerated/\n",
        )
        .unwrap();
        fs::write(ws_root.join(".justlessignore"), "ignored_by_jl.rs\n").unwrap();
        fs::create_dir_all(ws_root.join("src/generated")).unwrap();

        // Included files.
        fs::write(
            ws_root.join("src/top.rs"),
            "pub fn top_entry() -> usize {\n    1\n}\n",
        )
        .unwrap();
        fs::write(
            ws_root.join("src/nested/mid.rs"),
            "pub fn mid_entry() -> usize {\n    2\n}\n",
        )
        .unwrap();
        fs::write(
            ws_root.join("src/nested/deep/leaf.rs"),
            "pub fn leaf_entry() -> usize {\n    3\n}\n",
        )
        .unwrap();

        // Excluded / ignored / lockfile / binary files.
        fs::write(ws_root.join("src/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(ws_root.join("src/.justless/index.json"), "{}\n").unwrap();
        fs::write(ws_root.join("src/target/built.rs"), "pub fn built() {}\n").unwrap();
        fs::write(
            ws_root.join("src/node_modules/pkg/index.js"),
            "export function dep() {}\n",
        )
        .unwrap();
        fs::write(
            ws_root.join("src/ignored_by_git.rs"),
            "pub fn git_ignored() {}\n",
        )
        .unwrap();
        fs::write(
            ws_root.join("src/generated/gen.rs"),
            "pub fn generated() {}\n",
        )
        .unwrap();
        fs::write(
            ws_root.join("src/ignored_by_jl.rs"),
            "pub fn jl_ignored() {}\n",
        )
        .unwrap();
        fs::write(ws_root.join("src/Cargo.lock"), "# lockfile\nversion = 3\n").unwrap();
        fs::write(
            ws_root.join("src/package-lock.json"),
            "{\"lockfileVersion\":3}\n",
        )
        .unwrap();
        fs::write(ws_root.join("src/blob.bin"), b"\x00\x01\x02PNG\x00binary").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside_dir, ws_root.join("escape_link")).unwrap();
        }

        let ws = test_workspace(&ws_root);

        // Reject directories outside the workspace.
        assert!(
            resolve_read_target(&ws, "../outside", false)
                .unwrap_err()
                .to_string()
                .contains("outside the workspace")
        );
        assert!(
            resolve_read_target(&ws, "..", false)
                .unwrap_err()
                .to_string()
                .contains("outside the workspace")
        );
        assert!(
            resolve_read_target(&ws, outside_dir.to_str().unwrap(), false)
                .unwrap_err()
                .to_string()
                .contains("outside the workspace")
        );
        #[cfg(unix)]
        {
            assert!(
                resolve_read_target(&ws, "escape_link", false)
                    .unwrap_err()
                    .to_string()
                    .contains("outside the workspace")
            );
        }

        // Recursive walk includes only valid workspace source files.
        let src_target = resolve_read_target(&ws, "src", false).unwrap();
        let out = String::from_utf8(
            build_skeleton_output(
                &ws,
                std::slice::from_ref(&src_target),
                &default_skeleton_opts(4096),
                None,
            )
            .unwrap()
            .bytes,
        )
        .unwrap();
        assert!(out.starts_with("skeleton src/ · 3/3 files"), "{out}");
        assert!(out.contains("top_entry"), "{out}");
        assert!(out.contains("mid_entry"), "{out}");
        assert!(out.contains("leaf_entry"), "{out}");
        for forbidden in [
            "HEAD",
            "index.json",
            "built.rs",
            "node_modules",
            "ignored_by_git",
            "generated",
            "ignored_by_jl",
            "Cargo.lock",
            "package-lock.json",
            "blob.bin",
            "secret",
        ] {
            assert!(
                !out.contains(forbidden),
                "excluded item `{forbidden}` leaked into skeleton:\n{out}"
            );
        }

        // `--depth 1` restricts walk to direct children only.
        let depth1_opts = SkeletonOptions {
            max_depth: Some(1),
            ..default_skeleton_opts(4096)
        };
        let depth1_out = String::from_utf8(
            build_skeleton_output(&ws, &[src_target], &depth1_opts, None)
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(
            depth1_out.starts_with("skeleton src/ · 1/1 files"),
            "{depth1_out}"
        );
        assert!(depth1_out.contains("top_entry"), "{depth1_out}");
        assert!(!depth1_out.contains("mid_entry"), "{depth1_out}");
        assert!(!depth1_out.contains("leaf_entry"), "{depth1_out}");
    }

    #[test]
    fn directory_skeleton_json_mixed_targets_base_hoisting_and_session_dedup() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("packages/core/api")).unwrap();
        fs::create_dir_all(dir.path().join("packages/core/db")).unwrap();
        fs::write(
            dir.path().join("packages/core/api/routes.ts"),
            "export function handleHealth(): string {\n  return 'ok';\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("packages/core/db/pool.rs"),
            concat!(
                "pub struct Pool {\n    pub size: usize,\n}\n",
                "pub fn connect(size: usize) -> Pool {\n    Pool { size }\n}\n",
                "pub fn acquire(pool: &Pool) -> usize {\n    pool.size\n}\n",
                "pub fn release(pool: &Pool, slot: usize) -> usize {\n    pool.size + slot\n}\n",
                "pub fn health_check(pool: &Pool) -> bool {\n    pool.size > 0\n}\n",
                "pub fn drain(pool: &mut Pool) {\n    pool.size = 0;\n}\n",
                "pub fn resize(pool: &mut Pool, next: usize) {\n    pool.size = next;\n}\n",
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("README.md"),
            "# Core Package\n\nOverview text.\n",
        )
        .unwrap();

        let ws = test_workspace(dir.path());

        // 1. Multi-dir with common deep prefix hoists `base: packages/core/`.
        let api_dir = resolve_read_target(&ws, "packages/core/api", false).unwrap();
        let db_dir = resolve_read_target(&ws, "packages/core/db", false).unwrap();
        let multi_out = String::from_utf8(
            build_skeleton_output(
                &ws,
                &[api_dir.clone(), db_dir.clone()],
                &default_skeleton_opts(4096),
                None,
            )
            .unwrap()
            .bytes,
        )
        .unwrap();
        assert!(multi_out.contains("base: packages/core/\n"), "{multi_out}");
        assert!(multi_out.contains("=== api/routes.ts ("), "{multi_out}");
        assert!(multi_out.contains("=== db/pool.rs ("), "{multi_out}");

        // 2. Mixed DIR + FILE deduplicates overlapping files and includes standalone file.
        let dup_file = resolve_read_target(&ws, "packages/core/db/pool.rs", false).unwrap();
        let readme_file = resolve_read_target(&ws, "README.md", false).unwrap();
        let mixed_out = String::from_utf8(
            build_skeleton_output(
                &ws,
                &[db_dir.clone(), dup_file, readme_file],
                &default_skeleton_opts(4096),
                None,
            )
            .unwrap()
            .bytes,
        )
        .unwrap();
        assert!(
            mixed_out.contains("· 2/2 files · "),
            "overlapping file should be deduplicated:\n{mixed_out}"
        );
        assert_eq!(
            mixed_out.matches("=== packages/core/db/pool.rs (").count(),
            1,
            "pool.rs section should appear once:\n{mixed_out}"
        );
        assert!(mixed_out.contains("=== README.md ("), "{mixed_out}");

        // 3. `--json` structured output with tight budget populates `files` and `omitted_files`.
        let json_opts = SkeletonOptions {
            json_mode: true,
            ..default_skeleton_opts(4096)
        };
        let json_bytes = build_skeleton_output(&ws, &[api_dir, db_dir], &json_opts, None)
            .unwrap()
            .bytes;
        let parsed: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();
        assert_eq!(parsed["mode"], "skeleton");
        assert_eq!(parsed["base"], "packages/core/");
        assert_eq!(parsed["shown_files"], 2);
        assert_eq!(parsed["total_files"], 2);
        let files = parsed["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["file"], "packages/core/api/routes.ts");
        assert_eq!(files[0]["rel_path"], "api/routes.ts");
        assert_eq!(files[0]["revision"].as_str().unwrap().len(), 24);
        assert_eq!(files[0]["truncated"], false);
        assert!(!files[0]["entries"].as_array().unwrap().is_empty());
        assert_eq!(files[1]["file"], "packages/core/db/pool.rs");
        assert!(parsed["omitted_files"].as_array().unwrap().is_empty());

        // 4. Session dedup collapses unchanged complete files on second read; `--fresh` restores full output.
        let pkg_dir = resolve_read_target(&ws, "packages/core", false).unwrap();
        let session_opts = SkeletonOptions {
            can_dedup: true,
            ..default_skeleton_opts(4096)
        };
        let mut mem1 = crate::seen::Memory::open(&ws, Some("skel-sess"), false);
        let first = String::from_utf8(
            build_skeleton_output(
                &ws,
                std::slice::from_ref(&pkg_dir),
                &session_opts,
                mem1.as_mut(),
            )
            .unwrap()
            .bytes,
        )
        .unwrap();
        if let Some(m) = mem1 {
            m.save(&ws);
        }
        assert!(first.contains("struct Pool"), "{first}");

        let mut mem2 = crate::seen::Memory::open(&ws, Some("skel-sess"), false);
        let second = String::from_utf8(
            build_skeleton_output(
                &ws,
                std::slice::from_ref(&pkg_dir),
                &session_opts,
                mem2.as_mut(),
            )
            .unwrap()
            .bytes,
        )
        .unwrap();
        if let Some(m) = mem2 {
            m.save(&ws);
        }
        assert!(
            second.contains("outline unchanged"),
            "second session read should dedup unchanged files:\n{second}"
        );
        assert!(second.len() < first.len());

        let fresh_opts = SkeletonOptions {
            fresh: true,
            ..session_opts
        };
        let mut mem3 = crate::seen::Memory::open(&ws, Some("skel-sess"), true);
        let third = String::from_utf8(
            build_skeleton_output(&ws, &[pkg_dir], &fresh_opts, mem3.as_mut())
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(third.contains("struct Pool"), "{third}");
        assert!(!third.contains("outline unchanged"), "{third}");

        // 5. CLI flag parsing supports `--max-bytes`, `--budget`, `--depth`, `--match`, `--json`, and default `read DIR`.
        let read_cmd = commands()
            .into_iter()
            .find(|cmd| cmd.get_name() == "read")
            .unwrap();
        let m1 = read_cmd
            .clone()
            .try_get_matches_from([
                "read",
                "packages/core",
                "--max-bytes",
                "1024",
                "--depth",
                "2",
                "--match",
                "Pool",
                "--json",
            ])
            .unwrap();
        assert_eq!(m1.value_of("budget"), Some("1024"));
        assert_eq!(m1.value_of("depth"), Some("2"));
        assert_eq!(m1.value_of("match"), Some("Pool"));
        assert!(m1.is_present("json"));

        let m2 = read_cmd
            .try_get_matches_from(["read", "packages/core", "README.md"])
            .unwrap();
        let vals: Vec<&str> = m2.values_of("file").unwrap().collect();
        assert_eq!(vals, ["packages/core", "README.md"]);
    }

    #[test]
    fn ipynb_outline_strips_base64_surfaces_errors_and_expands_exact_cell() {
        let png_b64 = format!("iVBORw0KGgo{}", "QkJC".repeat(13_000));
        let notebook = serde_json::to_string_pretty(&serde_json::json!({
            "cells": [
                {
                    "cell_type": "markdown",
                    "metadata": {},
                    "source": ["# Sales Pipeline\n", "Quarterly forecast notebook."]
                },
                {
                    "cell_type": "code",
                    "execution_count": 1,
                    "metadata": {"tags": ["chart"]},
                    "outputs": [
                        {
                            "output_type": "display_data",
                            "data": {
                                "image/png": png_b64,
                                "text/plain": ["<Figure size 640x480 with 1 Axes>"]
                            },
                            "metadata": {}
                        }
                    ],
                    "source": [
                        "# Render revenue chart\n",
                        "import matplotlib.pyplot as plt\n",
                        "def build_chart(df):\n",
                        "    return df.plot()\n"
                    ]
                },
                {
                    "cell_type": "code",
                    "execution_count": 2,
                    "metadata": {},
                    "outputs": [
                        {
                            "output_type": "error",
                            "ename": "ZeroDivisionError",
                            "evalue": "\u{1b}[31mdivision by zero\u{1b}[0m",
                            "traceback": ["Traceback (most recent call last):", "ZeroDivisionError: division by zero"]
                        }
                    ],
                    "source": [
                        "# Margin check\n",
                        "ratio = total / 0\n"
                    ]
                },
                {
                    "cell_type": "raw",
                    "metadata": {},
                    "source": ["Raw appendix notes"]
                }
            ],
            "metadata": {"kernelspec": {"language": "python"}},
            "nbformat": 4,
            "nbformat_minor": 5
        }))
        .unwrap();

        let mut snap = Snapshot::from_text("notebooks/analysis.ipynb", notebook.clone());
        snap.index("notebooks/analysis.ipynb").unwrap();
        assert!(snap.is_notebook);
        let outline = snap.page("", None, 40, 4096).unwrap().display();

        assert!(
            outline.len() * 20 < notebook.len(),
            "expected >95% reduction: outline={} raw={}\n{outline}",
            outline.len(),
            notebook.len()
        );
        assert!(
            outline.contains("#1 ") && outline.contains(" md # Sales Pipeline"),
            "{outline}"
        );
        assert!(
            outline.contains("#2 ")
                && outline.contains(" cell [1] def build_chart(df):")
                && outline.contains("(+output:")
                && outline.contains("image/png)"),
            "{outline}"
        );
        assert!(
            !outline.contains("iVBORw0KGgo") && !outline.contains("QkJCQkJC"),
            "base64 payload must be absent from outline:\n{outline}"
        );
        assert!(
            outline.contains("#3 ")
                && outline.contains(" cell [2] ratio = total / 0")
                && outline.contains("[error: ZeroDivisionError: division by zero]"),
            "{outline}"
        );
        assert!(
            outline.contains("#4 ") && outline.contains(" raw Raw appendix notes"),
            "{outline}"
        );

        // Exact expand on cell #2 returns the exact JSON object bytes in the original file.
        let cell2_span = snap
            .resolve(&format!("{}#2", snap.revision), None, false)
            .unwrap();
        let expanded = &snap.text[cell2_span];
        assert!(expanded.starts_with('{') && expanded.ends_with('}'));
        let parsed_cell: serde_json::Value = serde_json::from_str(expanded).unwrap();
        assert_eq!(parsed_cell["cell_type"], "code");
        assert_eq!(parsed_cell["execution_count"], 1);
        assert!(expanded.contains("def build_chart(df):"));
        assert!(expanded.contains("iVBORw0KGgo"));
    }

    #[test]
    fn ipynb_invalid_or_non_notebook_falls_back_cleanly() {
        let mut corrupt = Snapshot::from_text(
            "broken.ipynb",
            "{ not valid json\nline 2\nline 3\n".to_owned(),
        );
        corrupt.index("broken.ipynb").unwrap();
        assert!(!corrupt.is_notebook);
        let corrupt_out = corrupt.page("", None, 40, 4096).unwrap().display();
        assert!(corrupt_out.contains("lines-1"), "{corrupt_out}");

        let mut non_nb = Snapshot::from_text(
            "plain.ipynb",
            "{\n  \"name\": \"not-a-notebook\",\n  \"version\": 1\n}\n".to_owned(),
        );
        non_nb.index("plain.ipynb").unwrap();
        assert!(!non_nb.is_notebook);
        let non_nb_out = non_nb.page("", None, 40, 4096).unwrap().display();
        assert!(!non_nb_out.is_empty());

        let mut empty_cells = Snapshot::from_text(
            "empty.ipynb",
            "{\n  \"cells\": [],\n  \"nbformat\": 4\n}\n".to_owned(),
        );
        empty_cells.index("empty.ipynb").unwrap();
        assert!(!empty_cells.is_notebook);
    }

    #[test]
    fn directory_skeleton_includes_ipynb_with_compact_cells() {
        let dir = tempfile::tempdir().unwrap();
        let nb_dir = dir.path().join("notebooks");
        fs::create_dir_all(&nb_dir).unwrap();

        let png_b64 = format!("iVBORw0KGgo{}", "QUFB".repeat(13_000));
        let raw_nb = serde_json::to_string_pretty(&serde_json::json!({
            "cells": [
                {
                    "cell_type": "markdown",
                    "metadata": {},
                    "source": ["# Exploration\n"]
                },
                {
                    "cell_type": "code",
                    "execution_count": 1,
                    "metadata": {},
                    "outputs": [{
                        "output_type": "display_data",
                        "data": {"image/png": png_b64},
                        "metadata": {}
                    }],
                    "source": ["def compute_metrics(df):\n", "    return df.describe()\n"]
                },
                {
                    "cell_type": "code",
                    "execution_count": 2,
                    "metadata": {},
                    "outputs": [{
                        "output_type": "error",
                        "ename": "ValueError",
                        "evalue": "empty frame",
                        "traceback": []
                    }],
                    "source": ["assert not df.empty\n"]
                }
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5
        }))
        .unwrap();
        fs::write(nb_dir.join("explore.ipynb"), &raw_nb).unwrap();
        fs::write(
            nb_dir.join("helper.py"),
            "def load_csv(path: str):\n    return open(path).read()\n",
        )
        .unwrap();

        let ws = test_workspace(dir.path());
        let target = resolve_read_target(&ws, "notebooks", false).unwrap();
        let skeleton = String::from_utf8(
            build_skeleton_output(&ws, &[target], &default_skeleton_opts(4096), None)
                .unwrap()
                .bytes,
        )
        .unwrap();

        assert!(skeleton.contains("=== explore.ipynb ("), "{skeleton}");
        assert!(skeleton.contains("=== helper.py ("), "{skeleton}");
        assert!(skeleton.contains("md # Exploration"), "{skeleton}");
        assert!(
            skeleton.contains("cell [1] def compute_metrics(df):")
                && skeleton.contains("image/png)"),
            "{skeleton}"
        );
        assert!(
            skeleton.contains("[error: ValueError: empty frame]"),
            "{skeleton}"
        );
        assert!(!skeleton.contains("iVBORw0KGgo"), "{skeleton}");
        assert!(skeleton.len() * 10 < raw_nb.len(), "{skeleton}");

        let rev = extract_file_revision(&skeleton, "explore.ipynb");
        let cell_id = extract_row_id(&skeleton, "explore.ipynb", "compute_metrics");
        let mut snap = Snapshot::open(&ws, "notebooks/explore.ipynb").unwrap();
        snap.index("notebooks/explore.ipynb").unwrap();
        let span = snap
            .resolve(&format!("{rev}#{cell_id}"), None, false)
            .unwrap();
        let cell_json: serde_json::Value = serde_json::from_str(&snap.text[span]).unwrap();
        assert_eq!(cell_json["execution_count"], 1);
    }

    #[test]
    fn terragrunt_and_terraform_lock_hcl_use_config_outline_while_tf_uses_grammar_hcl() {
        let tg = concat!(
            "include \"root\" {\n",
            "  path = find_in_parent_folders()\n",
            "}\n\n",
            "dependency \"vpc\" {\n",
            "  config_path = \"../vpc\"\n",
            "}\n\n",
            "inputs = {\n",
            "  env = \"prod\"\n",
            "  replicas = 3\n",
            "}\n",
        );
        let mut tg_snap = Snapshot::from_text("envs/prod/app/terragrunt.hcl", tg.to_owned());
        tg_snap.index("envs/prod/app/terragrunt.hcl").unwrap();
        assert_eq!(tg_snap.grammar, None);
        let tg_out = tg_snap.page("", None, 40, 4096).unwrap().display();
        assert!(
            tg_out.contains("include include.root")
                && tg_out.contains("path = find_in_parent_folders()"),
            "{tg_out}"
        );
        assert!(
            tg_out.contains("dependency dependency.vpc")
                && tg_out.contains("config_path = \"../vpc\""),
            "{tg_out}"
        );
        assert!(tg_out.contains("attr inputs"), "{tg_out}");
        let inputs_idx = tg_snap
            .entries
            .iter()
            .position(|e| e.kind == "attr" && e.name == "inputs")
            .expect("inputs attr entry");
        let inputs_span = tg_snap
            .resolve(
                &format!("{}#{}", tg_snap.revision, inputs_idx + 1),
                None,
                false,
            )
            .unwrap();
        assert_eq!(
            &tg_snap.text[inputs_span],
            "inputs = {\n  env = \"prod\"\n  replicas = 3\n}\n"
        );

        let lock = concat!(
            "provider \"registry.terraform.io/hashicorp/aws\" {\n",
            "  version     = \"5.40.0\"\n",
            "  constraints = \"~> 5.0\"\n",
            "}\n\n",
            "provider \"registry.terraform.io/hashicorp/random\" {\n",
            "  version = \"3.6.0\"\n",
            "}\n",
        );
        let mut lock_snap = Snapshot::from_text(".terraform.lock.hcl", lock.to_owned());
        lock_snap.index(".terraform.lock.hcl").unwrap();
        assert_eq!(lock_snap.grammar, None);
        let lock_out = lock_snap.page("", None, 40, 4096).unwrap().display();
        assert!(
            lock_out.contains("provider provider.registry.terraform.io/hashicorp/aws")
                || lock_out.contains("provider registry.terraform.io/hashicorp/aws"),
            "{lock_out}"
        );
        assert!(lock_out.contains("5.40.0"), "{lock_out}");
        assert!(lock_out.contains("3.6.0"), "{lock_out}");

        // Plain Terraform `.tf` stays on tree-sitter `Grammar::Hcl`.
        let tf = "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"my-logs\"\n}\n";
        let mut tf_snap = Snapshot::from_text("main.tf", tf.to_owned());
        tf_snap.index("main.tf").unwrap();
        assert_eq!(tf_snap.grammar, Some(Grammar::Hcl));
        assert!(
            tf_snap
                .entries
                .iter()
                .any(|e| e.kind == "resource" && e.name == "aws_s3_bucket.logs")
        );
    }
}
