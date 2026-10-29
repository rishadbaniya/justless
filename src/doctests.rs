//! Which Rust documentation rustdoc runs as doctests. Every Markdown code
//! block in an item's docs is a Rust example unless its info string names
//! another language: fenced with backticks or tildes, or indented. Docs come
//! from `///`, `//!`, `/** */`, `/*! */` and `#[doc = "..."]`, joined per item
//! and unindented the way rustdoc joins them. Documentation whose text cannot
//! be read here, such as `#[doc = concat!(..)]`, a `cfg_attr` doc or doc
//! comments inside a macro invocation, is reported as unknown so that callers
//! widen instead of skipping it.

use std::collections::BTreeSet;

use proc_macro2::{TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Expr, Lit, Meta};

/// A code block in documentation, as inclusive source lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Example {
    pub start: usize,
    pub end: usize,
    /// Marked `ignore`: listed by rustdoc, run only with `--ignored`.
    pub ignored: bool,
}

/// One item's documentation, or a file's or module's own.
#[derive(Debug, Default)]
pub(crate) struct Group {
    /// `//!` and `#![doc]`: the documentation of the enclosing file or module.
    pub inner: bool,
    /// Lines its doc comments and attributes cover.
    lines: BTreeSet<usize>,
    /// Code blocks rustdoc treats as Rust.
    pub examples: Vec<Example>,
    /// Paths `#[doc = include_str!("...")]` includes, as written.
    pub includes: Vec<String>,
    /// Documentation whose text cannot be read, such as `concat!`.
    pub unknown: bool,
}

impl Group {
    pub(crate) fn first(&self) -> usize {
        self.lines.first().copied().unwrap_or(0)
    }

    /// Whether `line` holds part of this documentation.
    pub(crate) fn covers(&self, line: usize) -> bool {
        self.lines.contains(&line)
    }

    /// Whether rustdoc runs an example here, `--ignored` ones included or not.
    pub(crate) fn runs(&self, ignored: bool) -> bool {
        self.examples
            .iter()
            .any(|example| ignored || !example.ignored)
    }

    /// Whether `line` lies in a code block rustdoc runs.
    pub(crate) fn in_example(&self, line: usize, ignored: bool) -> bool {
        self.examples.iter().any(|example| {
            (ignored || !example.ignored) && example.start <= line && line <= example.end
        })
    }
}

/// The documentation of one Rust file.
#[derive(Debug, Default)]
pub(crate) struct Docs {
    pub groups: Vec<Group>,
    /// Macro invocations, whose doc comments the parser does not see.
    macros: Vec<(usize, usize)>,
    /// Whether the file parsed. Without that, every doc line is unknown.
    parsed: bool,
}

impl Docs {
    pub(crate) fn read(text: &str) -> Docs {
        let Ok(file) = crate::rust_syntax::parse_file(text) else {
            return Docs::default();
        };
        let mut visitor = Attributes::default();
        visitor.visit_file(&file);
        let lines: Vec<&str> = text.lines().collect();
        let mut docs = Docs {
            groups: Vec::new(),
            macros: visitor.macros,
            parsed: true,
        };
        let mut attributes = visitor.attributes;
        attributes.sort_by_key(|attribute| (attribute.start, attribute.columns.0));
        let text_at = |line: usize| lines.get(line.wrapping_sub(1)).copied().unwrap_or("");
        for attribute in &mut attributes {
            let from: String = text_at(attribute.start)
                .chars()
                .skip(attribute.columns.0)
                .collect();
            attribute.sugared = from.starts_with("//") || from.starts_with("/*");
        }
        // Only whitespace before its first line, and whitespace or a comment
        // after its last: otherwise an item shares the line.
        let alone = |attribute: &Attribute| {
            let before: String = text_at(attribute.start)
                .chars()
                .take(attribute.columns.0)
                .collect();
            let after: String = text_at(attribute.end)
                .chars()
                .skip(attribute.columns.1)
                .collect();
            let after = after.trim();
            before.trim().is_empty() && (after.is_empty() || after.starts_with("//"))
        };
        // Lines any attribute covers, which may sit between one item's docs.
        let covered: BTreeSet<usize> = attributes
            .iter()
            .flat_map(|attribute| attribute.start..=attribute.end)
            .collect();
        let mut current: Vec<&Attribute> = Vec::new();
        for attribute in attributes.iter().filter(|a| a.doc.is_some()) {
            let joins = current.last().is_some_and(|last| {
                last.inner == attribute.inner
                    && alone(last)
                    && alone(attribute)
                    && (last.end + 1..attribute.start).all(|line| {
                        covered.contains(&line)
                            || lines.get(line - 1).is_some_and(|text| trivia(text))
                    })
            });
            if !joins && !current.is_empty() {
                docs.groups.push(group(&std::mem::take(&mut current)));
            }
            current.push(attribute);
        }
        if !current.is_empty() {
            docs.groups.push(group(&current));
        }
        docs
    }

