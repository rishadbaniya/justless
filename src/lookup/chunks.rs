//! Splits a file into the places lookup ranks: one per symbol, Markdown
//! section, configuration block or text window.

use std::borrow::Cow;
use std::path::Path;

use crate::polyglot::{Grammar, Parsers};
use crate::rust_syntax;

/// Field order inside a posting: name, signature, doc comments, body, path.
pub(crate) const NAME: usize = 0;
pub(crate) const SIG: usize = 1;
pub(crate) const DOC: usize = 2;
pub(crate) const BODY: usize = 3;
pub(crate) const PATH: usize = 4;
pub(crate) const FIELDS: usize = 5;

const MAX_NAME: usize = 100;
const MAX_CHUNKS: usize = 2_000;
const FILE_CHUNK_LINES: usize = 80;
const TEXT_WINDOW: usize = 60;
const DOC_BLOCK: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Code,
    Doc,
    Text,
    Data,
    Log,
}

impl Class {
    pub(crate) fn bits(self) -> u8 {
        match self {
            Self::Code => 0,
            Self::Doc => 1,
            Self::Text => 2,
            Self::Data => 3,
            Self::Log => 4,
        }
    }

    pub(crate) fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            0 => Self::Code,
            1 => Self::Doc,
            2 => Self::Text,
            3 => Self::Data,
            4 => Self::Log,
            _ => return None,
        })
    }

    /// The `source` value lookup reports for results of this class.
    pub(crate) fn source(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Doc => "doc",
            Self::Text => "text",
            Self::Data => "data",
            Self::Log => "log",
        }
    }

    /// How strongly results of this class compete: data files and logs rank low.
    pub(crate) fn prior(self) -> f64 {
        match self {
            Self::Code | Self::Doc => 1.0,
            Self::Text => 0.8,
            Self::Data => 0.2,
            Self::Log => 0.5,
        }
    }
}

/// One ranked place. Lines are 1-based and `start..=end` is the displayed range.
pub(crate) struct Chunk<'a> {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) declaration: usize,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) test: bool,
    pub(crate) class: Class,
    /// Text of the name, signature, doc and body fields. The path field is
    /// shared by every chunk of a file and added when the chunk is indexed.
    pub(crate) fields: [Vec<Cow<'a, str>>; 4],
}

impl<'a> Chunk<'a> {
    fn new(start: usize, end: usize, name: &str, kind: &str, class: Class) -> Self {
        Self {
            start,
            end,
            declaration: start,
            name: clip(name, MAX_NAME),
            kind: kind.to_owned(),
            test: false,
            class,
            fields: Default::default(),
        }
    }

    fn add(&mut self, field: usize, text: impl Into<Cow<'a, str>>) {
        self.fields[field].push(text.into());
    }
}

/// Clips `text` to at most `limit` bytes on a character boundary, marking the cut.
pub(crate) fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(limit.saturating_sub('…'.len_utf8()));
    format!("{}…", &text[..end])
}

const DATA_EXTENSIONS: &[&str] = &["csv", "tsv", "jsonl", "ndjson", "lock", "svg", "map", "log"];
const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "Gemfile.lock",
    "go.sum",
    "composer.lock",
];

fn file_name(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file)
}

fn file_stem(file: &str) -> &str {
    let name = file_name(file);
    Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name)
}

