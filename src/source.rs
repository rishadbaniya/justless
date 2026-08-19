use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use clap::{Arg, ArgMatches, Command as Cli};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::polyglot::{Grammar, Parsers};
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

pub fn commands() -> Vec<Cli<'static>> {
    vec![
        Cli::new("read")
            .about("Read a small file or page through a large file's outline")
            .arg(file())
            .arg(Arg::new("outline").long("outline"))
            .arg(
                Arg::new("json")
                    .long("json")
                    .help("Return a structured outline"),
            )
            .arg(Arg::new("match").long("match").value_name("TEXT"))
            .arg(
                Arg::new("after")
                    .long("after")
                    .value_name("REF")
                    .help("Continue after this revision-bound entry"),
            )
            .arg(crate::number("limit", "40", "Maximum entries per page"))
            .arg(crate::budget()),
        Cli::new("expand")
            .about("Print the exact source bytes for a symbol, outline reference or line range")
            .arg(file())
            .arg(Arg::new("selector").required(true))
            .arg(
                Arg::new("json")
                    .long("json")
                    .help("Include the edit reference, source lines and revision"),
            )
            .arg(revision()),
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
            ),
    ]
}

struct Entry {
    name: String,
    kind: String,
    bytes: Range<usize>,
    /// Where the outline signature starts, after Rust attributes and docs.
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
}

