use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use clap::{Arg, ArgMatches, Command as Cli};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use syn::visit::{self, Visit};
use tempfile::NamedTempFile;

use crate::index::Index;
use crate::polyglot::{Grammar, Parsers};
use crate::rust_syntax::Kind as RefKind;
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
    format_rejection(ws, file, edited, changes, verdict, false)
}

fn format_rejection(
    ws: &Workspace,
    file: &str,
    edited: &str,
    changes: &[Change],
    verdict: Option<&Verdict>,
    is_rename: bool,
) -> String {
    let Some(Verdict::Introduced {
        problem,
        unbalanced,
        ..
    }) = verdict
    else {
        return if is_rename {
            "rename rejected with 0 files modified".into()
        } else {
            "edit rejected".into()
        };
    };
    let mut out = if is_rename {
        format!(
            "rename aborted with 0 files modified: it leaves {file} unparseable\n{}\n",
            syntax_check::located(file, edited, problem)
        )
    } else {
        format!(
            "edit rejected: it leaves {file} unparseable, and the file is unchanged\n{}\n",
            syntax_check::located(file, edited, problem)
        )
    };
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
    let plural = if lines.contains(['-', ',', ' ']) {
        "s"
    } else {
        ""
    };
    let unbalanced_note = if *unbalanced {
        ", and its brackets no longer pair up"
    } else {
        ""
    };
    if is_rename {
        out.push_str(&format!(
            "The rename updated line{plural} {lines}. The file parsed before it{unbalanced_note}. Choose a different target name or update the affected file manually."
        ));
    } else {
        out.push_str(&format!(
            "The edit wrote line{plural} {lines}. The file parsed before it{unbalanced_note}. Fix the replacement, or pass --allow-parse-errors to write it anyway."
        ));
    }
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

// ---------------------------------------------------------------------------
// Deterministic Workspace-Wide AST-Safe Symbol Rename Engine
// ---------------------------------------------------------------------------

/// One exact identifier token occurrence slated for replacement inside a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameSpan {
    pub bytes: Range<usize>,
    pub line: usize,
    pub column: usize,
    pub snippet: String,
    /// Optional per-span replacement text (e.g. expanding `{ foo }` to `{ foo: bar }`
    /// to preserve JS/TS object property keys); defaults to `RenamePlan::new_name`.
    pub replacement: Option<String>,
}

/// All AST-verified identifier replacements planned for a single workspace file,
/// bound to that file's path-bound content revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRename {
    pub path: String,
    pub revision: String,
    pub replacements: Vec<RenameSpan>,
}

/// A deterministic, dry-runnable multi-file symbol rename plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenamePlan {
    pub qualified: String,
    pub old_name: String,
    pub new_name: String,
    pub kind: String,
    pub definition_file: String,
    pub definition_line: usize,
    pub files: Vec<FileRename>,
}

impl RenamePlan {
    pub fn total_replacements(&self) -> usize {
        self.files.iter().map(|file| file.replacements.len()).sum()
    }
}

/// Per-file summary returned by [`apply_symbol_rename`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenamedFile {
    pub path: String,
    pub revision: String,
    pub replacements: usize,
    pub lines: Vec<usize>,
}

/// Outcome of atomically applying a [`RenamePlan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameReport {
    pub qualified: String,
    pub old_name: String,
    pub new_name: String,
    pub files: Vec<RenamedFile>,
    pub total_replacements: usize,
}

/// Plans a workspace-wide, AST-safe rename of `symbol_query` to `new_name`.
///
/// Validates identifier syntax, reserved keywords, symbol uniqueness (never
/// guesses on ambiguous short names), and scope collisions before reading
/// candidate files discovered across the entire workspace index.
pub fn plan_symbol_rename(
    workspace: &Workspace,
    index: &Index,
    symbol_query: &str,
    new_name: &str,
    include_files: Option<&[String]>,
    root_given: bool,
) -> Result<RenamePlan> {
    let query = symbol_query.trim();
    ensure!(!query.is_empty(), "symbol query must not be empty");
    let target_id = index.select(query)?;
    let def = &index.definitions[target_id];
    ensure!(
        !matches!(def.kind.as_str(), "file" | "route"),
        "cannot rename {} symbol `{}`",
        def.kind,
        def.qualified
    );

    let (raw_short, scope) = symbol_short_and_scope(&def.qualified);
    let def_is_rust = def.location.file.ends_with(".rs");
    let def_grammar = if def_is_rust {
        None
    } else {
        index
            .grammar(&def.location.file)
            .or_else(|| Grammar::for_file(&def.location.file))
    };
    let old_name = if def_grammar == Some(Grammar::Sql) {
        raw_short.rsplit_once('.').map_or(raw_short, |(_, s)| s)
    } else {
        raw_short
    };
    ensure!(
        !old_name.is_empty() && !old_name.starts_with(['"', '/']),
        "symbol `{}` has non-identifier name `{old_name}` and cannot be renamed",
        def.qualified
    );
    ensure!(
        new_name != old_name,
        "new symbol name `{new_name}` is identical to current name `{old_name}`"
    );
    validate_rename_identifier(new_name, def_is_rust, def_grammar)?;

    let (candidate_files, files_importing_target) = discover_rename_files(
        workspace,
        index,
        target_id,
        old_name,
        include_files,
        root_given,
    )?;
    ensure!(
        !candidate_files.is_empty(),
        "no workspace files matched for renaming `{}`",
        def.qualified
    );

    check_rename_collisions(
        index,
        target_id,
        scope,
        new_name,
        &candidate_files,
        &files_importing_target,
    )?;

    let mut ts_parser: Option<tree_sitter::Parser> = None;
    let mut ts_lang: Option<Grammar> = None;
    let mut files = Vec::new();
    let mut saw_definition_file = false;

    for file in &candidate_files {
        let snap = Snapshot::open(workspace, file)
            .with_context(|| format!("failed to open {file} while planning rename"))?;
        let is_rust = file.ends_with(".rs");
        let grammar = if is_rust {
            None
        } else {
            index
                .grammar(file)
                .or_else(|| Grammar::detect(file, &snap.text))
        };
        if is_rust || grammar.is_some() {
            validate_rename_identifier(new_name, is_rust, grammar)?;
        } else if file == &def.location.file {
            bail!("AST-safe rename is not supported for {file}");
        } else {
            continue;
        }

        let raw_spans: Vec<(Range<usize>, Option<String>)> = if is_rust {
            find_rust_identifier_spans(&snap.text, old_name, &def.kind)
                .with_context(|| format!("failed to parse Rust AST in {file}"))?
                .into_iter()
                .map(|bytes| (bytes, None))
                .collect()
        } else if let Some(grammar) = grammar {
            find_treesitter_identifier_spans(
                &mut ts_parser,
                &mut ts_lang,
                grammar,
                file,
                &def.location.file,
                &def.kind,
                &snap.text,
                old_name,
                new_name,
            )?
        } else {
            Vec::new()
        };

        if raw_spans.is_empty() {
            continue;
        }
        if file == &def.location.file {
            saw_definition_file = true;
        }
        let replacements = raw_spans
            .into_iter()
            .map(|(bytes, custom)| make_rename_span(&snap, bytes, custom))
            .collect();
        files.push(FileRename {
            path: file.clone(),
            revision: snap.revision,
            replacements,
        });
    }

    ensure!(
        !files.is_empty(),
        "no AST identifier occurrences of `{old_name}` found to rename"
    );
    if candidate_files.contains(&def.location.file) {
        ensure!(
            saw_definition_file,
            "could not locate identifier `{old_name}` in definition file {}",
            def.location.file
        );
    }

    Ok(RenamePlan {
        qualified: def.qualified.clone(),
        old_name: old_name.to_owned(),
        new_name: new_name.to_owned(),
        kind: def.kind.clone(),
        definition_file: def.location.file.clone(),
        definition_line: def.declaration,
        files,
    })
}

/// Applies `plan` atomically across all planned workspace files.
///
/// Re-verifies every file's path-bound revision, validates all byte spans,
/// builds every modified file in memory, checks write permissions, and runs
/// `syntax_check` on every modified file BEFORE writing a single byte. If an
/// I/O error occurs mid-batch, any already-written files are rolled back to
/// their original contents.
pub fn apply_symbol_rename(workspace: &Workspace, plan: &RenamePlan) -> Result<RenameReport> {
    ensure!(!plan.files.is_empty(), "rename plan has no files to update");
    ensure!(!plan.old_name.is_empty(), "rename plan has empty old_name");
    ensure!(!plan.new_name.is_empty(), "rename plan has empty new_name");
    ensure!(
        !plan.new_name.contains('\0'),
        "replacement name must be valid text"
    );

    workspace.prepare_state()?;
    let _lock = lock_state(&workspace.state.join("edit.lock"))?;
    let max_file_bytes = workspace.settings()?.max_file_bytes;

    struct PreparedRename {
        path: String,
        abs_path: PathBuf,
        original: Vec<u8>,
        edited: String,
        new_revision: String,
        replacements: usize,
        lines: Vec<usize>,
    }

    let mut seen_paths = BTreeSet::new();
    let mut prepared = Vec::with_capacity(plan.files.len());

    // Phase 1: verify every revision, span, permission, and syntax parse in memory.
    for file_rename in &plan.files {
        ensure!(
            seen_paths.insert(file_rename.path.as_str()),
            "duplicate file `{}` in rename plan",
            file_rename.path
        );
        ensure!(
            !file_rename.replacements.is_empty(),
            "file `{}` in rename plan has no replacements",
            file_rename.path
        );

        let abs_path = workspace.source_path(&file_rename.path)?;
        let snap = Snapshot::open(workspace, &file_rename.path)
            .with_context(|| format!("failed to read {} for rename", file_rename.path))?;
        ensure!(
            snap.revision == file_rename.revision,
            "stale revision for {}: expected {}, found {}; rename aborted with 0 files modified",
            file_rename.path,
            file_rename.revision,
            snap.revision
        );

        let mut spans: Vec<(&Range<usize>, Option<&str>)> = file_rename
            .replacements
            .iter()
            .map(|r| (&r.bytes, r.replacement.as_deref()))
            .collect();
        spans.sort_by_key(|(range, _)| (range.start, range.end));
        let is_rust = file_rename.path.ends_with(".rs");
        for (idx, (span, _)) in spans.iter().enumerate() {
            ensure!(
                span.start < span.end
                    && span.end <= snap.text.len()
                    && snap.text.is_char_boundary(span.start)
                    && snap.text.is_char_boundary(span.end),
                "invalid rename span {:?} in {}; rename aborted with 0 files modified",
                span,
                file_rename.path
            );
            if idx > 0 {
                ensure!(
                    spans[idx - 1].0.end <= span.start,
                    "overlapping rename spans in {}; rename aborted with 0 files modified",
                    file_rename.path
                );
            }
            let current = &snap.text[(*span).clone()];
            let matches_old = current == plan.old_name
                || (is_rust && current.strip_prefix("r#") == Some(plan.old_name.as_str()));
            ensure!(
                matches_old,
                "span {:?} in {} holds `{current}` instead of `{}`; rename aborted with 0 files modified",
                span,
                file_rename.path,
                plan.old_name
            );
        }

        let mut edited = String::with_capacity(snap.text.len());
        let mut offset = 0usize;
        let mut shift = 0isize;
        let mut changes = Vec::with_capacity(spans.len());
        let mut lines = Vec::with_capacity(spans.len());
        for (span, custom) in &spans {
            lines.push(snap.line(span.start));
            edited.push_str(&snap.text[offset..span.start]);
            let after_start = span.start.saturating_add_signed(shift);
            let replacement = custom.unwrap_or(plan.new_name.as_str());
            edited.push_str(replacement);
            shift += replacement.len() as isize - span.len() as isize;
            changes.push(Change {
                before: (*span).clone(),
                after: after_start..after_start + replacement.len(),
            });
            offset = span.end;
        }
        edited.push_str(&snap.text[offset..]);
        lines.dedup();

        ensure!(
            edited.len() as u64 <= max_file_bytes,
            "renamed {} exceeds max_file_bytes; rename aborted with 0 files modified",
            file_rename.path
        );

        // Verify write permission up front before mutating any file.
        File::options()
            .write(true)
            .open(&abs_path)
            .with_context(|| {
                format!(
                    "{} is not writable; rename aborted with 0 files modified",
                    file_rename.path
                )
            })?;

        if edited != snap.text {
            let verdict = syntax_check::judge(
                &workspace.root,
                &file_rename.path,
                Some(&snap.text),
                &edited,
                &changes,
                true,
            );
            if matches!(
                verdict,
                Some(Verdict::Introduced {
                    parsed_before: true,
                    ..
                })
            ) {
                bail!(
                    "{}",
                    format_rejection(
                        workspace,
                        &file_rename.path,
                        &edited,
                        &changes,
                        verdict.as_ref(),
                        true,
                    )
                );
            }
        }

        let new_revision = revision_for(&file_rename.path, edited.as_bytes());
        prepared.push(PreparedRename {
            path: file_rename.path.clone(),
            abs_path,
            original: snap.text.into_bytes(),
            edited,
            new_revision,
            replacements: spans.len(),
            lines,
        });
    }

    // Phase 2: write all files, rolling back any already-written file on I/O error.
    let mut written: Vec<usize> = Vec::with_capacity(prepared.len());
    for (idx, item) in prepared.iter().enumerate() {
        if item.edited.as_bytes() == item.original.as_slice() {
            continue;
        }
        if let Err(err) = write_rename_bytes(&item.abs_path, item.edited.as_bytes()) {
            let mut rollback_errors = Vec::new();
            for &prev in written.iter().rev() {
                let prev_item = &prepared[prev];
                if let Err(rb_err) = write_rename_bytes(&prev_item.abs_path, &prev_item.original) {
                    rollback_errors.push(format!("{}: {rb_err}", prev_item.path));
                }
            }
            if rollback_errors.is_empty() {
                return Err(err).with_context(|| {
                    format!(
                        "failed writing {}; rolled back {} previously written file(s)",
                        item.path,
                        written.len()
                    )
                });
            }
            return Err(err).with_context(|| {
                format!(
                    "failed writing {}; rollback errors: {}",
                    item.path,
                    rollback_errors.join(", ")
                )
            });
        }
        written.push(idx);
    }

    let mut total_replacements = 0;
    let files = prepared
        .into_iter()
        .map(|item| {
            total_replacements += item.replacements;
            RenamedFile {
                path: item.path,
                revision: item.new_revision,
                replacements: item.replacements,
                lines: item.lines,
            }
        })
        .collect();

    Ok(RenameReport {
        qualified: plan.qualified.clone(),
        old_name: plan.old_name.clone(),
        new_name: plan.new_name.clone(),
        files,
        total_replacements,
    })
}

/// Renders a compact, high-signal preview of `plan` with per-file revisions,
/// occurrence counts, and `L<line>` snippets.
pub fn render_rename_plan(plan: &RenamePlan, dry_run: bool) -> String {
    let total = plan.total_replacements();
    let tag = if dry_run { "rename dry-run" } else { "rename" };
    let file_word = if plan.files.len() == 1 {
        "file"
    } else {
        "files"
    };
    let occ_word = if total == 1 {
        "occurrence"
    } else {
        "occurrences"
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "[{tag}] {} -> {} ({} · {} {file_word}, {total} {occ_word})",
        plan.qualified,
        plan.new_name,
        plan.kind,
        plan.files.len()
    );
    for file in &plan.files {
        let count = file.replacements.len();
        let word = if count == 1 {
            "occurrence"
        } else {
            "occurrences"
        };
        let _ = writeln!(
            out,
            "{} · rev {} · {count} {word}",
            file.path, file.revision
        );
        let mut by_line: BTreeMap<usize, (&str, usize)> = BTreeMap::new();
        for span in &file.replacements {
            by_line
                .entry(span.line)
                .and_modify(|(_, n)| *n += 1)
                .or_insert((span.snippet.as_str(), 1));
        }
        for (line, (snippet, count)) in by_line {
            if count > 1 {
                let _ = writeln!(out, "  L{line} (×{count}): {snippet}");
            } else {
                let _ = writeln!(out, "  L{line}: {snippet}");
            }
        }
    }
    out
}