fn extension(file: &str) -> String {
    Path::new(file_name(file))
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// How a file is chunked and how its results rank.
pub(crate) fn classify(file: &str, text: &str, lines: usize) -> Class {
    let name = file_name(file);
    let extension = extension(file);
    let lower = name.to_ascii_lowercase();
    if DATA_EXTENSIONS.contains(&extension.as_str())
        || lower.ends_with(".min.js")
        || lower.ends_with("-lock.json")
        || LOCKFILES.contains(&name)
        || text.len() > 300 * lines.max(1)
        || (extension == "json" && lines > 400)
    {
        Class::Data
    } else if matches!(extension.as_str(), "md" | "markdown" | "mdx") {
        Class::Doc
    } else if extension == "rs" || Grammar::for_file(file).is_some() {
        Class::Code
    } else {
        Class::Text
    }
}

/// Whether a path names test code.
pub(crate) fn test_path(file: &str) -> bool {
    let mut parts = file.split('/').collect::<Vec<_>>();
    let name = parts.pop().unwrap_or_default();
    parts
        .iter()
        .any(|part| matches!(*part, "test" | "tests" | "__tests__" | "spec" | "e2e"))
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || name.starts_with("test_")
        || file.ends_with("_tests.rs")
}

/// A comment line: `//`, `#` (not `#[`), `/*`, `*`, `--`, `;` or a triple quote.
pub(crate) fn is_comment(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("//")
        || (line.starts_with('#') && !line[1..].starts_with('['))
        || line.starts_with("/*")
        || line.starts_with('*')
        || line.starts_with("--")
        || line.starts_with(';')
        || line.starts_with("\"\"\"")
        || line.starts_with("'''")
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// The chunks of one file, in line order. `parsers` is reused across files.
pub(crate) fn chunk<'a>(file: &str, text: &'a str, parsers: &mut Parsers) -> Vec<Chunk<'a>> {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return Vec::new();
    }
    let chunks = match classify(file, text, lines.len()) {
        Class::Data => return data(file, &lines),
        Class::Doc => markdown(file, &lines),
        // Code without definitions is split like text but keeps its class.
        Class::Code => match code(file, text, &lines, parsers) {
            Some(Some(chunks)) => chunks,
            Some(None) => return data(file, &lines),
            None => plain(file, &lines, Class::Code),
        },
        Class::Text | Class::Log => plain(file, &lines, Class::Text),
    };
    if chunks.len() > MAX_CHUNKS {
        data(file, &lines)
    } else {
        chunks
    }
}

/// The chunks of a file split like plain text, for a file whose parser failed.
pub(crate) fn plain_text<'a>(file: &str, text: &'a str) -> Vec<Chunk<'a>> {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return Vec::new();
    }
    let class = match classify(file, text, lines.len()) {
        Class::Log => Class::Text,
        class => class,
    };
    plain(file, &lines, class)
}

fn data<'a>(file: &str, lines: &[&'a str]) -> Vec<Chunk<'a>> {
    let name = file_name(file);
    let mut chunk = Chunk::new(1, lines.len(), name, "data", Class::Data);
    chunk.add(NAME, name.to_owned());
    let first = lines[0];
    chunk.add(SIG, &first[..first.floor_char_boundary(400)]);
    vec![chunk]
}

struct Definition {
    start: usize,
    end: usize,
    declaration: usize,
    /// The simple name followed by the qualified path segments.
    terms: String,
    display: String,
    kind: String,
    test: bool,
}

fn definitions(file: &str, text: &str, lines: usize, parsers: &mut Parsers) -> Vec<Definition> {
    let mut definitions = Vec::new();
    if extension(file) == "rs" {
        let parsed = rust_syntax::parse(text);
        if let Ok(syntax) = &parsed {
            for symbol in &syntax.symbols {
                definitions.push(Definition {
                    start: symbol.start,
                    end: symbol.end,
                    declaration: symbol.declaration,
                    terms: format!("{} {}", symbol.name, symbol.path.join(" ")),
                    display: symbol.path.join("::"),
                    kind: symbol.kind.clone(),
                    test: symbol.test,
                });
            }
        }
        drop(parsed);
        // proc-macro2 keeps every parsed file's text in a thread-local source map.
        // Nothing parsed above outlives this point (the syntax facts hold plain line
        // numbers and a parse error has been dropped), so clearing it is safe.
        proc_macro2::extra::invalidate_current_thread_spans();
    } else if let Some(grammar) = Grammar::for_file(file)
        && let Ok(facts) = parsers.parse(grammar, text)
    {
        for definition in facts.definitions {
            definitions.push(Definition {
                start: definition.start,
                end: definition.end,
                declaration: definition.declaration,
                terms: format!("{} {}", definition.name, definition.path),
                display: definition.path,
                kind: definition.kind,
                test: definition.test,
            });
        }
    }
    definitions.retain(|definition| definition.kind != "file");
    for definition in &mut definitions {
        definition.end = definition.end.clamp(1, lines);
        definition.start = definition.start.clamp(1, definition.end);
        definition.declaration = definition
            .declaration
            .clamp(definition.start, definition.end);
    }
    definitions.sort_by_key(|definition| (definition.start, std::cmp::Reverse(definition.end)));
    definitions
}