fn revision_for(file: &str, bytes: &[u8]) -> String {
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

    fn line(&self, offset: usize) -> usize {
        self.lines.partition_point(|&start| start <= offset)
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
        if file.ends_with(".rs") {
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
        } else if let Some(grammar) = Grammar::for_file(file) {
            let facts = Parsers::default().parse(grammar, &self.text)?;
            imports = facts.import_ranges;
            for def in facts.definitions {
                self.entries.push(Entry {
                    name: def.path,
                    kind: def.kind,
                    signature_start: def.bytes.start,
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
        let markdown = matches!(
            std::path::Path::new(file)
                .extension()
                .and_then(|x| x.to_str()),
            Some("md" | "markdown")
        );
        if !markdown {
            return;
        }
        let mut open = Vec::<(usize, usize)>::new();
        let mut fence: Option<(char, usize)> = None;
        for (line, &start) in self.lines.iter().enumerate() {
            let end = self.lines.get(line + 1).copied().unwrap_or(self.text.len());
            let raw = self.text[start..end].trim_end();
            let content = raw.trim_start_matches(' ');
            if raw.len() - content.len() > 3 {
                continue;
            }
            if let Some(delimiter @ ('`' | '~')) = content.chars().next() {
                let width = content.chars().take_while(|&c| c == delimiter).count();
                if width >= 3 {
                    match fence {
                        None => fence = Some((delimiter, width)),
                        Some((d, w))
                            if d == delimiter
                                && width >= w
                                && content[width..].trim().is_empty() =>
                        {
                            fence = None
                        }
                        _ => {}
                    }
                    continue;
                }
            }
            if fence.is_some() {
                continue;
            }
            let level = content.bytes().take_while(|&c| c == b'#').count();
            if !(1..=6).contains(&level) || !content[level..].starts_with([' ', '\t']) {
                continue;
            }
            let mut title = content[level..].trim();
            // A closing run of #s after a space is not part of the heading text.
            let unclosed = title.trim_end_matches('#');
            if unclosed.is_empty() || unclosed.ends_with([' ', '\t']) {
                title = unclosed.trim_end();
            }
            while open.last().is_some_and(|(depth, _)| *depth >= level) {
                let (_, id) = open.pop().unwrap();
                self.entries[id].bytes.end = start;
            }
            open.push((level, self.entries.len()));
            self.entries.push(Entry {
                name: title.into(),
                kind: "heading".into(),
                signature_start: start,
                bytes: start..self.text.len(),
                header_end: end,
            });
        }
    }

    fn reference(&self, index: usize) -> String {
        format!("{}#{}", self.revision, index + 1)
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
        if selector.contains('#')
            && let Ok(id) = target.parse::<usize>()
        {
            return self
                .entries
                .get(id.checked_sub(1).context("entry IDs start at 1")?)
                .map(|entry| entry.bytes.clone())
                .context("unknown outline entry");
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
            parse_error: None,
            entries: Vec::new(),
            next: None,
        };
        if let Some(error) = &self.parse_error {
            // Leave room for the cursor line and one short row.
            let room = budget.saturating_sub(page.display().len() + 42 + 32 + 1);
            page.parse_error = (room >= 32).then(|| clipped(error, room.min(240)));
        }
        let mut size = page.display().len() + 42;
        let mut last = None;
        let filter = filter.to_lowercase();
        let words = |raw: &str| raw.split_whitespace().collect::<Vec<_>>().join(" ");
        for (index, e) in self.entries.iter().enumerate().skip(start) {
            let signature = words(&self.text[e.signature_start..e.header_end]);
            // Matching also covers the docs and attributes the row leaves out.
            if !filter.is_empty()
                && !e.name.to_lowercase().contains(&filter)
                && !words(&self.text[e.bytes.start..e.header_end])
                    .to_lowercase()
                    .contains(&filter)
            {
                continue;
            }
            let mut row = Row {
                id: index + 1,
                name: clipped(&e.name, 100),
                kind: e.kind.clone(),
                first: self.line(e.bytes.start),
                last: self.line(e.bytes.end.saturating_sub(1)),
                signature: clipped(&signature, 160),
            };
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
            last = Some(index);
        }
        Ok(page)
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
    let end = tokens[first..]
        .iter()
        .find_map(|token| match token {
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                Some(group.span_open().byte_range().start)
            }
            _ => None,
        })
        .unwrap_or(fragment.len());
    (start, end.max(start))
}

/// A selector's embedded revision, if any, and its target.
fn split_selector(selector: &str) -> (Option<&str>, &str) {
    selector
        .split_once('#')
        .filter(|(r, t)| {
            (r.is_empty() && t.parse::<usize>().is_ok())
                || (r.len() == 24 && r.bytes().all(|b| b.is_ascii_hexdigit()))
        })
        .map_or((None, selector), |(r, t)| ((!r.is_empty()).then_some(r), t))
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

/// Line ranges resolve without the outline, so they skip the parser.
fn needs_outline(selector: &str) -> bool {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_error: Option<String>,
    entries: Vec<Row>,
    next: Option<String>,
}
#[derive(Serialize)]
struct Row {
    id: usize,
    kind: String,
    name: String,
    first: usize,
    last: usize,
    signature: String,
}
impl Row {
    fn display(&self) -> String {
        format!(
            "#{} L{}-{} {} {}  {}\n",
            self.id, self.first, self.last, self.kind, self.name, self.signature
        )
    }
}
impl Page {
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
            out.push_str(&format!("next --after {next}\n"));
        }
        out
    }
}

pub fn run(ws: &Workspace, name: &str, args: &ArgMatches) -> Result<()> {
    let file = args.value_of("file").unwrap();
    let mut source = Snapshot::open(ws, file)?;
    if name == "read" {
        let budget = crate::context_budget(ws, args)?;
        let outline = ["outline", "json", "match", "after", "limit"]
            .iter()
            .any(|id| args.occurrences_of(id) > 0);
        if !outline && source.text.len() <= budget {
            return Ok(io::stdout().lock().write_all(source.text.as_bytes())?);
        }
        source.index(file)?;
        let page = source.page(
            args.value_of("match").unwrap_or(""),
            args.value_of("after"),
            crate::read_num(args, "limit", 1)?,
            budget,
        )?;
        if args.is_present("json") {
            serde_json::to_writer(io::stdout().lock(), &page)?;
        } else {
            print!("{}", page.display());
        }
    } else {
        if name == "edit" {
            return edit(ws, file, source, args);
        }
        let selector = args.value_of("selector").unwrap();
        if needs_outline(selector) {
            source.index(file)?;
        }
        let span = source.resolve(selector, args.value_of("revision"), false)?;
        let first = source.line(span.start);
        let last = source.line(span.end.saturating_sub(1));
        let content = &source.text[span.clone()];
        if args.is_present("json") {
            let anchor = source
                .entries
                .iter()
                .position(|entry| entry.bytes == span)
                .map(|index| source.reference(index))
                .unwrap_or_else(|| format!("{}#L{first}-{last}", source.revision));
            serde_json::to_writer(
                io::stdout().lock(),
                &serde_json::json!({
                    "file": file, "revision": source.revision, "selector": anchor, "first": first, "last": last, "content": content,
                }),
            )?;
        } else {
            io::stdout().lock().write_all(content.as_bytes())?;
        }
        crate::history::saw_source(
            ws,
            file,
            first,
            last,
            crate::capture::source_digest(source.text.as_bytes()),
        );
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
    ws.prepare_state()?;
    let _lock = lock_state(&ws.state.join("edit.lock"))?;
    let path = ws.source_path(file)?;
    let mut temp = NamedTempFile::new_in(path.parent().context("expected source directory")?)?;
    temp.write_all(edited.as_bytes())?;
    temp.as_file()
        .set_permissions(fs::metadata(&path)?.permissions())?;
    ensure!(
        fs::read(ws.source_path(file)?)? == source.text.as_bytes(),
        "stale revision; file changed during edit"
    );
    if args.is_present("dry-run") {
        let result = Command::new("git")
            .args(["diff", "--no-index", "--no-ext-diff", "--no-textconv", "--"])
            .arg(&path)
            .arg(temp.path())
            .output()?;
        ensure!(
            matches!(result.status.code(), Some(0 | 1)),
            "git diff failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        io::stdout().lock().write_all(&result.stdout)?;
    } else {
        if edited != source.text {
            temp.persist(path)?;
        }
        println!(
            "{} edits · revision {}",
            resolved.len(),
            revision_for(file, edited.as_bytes())
        );
    }
    Ok(())
}