    /// The documentation `line` belongs to.
    pub(crate) fn at(&self, line: usize) -> Option<&Group> {
        self.groups.iter().find(|group| group.covers(line))
    }

    /// Whether `line`, reading `text`, looks like documentation that cannot be
    /// read: a doc comment inside a macro invocation, or any in a file that
    /// does not parse.
    pub(crate) fn opaque(&self, line: usize, text: &str) -> bool {
        doc_like(text)
            && self.at(line).is_none()
            && (!self.parsed
                || self
                    .macros
                    .iter()
                    .any(|&(start, end)| start <= line && line <= end))
    }

    /// The outer documentation of an item spanning `start` to its declaration.
    pub(crate) fn outer(&self, start: usize, declaration: usize) -> impl Iterator<Item = &Group> {
        self.groups.iter().filter(move |group| {
            !group.inner && start <= group.first() && group.first() < declaration
        })
    }
}

/// Whether a line looks like documentation from its text alone.
pub(crate) fn doc_like(line: &str) -> bool {
    let line = line.trim_start();
    (line.starts_with("///") && !line.starts_with("////"))
        || line.starts_with("//!")
        || (line.starts_with("/**") && !line.starts_with("/***") && !line.starts_with("/**/"))
        || line.starts_with("/*!")
        || line.starts_with("#[doc")
        || line.starts_with("#![doc")
        || (line.contains("cfg_attr") && line.contains("doc"))
}

/// Whether a file may hold documentation lines that do not look like
/// documentation on their own: inside a `/** */` block or a `#[doc]` string
/// that spans lines. Without that, [`doc_like`] finds every doc line, and the
/// file need not be parsed for a line that is not one.
pub(crate) fn spans_lines(text: &str) -> bool {
    text.contains("/**") || text.contains("/*!") || text.contains("doc =") || text.contains("doc=")
}

/// Code blocks in a Markdown file that documentation includes, with 1-based lines.
pub(crate) fn markdown_examples(text: &str) -> Vec<Example> {
    let lines: Vec<(usize, String)> = text
        .lines()
        .enumerate()
        .map(|(at, line)| (at + 1, line.to_owned()))
        .collect();
    code_blocks(&lines)
}

/// Lines between one item's doc attributes that keep them together.
fn trivia(line: &str) -> bool {
    let line = line.trim();
    line.is_empty()
        || line.starts_with("//")
        || line.starts_with("/*")
        || line.starts_with('*')
        || line.ends_with("*/")
}

#[derive(Debug)]
enum Text {
    /// A string: a doc comment's text, or `#[doc = "..."]`.
    Literal(String),
    /// `#[doc = include_str!("...")]`.
    Include(String),
    /// Anything else that may add documentation.
    Unknown,
}

#[derive(Debug)]
struct Attribute {
    start: usize,
    end: usize,
    /// Character columns where it starts and ends.
    columns: (usize, usize),
    inner: bool,
    doc: Option<Text>,
    /// A doc comment rather than a written `#[doc]` attribute.
    sugared: bool,
}

#[derive(Default)]
struct Attributes {
    attributes: Vec<Attribute>,
    macros: Vec<(usize, usize)>,
}