/// Line numbers in `start..=end` outside every range in `nested`, which must be
/// sorted by start.
fn own_lines(start: usize, end: usize, nested: &[(usize, usize)]) -> Vec<usize> {
    let mut own = Vec::new();
    let mut line = start;
    for &(from, to) in nested {
        while line < from && line <= end {
            own.push(line);
            line += 1;
        }
        line = line.max(to + 1);
    }
    while line <= end {
        own.push(line);
        line += 1;
    }
    own
}

/// Ranges in `ranges` (sorted by start, then end descending) strictly inside
/// the range at `index`.
fn nested(ranges: &[(usize, usize)], index: usize) -> Vec<(usize, usize)> {
    let (start, end) = ranges[index];
    let mut inside = ranges
        .iter()
        .enumerate()
        .filter(|&(other, &(from, to))| {
            other != index && start <= from && to <= end && (from, to) != (start, end)
        })
        .map(|(_, &range)| range)
        .collect::<Vec<_>>();
    inside.sort_unstable();
    inside
}

fn starts_docstring(line: &str) -> bool {
    let line = line.trim_start();
    let line = line
        .strip_prefix(['r', 'u', 'R', 'U', 'b', 'f'])
        .unwrap_or(line);
    line.starts_with("\"\"\"") || line.starts_with("'''")
}