fn write_rename_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut target = File::options()
        .write(true)
        .open(path)
        .with_context(|| format!("{} is not writable", path.display()))?;
    let replacement = path
        .parent()
        .and_then(|dir| NamedTempFile::new_in(dir).ok())
        .filter(|temp| replaceable(path, temp));
    match replacement {
        Some(mut temp) => {
            temp.write_all(bytes)?;
            temp.as_file()
                .set_permissions(fs::metadata(path)?.permissions())?;
            if temp.persist(path).is_err() {
                write_in_place(&mut target, bytes)?;
            }
        }
        None => {
            write_in_place(&mut target, bytes)?;
        }
    }
    Ok(())
}

fn symbol_short_and_scope(qualified: &str) -> (&str, Option<&str>) {
    match qualified.rsplit_once("::") {
        Some((scope, short)) => (short, Some(scope)),
        None => (qualified, None),
    }
}

fn validate_rename_identifier(name: &str, is_rust: bool, grammar: Option<Grammar>) -> Result<()> {
    ensure!(!name.is_empty(), "new symbol name must not be empty");
    let allow_dollar = matches!(
        grammar.map(Grammar::family),
        Some(Grammar::JavaScript | Grammar::Java)
    );
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty name");
    let valid_start = first == '_' || first.is_alphabetic() || (allow_dollar && first == '$');
    ensure!(
        valid_start && chars.all(|c| c == '_' || c.is_alphanumeric() || (allow_dollar && c == '$')),
        "invalid symbol name `{name}`: must be a valid identifier without spaces or punctuation"
    );
    if is_language_keyword(name, is_rust, grammar) {
        let lang = if is_rust {
            "Rust"
        } else {
            grammar.map_or("target language", Grammar::name)
        };
        bail!("invalid symbol name `{name}`: reserved keyword in {lang}");
    }
    Ok(())
}

fn is_language_keyword(name: &str, is_rust: bool, grammar: Option<Grammar>) -> bool {
    const RUST: &[&str] = &[
        "_", "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod",
        "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
        "trait", "true", "type", "unsafe", "use", "where", "while", "abstract", "become", "box",
        "do", "final", "macro", "override", "priv", "try", "typeof", "unsized", "virtual", "yield",
    ];
    const JS_TS: &[&str] = &[
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "function",
        "if",
        "import",
        "in",
        "instanceof",
        "let",
        "new",
        "null",
        "return",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "typeof",
        "var",
        "void",
        "while",
        "with",
        "yield",
        "implements",
        "interface",
        "package",
        "private",
        "protected",
        "public",
        "static",
        "type",
    ];
    const PYTHON: &[&str] = &[
        "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
        "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
        "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return",
        "try", "while", "with", "yield",
    ];
    const GO: &[&str] = &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
    ];
    const JVM: &[&str] = &[
        "abstract",
        "assert",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "false",
        "final",
        "finally",
        "float",
        "for",
        "fun",
        "goto",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "int",
        "interface",
        "long",
        "native",
        "new",
        "null",
        "object",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "true",
        "try",
        "val",
        "var",
        "void",
        "volatile",
        "when",
        "while",
    ];
    const C_FAMILY: &[&str] = &[
        "auto",
        "break",
        "case",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "delete",
        "do",
        "double",
        "else",
        "enum",
        "extern",
        "false",
        "float",
        "for",
        "goto",
        "if",
        "inline",
        "int",
        "long",
        "namespace",
        "new",
        "nullptr",
        "register",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "struct",
        "switch",
        "template",
        "this",
        "throw",
        "true",
        "try",
        "typedef",
        "union",
        "unsigned",
        "virtual",
        "void",
        "volatile",
        "while",
    ];
    const CSHARP: &[&str] = &[
        "abstract",
        "as",
        "base",
        "bool",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "checked",
        "class",
        "const",
        "continue",
        "decimal",
        "default",
        "delegate",
        "do",
        "double",
        "else",
        "enum",
        "event",
        "explicit",
        "extern",
        "false",
        "finally",
        "fixed",
        "float",
        "for",
        "foreach",
        "goto",
        "if",
        "implicit",
        "in",
        "int",
        "interface",
        "internal",
        "is",
        "lock",
        "long",
        "namespace",
        "new",
        "null",
        "object",
        "operator",
        "out",
        "override",
        "params",
        "private",
        "protected",
        "public",
        "readonly",
        "ref",
        "return",
        "sbyte",
        "sealed",
        "short",
        "sizeof",
        "stackalloc",
        "static",
        "string",
        "struct",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "typeof",
        "uint",
        "ulong",
        "unchecked",
        "unsafe",
        "ushort",
        "using",
        "virtual",
        "void",
        "volatile",
        "while",
    ];
    const RUBY: &[&str] = &[
        "BEGIN", "END", "alias", "and", "begin", "break", "case", "class", "def", "do", "else",
        "elsif", "end", "ensure", "false", "for", "if", "in", "module", "next", "nil", "not", "or",
        "redo", "rescue", "retry", "return", "self", "super", "then", "true", "undef", "unless",
        "until", "when", "while", "yield",
    ];
    const PHP: &[&str] = &[
        "abstract",
        "and",
        "array",
        "as",
        "break",
        "callable",
        "case",
        "catch",
        "class",
        "clone",
        "const",
        "continue",
        "declare",
        "default",
        "do",
        "echo",
        "else",
        "elseif",
        "empty",
        "enddeclare",
        "endfor",
        "endforeach",
        "endif",
        "endswitch",
        "endwhile",
        "eval",
        "exit",
        "extends",
        "final",
        "finally",
        "fn",
        "for",
        "foreach",
        "function",
        "global",
        "goto",
        "if",
        "implements",
        "include",
        "instanceof",
        "interface",
        "isset",
        "list",
        "match",
        "namespace",
        "new",
        "or",
        "print",
        "private",
        "protected",
        "public",
        "readonly",
        "require",
        "return",
        "static",
        "switch",
        "throw",
        "trait",
        "try",
        "unset",
        "use",
        "var",
        "while",
        "xor",
        "yield",
    ];
    const DART: &[&str] = &[
        "abstract",
        "as",
        "assert",
        "async",
        "await",
        "base",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "covariant",
        "default",
        "deferred",
        "do",
        "dynamic",
        "else",
        "enum",
        "export",
        "extends",
        "extension",
        "external",
        "factory",
        "false",
        "final",
        "finally",
        "for",
        "get",
        "hide",
        "if",
        "implements",
        "import",
        "in",
        "interface",
        "is",
        "late",
        "library",
        "mixin",
        "new",
        "null",
        "on",
        "operator",
        "part",
        "required",
        "rethrow",
        "return",
        "sealed",
        "set",
        "show",
        "static",
        "super",
        "switch",
        "sync",
        "this",
        "throw",
        "true",
        "try",
        "typedef",
        "var",
        "void",
        "while",
        "with",
        "yield",
    ];
    const SWIFT: &[&str] = &[
        "_",
        "Self",
        "actor",
        "any",
        "as",
        "associatedtype",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "continue",
        "default",
        "defer",
        "deinit",
        "do",
        "else",
        "enum",
        "extension",
        "fallthrough",
        "false",
        "fileprivate",
        "final",
        "for",
        "func",
        "guard",
        "if",
        "import",
        "in",
        "init",
        "inout",
        "internal",
        "is",
        "lazy",
        "let",
        "mutating",
        "nil",
        "nonmutating",
        "open",
        "operator",
        "override",
        "precedencegroup",
        "private",
        "protocol",
        "public",
        "repeat",
        "rethrows",
        "return",
        "self",
        "some",
        "static",
        "struct",
        "subscript",
        "super",
        "switch",
        "throw",
        "throws",
        "true",
        "try",
        "typealias",
        "unowned",
        "var",
        "weak",
        "where",
        "while",
    ];
    const ELIXIR: &[&str] = &[
        "after",
        "and",
        "catch",
        "cond",
        "def",
        "defdelegate",
        "defexception",
        "defguard",
        "defguardp",
        "defimpl",
        "defmacro",
        "defmacrop",
        "defmodule",
        "defoverridable",
        "defp",
        "defprotocol",
        "defstruct",
        "do",
        "else",
        "end",
        "false",
        "fn",
        "for",
        "if",
        "in",
        "nil",
        "not",
        "or",
        "quote",
        "raise",
        "receive",
        "require",
        "rescue",
        "true",
        "try",
        "unless",
        "unquote",
        "use",
        "when",
        "with",
    ];
    const LUA: &[&str] = &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if",
        "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ];
    const ZIG: &[&str] = &[
        "_",
        "addrspace",
        "align",
        "allowzero",
        "and",
        "anyframe",
        "anytype",
        "asm",
        "async",
        "await",
        "break",
        "callconv",
        "catch",
        "comptime",
        "const",
        "continue",
        "defer",
        "else",
        "enum",
        "errdefer",
        "error",
        "export",
        "extern",
        "false",
        "fn",
        "for",
        "if",
        "inline",
        "linksection",
        "noalias",
        "noinline",
        "nosuspend",
        "null",
        "opaque",
        "or",
        "orelse",
        "packed",
        "pub",
        "resume",
        "return",
        "struct",
        "suspend",
        "switch",
        "test",
        "threadlocal",
        "true",
        "try",
        "undefined",
        "union",
        "unreachable",
        "usingnamespace",
        "var",
        "volatile",
        "while",
    ];
    const SQL: &[&str] = &[
        "all",
        "alter",
        "and",
        "as",
        "asc",
        "begin",
        "between",
        "by",
        "case",
        "cast",
        "check",
        "commit",
        "constraint",
        "create",
        "cross",
        "database",
        "declare",
        "default",
        "delete",
        "desc",
        "distinct",
        "drop",
        "else",
        "end",
        "except",
        "exists",
        "false",
        "for",
        "foreign",
        "from",
        "full",
        "function",
        "grant",
        "group",
        "having",
        "if",
        "ilike",
        "in",
        "index",
        "inner",
        "insert",
        "intersect",
        "into",
        "is",
        "join",
        "key",
        "left",
        "like",
        "limit",
        "loop",
        "not",
        "null",
        "offset",
        "on",
        "or",
        "order",
        "outer",
        "primary",
        "procedure",
        "recursive",
        "references",
        "returning",
        "returns",
        "revoke",
        "right",
        "rollback",
        "schema",
        "select",
        "set",
        "table",
        "then",
        "transaction",
        "trigger",
        "true",
        "truncate",
        "union",
        "unique",
        "update",
        "using",
        "values",
        "view",
        "when",
        "where",
        "while",
        "with",
    ];
    const FALLBACK: &[&str] = &[
        "if", "else", "for", "while", "return", "fn", "def", "func", "class", "struct", "enum",
        "true", "false", "null", "nil", "let", "var", "const", "import", "pub", "end", "do", "in",
    ];
    if !is_rust && grammar.map(Grammar::family) == Some(Grammar::Sql) {
        return SQL.iter().any(|kw| kw.eq_ignore_ascii_case(name));
    }
    let list = if is_rust {
        RUST
    } else {
        match grammar.map(Grammar::family) {
            Some(Grammar::JavaScript) => JS_TS,
            Some(Grammar::Python) => PYTHON,
            Some(Grammar::Go) => GO,
            Some(Grammar::Java) => JVM,
            Some(Grammar::C) => C_FAMILY,
            Some(Grammar::CSharp) => CSHARP,
            Some(Grammar::Ruby) => RUBY,
            Some(Grammar::Php) => PHP,
            Some(Grammar::Dart) => DART,
            Some(Grammar::Swift) => SWIFT,
            Some(Grammar::Elixir) => ELIXIR,
            Some(Grammar::Lua) => LUA,
            Some(Grammar::Zig) => ZIG,
            _ => FALLBACK,
        }
    };
    list.contains(&name)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RenameFilter {
    WholeWorkspace,
    Path(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenameLang {
    Rust,
    Polyglot(Grammar),
}

fn rename_lang_for_file(index: &Index, workspace: &Workspace, file: &str) -> Option<RenameLang> {
    if file.ends_with(".rs") {
        Some(RenameLang::Rust)
    } else {
        index
            .grammar(file)
            .or_else(|| Grammar::for_file(file))
            .or_else(|| {
                Snapshot::open(workspace, file)
                    .ok()
                    .and_then(|snap| Grammar::detect(file, &snap.text))
            })
            .map(|g| RenameLang::Polyglot(g.family()))
    }
}

fn normalize_rename_filter(
    workspace: &Workspace,
    raw: &str,
    root_given: bool,
) -> Result<RenameFilter> {
    let trimmed = raw.trim();
    ensure!(!trimmed.is_empty(), "include_files entry must not be empty");
    let slashed_raw = trimmed.replace('\\', "/");
    if matches!(slashed_raw.as_str(), "." | "./") {
        if root_given || workspace.cwd == workspace.root {
            return Ok(RenameFilter::WholeWorkspace);
        }
        if let Ok(rel) = workspace.cwd.strip_prefix(&workspace.root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            let clean = rel.trim_matches('/');
            if clean.is_empty() {
                return Ok(RenameFilter::WholeWorkspace);
            }
            return Ok(RenameFilter::Path(clean.to_owned()));
        }
        return Ok(RenameFilter::WholeWorkspace);
    }
    let mut resolved = PathBuf::new();
    for part in workspace.cwd.join(Path::new(trimmed)).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            part => resolved.push(part),
        }
    }
    if let Ok(canon) = resolved.canonicalize()
        && canon == workspace.root
    {
        return Ok(RenameFilter::WholeWorkspace);
    }
    let rel = workspace_file(workspace, trimmed, root_given)?;
    let slashed = rel.replace('\\', "/");
    let clean = slashed
        .strip_prefix("./")
        .unwrap_or(&slashed)
        .trim_end_matches('/');
    if clean.is_empty() || clean == "." {
        return Ok(RenameFilter::WholeWorkspace);
    }
    Ok(RenameFilter::Path(clean.to_owned()))
}