impl Attributes {
    fn record(&mut self, attribute: &syn::Attribute) {
        let span = attribute.span();
        let (start, end) = (span.start(), span.end());
        let inner = matches!(attribute.style, syn::AttrStyle::Inner(_));
        let doc = match &attribute.meta {
            Meta::NameValue(pair) if pair.path.is_ident("doc") => Some(match &pair.value {
                Expr::Lit(literal) => match &literal.lit {
                    Lit::Str(text) => Text::Literal(text.value()),
                    _ => Text::Unknown,
                },
                Expr::Macro(call) if call.mac.path.is_ident("include_str") => {
                    match call.mac.parse_body::<syn::LitStr>() {
                        Ok(path) => Text::Include(path.value()),
                        Err(_) => Text::Unknown,
                    }
                }
                _ => Text::Unknown,
            }),
            Meta::List(list) if list.path.is_ident("cfg_attr") && names_doc(&list.tokens) => {
                Some(Text::Unknown)
            }
            _ => None,
        };
        self.attributes.push(Attribute {
            start: start.line,
            end: end.line,
            columns: (start.column, end.column),
            inner,
            doc,
            sugared: false,
        });
    }
}

impl<'ast> Visit<'ast> for Attributes {
    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        self.record(attribute);
        visit::visit_attribute(self, attribute);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let span = mac.span();
        self.macros.push((span.start().line, span.end().line));
        visit::visit_macro(self, mac);
    }
}

/// Whether a `cfg_attr` sets documentation text with `doc = ...` anywhere in
/// its arguments. `doc(cfg(...))` and other `doc(...)` lists add none.
fn names_doc(tokens: &TokenStream) -> bool {
    let tokens: Vec<TokenTree> = tokens.clone().into_iter().collect();
    tokens.iter().enumerate().any(|(at, token)| match token {
        TokenTree::Ident(ident) => {
            ident == "doc"
                && matches!(tokens.get(at + 1), Some(TokenTree::Punct(punct)) if punct.as_char() == '=')
        }
        TokenTree::Group(group) => names_doc(&group.stream()),
        _ => false,
    })
}

/// The Markdown of one item's attributes and the code blocks in it.
fn group(attributes: &[&Attribute]) -> Group {
    let mut group = Group {
        inner: attributes[0].inner,
        ..Group::default()
    };
    let mut fragments = Vec::new();
    for attribute in attributes {
        group.lines.extend(attribute.start..=attribute.end);
        match attribute.doc.as_ref().expect("doc attribute") {
            Text::Literal(text) if attribute.sugared => {
                let (lines, skipped) = beautify(text, attribute.start != attribute.end);
                fragments.push(Fragment {
                    sugared: true,
                    lines: numbered(lines, attribute.start + skipped, attribute.end),
                });
            }
            Text::Literal(text) => fragments.push(Fragment {
                sugared: false,
                lines: numbered(
                    text.split('\n').map(str::to_owned).collect(),
                    attribute.start,
                    attribute.end,
                ),
            }),
            Text::Include(path) => group.includes.push(path.clone()),
            Text::Unknown => group.unknown = true,
        }
    }
    let lines = unindent(fragments);
    group.examples = code_blocks(&lines);
    group
}

struct Fragment {
    sugared: bool,
    lines: Vec<(usize, String)>,
}

/// Lines numbered from `start`, never past `end`.
fn numbered(lines: Vec<String>, start: usize, end: usize) -> Vec<(usize, String)> {
    lines
        .into_iter()
        .enumerate()
        .map(|(at, line)| ((start + at).min(end.max(start)), line))
        .collect()
}

/// rustdoc's cleanup of a block doc comment: a first line of stars goes, and
/// a ` * ` margin shared by every line is removed. Returns the lines and how
/// many leading lines were dropped.
fn beautify(text: &str, block: bool) -> (Vec<String>, usize) {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if !block || lines.len() < 2 {
        return (lines.into_iter().map(str::to_owned).collect(), 0);
    }
    let mut skipped = 0;
    if lines
        .first()
        .is_some_and(|line| line.chars().all(|c| c == '*'))
    {
        lines.remove(0);
        skipped = 1;
    }
    if lines
        .last()
        .is_some_and(|line| !line.is_empty() && line.chars().all(|c| c == '*'))
    {
        lines.pop();
    }
    // The margin: whitespace then one `*`, at the same column on every
    // non-blank line between the first and last non-blank lines.
    let content: Vec<&str> = lines
        .iter()
        .copied()
        .skip_while(|line| line.trim().is_empty())
        .collect();
    let content: Vec<&str> = {
        let end = content
            .iter()
            .rposition(|line| !line.trim().is_empty())
            .map_or(0, |at| at + 1);
        content[..end].to_vec()
    };
    let mut column = None;
    let mut margin = !content.is_empty();
    for line in &content {
        match line.find(|c: char| !matches!(c, ' ' | '\t')) {
            Some(at) if line[at..].starts_with('*') => {
                if *column.get_or_insert(at) != at {
                    margin = false;
                }
            }
            _ => margin = false,
        }
    }
    let lines = lines
        .into_iter()
        .map(|line| match column {
            Some(at) if margin && line.len() > at && line[..at].trim().is_empty() => {
                let rest = &line[at..];
                rest.strip_prefix('*').unwrap_or(rest).to_owned()
            }
            _ => line.to_owned(),
        })
        .collect();
    (lines, skipped)
}