/// Symbol chunks, `None` when the file has no definitions, or `Some(None)`
/// when it has too many to be worth splitting.
fn code<'a>(
    file: &str,
    text: &str,
    lines: &[&'a str],
    parsers: &mut Parsers,
) -> Option<Option<Vec<Chunk<'a>>>> {
    let definitions = definitions(file, text, lines.len(), parsers);
    if definitions.is_empty() {
        return None;
    }
    if definitions.len() > MAX_CHUNKS {
        return Some(None);
    }
    let line = |number: usize| lines[number - 1];
    let test_file = test_path(file);
    // A comment or decorator block directly above a definition is part of it,
    // but never reaches back into an earlier definition that already ended.
    let mut ranges = Vec::with_capacity(definitions.len());
    let mut open = Vec::<usize>::new();
    let mut closed = 0;
    for (index, definition) in definitions.iter().enumerate() {
        while let Some(&top) = open.last() {
            if definitions[top].end < definition.start {
                closed = closed.max(definitions[top].end);
                open.pop();
            } else {
                break;
            }
        }
        let mut first = definition.start;
        while first > 1
            && first - 1 > closed
            && definition.start - first < DOC_BLOCK
            && (is_comment(line(first - 1)) || line(first - 1).trim_start().starts_with('@'))
        {
            first -= 1;
        }
        ranges.push((first, definition.end));
        open.push(index);
    }
    let mut chunks = Vec::new();
    for (index, definition) in definitions.iter().enumerate() {
        let (first, end) = ranges[index];
        let own = own_lines(first, end, &nested(&ranges, index));
        let mut chunk = Chunk::new(
            first,
            end,
            &definition.display,
            &definition.kind,
            Class::Code,
        );
        chunk.declaration = definition.declaration;
        chunk.test = definition.test || test_file;
        chunk.add(NAME, definition.terms.clone());
        let mut used = vec![false; own.len()];
        let position = |number: usize| own.binary_search(&number).ok();
        // Doc: the block above and comments between the start and the name.
        for (slot, &number) in own.iter().enumerate() {
            if number < definition.start
                || (number < definition.declaration && is_comment(line(number)))
            {
                chunk.add(DOC, line(number));
                used[slot] = true;
            }
        }
        // Signature: from the name to the line that opens the body.
        let mut last_signature = definition.declaration;
        for number in definition.declaration..=end.min(definition.declaration + 3) {
            let Some(slot) = position(number) else {
                continue;
            };
            chunk.add(SIG, line(number));
            used[slot] = true;
            last_signature = number;
            if line(number).trim_end().ends_with(['{', ':', ';', ')']) {
                break;
            }
        }
        // A leading docstring belongs with the doc comments.
        if let Some(slot) = own.iter().position(|&number| number > last_signature)
            && starts_docstring(line(own[slot]))
        {
            let opening = line(own[slot]).trim();
            let quote = if opening.contains("'''") {
                "'''"
            } else {
                "\"\"\""
            };
            let closes_here = opening
                .find(quote)
                .is_some_and(|at| opening[at + 3..].contains(quote));
            for (offset, &number) in own[slot..].iter().take(DOC_BLOCK).enumerate() {
                chunk.add(DOC, line(number));
                used[slot + offset] = true;
                if closes_here || (offset > 0 && line(number).contains(quote)) {
                    break;
                }
            }
        }
        for (slot, &number) in own.iter().enumerate() {
            if !used[slot] {
                chunk.add(BODY, line(number));
            }
        }
        chunks.push(chunk);
    }
    // Lines outside every definition, such as imports and module comments.
    let mut covered = vec![false; lines.len() + 1];
    for &(first, end) in &ranges {
        covered[first..=end].fill(true);
    }
    let mut runs = Vec::<Vec<usize>>::new();
    let uncovered = covered
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, covered)| !**covered)
        .map(|(number, _)| number);
    for number in uncovered {
        match runs.last_mut() {
            Some(run) if number - run[run.len() - 1] <= 3 && number - run[0] < FILE_CHUNK_LINES => {
                run.push(number)
            }
            _ => runs.push(vec![number]),
        }
    }
    let stem = file_stem(file);
    for run in &runs {
        let first = run.iter().position(|&number| !is_blank(line(number)));
        let last = run.iter().rposition(|&number| !is_blank(line(number)));
        let (Some(first), Some(last)) = (first, last) else {
            continue;
        };
        let run = &run[first..=last];
        if run
            .iter()
            .filter(|&&number| !is_blank(line(number)))
            .count()
            < 2
        {
            continue;
        }
        let mut chunk = Chunk::new(run[0], run[run.len() - 1], stem, "file", Class::Code);
        chunk.test = test_file;
        chunk.declaration = run[0];
        chunk.add(NAME, stem.to_owned());
        for &number in run {
            let field = if is_comment(line(number)) { DOC } else { BODY };
            chunk.add(field, line(number));
        }
        chunks.push(chunk);
    }
    chunks.sort_by_key(|chunk| (chunk.start, std::cmp::Reverse(chunk.end)));
    Some(Some(chunks))
}

/// Markdown headings as `source` finds them: (line, level, title).
fn headings(lines: &[&str]) -> Vec<(usize, usize, String)> {
    let mut headings = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for (index, line) in lines.iter().enumerate() {
        let raw = line.trim_end();
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
                        if d == delimiter && width >= w && content[width..].trim().is_empty() =>
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
        headings.push((index + 1, level, content[level..].trim().to_owned()));
    }
    headings
}