fn normalize_rel_path(path: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

fn strip_string_quotes(raw: &str) -> &str {
    let s = raw.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"'))
            || (s.starts_with('\'') && s.ends_with('\''))
            || (s.starts_with('`') && s.ends_with('`')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn import_spec_matches_def_file(
    file: &str,
    family: Grammar,
    raw_spec: &str,
    def_file: &str,
) -> bool {
    let spec = strip_string_quotes(raw_spec);
    if spec.is_empty() {
        return false;
    }
    let def_norm = def_file.replace('\\', "/");
    let file_norm = file.replace('\\', "/");
    match family {
        Grammar::JavaScript => {
            if spec.starts_with('.') {
                let parent = Path::new(&file_norm)
                    .parent()
                    .unwrap_or_else(|| Path::new(""));
                let Some(joined) = normalize_rel_path(&parent.join(spec)) else {
                    return false;
                };
                if joined == def_norm {
                    return true;
                }
                let p = Path::new(&joined);
                let ext = p.extension().and_then(|e| e.to_str());
                for candidate_ext in ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"] {
                    if ext.is_none() {
                        if format!("{joined}.{candidate_ext}") == def_norm
                            || format!("{joined}/index.{candidate_ext}") == def_norm
                        {
                            return true;
                        }
                    } else if matches!(ext, Some("js" | "jsx" | "mjs" | "cjs"))
                        && p.with_extension(candidate_ext).to_str() == Some(def_norm.as_str())
                    {
                        return true;
                    }
                }
                false
            } else {
                let stripped = spec
                    .trim_start_matches("@/")
                    .trim_start_matches("~/")
                    .trim_start_matches("#/");
                let def_stem = Path::new(&def_norm)
                    .with_extension("")
                    .to_string_lossy()
                    .replace('\\', "/");
                let def_stem = def_stem.strip_suffix("/index").unwrap_or(&def_stem);
                let spec_stem = Path::new(stripped)
                    .with_extension("")
                    .to_string_lossy()
                    .replace('\\', "/");
                let spec_stem = spec_stem.strip_suffix("/index").unwrap_or(&spec_stem);
                def_stem == spec_stem
                    || def_stem
                        .strip_suffix(spec_stem)
                        .is_some_and(|prefix| prefix.ends_with('/'))
            }
        }
        Grammar::Python => {
            let dots = spec.bytes().take_while(|&b| b == b'.').count();
            let module = spec[dots..].replace('.', "/");
            if dots > 0 {
                let parent = Path::new(&file_norm)
                    .parent()
                    .unwrap_or_else(|| Path::new(""));
                let Some(anc) = parent.ancestors().nth(dots - 1) else {
                    return false;
                };
                let joined = if module.is_empty() {
                    anc.to_path_buf()
                } else {
                    anc.join(&module)
                };
                let Some(base) = normalize_rel_path(&joined) else {
                    return false;
                };
                if base.is_empty() {
                    def_norm == "__init__.py"
                } else {
                    def_norm == format!("{base}.py") || def_norm == format!("{base}/__init__.py")
                }
            } else {
                let simple = format!("{module}.py");
                let pkg = format!("{module}/__init__.py");
                def_norm == simple
                    || def_norm == pkg
                    || def_norm.ends_with(&format!("/{simple}"))
                    || def_norm.ends_with(&format!("/{pkg}"))
            }
        }
        Grammar::Go => {
            let def_dir = Path::new(&def_norm)
                .parent()
                .and_then(Path::to_str)
                .unwrap_or("");
            !def_dir.is_empty()
                && (spec == def_dir
                    || spec
                        .strip_suffix(def_dir)
                        .is_some_and(|prefix| prefix.ends_with('/'))
                    || spec.rsplit('/').next()
                        == Path::new(def_dir).file_name().and_then(|n| n.to_str()))
        }
        _ => {
            let def_stem = Path::new(&def_norm)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            !def_stem.is_empty()
                && (spec == def_norm
                    || spec.ends_with(&def_norm)
                    || Path::new(spec).file_stem().and_then(|s| s.to_str()) == Some(def_stem))
        }
    }
}

fn node_text<'a>(node: tree_sitter::Node<'_>, text: &'a str) -> &'a str {
    text.get(node.byte_range()).unwrap_or("")
}

fn contains_ident_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(off) = haystack[from..].find(needle) {
        let start = from + off;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '$'));
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '$'));
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len();
    }
    false
}