/// rustdoc's unindent: the least indentation across every fragment's
/// non-blank lines is removed, counting a raw `#[doc]` line one wider when
/// it sits beside doc comments.
fn unindent(fragments: Vec<Fragment>) -> Vec<(usize, String)> {
    let mixed = fragments
        .windows(2)
        .any(|pair| pair[0].sugared != pair[1].sugared)
        && fragments.iter().any(|fragment| fragment.sugared);
    let add = usize::from(mixed);
    let width = |line: &str| line.chars().take_while(|c| matches!(c, ' ' | '\t')).count();
    let least = fragments
        .iter()
        .flat_map(|fragment| {
            fragment
                .lines
                .iter()
                .filter(|(_, line)| !line.trim().is_empty())
                .map(move |(_, line)| width(line) + if fragment.sugared { 0 } else { add })
        })
        .min()
        .unwrap_or(0);
    let mut joined = Vec::new();
    for fragment in fragments {
        let indent = if !fragment.sugared && least > 0 {
            least - add
        } else {
            least
        };
        for (number, line) in fragment.lines {
            let line = if line.trim().is_empty() {
                line
            } else {
                line[indent.min(width(&line))..].to_owned()
            };
            joined.push((number, line));
        }
    }
    joined
}

/// Columns of leading whitespace, with tabs to the next multiple of four.
fn indentation(line: &str) -> (usize, &str) {
    let mut columns = 0;
    for (at, c) in line.char_indices() {
        match c {
            ' ' => columns += 1,
            '\t' => columns += 4 - columns % 4,
            _ => return (columns, &line[at..]),
        }
    }
    (columns, "")
}

/// A list item's marker and the column its content starts at, if `rest` opens one.
fn list_item(indent: usize, rest: &str) -> Option<usize> {
    let marker = if rest.starts_with(['-', '+', '*']) {
        1
    } else {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if !(1..=9).contains(&digits) || !rest[digits..].starts_with(['.', ')']) {
            return None;
        }
        digits + 1
    };
    let after = &rest[marker..];
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    let spaces = after.chars().take_while(|c| *c == ' ').count();
    let gap = if (1..=4).contains(&spaces) && spaces < after.len() {
        spaces
    } else {
        1
    };
    Some(indent + marker + gap)
}

/// An opening fence: its character, length and info string.
fn fence(rest: &str) -> Option<(char, usize, &str)> {
    let c = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let length = rest.chars().take_while(|&d| d == c).count();
    (length >= 3).then(|| (c, length, rest[length..].trim()))
}