fn markdown<'a>(file: &str, lines: &[&'a str]) -> Vec<Chunk<'a>> {
    let headings = headings(lines);
    let name = file_name(file);
    let stem = file_stem(file);
    let mut chunks = Vec::new();
    let preamble = headings
        .first()
        .map_or(lines.len(), |(line, _, _)| line - 1);
    if lines[..preamble]
        .iter()
        .filter(|line| !is_blank(line))
        .count()
        >= 2
    {
        let mut chunk = Chunk::new(1, preamble, name, "section", Class::Doc);
        chunk.add(NAME, name.to_owned());
        chunk.add(DOC, stem.to_owned());
        for line in &lines[..preamble] {
            chunk.add(BODY, *line);
        }
        chunks.push(chunk);
    }
    let mut ancestors = Vec::<(usize, &str)>::new();
    for (index, (start, level, title)) in headings.iter().enumerate() {
        let end = headings
            .get(index + 1)
            .map_or(lines.len(), |(next, _, _)| next - 1);
        while ancestors.last().is_some_and(|(depth, _)| depth >= level) {
            ancestors.pop();
        }
        let display = if title.is_empty() { name } else { title };
        let mut chunk = Chunk::new(*start, end, display, "section", Class::Doc);
        chunk.add(NAME, title.clone());
        for (_, ancestor) in &ancestors {
            chunk.add(DOC, ancestor.to_string());
        }
        chunk.add(DOC, stem.to_owned());
        for line in &lines[*start..end] {
            chunk.add(BODY, *line);
        }
        chunks.push(chunk);
        ancestors.push((*level, title));
    }
    chunks
}

/// The name of a `[section]` header line, such as TOML or INI tables.
fn section(line: &str) -> Option<&str> {
    let inner = line.trim().strip_prefix('[')?.strip_suffix(']')?;
    (!inner.is_empty() && !inner.contains(']')).then(|| inner.trim())
}

/// The key of a `key = value` or `"key": value` line.
fn key(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line.strip_prefix('"').unwrap_or(line);
    let length = line
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        .count();
    if length == 0 {
        return None;
    }
    let rest = &line[length..];
    let rest = rest.strip_prefix('"').unwrap_or(rest).trim_start();
    rest.starts_with([':', '=']).then(|| &line[..length])
}

/// Configuration and other text: split at `[section]` headers, then into windows.
fn plain<'a>(file: &str, lines: &[&'a str], class: Class) -> Vec<Chunk<'a>> {
    let name = file_name(file);
    let mut segments = Vec::<(usize, usize, &str)>::new();
    let mut current = (1, name);
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        if let Some(title) = section(line) {
            if number > current.0 {
                segments.push((current.0, number - 1, current.1));
            }
            current = (number, title);
        }
    }
    segments.push((current.0, lines.len(), current.1));
    let mut chunks = Vec::new();
    for (start, end, title) in segments {
        let mut first = start;
        while first <= end {
            let last = (first + TEXT_WINDOW - 1).min(end);
            let window = &lines[first - 1..last];
            if window.iter().any(|line| !is_blank(line)) {
                let mut chunk = Chunk::new(first, last, title, "text", class);
                chunk.declaration = first
                    + window
                        .iter()
                        .position(|line| !is_blank(line))
                        .unwrap_or_default();
                chunk.add(NAME, format!("{title} {name}"));
                for line in window {
                    if let Some(key) = key(line) {
                        chunk.add(SIG, key);
                    }
                    chunk.add(BODY, *line);
                }
                chunks.push(chunk);
            }
            first = last + 1;
        }
    }
    chunks
}