/// Inspects a polyglot file that has a file-level `Import` edge to `source_file`
/// and determines whether it actually imports `old_name` (explicitly or via
/// namespace/default/wildcard import) and whether it re-exports `old_name`.
fn polyglot_file_import_status(
    parser_slot: &mut Option<tree_sitter::Parser>,
    cached_lang: &mut Option<Grammar>,
    workspace: &Workspace,
    index: &Index,
    file: &str,
    source_file: &str,
    old_name: &str,
) -> (bool, bool, bool) {
    let Ok(snap) = Snapshot::open(workspace, file) else {
        return (false, false, false);
    };
    let Some(grammar) = index
        .grammar(file)
        .or_else(|| Grammar::detect(file, &snap.text))
    else {
        return (false, false, false);
    };
    let (parse_grammar, masked) = match grammar {
        Grammar::Vue | Grammar::Svelte | Grammar::Astro => {
            let (script_text, jsx) =
                crate::components::scripts(&snap.text, grammar == Grammar::Astro);
            let lang = if jsx {
                Grammar::Tsx
            } else {
                Grammar::TypeScript
            };
            (lang, Some(script_text))
        }
        _ => (grammar, None),
    };
    let parser = parser_slot.get_or_insert_with(tree_sitter::Parser::new);
    if *cached_lang != Some(parse_grammar) {
        *cached_lang = None;
        if parser.set_language(&parse_grammar.language()).is_err() {
            return (false, false, false);
        }
        *cached_lang = Some(parse_grammar);
    }
    let parse_input = masked.as_deref().unwrap_or(&snap.text);
    let Some(tree) = parser.parse(parse_input, None) else {
        return (false, false, false);
    };

    let family = grammar.family();
    let mut relevant = false;
    let mut reexports = false;
    let mut direct_import = false;
    let mut cursor = tree.walk();
    'walk: loop {
        let node = cursor.node();
        let kind = node.kind();
        match family {
            Grammar::JavaScript => match kind {
                "import_statement" => {
                    let matches_source = node.child_by_field_name("source").is_some_and(|src| {
                        import_spec_matches_def_file(
                            file,
                            family,
                            node_text(src, parse_input),
                            source_file,
                        )
                    });
                    if matches_source {
                        let stmt = node_text(node, parse_input);
                        let mut has_ns_or_default = false;
                        let mut has_named_old = false;
                        for i in 0..node.child_count() {
                            if let Some(child) = node.child(i as u32)
                                && child.kind() == "import_clause"
                            {
                                for j in 0..child.child_count() {
                                    if let Some(cc) = child.child(j as u32) {
                                        match cc.kind() {
                                            "namespace_import" | "identifier" => {
                                                has_ns_or_default = true;
                                            }
                                            "named_imports"
                                                if contains_ident_word(
                                                    node_text(cc, parse_input),
                                                    old_name,
                                                ) =>
                                            {
                                                has_named_old = true;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                        if has_ns_or_default || has_named_old || stmt.contains('*') {
                            relevant = true;
                            if has_named_old {
                                direct_import = true;
                            }
                        }
                    }
                }
                "export_statement" => {
                    let matches_source = node.child_by_field_name("source").is_some_and(|src| {
                        import_spec_matches_def_file(
                            file,
                            family,
                            node_text(src, parse_input),
                            source_file,
                        )
                    });
                    if matches_source {
                        let stmt = node_text(node, parse_input);
                        let is_star = stmt.contains('*');
                        let mentions_old = contains_ident_word(stmt, old_name);
                        if is_star || mentions_old {
                            relevant = true;
                            reexports = true;
                        }
                    }
                }
                "variable_declarator" => {
                    if let Some(val) = node.child_by_field_name("value")
                        && val.kind() == "call_expression"
                        && val
                            .child_by_field_name("function")
                            .is_some_and(|f| node_text(f, parse_input) == "require")
                        && let Some(args) = val.child_by_field_name("arguments")
                        && (0..args.named_child_count()).any(|i| {
                            args.named_child(i as u32).is_some_and(|arg| {
                                import_spec_matches_def_file(
                                    file,
                                    family,
                                    node_text(arg, parse_input),
                                    source_file,
                                )
                            })
                        })
                        && let Some(lhs) = node.child_by_field_name("name")
                    {
                        if lhs.kind() == "identifier" {
                            relevant = true;
                        } else if contains_ident_word(node_text(lhs, parse_input), old_name) {
                            relevant = true;
                            direct_import = true;
                        }
                    }
                }
                _ => {}
            },
            Grammar::Python => match kind {
                "import_statement" => {
                    let stmt = node_text(node, parse_input);
                    // Any `import pkg.util` or `import pkg.util as u` targeting `source_file`
                    // is a namespace import.
                    for i in 0..node.named_child_count() {
                        if let Some(child) = node.named_child(i as u32) {
                            let spec = if child.kind() == "aliased_import" {
                                child
                                    .child_by_field_name("name")
                                    .map_or("", |n| node_text(n, parse_input))
                            } else {
                                node_text(child, parse_input)
                            };
                            if import_spec_matches_def_file(file, family, spec, source_file) {
                                relevant = true;
                            }
                        }
                    }
                    if !relevant && contains_ident_word(stmt, old_name) {
                        relevant = true;
                    }
                }
                "import_from_statement" => {
                    let mod_text = node
                        .child_by_field_name("module_name")
                        .map_or("", |m| node_text(m, parse_input));
                    let mod_matches =
                        import_spec_matches_def_file(file, family, mod_text, source_file);
                    let mut imports_module_itself = false;
                    let mut wildcard = false;
                    let mut named_old = false;
                    for i in 0..node.named_child_count() {
                        let Some(child) = node.named_child(i as u32) else {
                            continue;
                        };
                        if node.child_by_field_name("module_name") == Some(child) {
                            continue;
                        }
                        if child.kind() == "wildcard_import" {
                            wildcard = true;
                            continue;
                        }
                        let imported_name = if child.kind() == "aliased_import" {
                            child
                                .child_by_field_name("name")
                                .map_or("", |n| node_text(n, parse_input))
                        } else {
                            node_text(child, parse_input)
                        };
                        if imported_name == old_name {
                            named_old = true;
                        }
                        let combined = if mod_text.ends_with('.') || mod_text.is_empty() {
                            format!("{mod_text}{imported_name}")
                        } else {
                            format!("{mod_text}.{imported_name}")
                        };
                        if import_spec_matches_def_file(file, family, &combined, source_file) {
                            imports_module_itself = true;
                        }
                    }
                    if imports_module_itself || (mod_matches && (wildcard || named_old)) {
                        relevant = true;
                        if named_old {
                            direct_import = true;
                        }
                        if file.ends_with("__init__.py") {
                            reexports = true;
                        }
                    }
                }
                _ => {}
            },
            _ => {
                // Other Tree-sitter languages (Go, Dart, Swift, Ruby, PHP, Java, C/C++):
                // include unless the import explicitly uses a selective list that omits `old_name`.
                if kind.contains("import") || kind.contains("use") {
                    let stmt = node_text(node, parse_input);
                    if contains_ident_word(stmt, old_name) {
                        relevant = true;
                        direct_import = true;
                    } else if !(stmt.contains(" show ") && !stmt.contains(old_name)) {
                        relevant = true;
                    }
                }
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    (relevant, reexports, direct_import)
}

fn rust_file_module_path(file: &str) -> Vec<String> {
    let slashed = file.replace('\\', "/");
    let without_ext = slashed.strip_suffix(".rs").unwrap_or(&slashed);
    let after_src = if let Some((_, tail)) = without_ext.rsplit_once("/src/") {
        tail
    } else if let Some(tail) = without_ext.strip_prefix("src/") {
        tail
    } else {
        return Vec::new();
    };
    let parts: Vec<&str> = after_src.split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        [] | ["lib"] | ["main"] => Vec::new(),
        [init @ .., "mod"] => init.iter().map(|s| (*s).to_owned()).collect(),
        _ => parts.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn rust_crate_root_prefix(file: &str) -> &str {
    file.rsplit_once("/src/").map_or("", |(prefix, _)| prefix)
}

fn resolve_rust_use_prefix(
    current_mod: &[String],
    crate_name: &str,
    prefix: &[String],
) -> Option<Vec<String>> {
    let (first, rest) = prefix.split_first()?;
    match first.as_str() {
        "crate" => Some(rest.to_vec()),
        "self" => {
            let mut out = current_mod.to_vec();
            out.extend_from_slice(rest);
            Some(out)
        }
        "super" => {
            let mut out = current_mod.to_vec();
            out.pop()?;
            for seg in rest {
                if seg == "super" {
                    out.pop()?;
                } else if seg != "self" {
                    out.push(seg.clone());
                }
            }
            Some(out)
        }
        name if name == crate_name => Some(rest.to_vec()),
        _ => {
            let mut out = current_mod.to_vec();
            out.extend_from_slice(prefix);
            Some(out)
        }
    }
}

struct RustUseEntry {
    is_pub: bool,
    current_mod: Vec<String>,
    prefix: Vec<String>,
    leaf: Option<String>, // None for glob `*`
}

fn collect_rust_uses_in_items(
    items: &[syn::Item],
    current_mod: &mut Vec<String>,
    out: &mut Vec<RustUseEntry>,
) {
    fn walk_tree(
        tree: &syn::UseTree,
        is_pub: bool,
        current_mod: &[String],
        prefix: &mut Vec<String>,
        out: &mut Vec<RustUseEntry>,
    ) {
        match tree {
            syn::UseTree::Path(part) => {
                let saved = prefix.len();
                let ident = part.ident.to_string();
                let clean = ident.strip_prefix("r#").unwrap_or(&ident).to_owned();
                prefix.push(clean);
                walk_tree(&part.tree, is_pub, current_mod, prefix, out);
                prefix.truncate(saved);
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    walk_tree(item, is_pub, current_mod, prefix, out);
                }
            }
            syn::UseTree::Glob(_) => {
                out.push(RustUseEntry {
                    is_pub,
                    current_mod: current_mod.to_vec(),
                    prefix: prefix.clone(),
                    leaf: None,
                });
            }
            syn::UseTree::Name(name) => {
                let ident = name.ident.to_string();
                let clean = ident.strip_prefix("r#").unwrap_or(&ident).to_owned();
                if clean != "self" {
                    out.push(RustUseEntry {
                        is_pub,
                        current_mod: current_mod.to_vec(),
                        prefix: prefix.clone(),
                        leaf: Some(clean),
                    });
                }
            }
            syn::UseTree::Rename(rename) => {
                let ident = rename.ident.to_string();
                let clean = ident.strip_prefix("r#").unwrap_or(&ident).to_owned();
                if clean != "self" {
                    out.push(RustUseEntry {
                        is_pub,
                        current_mod: current_mod.to_vec(),
                        prefix: prefix.clone(),
                        leaf: Some(clean),
                    });
                }
            }
        }
    }

    for item in items {
        match item {
            syn::Item::Use(use_item) => {
                let is_pub = !matches!(use_item.vis, syn::Visibility::Inherited);
                let mut prefix = Vec::new();
                walk_tree(&use_item.tree, is_pub, current_mod, &mut prefix, out);
            }
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    let ident = m.ident.to_string();
                    let clean = ident.strip_prefix("r#").unwrap_or(&ident).to_owned();
                    current_mod.push(clean);
                    collect_rust_uses_in_items(inner, current_mod, out);
                    current_mod.pop();
                }
            }
            _ => {}
        }
    }
}

fn rust_file_has_qualified_reexport_path(
    ast: &syn::File,
    file_mod: &[String],
    crate_name: &str,
    old_name: &str,
    reexported_mods: &BTreeSet<Vec<String>>,
) -> bool {
    struct PathFinder<'a> {
        file_mod: &'a [String],
        crate_name: &'a str,
        old_name: &'a str,
        reexported_mods: &'a BTreeSet<Vec<String>>,
        found: bool,
    }
    impl<'ast> Visit<'ast> for PathFinder<'_> {
        fn visit_attribute(&mut self, _node: &'ast syn::Attribute) {}
        fn visit_path(&mut self, path: &'ast syn::Path) {
            if self.found {
                return;
            }
            if path.segments.len() >= 2 {
                let last_s = path.segments.last().unwrap().ident.to_string();
                let last_clean = last_s.strip_prefix("r#").unwrap_or(&last_s);
                if last_clean == self.old_name {
                    let prefix: Vec<String> = path
                        .segments
                        .iter()
                        .take(path.segments.len() - 1)
                        .map(|seg| {
                            let s = seg.ident.to_string();
                            s.strip_prefix("r#").unwrap_or(&s).to_owned()
                        })
                        .collect();
                    if let Some(resolved) =
                        resolve_rust_use_prefix(self.file_mod, self.crate_name, &prefix)
                        && self.reexported_mods.contains(&resolved)
                    {
                        self.found = true;
                        return;
                    }
                }
            }
            visit::visit_path(self, path);
        }
    }
    let mut finder = PathFinder {
        file_mod,
        crate_name,
        old_name,
        reexported_mods,
        found: false,
    };
    finder.visit_file(ast);
    finder.found
}

/// Discovers Rust files that transitively `pub use` re-export `target_id` and
/// every consumer file that imports or references `old_name` through those
/// re-exporting modules.
fn discover_rust_reexport_files(
    workspace: &Workspace,
    index: &Index,
    target_id: usize,
    old_name: &str,
    candidates: &mut BTreeSet<String>,
    importing_target: &mut BTreeSet<String>,
) {
    let def = &index.definitions[target_id];
    let qualified_parts: Vec<&str> = def.qualified.split("::").collect();
    let Some(&crate_name) = qualified_parts.first() else {
        return;
    };
    let def_mod: Vec<String> = if qualified_parts.len() >= 2 {
        qualified_parts[1..qualified_parts.len() - 1]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    } else {
        Vec::new()
    };
    let def_crate_prefix = rust_crate_root_prefix(&def.location.file);

    let mut available_mods: BTreeSet<Vec<String>> = BTreeSet::new();
    available_mods.insert(def_mod);
    let mut reexported_mods: BTreeSet<Vec<String>> = BTreeSet::new();

    // Parse candidate Rust files lazily (cached per path) for `pub use` analysis.
    type ParsedRustFile = (Vec<String>, syn::File, Vec<RustUseEntry>);
    type ParsedRustCache = BTreeMap<String, Option<ParsedRustFile>>;
    let mut parsed_cache: ParsedRustCache = BTreeMap::new();
    let load_rust = |path: &str, cache: &mut ParsedRustCache| -> bool {
        if !cache.contains_key(path) {
            let entry = Snapshot::open(workspace, path).ok().and_then(|snap| {
                if !snap.text.contains("use ") && !snap.text.contains(old_name) {
                    return None;
                }
                let ast = crate::rust_syntax::parse_file(&snap.text).ok()?;
                let mut fmod = rust_file_module_path(path);
                let mut uses = Vec::new();
                collect_rust_uses_in_items(&ast.items, &mut fmod, &mut uses);
                Some((rust_file_module_path(path), ast, uses))
            });
            cache.insert(path.to_owned(), entry);
        }
        cache.get(path).is_some_and(Option::is_some)
    };

    // Collect all Rust files in the workspace that might contain `pub use`.
    let rs_files: Vec<String> = index
        .files
        .keys()
        .filter(|path| path.ends_with(".rs"))
        .cloned()
        .collect();

    // Pass 1: Fixed-point discovery of `pub use` re-exporters (named and glob).
    loop {
        let mut grew = false;
        for rs_file in &rs_files {
            if rust_crate_root_prefix(rs_file) != def_crate_prefix && !candidates.contains(rs_file)
            {
                // Only cross-crate files that explicitly contain `pub use` and `old_name`
                // can re-export this symbol.
                let Ok(snap) = Snapshot::open(workspace, rs_file) else {
                    continue;
                };
                if !snap.text.contains("use ") || !snap.text.contains(old_name) {
                    continue;
                }
            }
            if !load_rust(rs_file, &mut parsed_cache) {
                continue;
            }
            let Some((_, _, uses)) = parsed_cache.get(rs_file).and_then(Option::as_ref) else {
                continue;
            };
            let mut file_reexports_named = false;
            for entry in uses {
                if !entry.is_pub {
                    continue;
                }
                let matches_target = match &entry.leaf {
                    Some(leaf) if leaf == old_name => {
                        resolve_rust_use_prefix(&entry.current_mod, crate_name, &entry.prefix)
                            .is_some_and(|m| available_mods.contains(&m))
                    }
                    None => resolve_rust_use_prefix(&entry.current_mod, crate_name, &entry.prefix)
                        .is_some_and(|m| available_mods.contains(&m)),
                    _ => false,
                };
                if matches_target {
                    if entry.leaf.is_some() {
                        file_reexports_named = true;
                    }
                    if available_mods.insert(entry.current_mod.clone()) {
                        grew = true;
                    }
                    if reexported_mods.insert(entry.current_mod.clone()) {
                        grew = true;
                    }
                }
            }
            if file_reexports_named {
                candidates.insert(rs_file.clone());
                importing_target.insert(rs_file.clone());
            }
        }
        if !grew {
            break;
        }
    }

    if reexported_mods.is_empty() {
        return;
    }

    // Pass 2: Find every Rust file importing or referencing `old_name` through
    // any discovered re-exporting module.
    for rs_file in &rs_files {
        if candidates.contains(rs_file) {
            continue;
        }
        let Ok(snap) = Snapshot::open(workspace, rs_file) else {
            continue;
        };
        if !contains_ident_word(&snap.text, old_name) {
            continue;
        }
        if !load_rust(rs_file, &mut parsed_cache) {
            continue;
        }
        let Some((file_mod, ast, uses)) = parsed_cache.get(rs_file).and_then(Option::as_ref) else {
            continue;
        };
        let mut imports_via_reexport = false;
        let mut uses_reexport_glob = false;
        for entry in uses {
            let Some(resolved) =
                resolve_rust_use_prefix(&entry.current_mod, crate_name, &entry.prefix)
            else {
                continue;
            };
            if !reexported_mods.contains(&resolved) {
                continue;
            }
            match &entry.leaf {
                Some(leaf) if leaf == old_name => imports_via_reexport = true,
                None => uses_reexport_glob = true,
                _ => {}
            }
        }
        if imports_via_reexport
            || uses_reexport_glob
            || rust_file_has_qualified_reexport_path(
                ast,
                file_mod,
                crate_name,
                old_name,
                &reexported_mods,
            )
        {
            candidates.insert(rs_file.clone());
            if imports_via_reexport {
                importing_target.insert(rs_file.clone());
            }
        }
    }
}

fn discover_rename_files(
    workspace: &Workspace,
    index: &Index,
    target_id: usize,
    old_name: &str,
    include_files: Option<&[String]>,
    root_given: bool,
) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let def = &index.definitions[target_id];
    let mut candidates = BTreeSet::new();
    let mut importing_target = BTreeSet::new();
    candidates.insert(def.location.file.clone());

    for edge in index.incoming(target_id) {
        candidates.insert(edge.location.file.clone());
        if edge.kind == RefKind::Import {
            importing_target.insert(edge.location.file.clone());
        }
        if let Some(from_id) = edge.from {
            candidates.insert(index.definitions[from_id].location.file.clone());
        }
    }

    if def.location.file.ends_with(".rs") {
        discover_rust_reexport_files(
            workspace,
            index,
            target_id,
            old_name,
            &mut candidates,
            &mut importing_target,
        );
    } else {
        // Include polyglot files that import the definition's file module (or a
        // transitive barrel re-export) ONLY when they have a namespace/default/wildcard
        // import or mention `old_name` in an import/export statement.
        let competing_files: BTreeSet<&str> = index
            .definitions
            .iter()
            .enumerate()
            .filter(|&(id, d)| {
                id != target_id
                    && d.kind != "file"
                    && symbol_short_and_scope(&d.qualified).0 == old_name
            })
            .map(|(_, d)| d.location.file.as_str())
            .collect();

        let mut ts_parser: Option<tree_sitter::Parser> = None;
        let mut ts_lang: Option<Grammar> = None;
        let mut queue = vec![def.location.file.clone()];
        let mut visited_sources = BTreeSet::new();
        visited_sources.insert(def.location.file.clone());

        while let Some(source_file) = queue.pop() {
            let Some(file_def_id) = index
                .definitions
                .iter()
                .position(|d| d.kind == "file" && d.location.file == source_file)
            else {
                continue;
            };
            for edge in index.incoming(file_def_id) {
                if edge.kind != RefKind::Import
                    || competing_files.contains(edge.location.file.as_str())
                {
                    continue;
                }
                let (relevant, reexports, direct_import) = polyglot_file_import_status(
                    &mut ts_parser,
                    &mut ts_lang,
                    workspace,
                    index,
                    &edge.location.file,
                    &source_file,
                    old_name,
                );
                if relevant {
                    candidates.insert(edge.location.file.clone());
                    if direct_import {
                        importing_target.insert(edge.location.file.clone());
                    }
                }
                if reexports && visited_sources.insert(edge.location.file.clone()) {
                    queue.push(edge.location.file.clone());
                }
            }
        }
    }

    if let Some(filters) = include_files {
        let target_lang = rename_lang_for_file(index, workspace, &def.location.file);
        let mut normalized = Vec::with_capacity(filters.len());
        for raw in filters {
            normalized.push(normalize_rename_filter(workspace, raw, root_given)?);
        }
        let whole_workspace = normalized
            .iter()
            .any(|entry| matches!(entry, RenameFilter::WholeWorkspace));
        if !whole_workspace {
            candidates.retain(|file| {
                normalized.iter().any(|entry| match entry {
                    RenameFilter::WholeWorkspace => true,
                    RenameFilter::Path(prefix) => {
                        file == prefix
                            || file
                                .strip_prefix(prefix.as_str())
                                .is_some_and(|rest| rest.starts_with('/'))
                    }
                })
            });
        }
        for entry in &normalized {
            let RenameFilter::Path(path_str) = entry else {
                continue;
            };
            if workspace
                .source_path(path_str)
                .is_ok_and(|path| path.is_file())
            {
                let entry_lang = rename_lang_for_file(index, workspace, path_str);
                ensure!(
                    entry_lang.is_some() && entry_lang == target_lang,
                    "--file `{path_str}` is not in the same language family as `{}` ({})",
                    def.qualified,
                    def.location.file
                );
                candidates.insert(path_str.clone());
            }
        }
    }

    Ok((candidates, importing_target))
}

fn check_rename_collisions(
    index: &Index,
    target_id: usize,
    scope: Option<&str>,
    new_name: &str,
    candidate_files: &BTreeSet<String>,
    importing_target: &BTreeSet<String>,
) -> Result<()> {
    let def = &index.definitions[target_id];
    let expected_qualified = scope.map_or_else(
        || new_name.to_owned(),
        |prefix| format!("{prefix}::{new_name}"),
    );

    for (other_id, other) in index.definitions.iter().enumerate() {
        if other_id == target_id || other.kind == "file" {
            continue;
        }
        if other.qualified == expected_qualified {
            bail!(
                "name collision: `{new_name}` already exists as `{}` at {}",
                other.qualified,
                other.location
            );
        }
        let (other_short, other_scope) = symbol_short_and_scope(&other.qualified);
        if other_short != new_name {
            continue;
        }
        if other.location.file == def.location.file && other_scope == scope {
            bail!(
                "name collision: `{new_name}` is already defined in {} at {}",
                other.location.file,
                other.location
            );
        }
        if candidate_files.contains(&other.location.file)
            && importing_target.contains(&other.location.file)
        {
            let file_level = if other.location.file.ends_with(".rs") {
                !matches!(other.kind.as_str(), "method" | "field" | "variant")
            } else {
                other_scope == Some(other.location.file.as_str())
            };
            if file_level {
                bail!(
                    "name collision: `{new_name}` is already defined at {} and would conflict with renamed import in {}",
                    other.location,
                    other.location.file
                );
            }
        }
    }
    Ok(())
}

fn make_rename_span(
    snap: &Snapshot,
    bytes: Range<usize>,
    replacement: Option<String>,
) -> RenameSpan {
    let line = snap.line(bytes.start);
    let line_idx = line.saturating_sub(1);
    let line_start = snap.lines.get(line_idx).copied().unwrap_or(0);
    let line_end = snap
        .lines
        .get(line_idx + 1)
        .copied()
        .unwrap_or(snap.text.len());
    let column = snap
        .text
        .get(line_start..bytes.start)
        .map_or(1, |prefix| prefix.chars().count() + 1);
    let raw_line = snap
        .text
        .get(line_start..line_end)
        .unwrap_or_default()
        .trim_end_matches(['\n', '\r']);
    RenameSpan {
        bytes,
        line,
        column,
        snippet: clipped(raw_line, 120),
        replacement,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RustPathRole {
    Value,
    Type,
    Trait,
    Macro,
    Module,
}

struct RustRenameVisitor<'a> {
    text: &'a str,
    offset: usize,
    old_name: &'a str,
    def_kind: &'a str,
    require_raw: bool,
    locals: Vec<String>,
    scope_starts: Vec<usize>,
    locals_floor: usize,
    in_match_pat: bool,
    spans: Vec<Range<usize>>,
}

impl<'a> RustRenameVisitor<'a> {
    fn is_module(&self) -> bool {
        matches!(self.def_kind, "mod" | "module")
    }

    fn is_method(&self) -> bool {
        self.def_kind == "method"
    }

    fn is_field(&self) -> bool {
        self.def_kind == "field"
    }

    fn is_type_like(&self) -> bool {
        matches!(
            self.def_kind,
            "struct" | "enum" | "union" | "trait" | "trait alias" | "type"
        )
    }

    fn clean_ident(ident: &syn::Ident) -> String {
        let s = ident.to_string();
        s.strip_prefix("r#").unwrap_or(&s).to_owned()
    }

    fn push_scope(&mut self) {
        self.scope_starts.push(self.locals.len());
    }

    fn pop_scope(&mut self) {
        if let Some(start) = self.scope_starts.pop() {
            self.locals.truncate(start);
        }
    }

    fn bind_local(&mut self, ident: &syn::Ident) {
        self.locals.push(Self::clean_ident(ident));
    }

    fn is_shadowed_local(&self) -> bool {
        self.locals[self.locals_floor..]
            .iter()
            .any(|name| name == self.old_name)
    }

    fn record_ident(&mut self, ident: &syn::Ident) {
        let ident_text = ident.to_string();
        let clean = ident_text.strip_prefix("r#").unwrap_or(&ident_text);
        if clean != self.old_name {
            return;
        }
        if self.require_raw && !ident_text.starts_with("r#") {
            return;
        }
        let raw = ident.span().byte_range();
        let start = raw.start + self.offset;
        let end = raw.end + self.offset;
        if start > 0 && self.text.as_bytes()[start - 1] == b'\'' {
            return;
        }
        if let Some(slice) = self.text.get(start..end) {
            let matches = if self.require_raw {
                slice.strip_prefix("r#") == Some(self.old_name)
            } else {
                slice == self.old_name || slice.strip_prefix("r#") == Some(self.old_name)
            };
            if matches {
                self.spans.push(start..end);
            }
        }
    }

    fn visit_rust_path<'ast>(
        &mut self,
        qself: Option<&'ast syn::QSelf>,
        path: &'ast syn::Path,
        role: RustPathRole,
    ) {
        if let Some(qs) = qself {
            self.visit_type(&qs.ty);
        }
        for segment in &path.segments {
            self.visit_path_arguments(&segment.arguments);
        }
        let n = path.segments.len();
        if n == 0 {
            return;
        }
        if let Some(qs) = qself
            && qs.position > 0
            && qs.position <= n
        {
            for (idx, seg) in path.segments.iter().take(qs.position).enumerate() {
                if idx + 1 < qs.position {
                    if self.is_module() {
                        self.record_ident(&seg.ident);
                    }
                } else if matches!(self.def_kind, "trait" | "trait alias") {
                    self.record_ident(&seg.ident);
                }
            }
            if qs.position < n
                && matches!(
                    self.def_kind,
                    "method" | "associated type" | "associated constant" | "function" | "constant"
                )
            {
                self.record_ident(&path.segments[n - 1].ident);
            }
            return;
        }

        for (idx, seg) in path.segments.iter().take(n - 1).enumerate() {
            let match_non_terminal = self.is_module()
                || (self.is_type_like()
                    && idx + 1 == n - 1
                    && role != RustPathRole::Module
                    && (!matches!(seg.arguments, syn::PathArguments::None)
                        || (self.old_name.starts_with(char::is_uppercase)
                            && Self::clean_ident(&path.segments[n - 1].ident) != self.old_name)));
            if match_non_terminal {
                self.record_ident(&seg.ident);
            }
        }

        let last = &path.segments[n - 1];
        let match_terminal = match role {
            RustPathRole::Module => self.is_module(),
            RustPathRole::Macro => self.def_kind == "macro",
            RustPathRole::Trait => matches!(self.def_kind, "trait" | "trait alias"),
            RustPathRole::Type => {
                self.is_type_like()
                    || (self.def_kind == "associated type" && (n >= 2 || qself.is_some()))
            }
            RustPathRole::Value => {
                if (n == 1
                    && path.leading_colon.is_none()
                    && qself.is_none()
                    && self.is_shadowed_local())
                    || self.is_field()
                    || self.is_module()
                {
                    false
                } else if self.is_method() || self.def_kind == "associated constant" {
                    n >= 2 || qself.is_some()
                } else {
                    true
                }
            }
        };
        if match_terminal {
            self.record_ident(&last.ident);
        }
    }

    fn walk_use_tree(&mut self, tree: &syn::UseTree, matched_parent_mod: bool) {
        match tree {
            syn::UseTree::Path(part) => {
                let mut matched = matched_parent_mod;
                if self.is_module() && Self::clean_ident(&part.ident) == self.old_name {
                    self.record_ident(&part.ident);
                    matched = true;
                }
                self.walk_use_tree(&part.tree, matched);
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.walk_use_tree(item, matched_parent_mod);
                }
            }
            syn::UseTree::Glob(_) => {}
            syn::UseTree::Name(name) => {
                if Self::clean_ident(&name.ident) != "self"
                    && !self.is_field()
                    && !self.is_method()
                    && (!self.is_module() || !matched_parent_mod)
                {
                    self.record_ident(&name.ident);
                }
            }
            syn::UseTree::Rename(rename) => {
                let orig = Self::clean_ident(&rename.ident);
                let alias = Self::clean_ident(&rename.rename);
                if orig != "self"
                    && !self.is_field()
                    && !self.is_method()
                    && (!self.is_module() || !matched_parent_mod)
                {
                    self.record_ident(&rename.ident);
                }
                if alias == self.old_name && orig != self.old_name {
                    self.bind_local(&rename.rename);
                }
            }
        }
    }

    fn visit_fn_signature(&mut self, sig: &syn::Signature) {
        self.visit_generics(&sig.generics);
        for arg in &sig.inputs {
            match arg {
                syn::FnArg::Receiver(recv) => {
                    self.visit_type(&recv.ty);
                }
                syn::FnArg::Typed(pat_type) => {
                    self.visit_type(&pat_type.ty);
                    let prev_match = std::mem::replace(&mut self.in_match_pat, false);
                    self.visit_pat(&pat_type.pat);
                    self.in_match_pat = prev_match;
                }
            }
        }
        self.visit_return_type(&sig.output);
    }
}

impl<'ast> Visit<'ast> for RustRenameVisitor<'_> {
    fn visit_attribute(&mut self, _node: &'ast syn::Attribute) {}

    fn visit_lifetime(&mut self, _node: &'ast syn::Lifetime) {}

    fn visit_label(&mut self, _node: &'ast syn::Label) {}

    fn visit_vis_restricted(&mut self, node: &'ast syn::VisRestricted) {
        self.visit_rust_path(None, &node.path, RustPathRole::Module);
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        self.visit_visibility(&node.vis);
        self.walk_use_tree(&node.tree, false);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "function" {
            self.record_ident(&node.sig.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.push_scope();
        self.visit_fn_signature(&node.sig);
        self.visit_block(&node.block);
        self.pop_scope();
        self.locals_floor = prev_floor;
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.visit_visibility(&node.vis);
        if self.is_module() {
            self.record_ident(&node.ident);
        }
        if let Some((_, items)) = &node.content {
            let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
            self.push_scope();
            for item in items {
                self.visit_item(item);
            }
            self.pop_scope();
            self.locals_floor = prev_floor;
        }
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "struct" {
            self.record_ident(&node.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.visit_generics(&node.generics);
        self.visit_fields(&node.fields);
        self.locals_floor = prev_floor;
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "enum" {
            self.record_ident(&node.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.visit_generics(&node.generics);
        for variant in &node.variants {
            self.visit_variant(variant);
        }
        self.locals_floor = prev_floor;
    }

    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "union" {
            self.record_ident(&node.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.visit_generics(&node.generics);
        self.visit_fields_named(&node.fields);
        self.locals_floor = prev_floor;
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "trait" {
            self.record_ident(&node.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.visit_generics(&node.generics);
        for bound in &node.supertraits {
            self.visit_type_param_bound(bound);
        }
        for item in &node.items {
            self.visit_trait_item(item);
        }
        self.locals_floor = prev_floor;
    }

    fn visit_item_trait_alias(&mut self, node: &'ast syn::ItemTraitAlias) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "trait alias" {
            self.record_ident(&node.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.visit_generics(&node.generics);
        for bound in &node.bounds {
            self.visit_type_param_bound(bound);
        }
        self.locals_floor = prev_floor;
    }

    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "type" {
            self.record_ident(&node.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.visit_generics(&node.generics);
        self.visit_type(&node.ty);
        self.locals_floor = prev_floor;
    }

    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "constant" {
            self.record_ident(&node.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.visit_generics(&node.generics);
        self.visit_type(&node.ty);
        self.visit_expr(&node.expr);
        self.locals_floor = prev_floor;
    }

    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "static" {
            self.record_ident(&node.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.visit_type(&node.ty);
        self.visit_expr(&node.expr);
        self.locals_floor = prev_floor;
    }

    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        if let Some(ident) = &node.ident
            && self.def_kind == "macro"
        {
            self.record_ident(ident);
        }
        if node.ident.is_none() {
            self.visit_macro(&node.mac);
        }
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.visit_generics(&node.generics);
        if let Some((_, trait_path, _)) = &node.trait_ {
            self.visit_rust_path(None, trait_path, RustPathRole::Trait);
        }
        self.visit_type(&node.self_ty);
        for item in &node.items {
            self.visit_impl_item(item);
        }
        self.locals_floor = prev_floor;
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.visit_visibility(&node.vis);
        if self.is_method() {
            self.record_ident(&node.sig.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.push_scope();
        self.visit_fn_signature(&node.sig);
        self.visit_block(&node.block);
        self.pop_scope();
        self.locals_floor = prev_floor;
    }

    fn visit_impl_item_type(&mut self, node: &'ast syn::ImplItemType) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "associated type" {
            self.record_ident(&node.ident);
        }
        self.visit_generics(&node.generics);
        self.visit_type(&node.ty);
    }

    fn visit_impl_item_const(&mut self, node: &'ast syn::ImplItemConst) {
        self.visit_visibility(&node.vis);
        if self.def_kind == "associated constant" {
            self.record_ident(&node.ident);
        }
        self.visit_generics(&node.generics);
        self.visit_type(&node.ty);
        self.visit_expr(&node.expr);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if self.is_method() {
            self.record_ident(&node.sig.ident);
        }
        let prev_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
        self.push_scope();
        self.visit_fn_signature(&node.sig);
        if let Some(block) = &node.default {
            self.visit_block(block);
        }
        self.pop_scope();
        self.locals_floor = prev_floor;
    }

    fn visit_trait_item_type(&mut self, node: &'ast syn::TraitItemType) {
        if self.def_kind == "associated type" {
            self.record_ident(&node.ident);
        }
        self.visit_generics(&node.generics);
        for bound in &node.bounds {
            self.visit_type_param_bound(bound);
        }
        if let Some((_, ty)) = &node.default {
            self.visit_type(ty);
        }
    }

    fn visit_trait_item_const(&mut self, node: &'ast syn::TraitItemConst) {
        if self.def_kind == "associated constant" {
            self.record_ident(&node.ident);
        }
        self.visit_generics(&node.generics);
        self.visit_type(&node.ty);
        if let Some((_, expr)) = &node.default {
            self.visit_expr(expr);
        }
    }

    fn visit_field(&mut self, node: &'ast syn::Field) {
        self.visit_visibility(&node.vis);
        if self.is_field()
            && let Some(ident) = &node.ident
        {
            self.record_ident(ident);
        }
        self.visit_type(&node.ty);
    }

    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        if self.def_kind == "variant" {
            self.record_ident(&node.ident);
        }
        self.visit_fields(&node.fields);
        if let Some((_, expr)) = &node.discriminant {
            self.visit_expr(expr);
        }
    }

    fn visit_field_value(&mut self, node: &'ast syn::FieldValue) {
        if node.colon_token.is_some() {
            if self.is_field()
                && let syn::Member::Named(ident) = &node.member
            {
                self.record_ident(ident);
            }
            self.visit_expr(&node.expr);
        } else if self.is_field()
            && let syn::Member::Named(ident) = &node.member
        {
            self.record_ident(ident);
        }
    }

    fn visit_field_pat(&mut self, node: &'ast syn::FieldPat) {
        if node.colon_token.is_some() {
            if self.is_field()
                && let syn::Member::Named(ident) = &node.member
            {
                self.record_ident(ident);
            }
            self.visit_pat(&node.pat);
        } else {
            if self.is_field()
                && let syn::Member::Named(ident) = &node.member
            {
                self.record_ident(ident);
            }
            let prev_match = std::mem::replace(&mut self.in_match_pat, false);
            self.visit_pat(&node.pat);
            self.in_match_pat = prev_match;
        }
    }

    fn visit_expr_field(&mut self, node: &'ast syn::ExprField) {
        self.visit_expr(&node.base);
        if self.is_field()
            && let syn::Member::Named(ident) = &node.member
        {
            self.record_ident(ident);
        }
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.visit_expr(&node.receiver);
        if self.is_method() {
            self.record_ident(&node.method);
        }
        if let Some(turbo) = &node.turbofish {
            self.visit_angle_bracketed_generic_arguments(turbo);
        }
        for arg in &node.args {
            self.visit_expr(arg);
        }
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        self.visit_rust_path(node.qself.as_ref(), &node.path, RustPathRole::Value);
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        let role = if self.def_kind == "variant" {
            RustPathRole::Value
        } else {
            RustPathRole::Type
        };
        self.visit_rust_path(node.qself.as_ref(), &node.path, role);
        for field in &node.fields {
            self.visit_field_value(field);
        }
        if let Some(rest) = &node.rest {
            self.visit_expr(rest);
        }
    }

    fn visit_type_path(&mut self, node: &'ast syn::TypePath) {
        self.visit_rust_path(node.qself.as_ref(), &node.path, RustPathRole::Type);
    }

    fn visit_trait_bound(&mut self, node: &'ast syn::TraitBound) {
        self.visit_rust_path(None, &node.path, RustPathRole::Trait);
    }

    fn visit_assoc_type(&mut self, node: &'ast syn::AssocType) {
        if self.def_kind == "associated type" {
            self.record_ident(&node.ident);
        }
        if let Some(generics) = &node.generics {
            self.visit_angle_bracketed_generic_arguments(generics);
        }
        self.visit_type(&node.ty);
    }

    fn visit_assoc_const(&mut self, node: &'ast syn::AssocConst) {
        if self.def_kind == "associated constant" {
            self.record_ident(&node.ident);
        }
        if let Some(generics) = &node.generics {
            self.visit_angle_bracketed_generic_arguments(generics);
        }
        self.visit_expr(&node.value);
    }

    fn visit_pat_struct(&mut self, node: &'ast syn::PatStruct) {
        let role = if self.def_kind == "variant" {
            RustPathRole::Value
        } else {
            RustPathRole::Type
        };
        self.visit_rust_path(node.qself.as_ref(), &node.path, role);
        for field in &node.fields {
            self.visit_field_pat(field);
        }
    }

    fn visit_pat_tuple_struct(&mut self, node: &'ast syn::PatTupleStruct) {
        let role = if self.def_kind == "variant" {
            RustPathRole::Value
        } else {
            RustPathRole::Type
        };
        self.visit_rust_path(node.qself.as_ref(), &node.path, role);
        for elem in &node.elems {
            self.visit_pat(elem);
        }
    }

    fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
        let bare_match_pat = self.in_match_pat
            && node.by_ref.is_none()
            && node.mutability.is_none()
            && node.subpat.is_none();
        if bare_match_pat
            && (self.def_kind == "constant"
                || (self.def_kind == "struct" && self.old_name.starts_with(char::is_uppercase)))
            && Self::clean_ident(&node.ident) == self.old_name
        {
            self.record_ident(&node.ident);
            return;
        }
        self.bind_local(&node.ident);
        if let Some((_, subpat)) = &node.subpat {
            self.visit_pat(subpat);
        }
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let Some(init) = &node.init {
            self.visit_expr(&init.expr);
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
        }
        let prev_match = std::mem::replace(&mut self.in_match_pat, false);
        self.visit_pat(&node.pat);
        self.in_match_pat = prev_match;
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        self.push_scope();
        for stmt in &node.stmts {
            self.visit_stmt(stmt);
        }
        self.pop_scope();
    }

    fn visit_expr_let(&mut self, node: &'ast syn::ExprLet) {
        self.visit_expr(&node.expr);
        let prev_match = std::mem::replace(&mut self.in_match_pat, true);
        self.visit_pat(&node.pat);
        self.in_match_pat = prev_match;
    }

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.push_scope();
        self.visit_expr(&node.cond);
        self.visit_block(&node.then_branch);
        self.pop_scope();
        if let Some((_, else_expr)) = &node.else_branch {
            self.visit_expr(else_expr);
        }
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.push_scope();
        self.visit_expr(&node.cond);
        self.visit_block(&node.body);
        self.pop_scope();
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.visit_expr(&node.expr);
        self.push_scope();
        let prev_match = std::mem::replace(&mut self.in_match_pat, false);
        self.visit_pat(&node.pat);
        self.in_match_pat = prev_match;
        self.visit_block(&node.body);
        self.pop_scope();
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        self.push_scope();
        let prev_match = std::mem::replace(&mut self.in_match_pat, true);
        self.visit_pat(&node.pat);
        self.in_match_pat = prev_match;
        if let Some((_, guard)) = &node.guard {
            self.visit_expr(guard);
        }
        self.visit_expr(&node.body);
        self.pop_scope();
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        self.push_scope();
        let prev_match = std::mem::replace(&mut self.in_match_pat, false);
        for input in &node.inputs {
            self.visit_pat(input);
        }
        self.in_match_pat = prev_match;
        self.visit_return_type(&node.output);
        self.visit_expr(&node.body);
        self.pop_scope();
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.visit_rust_path(None, &mac.path, RustPathRole::Macro);
        let mac_name = mac
            .path
            .segments
            .last()
            .map(|seg| Self::clean_ident(&seg.ident))
            .unwrap_or_default();
        let standard = matches!(
            mac_name.as_str(),
            "assert"
                | "assert_eq"
                | "assert_ne"
                | "debug_assert"
                | "debug_assert_eq"
                | "debug_assert_ne"
                | "format"
                | "format_args"
                | "write"
                | "writeln"
                | "unreachable"
                | "todo"
                | "unimplemented"
                | "print"
                | "println"
                | "eprint"
                | "eprintln"
                | "panic"
                | "dbg"
                | "vec"
                | "ensure"
                | "bail"
                | "anyhow"
        );
        if standard
            && let Ok(expressions) = mac.parse_body_with(
                syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
            )
        {
            for expr in &expressions {
                match expr {
                    syn::Expr::Assign(assign) if matches!(assign.left.as_ref(), syn::Expr::Path(p) if p.path.segments.len() == 1) =>
                    {
                        self.visit_expr(&assign.right);
                    }
                    _ => self.visit_expr(expr),
                }
            }
        }
    }
}

/// Locates every AST-role-verified Rust identifier occurrence of `old_name`
/// using `syn::visit::Visit`, skipping attributes, comments, strings,
/// lifetimes, labels, unrelated fields/methods, non-module path prefixes,
/// bare keywords when renaming raw identifiers, and shadowed locals.
fn find_rust_identifier_spans(
    text: &str,
    old_name: &str,
    def_kind: &str,
) -> Result<Vec<Range<usize>>> {
    let file = crate::rust_syntax::parse_file(text)
        .map_err(|err| anyhow::anyhow!("Rust parse error: {err}"))?;
    let offset = usize::from(text.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
        + file.shebang.as_ref().map_or(0, String::len);
    let mut visitor = RustRenameVisitor {
        text,
        offset,
        old_name,
        def_kind,
        require_raw: is_language_keyword(old_name, true, None),
        locals: Vec::with_capacity(32),
        scope_starts: Vec::with_capacity(16),
        locals_floor: 0,
        in_match_pat: false,
        spans: Vec::new(),
    };
    visitor.visit_file(&file);
    visitor.spans.sort_by_key(|range| (range.start, range.end));
    visitor.spans.dedup();
    Ok(visitor.spans)
}

fn is_treesitter_identifier(grammar: Grammar, node: tree_sitter::Node<'_>) -> bool {
    if !node.is_named() || node.child_count() != 0 || node.is_missing() || node.is_error() {
        return false;
    }
    match node.kind() {
        "identifier"
        | "type_identifier"
        | "property_identifier"
        | "field_identifier"
        | "shorthand_property_identifier"
        | "shorthand_property_identifier_pattern"
        | "namespace_identifier"
        | "package_identifier"
        | "simple_identifier" => true,
        "constant" if grammar == Grammar::Ruby => true,
        "name" if grammar == Grammar::Php => true,
        "alias" if grammar == Grammar::Elixir => true,
        "simple_name" if grammar == Grammar::PowerShell => true,
        _ => false,
    }
}

fn component_template_mentions_ident(raw: &str, masked: &str, ident: &str) -> bool {
    if raw.len() != masked.len() || ident.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(off) = raw[from..].find(ident) {
        let start = from + off;
        let end = start + ident.len();
        from = end;
        if !masked.as_bytes()[start..end].iter().all(|&b| b == b' ') {
            continue;
        }
        let before_ok = raw[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '$'));
        let after_ok = raw[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '$'));
        if !before_ok || !after_ok {
            continue;
        }
        let prefix = &raw[..start];
        if let Some(open) = prefix.rfind("<!--")
            && prefix[open + 4..].rfind("-->").is_none()
        {
            continue;
        }
        return true;
    }
    false
}

fn collect_treesitter_namespace_imports(
    tree: &tree_sitter::Tree,
    text: &str,
    grammar: Grammar,
    file: &str,
    def_file: &str,
) -> BTreeSet<String> {
    let mut namespaces = BTreeSet::new();
    let family = grammar.family();
    let mut cursor = tree.walk();
    'walk: loop {
        let node = cursor.node();
        let kind = node.kind();
        match family {
            Grammar::JavaScript => match kind {
                "import_statement" => {
                    let matches = node.child_by_field_name("source").is_some_and(|src| {
                        import_spec_matches_def_file(file, family, node_text(src, text), def_file)
                    });
                    if matches {
                        for i in 0..node.child_count() {
                            if let Some(clause) = node.child(i as u32)
                                && clause.kind() == "import_clause"
                            {
                                for j in 0..clause.child_count() {
                                    let Some(cc) = clause.child(j as u32) else {
                                        continue;
                                    };
                                    if cc.kind() == "identifier" {
                                        namespaces.insert(node_text(cc, text).to_owned());
                                    } else if cc.kind() == "namespace_import" {
                                        for k in 0..cc.named_child_count() {
                                            if let Some(id) = cc.named_child(k as u32)
                                                && id.kind() == "identifier"
                                            {
                                                namespaces.insert(node_text(id, text).to_owned());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                "variable_declarator" => {
                    if let Some(lhs) = node.child_by_field_name("name")
                        && lhs.kind() == "identifier"
                        && let Some(val) = node.child_by_field_name("value")
                        && val.kind() == "call_expression"
                        && val
                            .child_by_field_name("function")
                            .is_some_and(|f| node_text(f, text) == "require")
                        && let Some(args) = val.child_by_field_name("arguments")
                        && (0..args.named_child_count()).any(|i| {
                            args.named_child(i as u32).is_some_and(|arg| {
                                import_spec_matches_def_file(
                                    file,
                                    family,
                                    node_text(arg, text),
                                    def_file,
                                )
                            })
                        })
                    {
                        namespaces.insert(node_text(lhs, text).to_owned());
                    }
                }
                _ => {}
            },
            Grammar::Python => match kind {
                "import_statement" => {
                    for i in 0..node.named_child_count() {
                        let Some(child) = node.named_child(i as u32) else {
                            continue;
                        };
                        if child.kind() == "aliased_import" {
                            let spec = child
                                .child_by_field_name("name")
                                .map_or("", |n| node_text(n, text));
                            if import_spec_matches_def_file(file, family, spec, def_file)
                                && let Some(alias) = child.child_by_field_name("alias")
                            {
                                namespaces.insert(node_text(alias, text).to_owned());
                            }
                        } else {
                            let spec = node_text(child, text);
                            if import_spec_matches_def_file(file, family, spec, def_file) {
                                namespaces.insert(spec.to_owned());
                            }
                        }
                    }
                }
                "import_from_statement" => {
                    let mod_text = node
                        .child_by_field_name("module_name")
                        .map_or("", |m| node_text(m, text));
                    for i in 0..node.named_child_count() {
                        let Some(child) = node.named_child(i as u32) else {
                            continue;
                        };
                        if node.child_by_field_name("module_name") == Some(child) {
                            continue;
                        }
                        let (imported_name, binding) = if child.kind() == "aliased_import" {
                            (
                                child
                                    .child_by_field_name("name")
                                    .map_or("", |n| node_text(n, text)),
                                child
                                    .child_by_field_name("alias")
                                    .map_or("", |n| node_text(n, text)),
                            )
                        } else {
                            let name = node_text(child, text);
                            (name, name)
                        };
                        let combined = if mod_text.ends_with('.') || mod_text.is_empty() {
                            format!("{mod_text}{imported_name}")
                        } else {
                            format!("{mod_text}.{imported_name}")
                        };
                        if import_spec_matches_def_file(file, family, &combined, def_file)
                            && !binding.is_empty()
                        {
                            namespaces.insert(binding.to_owned());
                        }
                    }
                }
                _ => {}
            },
            Grammar::Go => {
                if kind == "import_spec"
                    && let Some(path_node) = node.child_by_field_name("path")
                {
                    let spec = strip_string_quotes(node_text(path_node, text));
                    if import_spec_matches_def_file(file, family, spec, def_file) {
                        if let Some(alias) = node.child_by_field_name("name") {
                            let name = node_text(alias, text);
                            if !matches!(name, "." | "_" | "") {
                                namespaces.insert(name.to_owned());
                            }
                        } else if let Some(last) = spec.rsplit('/').next()
                            && !last.is_empty()
                        {
                            namespaces.insert(last.to_owned());
                        }
                    }
                }
            }
            _ => {}
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    namespaces
}

fn fn_params_bind_name(
    fn_node: tree_sitter::Node<'_>,
    text: &str,
    family: Grammar,
    old_name: &str,
) -> bool {
    let Some(params) = fn_node
        .child_by_field_name("parameters")
        .or_else(|| fn_node.child_by_field_name("parameter"))
    else {
        return false;
    };
    if params.kind() == "identifier" {
        return node_text(params, text) == old_name;
    }
    for i in 0..params.named_child_count() {
        let Some(param) = params.named_child(i as u32) else {
            continue;
        };
        let pkind = param.kind();
        match family {
            Grammar::Python => match pkind {
                "identifier" if node_text(param, text) == old_name => return true,
                "default_parameter" | "typed_default_parameter" => {
                    if param
                        .child_by_field_name("name")
                        .is_some_and(|n| node_text(n, text) == old_name)
                    {
                        return true;
                    }
                }
                "typed_parameter" | "list_splat_pattern" | "dictionary_splat_pattern"
                    if param.named_child(0).is_some_and(|n| {
                        n.kind() == "identifier" && node_text(n, text) == old_name
                    }) =>
                {
                    return true;
                }
                _ => {}
            },
            Grammar::JavaScript => match pkind {
                "identifier" if node_text(param, text) == old_name => return true,
                "required_parameter" | "optional_parameter"
                    if param.child_by_field_name("pattern").is_some_and(|p| {
                        p.kind() == "identifier" && node_text(p, text) == old_name
                    }) =>
                {
                    return true;
                }
                "assignment_pattern"
                    if param.child_by_field_name("left").is_some_and(|p| {
                        p.kind() == "identifier" && node_text(p, text) == old_name
                    }) =>
                {
                    return true;
                }
                "rest_pattern"
                    if param.named_child(0).is_some_and(|p| {
                        p.kind() == "identifier" && node_text(p, text) == old_name
                    }) =>
                {
                    return true;
                }
                _ => {}
            },
            _ => {}
        }
    }
    false
}

/// Locates every AST-role-verified Tree-sitter identifier leaf equal to
/// `old_name`, preserving comments, string literals, object keys, Python
/// kwargs/parameters, PHP `$vars`, and failing closed on unsafe JS/TS
/// destructuring patterns or Vue/Svelte/Astro template references.
#[allow(clippy::too_many_arguments)]
fn find_treesitter_identifier_spans(
    parser_slot: &mut Option<tree_sitter::Parser>,
    cached_lang: &mut Option<Grammar>,
    grammar: Grammar,
    file: &str,
    def_file: &str,
    def_kind: &str,
    text: &str,
    old_name: &str,
    new_name: &str,
) -> Result<Vec<(Range<usize>, Option<String>)>> {
    let (parse_grammar, masked) = match grammar {
        Grammar::Vue | Grammar::Svelte | Grammar::Astro => {
            let (script_text, jsx) = crate::components::scripts(text, grammar == Grammar::Astro);
            if component_template_mentions_ident(text, &script_text, old_name) {
                bail!(
                    "template in {file} references '{old_name}' outside <script>; update manually to avoid desync"
                );
            }
            let lang = if jsx {
                Grammar::Tsx
            } else {
                Grammar::TypeScript
            };
            (lang, Some(script_text))
        }
        _ => (grammar, None),
    };
    let parser = parser_slot.get_or_insert_with(tree_sitter::Parser::new);
    if *cached_lang != Some(parse_grammar) {
        *cached_lang = None;
        parser
            .set_language(&parse_grammar.language())
            .with_context(|| format!("failed to load {} grammar", parse_grammar.name()))?;
        *cached_lang = Some(parse_grammar);
    }
    let parse_input = masked.as_deref().unwrap_or(text);
    let tree = parser
        .parse(parse_input, None)
        .with_context(|| format!("failed to parse AST in {file}: Tree-sitter parse cancelled"))?;

    let family = parse_grammar.family();
    let is_member_def = matches!(def_kind, "method" | "field" | "property");
    let is_var_def = matches!(def_kind, "variable" | "local");
    let is_module_def = matches!(def_kind, "module" | "mod");
    let namespaces =
        collect_treesitter_namespace_imports(&tree, parse_input, parse_grammar, file, def_file);

    let mut spans: Vec<(Range<usize>, Option<String>)> = Vec::new();
    let mut shadow_ranges: Vec<Range<usize>> = Vec::new();
    let mut cursor = tree.walk();
    'walk: loop {
        let node = cursor.node();
        let kind = node.kind();
        let skip_subtree = kind.contains("comment");

        // Track function bodies where a parameter shadows `old_name`.
        if !skip_subtree
            && !is_var_def
            && matches!(
                kind,
                "function_definition"
                    | "function_declaration"
                    | "function_expression"
                    | "arrow_function"
                    | "method_definition"
            )
            && fn_params_bind_name(node, parse_input, family, old_name)
            && let Some(body) = node.child_by_field_name("body")
        {
            shadow_ranges.push(body.byte_range());
        }

        if !skip_subtree && is_treesitter_identifier(parse_grammar, node) {
            let range = node.byte_range();
            if text.get(range.clone()) == Some(old_name) {
                let parent = node.parent();
                let pkind = parent.map_or("", |p| p.kind());
                let is_shadowed = shadow_ranges
                    .iter()
                    .any(|r| r.start <= range.start && range.end <= r.end);

                // 1. JS/TS shorthand object literals & destructuring patterns.
                if family == Grammar::JavaScript && kind == "shorthand_property_identifier_pattern"
                {
                    let line = syntax_check::line_column(text, range.start).0;
                    bail!(
                        "shorthand destructuring `{{ {old_name} }}` in {file}:{line} would change object property keys; rewrite to explicit `{old_name}: alias` before renaming"
                    );
                }
                if family == Grammar::JavaScript && kind == "shorthand_property_identifier" {
                    if is_member_def {
                        let line = syntax_check::line_column(text, range.start).0;
                        bail!(
                            "shorthand property `{{ {old_name} }}` in {file}:{line} also binds a variable; expand explicitly before renaming"
                        );
                    }
                    if !is_shadowed {
                        spans.push((range, Some(format!("{old_name}: {new_name}"))));
                    }
                } else {
                    let mut include = true;

                    // 2. Property / field identifiers across all languages.
                    if matches!(kind, "property_identifier" | "field_identifier") {
                        include = if is_member_def {
                            true
                        } else if pkind == "member_expression"
                            && parent.and_then(|p| p.child_by_field_name("property")) == Some(node)
                        {
                            parent
                                .and_then(|p| p.child_by_field_name("object"))
                                .is_some_and(|obj| namespaces.contains(node_text(obj, parse_input)))
                        } else if pkind == "selector_expression"
                            && parent.and_then(|p| p.child_by_field_name("field")) == Some(node)
                        {
                            parent
                                .and_then(|p| p.child_by_field_name("operand"))
                                .is_some_and(|obj| namespaces.contains(node_text(obj, parse_input)))
                        } else {
                            false
                        };
                    }

                    // 3. JS/TS AST-role filtering.
                    if include && family == Grammar::JavaScript {
                        let is_import_or_export_alias =
                            matches!(pkind, "import_specifier" | "export_specifier")
                                && parent.and_then(|p| p.child_by_field_name("alias"))
                                    == Some(node);
                        let is_ns_alias = pkind == "namespace_import" && !is_module_def;
                        let is_param_binding = !is_var_def
                            && ((matches!(pkind, "required_parameter" | "optional_parameter")
                                && parent.and_then(|p| p.child_by_field_name("pattern"))
                                    == Some(node))
                                || (pkind == "assignment_pattern"
                                    && parent.and_then(|p| p.child_by_field_name("left"))
                                        == Some(node)
                                    && parent
                                        .and_then(|p| p.parent())
                                        .is_some_and(|gp| gp.kind() == "formal_parameters"))
                                || pkind == "formal_parameters"
                                || (pkind == "arrow_function"
                                    && parent.and_then(|p| p.child_by_field_name("parameter"))
                                        == Some(node))
                                || (pkind == "rest_pattern"
                                    && parent.and_then(|p| p.named_child(0)) == Some(node))
                                || (pkind == "pair_pattern"
                                    && parent.and_then(|p| p.child_by_field_name("value"))
                                        == Some(node)));
                        if is_import_or_export_alias
                            || is_ns_alias
                            || is_param_binding
                            || (is_shadowed && kind == "identifier")
                        {
                            include = false;
                        }
                    }

                    // 4. Python AST-role filtering (kwargs, parameter names, attribute receivers, aliases).
                    if include && family == Grammar::Python {
                        let is_kwarg_name = pkind == "keyword_argument"
                            && parent.and_then(|p| p.child_by_field_name("name")) == Some(node);
                        let is_param_name = !is_var_def
                            && ((matches!(pkind, "default_parameter" | "typed_default_parameter")
                                && parent.and_then(|p| p.child_by_field_name("name"))
                                    == Some(node))
                                || (pkind == "typed_parameter"
                                    && parent.and_then(|p| p.named_child(0)) == Some(node))
                                || matches!(
                                    pkind,
                                    "parameters"
                                        | "lambda_parameters"
                                        | "list_splat_pattern"
                                        | "dictionary_splat_pattern"
                                ));
                        let is_import_alias = pkind == "aliased_import"
                            && parent.and_then(|p| p.child_by_field_name("alias")) == Some(node);
                        let is_import_module_seg = !is_module_def
                            && pkind == "dotted_name"
                            && parent.and_then(|p| p.parent()).is_some_and(|gp| {
                                gp.kind() == "import_from_statement"
                                    && gp.child_by_field_name("module_name") == parent
                            });
                        let is_unrelated_attr = pkind == "attribute"
                            && parent.and_then(|p| p.child_by_field_name("attribute"))
                                == Some(node)
                            && !is_member_def
                            && !parent
                                .and_then(|p| p.child_by_field_name("object"))
                                .is_some_and(|obj| {
                                    namespaces.contains(node_text(obj, parse_input))
                                });
                        if is_kwarg_name
                            || is_param_name
                            || is_import_alias
                            || is_import_module_seg
                            || is_unrelated_attr
                            || is_shadowed
                        {
                            include = false;
                        }
                    }

                    // 5. PHP AST-role filtering (`$var` inside `variable_name` & member accesses).
                    if include && family == Grammar::Php {
                        let is_php_var = pkind == "variable_name" && !is_var_def;
                        let is_unrelated_member =
                            matches!(pkind, "member_access_expression" | "member_call_expression")
                                && parent.and_then(|p| p.child_by_field_name("name")) == Some(node)
                                && !is_member_def;
                        let is_php_alias = pkind == "namespace_aliasing_clause";
                        if is_php_var || is_unrelated_member || is_php_alias {
                            include = false;
                        }
                    }

                    if include {
                        spans.push((range, None));
                    }
                }
            }
        }
        if !skip_subtree && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    spans.sort_by_key(|(range, _)| (range.start, range.end));
    spans.dedup_by(|a, b| a.0 == b.0);
    Ok(spans)
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

    #[test]
    fn rename_rust_multifile_updates_definition_and_caller_preserving_strings_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub mod math;\npub mod caller;\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/math.rs"),
            concat!(
                "/// old_fn doc comment stays untouched\n",
                "pub fn old_fn(x: i32) -> i32 {\n",
                "    // old_fn inline comment\n",
                "    let label = \"old_fn\";\n",
                "    let old_fn_helper = x + 1;\n",
                "    label.len() as i32 + old_fn_helper\n",
                "}\n",
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("src/caller.rs"),
            concat!(
                "use crate::math::old_fn;\n\n",
                "// old_fn should not change in comment\n",
                "pub fn run(n: i32) -> (&'static str, i32) {\n",
                "    let my_old_fn_2 = old_fn(n);\n",
                "    (\"old_fn\", my_old_fn_2)\n",
                "}\n",
            ),
        )
        .unwrap();

        let ws = test_workspace(dir.path());
        let index = Index::open(&ws).unwrap();
        let plan = plan_symbol_rename(&ws, &index, "old_fn", "compute_score", None, true).unwrap();
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.total_replacements(), 3);

        let preview = render_rename_plan(&plan, true);
        assert!(preview.contains("[rename dry-run]"), "{preview}");
        assert!(preview.contains("src/math.rs · rev "), "{preview}");
        assert!(preview.contains("src/caller.rs · rev "), "{preview}");
        assert!(preview.contains("L2:"), "{preview}");
        assert!(preview.contains("L5:"), "{preview}");

        let report = apply_symbol_rename(&ws, &plan).unwrap();
        assert_eq!(report.files.len(), 2);
        assert_eq!(report.total_replacements, 3);

        let math = fs::read_to_string(dir.path().join("src/math.rs")).unwrap();
        let caller = fs::read_to_string(dir.path().join("src/caller.rs")).unwrap();
        assert!(
            math.contains("pub fn compute_score(x: i32) -> i32"),
            "{math}"
        );
        assert!(
            math.contains("/// old_fn doc comment stays untouched"),
            "{math}"
        );
        assert!(math.contains("// old_fn inline comment"), "{math}");
        assert!(math.contains("let label = \"old_fn\";"), "{math}");
        assert!(math.contains("let old_fn_helper = x + 1;"), "{math}");

        assert!(
            caller.contains("use crate::math::compute_score;"),
            "{caller}"
        );
        assert!(
            caller.contains("let my_old_fn_2 = compute_score(n);"),
            "{caller}"
        );
        assert!(
            caller.contains("// old_fn should not change in comment"),
            "{caller}"
        );
        assert!(caller.contains("(\"old_fn\", my_old_fn_2)"), "{caller}");

        assert_eq!(
            syntax_check::judge(&ws.root, "src/math.rs", None, &math, &[], false),
            Some(Verdict::Clean)
        );
        assert_eq!(
            syntax_check::judge(&ws.root, "src/caller.rs", None, &caller, &[], false),
            Some(Verdict::Clean)
        );
    }

    #[test]
    fn rename_polyglot_typescript_and_python_multifile_ast_safe() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("pkg")).unwrap();

        fs::write(
            dir.path().join("src/math.ts"),
            concat!(
                "// calcTotal comment\n",
                "export function calcTotal(a: number, b: number): number {\n",
                "  const tag = \"calcTotal\";\n",
                "  const calcTotal_v2 = a + b;\n",
                "  return tag.length + calcTotal_v2;\n",
                "}\n",
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("src/app.ts"),
            concat!(
                "import { calcTotal } from \"./math\";\n\n",
                "export function run(): string {\n",
                "  /* calcTotal block comment */\n",
                "  const my_calcTotal_2 = calcTotal(2, 3);\n",
                "  return `calcTotal=${my_calcTotal_2}`;\n",
                "}\n",
            ),
        )
        .unwrap();

        fs::write(
            dir.path().join("pkg/util.py"),
            concat!(
                "# fetch_user comment\n",
                "def fetch_user(uid: int) -> str:\n",
                "    \"\"\"fetch_user docstring\"\"\"\n",
                "    label = \"fetch_user\"\n",
                "    fetch_user_cache = str(uid)\n",
                "    return label + fetch_user_cache\n",
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("pkg/service.py"),
            concat!(
                "from pkg.util import fetch_user\n\n",
                "def handle(uid: int) -> str:\n",
                "    # fetch_user note\n",
                "    my_fetch_user_2 = fetch_user(uid)\n",
                "    return f\"fetch_user:{my_fetch_user_2}\"\n",
            ),
        )
        .unwrap();

        let ws = test_workspace(dir.path());
        let index = Index::open(&ws).unwrap();

        let ts_plan =
            plan_symbol_rename(&ws, &index, "calcTotal", "sumValues", None, true).unwrap();
        assert_eq!(ts_plan.files.len(), 2);
        assert_eq!(ts_plan.total_replacements(), 3);
        let ts_report = apply_symbol_rename(&ws, &ts_plan).unwrap();
        assert_eq!(ts_report.total_replacements, 3);

        let ts_math = fs::read_to_string(dir.path().join("src/math.ts")).unwrap();
        let ts_app = fs::read_to_string(dir.path().join("src/app.ts")).unwrap();
        assert!(ts_math.contains("export function sumValues("), "{ts_math}");
        assert!(ts_math.contains("// calcTotal comment"), "{ts_math}");
        assert!(ts_math.contains("const tag = \"calcTotal\";"), "{ts_math}");
        assert!(ts_math.contains("const calcTotal_v2 = a + b;"), "{ts_math}");
        assert!(
            ts_app.contains("import { sumValues } from \"./math\";"),
            "{ts_app}"
        );
        assert!(
            ts_app.contains("const my_calcTotal_2 = sumValues(2, 3);"),
            "{ts_app}"
        );
        assert!(ts_app.contains("/* calcTotal block comment */"), "{ts_app}");
        assert!(ts_app.contains("`calcTotal=${my_calcTotal_2}`"), "{ts_app}");

        let py_plan =
            plan_symbol_rename(&ws, &index, "fetch_user", "load_account", None, true).unwrap();
        assert_eq!(py_plan.files.len(), 2);
        assert_eq!(py_plan.total_replacements(), 3);
        let py_report = apply_symbol_rename(&ws, &py_plan).unwrap();
        assert_eq!(py_report.total_replacements, 3);

        let py_util = fs::read_to_string(dir.path().join("pkg/util.py")).unwrap();
        let py_service = fs::read_to_string(dir.path().join("pkg/service.py")).unwrap();
        assert!(
            py_util.contains("def load_account(uid: int) -> str:"),
            "{py_util}"
        );
        assert!(py_util.contains("# fetch_user comment"), "{py_util}");
        assert!(
            py_util.contains("\"\"\"fetch_user docstring\"\"\""),
            "{py_util}"
        );
        assert!(py_util.contains("label = \"fetch_user\""), "{py_util}");
        assert!(py_util.contains("fetch_user_cache = str(uid)"), "{py_util}");
        assert!(
            py_service.contains("from pkg.util import load_account"),
            "{py_service}"
        );
        assert!(
            py_service.contains("my_fetch_user_2 = load_account(uid)"),
            "{py_service}"
        );
        assert!(py_service.contains("# fetch_user note"), "{py_service}");
        assert!(
            py_service.contains("f\"fetch_user:{my_fetch_user_2}\""),
            "{py_service}"
        );
    }

    #[test]
    fn rename_rejects_ambiguous_invalid_identifier_keyword_and_collision_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let lib_src = "pub mod a;\npub mod b;\n";
        let a_src = "pub fn dup() -> i32 { 1 }\npub fn taken_name() -> i32 { 2 }\n";
        let b_src = "pub fn dup() -> i32 { 3 }\n";
        let py_src = "def py_worker():\n    return 1\n";
        fs::write(dir.path().join("src/lib.rs"), lib_src).unwrap();
        fs::write(dir.path().join("src/a.rs"), a_src).unwrap();
        fs::write(dir.path().join("src/b.rs"), b_src).unwrap();
        fs::write(dir.path().join("worker.py"), py_src).unwrap();

        let ws = test_workspace(dir.path());
        let index = Index::open(&ws).unwrap();

        let ambiguous = plan_symbol_rename(&ws, &index, "dup", "unique_fn", None, true)
            .unwrap_err()
            .to_string();
        assert!(ambiguous.contains("ambiguous"), "{ambiguous}");
        assert!(ambiguous.contains("demo::a::dup"), "{ambiguous}");
        assert!(ambiguous.contains("demo::b::dup"), "{ambiguous}");

        for bad in ["", "123bad", "bad name", "bad-name", "fn", "struct", "_"] {
            let err = plan_symbol_rename(&ws, &index, "a::dup", bad, None, true)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("invalid") || err.contains("empty") || err.contains("keyword"),
                "expected identifier error for `{bad}`, got: {err}"
            );
        }

        let py_kw = plan_symbol_rename(&ws, &index, "py_worker", "def", None, true)
            .unwrap_err()
            .to_string();
        assert!(py_kw.contains("keyword"), "{py_kw}");

        let collision = plan_symbol_rename(&ws, &index, "a::dup", "taken_name", None, true)
            .unwrap_err()
            .to_string();
        assert!(collision.contains("collision"), "{collision}");

        // Every file must remain byte-for-byte untouched.
        assert_eq!(
            fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            lib_src
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/a.rs")).unwrap(),
            a_src
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/b.rs")).unwrap(),
            b_src
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("worker.py")).unwrap(),
            py_src
        );
    }

    #[test]
    fn rename_aborts_atomically_on_stale_concurrent_edit_leaving_all_files_untouched() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub mod math;\npub mod caller;\n",
        )
        .unwrap();
        let math_orig = "pub fn old_fn(x: i32) -> i32 {\n    x + 1\n}\n";
        let caller_orig = "use crate::math::old_fn;\n\npub fn run() -> i32 {\n    old_fn(5)\n}\n";
        fs::write(dir.path().join("src/math.rs"), math_orig).unwrap();
        fs::write(dir.path().join("src/caller.rs"), caller_orig).unwrap();

        let ws = test_workspace(dir.path());
        let index = Index::open(&ws).unwrap();
        let plan = plan_symbol_rename(&ws, &index, "old_fn", "new_fn", None, true).unwrap();
        assert_eq!(plan.files.len(), 2);

        // Simulate a concurrent edit to the second planned file after planning.
        let caller_concurrent = format!("{caller_orig}// concurrent edit\n");
        fs::write(dir.path().join("src/caller.rs"), &caller_concurrent).unwrap();

        let err = apply_symbol_rename(&ws, &plan).unwrap_err().to_string();
        assert!(err.contains("stale revision"), "{err}");

        // Atomic guarantee: NEITHER file was modified by apply_symbol_rename.
        assert_eq!(
            fs::read_to_string(dir.path().join("src/math.rs")).unwrap(),
            math_orig
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/caller.rs")).unwrap(),
            caller_concurrent
        );
    }

    #[test]
    fn rename_rust_ast_roles_preserve_paths_attrs_fields_locals_and_raw_keywords() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            concat!(
                "pub mod foo;\n",
                "pub mod inline;\n",
                "pub mod raw;\n",
                "pub mod caller;\n",
                "pub mod api;\n",
                "pub mod reexport_user;\n",
                "pub use api::foo as _unused_alias;\n",
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("src/foo.rs"),
            concat!(
                "#[inline]\n",
                "pub fn foo(x: i32) -> i32 {\n",
                "    x + 1\n",
                "}\n",
            ),
        )
        .unwrap();
        fs::write(dir.path().join("src/api.rs"), "pub use crate::foo::foo;\n").unwrap();
        fs::write(
            dir.path().join("src/reexport_user.rs"),
            concat!(
                "use crate::api::foo;\n\n",
                "pub fn via_reexport() -> i32 {\n",
                "    foo(9)\n",
                "}\n",
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("src/caller.rs"),
            concat!(
                "use crate::foo::foo;\n\n",
                "pub struct Holder {\n",
                "    pub foo: i32,\n",
                "}\n\n",
                "impl Holder {\n",
                "    pub fn foo(&self) -> i32 { self.foo }\n",
                "}\n\n",
                "pub fn call_all(h: Holder) -> i32 {\n",
                "    let direct = crate::foo::foo(1) + foo(2);\n",
                "    let made = Holder { foo: direct };\n",
                "    let shadowed = {\n",
                "        let foo = 10;\n",
                "        foo + made.foo + h.foo()\n",
                "    };\n",
                "    shadowed\n",
                "}\n\n",
                "pub fn with_param(foo: i32) -> i32 {\n",
                "    match Some(foo) {\n",
                "        Some(foo) => foo,\n",
                "        None => foo,\n",
                "    }\n",
                "}\n",
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("src/inline.rs"),
            concat!(
                "#[inline]\n",
                "pub fn inline() -> i32 {\n",
                "    1\n",
                "}\n",
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("src/raw.rs"),
            concat!(
                "pub type Alias = i32;\n",
                "pub fn r#type() -> Alias {\n",
                "    42\n",
                "}\n",
                "pub fn call_raw() -> Alias {\n",
                "    r#type()\n",
                "}\n",
            ),
        )
        .unwrap();

        let ws = test_workspace(dir.path());
        let index = Index::open(&ws).unwrap();

        // 1. Rename `demo::foo::foo` -> `bar`: preserves `crate::foo::`, struct field, method, and shadowed locals,
        //    AND transitively discovers `src/api.rs` (`pub use`) and `src/reexport_user.rs`!
        let plan = plan_symbol_rename(&ws, &index, "demo::foo::foo", "bar", None, true).unwrap();
        apply_symbol_rename(&ws, &plan).unwrap();

        let caller = fs::read_to_string(dir.path().join("src/caller.rs")).unwrap();
        assert!(caller.contains("use crate::foo::bar;"), "{caller}");
        assert!(
            caller.contains("let direct = crate::foo::bar(1) + bar(2);"),
            "{caller}"
        );
        assert!(caller.contains("pub foo: i32,"), "{caller}");
        assert!(
            caller.contains("pub fn foo(&self) -> i32 { self.foo }"),
            "{caller}"
        );
        assert!(
            caller.contains("let made = Holder { foo: direct };"),
            "{caller}"
        );
        assert!(caller.contains("let foo = 10;"), "{caller}");
        assert!(caller.contains("foo + made.foo + h.foo()"), "{caller}");
        assert!(
            caller.contains("pub fn with_param(foo: i32) -> i32"),
            "{caller}"
        );
        assert!(caller.contains("Some(foo) => foo,"), "{caller}");
        assert!(caller.contains("None => foo,"), "{caller}");

        let api = fs::read_to_string(dir.path().join("src/api.rs")).unwrap();
        let reexport_user = fs::read_to_string(dir.path().join("src/reexport_user.rs")).unwrap();
        assert!(api.contains("pub use crate::foo::bar;"), "{api}");
        assert!(
            reexport_user.contains("use crate::api::bar;"),
            "{reexport_user}"
        );
        assert!(reexport_user.contains("bar(9)"), "{reexport_user}");

        // 2. Rename `demo::inline::inline` -> `fast_path`: attribute `#[inline]` MUST remain intact.
        let index = Index::open(&ws).unwrap();
        let plan_inline =
            plan_symbol_rename(&ws, &index, "demo::inline::inline", "fast_path", None, true)
                .unwrap();
        apply_symbol_rename(&ws, &plan_inline).unwrap();
        let inline_src = fs::read_to_string(dir.path().join("src/inline.rs")).unwrap();
        assert!(
            inline_src.contains("#[inline]\npub fn fast_path()"),
            "{inline_src}"
        );

        // 3. Rename raw identifier `demo::raw::type` (`pub fn r#type`) -> `kind_fn`:
        //    bare `pub type Alias = i32;` MUST remain untouched.
        let index = Index::open(&ws).unwrap();
        let plan_raw =
            plan_symbol_rename(&ws, &index, "demo::raw::type", "kind_fn", None, true).unwrap();
        apply_symbol_rename(&ws, &plan_raw).unwrap();
        let raw_src = fs::read_to_string(dir.path().join("src/raw.rs")).unwrap();
        assert!(raw_src.contains("pub type Alias = i32;"), "{raw_src}");
        assert!(raw_src.contains("pub fn kind_fn() -> Alias"), "{raw_src}");
        assert!(raw_src.contains("kind_fn()"), "{raw_src}");
    }

    #[test]
    fn rename_polyglot_ast_roles_shorthand_kwargs_php_vue_and_syntax_rejection() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("web")).unwrap();
        fs::create_dir_all(dir.path().join("py")).unwrap();
        fs::create_dir_all(dir.path().join("php")).unwrap();
        fs::create_dir_all(dir.path().join("mobile")).unwrap();

        // TypeScript: namespace import + shorthand object literal + unrelated property/key + unrelated importer.
        fs::write(
            dir.path().join("web/math.ts"),
            "export function calcTotal(a: number): number { return a + 1; }\nexport function other(): number { return 0; }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("web/app.ts"),
            concat!(
                "import { calcTotal } from \"./math\";\n",
                "import * as mathNs from \"./math\";\n",
                "const cfg = { calcTotal: 99 };\n",
                "export const api = { calcTotal };\n",
                "export function run(x: number): number {\n",
                "  return calcTotal(x) + mathNs.calcTotal(x) + cfg.calcTotal;\n",
                "}\n",
            ),
        )
        .unwrap();
        let unrelated_ts_orig = concat!(
            "import { other } from \"./math\";\n",
            "export const cfg = { calcTotal: 42 };\n",
            "export function keep(): number { return other() + cfg.calcTotal; }\n",
        );
        fs::write(dir.path().join("web/unrelated.ts"), unrelated_ts_orig).unwrap();

        // Python: keyword arguments + default parameter names + shadowed body.
        fs::write(
            dir.path().join("py/util.py"),
            "def fetch_user(uid: int) -> int:\n    return uid + 1\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("py/service.py"),
            concat!(
                "from py.util import fetch_user\n\n",
                "def sink(**kw):\n",
                "    return kw\n\n",
                "def helper(fetch_user=None):\n",
                "    return fetch_user\n\n",
                "def run(uid: int):\n",
                "    sink(fetch_user=1, cb=fetch_user)\n",
                "    return fetch_user(uid)\n",
            ),
        )
        .unwrap();

        // PHP: `$score` variable must stay untouched when renaming `function score`.
        fs::write(
            dir.path().join("php/math.php"),
            "<?php\nfunction score($x) {\n    $score = $x + 1;\n    return $score;\n}\nfunction run() {\n    $score = score(5);\n    return $score;\n}\n",
        )
        .unwrap();

        // Dart & Swift keyword checks.
        fs::write(
            dir.path().join("mobile/calc.dart"),
            "int dartCalc(int x) {\n  return x + 1;\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("mobile/calc.swift"),
            "func swiftCalc(_ x: Int) -> Int {\n    return x + 1\n}\n",
        )
        .unwrap();

        let ws = test_workspace(dir.path());
        let index = Index::open(&ws).unwrap();

        // 1. TypeScript rename: expands `{ calcTotal }` -> `{ calcTotal: sumValues }`,
        //    renames `mathNs.calcTotal`, keeps `{ calcTotal: 99 }`, `cfg.calcTotal`, and `unrelated.ts`!
        let ts_plan =
            plan_symbol_rename(&ws, &index, "calcTotal", "sumValues", None, true).unwrap();
        assert!(
            !ts_plan.files.iter().any(|f| f.path == "web/unrelated.ts"),
            "unrelated importer should not be planned: {:?}",
            ts_plan.files
        );
        apply_symbol_rename(&ws, &ts_plan).unwrap();
        let app_after = fs::read_to_string(dir.path().join("web/app.ts")).unwrap();
        assert!(
            app_after.contains("const cfg = { calcTotal: 99 };"),
            "{app_after}"
        );
        assert!(
            app_after.contains("export const api = { calcTotal: sumValues };"),
            "{app_after}"
        );
        assert!(
            app_after.contains("return sumValues(x) + mathNs.sumValues(x) + cfg.calcTotal;"),
            "{app_after}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("web/unrelated.ts")).unwrap(),
            unrelated_ts_orig
        );

        // 2. JS/TS shorthand destructuring fails closed with 0 files modified.
        fs::write(
            dir.path().join("web/destruct.ts"),
            "import { sumValues } from \"./math\";\nexport function unpack({ sumValues }: any) { return sumValues; }\n",
        )
        .unwrap();
        let index = Index::open(&ws).unwrap();
        let err = plan_symbol_rename(&ws, &index, "sumValues", "totalValue", None, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("shorthand destructuring"), "{err}");
        fs::remove_file(dir.path().join("web/destruct.ts")).unwrap();

        // 3. Vue template reference outside `<script>` fails closed with exact message.
        fs::write(
            dir.path().join("web/App.vue"),
            "<script setup lang=\"ts\">\nimport { sumValues } from \"./math\";\nconst v = sumValues(1);\n</script>\n<template>\n  <div>{{ sumValues(2) }}</div>\n</template>\n",
        )
        .unwrap();
        let index = Index::open(&ws).unwrap();
        let vue_err = plan_symbol_rename(&ws, &index, "sumValues", "totalValue", None, true)
            .unwrap_err()
            .to_string();
        assert!(
            vue_err.contains("template in web/App.vue references 'sumValues' outside <script>; update manually to avoid desync"),
            "{vue_err}"
        );
        fs::remove_file(dir.path().join("web/App.vue")).unwrap();

        // 4. Python kwargs & default params stay untouched while value/call sites rename.
        let index = Index::open(&ws).unwrap();
        let py_plan =
            plan_symbol_rename(&ws, &index, "fetch_user", "load_account", None, true).unwrap();
        apply_symbol_rename(&ws, &py_plan).unwrap();
        let py_service = fs::read_to_string(dir.path().join("py/service.py")).unwrap();
        assert!(
            py_service.contains("from py.util import load_account"),
            "{py_service}"
        );
        assert!(
            py_service.contains("def helper(fetch_user=None):\n    return fetch_user"),
            "{py_service}"
        );
        assert!(
            py_service.contains("sink(fetch_user=1, cb=load_account)"),
            "{py_service}"
        );
        assert!(
            py_service.contains("return load_account(uid)"),
            "{py_service}"
        );

        // 5. PHP `$score` stays untouched while `function score` and `score(5)` rename.
        let php_plan =
            plan_symbol_rename(&ws, &index, "score", "compute_points", None, true).unwrap();
        apply_symbol_rename(&ws, &php_plan).unwrap();
        let php_after = fs::read_to_string(dir.path().join("php/math.php")).unwrap();
        assert!(
            php_after.contains("function compute_points($x)"),
            "{php_after}"
        );
        assert!(php_after.contains("$score = $x + 1;"), "{php_after}");
        assert!(
            php_after.contains("$score = compute_points(5);"),
            "{php_after}"
        );

        // 6. Dart, Swift, JS/TS `async`, Elixir, Lua, Zig, SQL keywords are rejected.
        assert!(is_language_keyword("_", true, None));
        assert!(is_language_keyword(
            "async",
            false,
            Some(Grammar::TypeScript)
        ));
        assert!(is_language_keyword("final", false, Some(Grammar::Dart)));
        assert!(is_language_keyword("protocol", false, Some(Grammar::Swift)));
        assert!(is_language_keyword(
            "defmodule",
            false,
            Some(Grammar::Elixir)
        ));
        assert!(is_language_keyword("local", false, Some(Grammar::Lua)));
        assert!(is_language_keyword("comptime", false, Some(Grammar::Zig)));
        assert!(is_language_keyword("SELECT", false, Some(Grammar::Sql)));
        let dart_kw = plan_symbol_rename(&ws, &index, "dartCalc", "final", None, true)
            .unwrap_err()
            .to_string();
        assert!(dart_kw.contains("reserved keyword in Dart"), "{dart_kw}");
        let swift_kw = plan_symbol_rename(&ws, &index, "swiftCalc", "protocol", None, true)
            .unwrap_err()
            .to_string();
        assert!(swift_kw.contains("reserved keyword in Swift"), "{swift_kw}");

        // 7. Synthetic syntax-breaking plan on a cleanly parsed Tree-sitter file MUST abort
        //    with 0 files modified and NEVER mention `--allow-parse-errors`.
        let dart_orig = fs::read_to_string(dir.path().join("mobile/calc.dart")).unwrap();
        let snap = Snapshot::open(&ws, "mobile/calc.dart").unwrap();
        let start = snap.text.find("dartCalc").unwrap();
        let broken_plan = RenamePlan {
            qualified: "mobile/calc.dart::dartCalc".into(),
            old_name: "dartCalc".into(),
            new_name: "final".into(),
            kind: "function".into(),
            definition_file: "mobile/calc.dart".into(),
            definition_line: 1,
            files: vec![FileRename {
                path: "mobile/calc.dart".into(),
                revision: snap.revision.clone(),
                replacements: vec![make_rename_span(
                    &snap,
                    start..start + "dartCalc".len(),
                    None,
                )],
            }],
        };
        let syntax_err = apply_symbol_rename(&ws, &broken_plan)
            .unwrap_err()
            .to_string();
        assert!(
            syntax_err.contains("0 files were modified") || syntax_err.contains("0 files modified"),
            "{syntax_err}"
        );
        assert!(
            !syntax_err.contains("--allow-parse-errors"),
            "rename syntax rejection must not mention nonexistent flag: {syntax_err}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("mobile/calc.dart")).unwrap(),
            dart_orig
        );
    }

    #[test]
    fn rename_file_filters_handle_dot_subdir_and_reject_cross_language() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub mod sub;\n").unwrap();
        fs::write(dir.path().join("src/sub/mod.rs"), "pub mod a;\n").unwrap();
        fs::write(
            dir.path().join("src/sub/a.rs"),
            "pub fn helper(x: i32) -> i32 { x + 1 }\n",
        )
        .unwrap();
        let py_orig = "def helper(x):\n    return x\n";
        fs::write(dir.path().join("unrelated.py"), py_orig).unwrap();

        let ws = test_workspace(dir.path());
        let index = Index::open(&ws).unwrap();

        // 1. `--file .` and `--file ./` match whole workspace cleanly.
        for dot in [".", "./"] {
            let filter = vec![dot.to_owned()];
            let plan = plan_symbol_rename(
                &ws,
                &index,
                "demo::sub::a::helper",
                "assist",
                Some(&filter),
                true,
            )
            .unwrap();
            assert_eq!(plan.files.len(), 1);
            assert_eq!(plan.files[0].path, "src/sub/a.rs");
        }

        // 2. Cross-language `--file unrelated.py` is rejected when renaming a Rust symbol.
        let bad = vec!["unrelated.py".to_owned()];
        let err = plan_symbol_rename(
            &ws,
            &index,
            "demo::sub::a::helper",
            "assist",
            Some(&bad),
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not in the same language family"), "{err}");

        // 3. Running from subdirectory `src/sub` without `--root` resolves `--file a.rs` -> `src/sub/a.rs`.
        let sub_ws = Workspace {
            root: ws.root.clone(),
            cwd: ws.root.join("src/sub"),
            state: ws.state.clone(),
            origin: ws.origin.clone(),
        };
        let sub_filter = vec!["a.rs".to_owned()];
        let sub_plan = plan_symbol_rename(
            &sub_ws,
            &index,
            "demo::sub::a::helper",
            "assist",
            Some(&sub_filter),
            false,
        )
        .unwrap();
        assert_eq!(sub_plan.files.len(), 1);
        assert_eq!(sub_plan.files[0].path, "src/sub/a.rs");
        assert_eq!(
            fs::read_to_string(dir.path().join("unrelated.py")).unwrap(),
            py_orig
        );
    }
}