/// The code blocks in Markdown lines, each numbered with its source line,
/// as CommonMark finds them. Where the structure is unclear, a block is
/// assumed: an extra example costs a run, a missed one a wrong verdict.
fn code_blocks(lines: &[(usize, String)]) -> Vec<Example> {
    struct Open {
        c: char,
        length: usize,
        start: usize,
        rust: Rust,
    }
    let mut found = Vec::new();
    let mut open: Option<Open> = None;
    // An indented block: its first and last non-blank lines.
    let mut indented: Option<(usize, usize)> = None;
    let mut paragraph = false;
    let mut list: Option<usize> = None;
    let close_indented = |indented: &mut Option<(usize, usize)>, found: &mut Vec<Example>| {
        if let Some((start, end)) = indented.take() {
            found.push(Example {
                start,
                end,
                ignored: false,
            });
        }
    };
    for (number, raw) in lines {
        // Block quotes hold code blocks too; their markers are not indentation.
        let mut line = raw.as_str();
        loop {
            let (indent, rest) = indentation(line);
            match rest.strip_prefix('>') {
                Some(inner) if indent < 4 => {
                    line = inner.strip_prefix(' ').unwrap_or(inner);
                }
                _ => break,
            }
        }
        let (indent, rest) = indentation(line);
        if let Some(fenced) = &open {
            let closing = rest.chars().take_while(|&d| d == fenced.c).count();
            if closing >= fenced.length && rest[closing..].trim().is_empty() {
                let rust = fenced.rust;
                let start = fenced.start;
                open = None;
                if rust != Rust::No {
                    found.push(Example {
                        start,
                        end: *number,
                        ignored: rust == Rust::Ignored,
                    });
                }
                paragraph = false;
            }
            continue;
        }
        if rest.is_empty() {
            paragraph = false;
            continue;
        }
        // Content of an open list item is measured from its content column.
        let relative = match list {
            Some(column) if indent >= column => indent - column,
            Some(_) if paragraph => indent,
            Some(_) => {
                list = None;
                indent
            }
            None => indent,
        };
        if relative >= 4 && !paragraph {
            match &mut indented {
                Some((_, end)) => *end = *number,
                None => indented = Some((*number, *number)),
            }
            continue;
        }
        if relative >= 4 && paragraph {
            // A lazy continuation of the paragraph above.
            continue;
        }
        close_indented(&mut indented, &mut found);
        if let Some((c, length, info)) = fence(rest) {
            open = Some(Open {
                c,
                length,
                start: *number,
                rust: rust(c, info),
            });
            paragraph = false;
        } else if let Some(column) = list_item(indent, rest) {
            list = Some(column);
            let content = rest.get(column - indent..).unwrap_or("").trim_start();
            if let Some((c, length, info)) = fence(content) {
                open = Some(Open {
                    c,
                    length,
                    start: *number,
                    rust: rust(c, info),
                });
                paragraph = false;
            } else {
                paragraph = !content.is_empty();
            }
        } else {
            // A heading or a rule is a block of one line; text continues.
            paragraph = !(heading(rest) || thematic_break(rest));
        }
    }
    close_indented(&mut indented, &mut found);
    // An unclosed fence runs to the end of the documentation.
    if let Some(fenced) = open
        && fenced.rust != Rust::No
    {
        found.push(Example {
            start: fenced.start,
            end: lines.last().map_or(fenced.start, |(number, _)| *number),
            ignored: fenced.rust == Rust::Ignored,
        });
    }
    found.sort_by_key(|example| (example.start, example.end));
    found
}

fn heading(rest: &str) -> bool {
    let hashes = rest.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes)
        && rest[hashes..]
            .chars()
            .next()
            .is_none_or(|c| c == ' ' || c == '\t')
}