/// Line numbers of a chunk whose text was indexed, given the ranges of the
/// other chunks in its file. Used to pick snippet lines without parsing again.
pub(crate) fn indexed_lines(
    class: Class,
    range: (usize, usize),
    others: impl Iterator<Item = (usize, usize)>,
) -> Vec<usize> {
    if class == Class::Data {
        return vec![range.0];
    }
    let mut inside = others
        .filter(|&(from, to)| range.0 <= from && to <= range.1 && (from, to) != range)
        .collect::<Vec<_>>();
    inside.sort_unstable();
    own_lines(range.0, range.1, &inside)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(chunks: &[Chunk]) -> Vec<String> {
        chunks
            .iter()
            .map(|chunk| {
                format!(
                    "{}-{} {} {}{}",
                    chunk.start,
                    chunk.end,
                    chunk.kind,
                    chunk.name,
                    if chunk.test { " test" } else { "" }
                )
            })
            .collect()
    }

    fn field(chunk: &Chunk, field: usize) -> Vec<String> {
        chunk.fields[field]
            .iter()
            .map(|text| text.trim().to_owned())
            .collect()
    }

    #[test]
    fn rust_children_are_excluded_from_their_parent() {
        let source = "\
//! Retry policy.
use std::time::Duration;

/// How long to wait.
#[derive(Debug)]
pub struct Policy {
    attempts: u32,
}

/// Shared behaviour.
pub trait Backoff {
    /// Delay before an attempt.
    fn delay(&self, attempt: u32) -> Duration;

    /// Whether to give up.
    fn give_up(&self) -> bool {
        false
    }
}
";
        let chunks = chunk("src/policy.rs", source, &mut Parsers::default());
        assert_eq!(
            summary(&chunks),
            [
                "1-2 file policy",
                "4-8 struct Policy",
                "10-19 trait Backoff",
                "12-13 method Backoff::delay",
                "15-18 method Backoff::give_up"
            ]
        );
        let policy = &chunks[1];
        assert_eq!(field(policy, DOC), ["/// How long to wait."]);
        assert_eq!(field(policy, SIG), ["pub struct Policy {"]);
        assert_eq!(
            field(policy, BODY),
            ["#[derive(Debug)]", "attempts: u32,", "}"]
        );
        let backoff = &chunks[2];
        assert_eq!(field(backoff, DOC), ["/// Shared behaviour."]);
        assert_eq!(field(backoff, SIG), ["pub trait Backoff {"]);
        assert_eq!(field(backoff, BODY), ["", "}"]);
        assert_eq!(field(&chunks[3], DOC), ["/// Delay before an attempt."]);
        assert_eq!(field(&chunks[0], DOC), ["//! Retry policy."]);
        assert_eq!(
            indexed_lines(
                Class::Code,
                (10, 19),
                [(12, 13), (15, 18), (4, 8)].into_iter()
            ),
            [10, 11, 14, 19]
        );
    }

    #[test]
    fn python_decorators_and_docstrings() {
        let source = "\
import time


@retry(times=3)
def download(url, tries=3):
    \"\"\"Download a file,
    retrying on failure.\"\"\"
    return fetch(url)


def test_download():
    assert download('x')
";
        let chunks = chunk("scripts/fetch.py", source, &mut Parsers::default());
        assert_eq!(
            summary(&chunks),
            ["4-8 function download", "11-12 function test_download test"]
        );
        let download = &chunks[0];
        assert_eq!(download.declaration, 5);
        assert_eq!(
            field(download, DOC),
            [
                "@retry(times=3)",
                "\"\"\"Download a file,",
                "retrying on failure.\"\"\""
            ]
        );
        assert_eq!(field(download, SIG), ["def download(url, tries=3):"]);
        assert_eq!(field(download, BODY), ["return fetch(url)"]);
    }

    #[test]
    fn markdown_sections_follow_source_headings() {
        let source = "\
Intro line one.
Intro line two.
# Operations
```sh
# not a heading
```
## Deploy
Ship it.
### Rollback
Undo it.
## Retries
Downloads are retried.
";
        let chunks = chunk("docs/operations.md", source, &mut Parsers::default());
        assert_eq!(
            summary(&chunks),
            [
                "1-2 section operations.md",
                "3-6 section Operations",
                "7-8 section Deploy",
                "9-10 section Rollback",
                "11-12 section Retries"
            ]
        );
        assert_eq!(
            field(&chunks[3], DOC),
            ["Operations", "Deploy", "operations"]
        );
        assert_eq!(field(&chunks[4], DOC), ["Operations", "operations"]);
        assert_eq!(field(&chunks[1], BODY), ["```sh", "# not a heading", "```"]);
    }

    #[test]
    fn configuration_sections_and_keys() {
        let source = "\
name = \"app\"

[server]
port = 8080

[retry]
max_attempts = 5
\"base_delay_ms\": 250
";
        let chunks = chunk("config/app.toml", source, &mut Parsers::default());
        assert_eq!(
            summary(&chunks),
            ["1-2 text app.toml", "3-5 text server", "6-8 text retry"]
        );
        assert_eq!(field(&chunks[2], SIG), ["max_attempts", "base_delay_ms"]);
        assert_eq!(field(&chunks[0], SIG), ["name"]);
        let long = (1..=130)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let chunks = chunk("notes.txt", &long, &mut Parsers::default());
        assert_eq!(
            summary(&chunks),
            [
                "1-60 text notes.txt",
                "61-120 text notes.txt",
                "121-130 text notes.txt"
            ]
        );
    }

    #[test]
    fn data_files_index_only_their_first_line() {
        let lock = "{\n  \"retry\": 1\n}\n";
        let chunks = chunk("package-lock.json", lock, &mut Parsers::default());
        assert_eq!(summary(&chunks), ["1-3 data package-lock.json"]);
        assert!(chunks[0].fields[BODY].is_empty());
        assert_eq!(field(&chunks[0], SIG), ["{"]);
        let wide = format!("{}\n", "x".repeat(301));
        assert_eq!(classify("notes.txt", &wide, 1), Class::Data);
        assert_eq!(classify("notes.txt", &"x".repeat(300), 1), Class::Text);
        assert_eq!(classify("data/events.csv", "a,b\n", 1), Class::Data);
        assert_eq!(classify("web/app.min.js", "a\n", 1), Class::Data);
        assert_eq!(classify("go.sum", "a\n", 1), Class::Data);
        assert_eq!(classify("docs/guide.mdx", "a\n", 1), Class::Doc);
        assert_eq!(classify("web/client.ts", "a\n", 1), Class::Code);
        let many = (0..2_001)
            .map(|n| format!("[s{n}]\nk = 1\n"))
            .collect::<String>();
        let chunks = chunk("big.ini", &many, &mut Parsers::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].class, Class::Data);
    }

    #[test]
    fn test_paths_and_comments() {
        for path in [
            "tests/cli.rs",
            "src/__tests__/a.ts",
            "a/spec/b.rb",
            "web/a.test.ts",
            "web/a.spec.js",
            "pkg/a_test.go",
            "test_fetch.py",
            "py/test_fetch.py",
            "src/lookup_tests.rs",
        ] {
            assert!(test_path(path), "{path}");
        }
        for path in ["src/contest.rs", "latest/a.rs", "src/testing.rs"] {
            assert!(!test_path(path), "{path}");
        }
        for line in [
            "// a", "  # a", "#", "/* a", " * a", "-- a", "; a", "\"\"\"a", "'''",
        ] {
            assert!(is_comment(line), "{line}");
        }
        for line in ["#[derive(Debug)]", "let a = 1;", "@decorator", ""] {
            assert!(!is_comment(line), "{line}");
        }
        assert_eq!(clip(&"é".repeat(60), 100).len(), 99);
        assert!(clip(&"é".repeat(60), 100).ends_with('…'));
    }

    #[test]
    fn unparsable_code_falls_back_to_text() {
        let chunks = chunk(
            "src/broken.rs",
            "fn broken( {\n  let x\n",
            &mut Parsers::default(),
        );
        assert_eq!(summary(&chunks), ["1-2 text broken.rs"]);
        assert_eq!(chunks[0].class, Class::Code);
        let generated = (0..2_001)
            .map(|n| format!("fn f{n}() {{}}\n"))
            .collect::<String>();
        let chunks = chunk("src/generated.rs", &generated, &mut Parsers::default());
        assert_eq!(summary(&chunks), ["1-2001 data generated.rs"]);
    }
}