fn thematic_break(rest: &str) -> bool {
    let marks: Vec<char> = rest.chars().filter(|c| !c.is_whitespace()).collect();
    marks.len() >= 3 && matches!(marks[0], '-' | '*' | '_') && marks.iter().all(|&c| c == marks[0])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rust {
    Yes,
    /// Rust marked `ignore`, which runs only with `--ignored`.
    Ignored,
    No,
}

/// Whether rustdoc treats a fenced block with this info string as Rust, as
/// its `LangString::parse` decides: tokens split on commas, spaces and
/// tabs, `{...}` attributes aside. An unknown word makes the block another
/// language unless a Rust-only word, such as `rust` or `should_panic` ahead
/// of it, says otherwise.
fn rust(c: char, info: &str) -> Rust {
    if c == '`' && info.contains('`') {
        // Not a fence to CommonMark, which leaves rustdoc's view unclear.
        return Rust::Yes;
    }
    let mut words = Vec::new();
    let mut rest = info;
    while let Some(at) = rest.find('{') {
        words.extend(rest[..at].split([',', ' ', '\t']));
        match rest[at..].find('}') {
            Some(close) => rest = &rest[at + close + 1..],
            // An unclosed attribute block: rustdoc reports it; assume Rust.
            None => return Rust::Yes,
        }
    }
    words.extend(rest.split([',', ' ', '\t']));
    let (mut rust, mut other, mut custom) = (false, false, false);
    let (mut ignore, mut ignore_some) = (false, false);
    for word in words.into_iter().filter(|word| !word.is_empty()) {
        match word {
            "rust" => rust = true,
            "should_panic" | "no_run" => rust = !other,
            "ignore" => {
                ignore = true;
                rust = !other;
            }
            _ if word.starts_with("ignore-") => {
                ignore_some = true;
                rust = !other;
            }
            "test_harness" | "compile_fail" | "standalone_crate" => rust = !other || rust,
            "custom" => custom = true,
            _ if word.starts_with("edition") => {}
            _ if word
                .strip_prefix("rust")
                .is_some_and(|year| matches!(year, "2015" | "2018" | "2021" | "2024")) => {}
            _ => other = true,
        }
    }
    if custom || (other && !rust) {
        Rust::No
    } else if ignore && !ignore_some {
        Rust::Ignored
    } else {
        Rust::Yes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn examples(text: &str) -> Vec<(usize, usize)> {
        let docs = Docs::read(text);
        docs.groups
            .iter()
            .flat_map(|group| group.examples.iter().filter(|e| !e.ignored))
            .map(|example| (example.start, example.end))
            .collect()
    }

    #[test]
    fn info_strings_follow_rustdoc() {
        // Checked against `cargo test --doc -- --list` with rustdoc 1.93.
        let runs = [
            "",
            "rust",
            "no_run",
            "should_panic",
            "compile_fail",
            "compile_fail,E0308",
            "edition2018",
            "edition2024",
            "should_panic,text",
            "rust,text",
            "text,rust",
            "{.example}",
            "{class=foo}",
            "rust {.x}",
            "ignore-wasm32",
            "test_harness",
            "standalone_crate",
            "rust2021",
            "  rust  ",
            "should_panic text",
            "no_run,sh",
            "{ .example }",
            "rust,",
            ",",
        ];
        for info in runs {
            assert_eq!(rust('`', info), Rust::Yes, "{info:?}");
        }
        let other = [
            "text",
            "sh",
            "json",
            "toml",
            "E0308",
            "text,should_panic",
            "unknownword",
            "Rust",
            "rs",
            "console",
            "custom",
            "rust;ignore",
            "allow_fail",
            "edition2015,text",
            "c",
            "should-panic",
            "no-run",
        ];
        for info in other {
            assert_eq!(rust('`', info), Rust::No, "{info:?}");
        }
        assert_eq!(rust('`', "ignore"), Rust::Ignored);
        assert_eq!(rust('`', "rust,ignore"), Rust::Ignored);
        assert_eq!(rust('~', "text"), Rust::No);
    }

    #[test]
    fn every_code_block_form_is_an_example() {
        let fence = "```";
        // Indented, after the common margin of `///` is removed.
        assert_eq!(
            examples("/// Ex.\n///\n///     assert!(true);\nfn a() {}\n"),
            [(3, 3)]
        );
        assert_eq!(
            examples("///Ex.\n///\n///    assert!(true);\nfn a() {}\n"),
            [(3, 3)]
        );
        // Tilde fences, with or without an info string.
        assert_eq!(
            examples("/// ~~~\n/// assert!(true);\n/// ~~~\nfn a() {}\n"),
            [(1, 3)]
        );
        assert!(examples("/// ~~~text\n/// x\n/// ~~~\nfn a() {}\n").is_empty());
        // Inner docs, block comments and attributes.
        assert_eq!(
            examples("//! Crate.\n//!\n//!     assert!(true);\nfn a() {}\n"),
            [(3, 3)]
        );
        assert_eq!(
            examples(&format!(
                "/**\n * Ex.\n *\n * {fence}\n * assert!(true);\n * {fence}\n */\nfn a() {{}}\n"
            )),
            [(4, 6)]
        );
        assert_eq!(
            examples("/*! Crate.\n\n~~~\nassert!(true);\n~~~\n*/\nfn a() {}\n"),
            [(3, 5)]
        );
        assert_eq!(
            examples(
                "#[doc = \"Ex.\"]\n#[doc = \"\"]\n#[doc = \"    assert!(true);\"]\nfn a() {}\n"
            ),
            [(3, 3)]
        );
        // A raw line beside doc comments counts one column wider.
        assert_eq!(
            examples("/// Text\n///\n#[doc = \"    assert!(true);\"]\nfn a() {}\n"),
            [(3, 3)]
        );
        // Other attributes between doc comments keep them one item's docs.
        assert_eq!(
            examples(&format!(
                "/// {fence}\n#[inline]\n/// assert!(true);\n/// {fence}\nfn a() {{}}\n"
            )),
            [(1, 4)]
        );
        // Code inside a list item, a block quote, after a heading, unclosed.
        assert_eq!(
            examples("/// - item\n///\n///       assert!(true);\nfn a() {}\n"),
            [(3, 3)]
        );
        assert_eq!(
            examples(&format!(
                "/// > {fence}\n/// > assert!(true);\n/// > {fence}\nfn a() {{}}\n"
            )),
            [(1, 3)]
        );
        assert_eq!(
            examples("/// # Examples\n///     assert!(true);\nfn a() {}\n"),
            [(2, 2)]
        );
        assert_eq!(
            examples(&format!(
                "/// Text\n///\n/// {fence}\n/// assert!(true);\nfn a() {{}}\n"
            )),
            [(3, 4)]
        );
    }

    #[test]
    fn prose_that_only_looks_indented_is_not_an_example() {
        // rustdoc lists no doctest for any of these.
        for text in [
            "/// - item\n///\n///     continued\nfn a() {}\n",
            "/// Text\n///     continued\nfn a() {}\n",
            "/// Text\n///\n///\tassert!(true);\nfn a() {}\n",
            "/// <div>\n///     assert!(true);\n/// </div>\nfn a() {}\n",
            "/// ```sh\n/// probe --price 7\n/// ```\nfn a() {}\n",
        ] {
            assert!(examples(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn unreadable_documentation_is_unknown() {
        let docs = Docs::read(
            "#[doc = concat!(\"a\", \"b\")]\nfn a() {}\n#[cfg_attr(all(), doc = \"x\")]\nfn b() {}\n",
        );
        assert!(docs.groups.iter().all(|group| group.unknown));
        assert_eq!(docs.groups.len(), 2);
        // `doc(cfg(...))` adds no text, so it neither documents nor widens.
        let docs = Docs::read("#[cfg_attr(docsrs, doc(cfg(feature = \"x\")))]\nfn a() {}\n");
        assert!(docs.groups.is_empty());
        let docs = Docs::read(
            "macro_rules! make {\n    () => {\n        /// ```\n        /// assert!(true);\n        /// ```\n        pub fn made() {}\n    };\n}\n",
        );
        assert!(docs.opaque(4, "        /// assert!(true);"));
        assert!(!docs.opaque(6, "        pub fn made() {}"));
        // A file that does not parse leaves every doc line unknown.
        let docs = Docs::read("/// ```\n/// assert!(true);\nfn a( {}\n");
        assert!(docs.opaque(2, "/// assert!(true);"));
        // An included file is named, not read.
        let docs = Docs::read("#![doc = include_str!(\"../README.md\")]\nfn a() {}\n");
        assert_eq!(docs.groups[0].includes, ["../README.md"]);
        assert!(docs.groups[0].inner);
    }

    #[test]
    fn separate_items_keep_separate_docs() {
        let docs = Docs::read(
            "/// ```\n/// assert!(true);\n/// ```\nfn a() {}\n/// Prose.\nfn b() {}\npub enum E {\n    /// ~~~\n    /// assert!(true);\n    /// ~~~\n    A,\n}\n",
        );
        assert_eq!(docs.groups.len(), 3);
        assert!(docs.at(5).is_some_and(|group| group.examples.is_empty()));
        assert!(docs.at(9).is_some_and(|group| group.in_example(9, false)));
        assert_eq!(docs.outer(1, 4).count(), 1);
    }

    #[test]
    fn included_markdown_has_its_own_examples() {
        let found =
            markdown_examples("# Title\n\nExample:\n\n    assert!(true);\n\n```text\nx\n```\n");
        assert_eq!(
            found,
            [Example {
                start: 5,
                end: 5,
                ignored: false
            }]
        );
    }
}
