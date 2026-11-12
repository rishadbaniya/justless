//! Markdown headings by the CommonMark block rules, shared by everything that
//! splits a Markdown file into sections: the `read` outline that `expand` and
//! `edit` select from, and the sections `lookup` ranks.
//!
//! A heading is an ATX line such as `## Setup`, or a setext heading: paragraph
//! text underlined with `=` (level 1) or `-` (level 2). Only headings outside
//! block quotes and list items start sections. Fenced and indented code, HTML
//! blocks and YAML front matter hold none, and a `---` that cannot underline a
//! paragraph is a thematic break.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

/// A heading outside any block quote or list item. Lines are 0-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    /// The ATX line, or a setext heading's first line of text.
    pub first: usize,
    /// The ATX line, or the setext underline.
    pub last: usize,
    pub level: usize,
    /// The heading text, without markers, a closing run of `#` or the
    /// underline. The lines of a setext heading are joined with spaces.
    pub title: String,
}

/// A heading and its section, which runs until the next heading of the same
/// or a higher level.
#[derive(Debug, PartialEq, Eq)]
pub struct Section {
    pub heading: Heading,
    /// The line after the section: the next such heading's first line, or the
    /// line count.
    pub end: usize,
}

/// The headings of a Markdown file, given its lines without line endings. A
/// trailing `\r` is ignored.
pub fn headings<S: AsRef<str>>(lines: &[S]) -> Vec<Heading> {
    let mut scanner = Scanner::default();
    for (number, line) in lines.iter().enumerate().skip(front_matter(lines)) {
        let mut line = line.as_ref();
        if number == 0 {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        scanner.line(number, line);
    }
    scanner.headings
}

/// The sections of a Markdown file, in heading order.
pub fn sections<S: AsRef<str>>(lines: &[S]) -> Vec<Section> {
    let headings = headings(lines);
    let mut ends = vec![lines.len(); headings.len()];
    let mut open = Vec::<usize>::new();
    for (index, heading) in headings.iter().enumerate() {
        while let Some(&outer) = open.last()
            && headings[outer].level >= heading.level
        {
            ends[outer] = heading.first;
            open.pop();
        }
        open.push(index);
    }
    headings
        .into_iter()
        .zip(ends)
        .map(|(heading, end)| Section { heading, end })
        .collect()
}

/// The lines a YAML front matter block takes at the top of a file: `---`
/// through a closing `---` or `...`. Without a closing line there is none.
fn front_matter<S: AsRef<str>>(lines: &[S]) -> usize {
    fn trimmed(line: &impl AsRef<str>) -> &str {
        line.as_ref().trim_end_matches([' ', '\t', '\r'])
    }
    match lines.first() {
        Some(first) if trimmed(first).trim_start_matches('\u{feff}') == "---" => lines[1..]
            .iter()
            .position(|line| matches!(trimmed(line), "---" | "..."))
            .map_or(0, |closing| closing + 2),
        _ => 0,
    }
}

#[derive(Default)]
struct Scanner<'a> {
    /// Open block quotes and list items, outermost first.
    containers: Vec<Container>,
    /// The open leaf block, inside the innermost container.
    leaf: Leaf<'a>,
    headings: Vec<Heading>,
}

#[derive(Clone, Copy)]
enum Container {
    Quote,
    /// A list item whose content starts `width` columns past its parent's,
    /// and whether it holds a block yet.
    Item {
        width: usize,
        filled: bool,
    },
}

#[derive(Default)]
enum Leaf<'a> {
    #[default]
    None,
    /// Paragraph lines as (line, text), and how many of them at the start are
    /// link reference definitions.
    Paragraph {
        lines: Vec<(usize, &'a str)>,
        definitions: usize,
    },
    /// A fenced code block opened by `width` backticks or tildes.
    Fence {
        marker: u8,
        width: usize,
    },
    /// An indented code block.
    Code,
    Html(HtmlEnd),
}

#[derive(Clone, Copy)]
enum HtmlEnd {
    /// Ends after the first line, including the opening line, that contains
    /// one of these, compared without case.
    Contains(&'static [&'static str]),
    /// Ends at a blank line.
    Blank,
}

/// A line with tabs expanded to spaces at four-column stops, as CommonMark
/// measures indentation.
struct Expanded<'a> {
    original: &'a str,
    text: Cow<'a, str>,
    /// The original byte of each expanded byte, when tabs were expanded.
    origin: Option<Vec<usize>>,
}

impl<'a> Expanded<'a> {
    fn new(original: &'a str) -> Self {
        if !original.contains('\t') {
            return Self {
                original,
                text: Cow::Borrowed(original),
                origin: None,
            };
        }
        let mut text = String::with_capacity(original.len() + 8);
        let mut origin = Vec::with_capacity(original.len() + 8);
        let mut column = 0;
        for (at, character) in original.char_indices() {
            if character == '\t' {
                let width = 4 - column % 4;
                text.extend(std::iter::repeat_n(' ', width));
                origin.extend(std::iter::repeat_n(at, width));
                column += width;
            } else {
                text.push(character);
                origin.extend(at..at + character.len_utf8());
                column += 1;
            }
        }
        Self {
            original,
            text: Cow::Owned(text),
            origin: Some(origin),
        }
    }

    /// The original text of an expanded range, trimmed of spaces.
    fn slice(&self, range: Range<usize>) -> &'a str {
        let piece = &self.text[range.clone()];
        let start = range.start + (piece.len() - piece.trim_start_matches(' ').len());
        let end = range.start + piece.trim_end_matches(' ').len();
        let original = self.original;
        if start >= end {
            return "";
        }
        match &self.origin {
            None => &original[start..end],
            Some(origin) => &original[origin[start]..origin[end - 1] + 1],
        }
    }
}

/// The spaces at `at` and the offset after them.
fn indentation(text: &str, at: usize) -> (usize, usize) {
    let spaces = text[at..].bytes().take_while(|&byte| byte == b' ').count();
    (spaces, at + spaces)
}

impl<'a> Scanner<'a> {
    fn line(&mut self, number: usize, line: &'a str) {
        let line = Expanded::new(line.strip_suffix('\r').unwrap_or(line));
        let text = &*line.text;
        let mut at = 0;
        // Each open container must continue on this line, outermost first.
        let mut matched = 0;
        while let Some(container) = self.containers.get_mut(matched) {
            let (indent, next) = indentation(text, at);
            let blank = next == text.len();
            match container {
                Container::Quote if indent <= 3 && text[next..].starts_with('>') => {
                    at = next + 1;
                    if text[at..].starts_with(' ') {
                        at += 1;
                    }
                }
                Container::Item { filled: true, .. } if blank => at = next,
                Container::Item { width, .. } if !blank && indent >= *width => at += *width,
                _ => break,
            }
            matched += 1;
        }
        let continued = matched == self.containers.len();
        let blank = indentation(text, at).1 == text.len();
        // The open code or HTML block takes the line when its containers do.
        if continued {
            match self.leaf {
                Leaf::Fence { marker, width } => {
                    let (indent, next) = indentation(text, at);
                    if indent <= 3 && closes_fence(&text[next..], marker, width) {
                        self.leaf = Leaf::None;
                    }
                    return;
                }
                Leaf::Code if blank || indentation(text, at).0 >= 4 => return,
                Leaf::Code => self.leaf = Leaf::None,
                Leaf::Html(HtmlEnd::Blank) => {
                    if blank {
                        self.leaf = Leaf::None;
                    }
                    return;
                }
                Leaf::Html(HtmlEnd::Contains(ends)) => {
                    if contains(&text[at..], ends) {
                        self.leaf = Leaf::None;
                    }
                    return;
                }
                Leaf::Paragraph { .. } | Leaf::None => {}
            }
        }
        if blank {
            self.close(&mut matched);
            self.leaf = Leaf::None;
            return;
        }
        // `paragraph`: the open paragraph continues here, so this line may
        // underline it. `lazy`: a paragraph is open but a container around it
        // did not continue, so only paragraph text continues it.
        let open = matches!(self.leaf, Leaf::Paragraph { .. });
        let mut paragraph = continued && open;
        let mut lazy = !continued && open;
        loop {
            let (indent, next) = indentation(text, at);
            let rest = &text[next..];
            if rest.is_empty() {
                // An empty block quote or list item opened on this line.
                self.leaf = Leaf::None;
                return;
            }
            if indent >= 4 {
                // Indented code cannot interrupt a paragraph.
                if paragraph || lazy {
                    break;
                }
                self.start(&mut matched, Leaf::Code);
                return;
            }
            if rest.starts_with('>') {
                self.start(&mut matched, Leaf::None);
                at = next + 1;
                if text[at..].starts_with(' ') {
                    at += 1;
                }
                self.containers.push(Container::Quote);
                matched += 1;
                (paragraph, lazy) = (false, false);
                continue;
            }
            if let Some((level, title)) = atx(rest) {
                self.start(&mut matched, Leaf::None);
                if self.containers.is_empty() {
                    self.headings.push(Heading {
                        first: number,
                        last: number,
                        level,
                        title: line.slice(next + title.start..next + title.end).to_owned(),
                    });
                }
                return;
            }
            if let Some((marker, width)) = fence(rest) {
                self.start(&mut matched, Leaf::Fence { marker, width });
                return;
            }
            if let Some(end) = html(rest, paragraph || lazy) {
                let leaf = match end {
                    HtmlEnd::Contains(ends) if contains(rest, ends) => Leaf::None,
                    end => Leaf::Html(end),
                };
                self.start(&mut matched, leaf);
                return;
            }
            if paragraph
                && let Some(level) = setext(rest)
                && self.underline(number, level)
            {
                return;
            }
            if thematic_break(rest) {
                self.start(&mut matched, Leaf::None);
                return;
            }
            if let Some((padding, empty)) = list_marker(rest, paragraph) {
                self.start(&mut matched, Leaf::None);
                self.containers.push(Container::Item {
                    width: indent + padding,
                    filled: !empty,
                });
                matched += 1;
                at = if empty { text.len() } else { next + padding };
                (paragraph, lazy) = (false, false);
                continue;
            }
            break;
        }
        let (_, next) = indentation(text, at);
        let content = line.slice(next..text.len());
        if lazy {
            self.add(number, content);
            return;
        }
        self.close(&mut matched);
        if paragraph {
            self.add(number, content);
        } else {
            self.start(
                &mut matched,
                Leaf::Paragraph {
                    lines: Vec::new(),
                    definitions: 0,
                },
            );
            self.add(number, content);
        }
    }

    /// Closes the containers that did not continue, with the leaf inside them.
    fn close(&mut self, matched: &mut usize) {
        if *matched < self.containers.len() {
            self.containers.truncate(*matched);
            self.leaf = Leaf::None;
        }
        *matched = self.containers.len();
    }

    /// Starts a block in the innermost container that continued, closing the
    /// open leaf.
    fn start(&mut self, matched: &mut usize, leaf: Leaf<'a>) {
        self.close(matched);
        if let Some(Container::Item { filled, .. }) = self.containers.last_mut() {
            *filled = true;
        }
        self.leaf = leaf;
    }

    /// Adds a line of text to the open paragraph.
    fn add(&mut self, number: usize, content: &'a str) {
        if let Leaf::Paragraph { lines, definitions } = &mut self.leaf {
            if *definitions == lines.len() && definition(content) {
                *definitions += 1;
            }
            lines.push((number, content));
        }
    }

    /// Turns the open paragraph into a setext heading, unless it holds only
    /// link reference definitions.
    fn underline(&mut self, number: usize, level: usize) -> bool {
        let Leaf::Paragraph { lines, definitions } = &self.leaf else {
            return false;
        };
        let Some(&(first, _)) = lines.get(*definitions) else {
            return false;
        };
        if self.containers.is_empty() {
            let title = lines[*definitions..]
                .iter()
                .map(|(_, text)| *text)
                .collect::<Vec<_>>()
                .join(" ");
            self.headings.push(Heading {
                first,
                last: number,
                level,
                title,
            });
        }
        self.leaf = Leaf::None;
        true
    }
}

/// An ATX heading's level and the range of its text in `rest`, which starts
/// at the first `#`.
fn atx(rest: &str) -> Option<(usize, Range<usize>)> {
    let level = rest.bytes().take_while(|&byte| byte == b'#').count();
    let after = &rest[level..];
    if !(1..=6).contains(&level) || !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    let start = level + (after.len() - after.trim_start_matches(' ').len());
    let mut end = rest.trim_end_matches(' ').len().max(start);
    // A closing run of #s counts only after a space, or as the whole text.
    let unclosed = rest[start..end].trim_end_matches('#');
    if unclosed.is_empty() || unclosed.ends_with(' ') {
        end = start + unclosed.trim_end_matches(' ').len();
    }
    Some((level, start..end))
}

/// A fence opening: three or more backticks or tildes, and for backticks an
/// info string without one.
fn fence(rest: &str) -> Option<(u8, usize)> {
    let marker = *rest.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let width = rest.bytes().take_while(|&byte| byte == marker).count();
    (width >= 3 && !(marker == b'`' && rest[width..].contains('`'))).then_some((marker, width))
}

fn closes_fence(rest: &str, marker: u8, width: usize) -> bool {
    let run = rest.bytes().take_while(|&byte| byte == marker).count();
    run >= width && rest[run..].trim_matches(' ').is_empty()
}

/// A setext underline's level: a run of `=` for 1 or `-` for 2.
fn setext(rest: &str) -> Option<usize> {
    let underline = rest.trim_end_matches(' ');
    let level = match underline.as_bytes().first()? {
        b'=' => 1,
        b'-' => 2,
        _ => return None,
    };
    underline
        .bytes()
        .all(|byte| byte == underline.as_bytes()[0])
        .then_some(level)
}

/// Three or more `*`, `-` or `_`, alone or with spaces between.
fn thematic_break(rest: &str) -> bool {
    let Some(&marker @ (b'*' | b'-' | b'_')) = rest.as_bytes().first() else {
        return false;
    };
    rest.bytes().all(|byte| byte == marker || byte == b' ')
        && rest.bytes().filter(|&byte| byte == marker).count() >= 3
}

/// A list item marker: the columns from the marker to the item's content, and
/// whether the item starts empty. An item that interrupts a paragraph must
/// have content, and an ordered one must start at 1.
fn list_marker(rest: &str, interrupts: bool) -> Option<(usize, bool)> {
    let bytes = rest.as_bytes();
    let (length, start) = match bytes.first()? {
        b'-' | b'+' | b'*' => (1, None),
        b'0'..=b'9' => {
            let digits = bytes
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits > 9 || !matches!(bytes.get(digits), Some(b'.' | b')')) {
                return None;
            }
            (digits + 1, rest[..digits].parse::<u32>().ok())
        }
        _ => return None,
    };
    let after = &rest[length..];
    if !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    let spaces = after.len() - after.trim_start_matches(' ').len();
    let empty = spaces == after.len();
    if interrupts && (empty || start.is_some_and(|start| start != 1)) {
        return None;
    }
    let padding = if empty || spaces > 4 {
        length + 1
    } else {
        length + spaces
    };
    Some((padding, empty))
}

/// Tags that start an HTML block ending at a blank line.
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

/// A line holding only one complete open or closing tag.
static LONE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    let attribute = r#"\s+[A-Za-z_:][A-Za-z0-9_.:-]*(?:\s*=\s*(?:[^\s"'=<>`]+|'[^']*'|"[^"]*"))?"#;
    Regex::new(&format!(
        r"^(?:<([A-Za-z][A-Za-z0-9-]*)(?:{attribute})*\s*/?>|</([A-Za-z][A-Za-z0-9-]*)\s*>)\s*$"
    ))
    .unwrap()
});

/// A link reference definition on one line, such as `[docs]: https://… "Docs"`.
static DEFINITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^\[((?:[^\[\]\\]|\\.)+)\]:[ ]*(?:<(?:[^<>\\]|\\.)*>|[^\s<]\S*)(?:[ ]+(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\((?:[^()\\]|\\.)*\)))?[ ]*$"#,
    )
    .unwrap()
});

fn definition(content: &str) -> bool {
    content.starts_with('[')
        && DEFINITION
            .captures(content)
            .is_some_and(|definition| !definition[1].trim().is_empty())
}

/// The end condition of an HTML block that starts `rest`. A lone tag of
/// another name cannot interrupt a paragraph.
fn html(rest: &str, interrupts: bool) -> Option<HtmlEnd> {
    if !rest.starts_with('<') {
        return None;
    }
    let lower = rest.to_ascii_lowercase();
    let ends_name = |name: &str| {
        lower[1..].starts_with(name)
            && lower[1 + name.len()..]
                .chars()
                .next()
                .is_none_or(|next| next == ' ' || next == '>')
    };
    if ["pre", "script", "style", "textarea"]
        .iter()
        .any(|name| ends_name(name))
    {
        return Some(HtmlEnd::Contains(&[
            "</pre>",
            "</script>",
            "</style>",
            "</textarea>",
        ]));
    }
    if lower.starts_with("<!--") {
        return Some(HtmlEnd::Contains(&["-->"]));
    }
    if lower.starts_with("<?") {
        return Some(HtmlEnd::Contains(&["?>"]));
    }
    if lower
        .strip_prefix("<!")
        .is_some_and(|after| after.starts_with(|c: char| c.is_ascii_alphabetic()))
    {
        return Some(HtmlEnd::Contains(&[">"]));
    }
    if lower.starts_with("<![cdata[") {
        return Some(HtmlEnd::Contains(&["]]>"]));
    }
    let tag = lower[1..].strip_prefix('/').unwrap_or(&lower[1..]);
    let name = &tag[..tag
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric())
        .count()];
    let after = &tag[name.len()..];
    if BLOCK_TAGS.contains(&name)
        && (after.is_empty() || after.starts_with([' ', '>']) || after.starts_with("/>"))
    {
        return Some(HtmlEnd::Blank);
    }
    let lone = LONE_TAG.captures(rest)?;
    let name = lone.get(1).or(lone.get(2))?.as_str().to_ascii_lowercase();
    (!interrupts && !matches!(name.as_str(), "pre" | "script" | "style" | "textarea"))
        .then_some(HtmlEnd::Blank)
}

fn contains(text: &str, ends: &[&str]) -> bool {
    let lower = text.to_ascii_lowercase();
    ends.iter().any(|end| lower.contains(end))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (first line, last line, level, title), lines 1-based.
    fn found(text: &str) -> Vec<(usize, usize, usize, String)> {
        headings(&text.lines().collect::<Vec<_>>())
            .into_iter()
            .map(|heading| {
                (
                    heading.first + 1,
                    heading.last + 1,
                    heading.level,
                    heading.title,
                )
            })
            .collect()
    }

    fn titles(text: &str) -> Vec<String> {
        found(text)
            .into_iter()
            .map(|(_, _, _, title)| title)
            .collect()
    }

    #[test]
    fn setext_headings_underline_a_paragraph() {
        assert_eq!(
            found("# Install\n\nRun it.\n\nUsage\n=====\n\nCall it.\n\n## Flags\n"),
            [
                (1, 1, 1, "Install".to_owned()),
                (5, 6, 1, "Usage".to_owned()),
                (10, 10, 2, "Flags".to_owned())
            ]
        );
        // Any length of underline, and a heading may span several lines.
        assert_eq!(
            found("A\n=\nB\n-\n\nSeveral\n  lines here\n---   \n"),
            [
                (1, 2, 1, "A".to_owned()),
                (3, 4, 2, "B".to_owned()),
                (6, 8, 2, "Several lines here".to_owned())
            ]
        );
        // Text directly above an underline belongs to the heading.
        assert_eq!(
            found("# Install\nRun it.\nUsage\n===\n"),
            [
                (1, 1, 1, "Install".to_owned()),
                (2, 4, 1, "Run it. Usage".to_owned())
            ]
        );
        assert_eq!(titles("   Indented\n   ===\n"), ["Indented"]);
        assert_eq!(titles("Title\r\n=====\r\n"), ["Title"]);
        let split_on_newlines = headings(&["Title\r", "=====\r", "Text\t\r"]);
        assert_eq!(split_on_newlines[0].title, "Title");
        assert_eq!(split_on_newlines[0].last, 1);
        assert_eq!(
            titles("Para\n    continued\n---\n"),
            ["Para continued"],
            "an indented line continues a paragraph"
        );
    }

    #[test]
    fn underlines_that_follow_no_paragraph_are_not_headings() {
        for text in [
            "Text\n\n---\n",
            "Text\n\n===\n",
            "- item\n---\n",
            "- item\n===\n",
            "1. item\n---\n",
            "> quoted\n---\n",
            "> quoted\n===\n",
            "    indented code\n---\n",
            "```\ncode\n```\n---\n",
            "# Title\n---\n",
            "***\n---\n",
            "Foo\n= =\n",
            "Foo\n--- -\n",
            "Foo\n    ---\n",
            "[docs]: https://example.com \"Docs\"\n===\n",
            "[docs]: <https://example.com>\n---\n",
        ] {
            assert!(
                found(text).iter().all(|(_, _, _, title)| title == "Title"),
                "{text:?}: {:?}",
                found(text)
            );
        }
        // Definitions above the text are not part of the heading.
        assert_eq!(
            found("[docs]: /docs\nTitle\n---\n"),
            [(2, 3, 2, "Title".to_owned())]
        );
        assert_eq!(
            titles("[not a definition]: two words\n---\n"),
            ["[not a definition]: two words"]
        );
    }

    #[test]
    fn atx_headings_need_a_space_and_at_most_three_spaces_of_indent() {
        assert_eq!(
            titles(
                "#tag\n#\tTabbed\n   # Three\n# Closed ##\n# C#\n# Escaped \\#\n#\n### ###\n####### Seven\n#5 bolt\n"
            ),
            ["Tabbed", "Three", "Closed", "C#", "Escaped \\#", "", ""]
        );
        assert_eq!(titles("Para\n    # Four\n"), Vec::<String>::new());
        assert_eq!(titles("\n    # Four\n"), Vec::<String>::new());
        assert_eq!(titles("\u{feff}# Marked\n"), ["Marked"]);
        // An ATX heading interrupts a paragraph.
        assert_eq!(titles("Para\n## Next\n"), ["Next"]);
    }

    #[test]
    fn code_html_and_front_matter_hold_no_headings() {
        for (text, expected) in [
            ("```sh title=\"x\"\n# a\nB\n===\n```\n# after\n", "after"),
            ("~~~~ info ```\n# a\n~~~\n# b\n~~~~\n# after\n", "after"),
            ("````\n```\n# a\n````\n# after\n", "after"),
            ("  ```\n  # a\n  ```\n# after\n", "after"),
            ("<div>\n# a\n\n# after\n", "after"),
            ("<DIV class=\"x\">\n# a\n\n# after\n", "after"),
            ("<details>\n<summary>More</summary>\n\n# after\n", "after"),
            ("<!-- a\n# a\n-->\n# after\n", "after"),
            ("<!-- one line -->\n# after\n", "after"),
            ("<pre>\n# a\n\n# a\n</PRE>\n# after\n", "after"),
            ("<?php\n# a\n?>\n# after\n", "after"),
            ("<!DOCTYPE html>\n# after\n", "after"),
            ("<![CDATA[\n# a\n]]>\n# after\n", "after"),
            ("<custom-tag attr='1'>\n# a\n\n# after\n", "after"),
            ("---\ntitle: x\n# a\n---\n# after\n", "after"),
            ("---\ntitle: x\n...\nAfter\n===\n", "After"),
        ] {
            assert_eq!(titles(text), [expected], "{text:?}");
        }
        // A backtick fence's info string has no backticks, so this is text.
        assert_eq!(titles("``` not `a` fence\n# after\n"), ["after"]);
        // An unclosed fence runs to the end of the file.
        assert_eq!(titles("# before\n```\n# a\n"), ["before"]);
        // Without a closing line, `---` at the top is a thematic break.
        assert_eq!(titles("---\n# first\nText\n"), ["first"]);
        // A lone tag cannot interrupt a paragraph.
        assert_eq!(titles("Para\n<span>\n===\n"), ["Para <span>"]);
    }

    #[test]
    fn headings_inside_quotes_and_list_items_start_no_section() {
        assert_eq!(titles("> # quoted\n> Quote\n> ---\n# after\n"), ["after"]);
        assert_eq!(
            titles("- item\n\n  # nested\n  Text\n  ---\n# after\n"),
            ["after"]
        );
        assert_eq!(titles("1. one\n   two\n   ===\n"), Vec::<String>::new());
        // A line indented less than the item's content closes it.
        assert_eq!(titles("- item\n\nUsage\n---\n"), ["Usage"]);
        assert_eq!(titles("- item\n # out\n"), ["out"]);
        // A fence closes with the list item that holds it.
        assert_eq!(titles("- x\n  ```\n  # a\n# after\n"), ["after"]);
        assert_eq!(titles("> ```\n> # a\n# after\n"), ["after"]);
        // An empty item followed by a blank line holds nothing more.
        assert_eq!(titles("-\n\n  Text\n  ---\n"), ["Text"]);
        // An ordered item interrupts a paragraph only from 1.
        assert_eq!(titles("Version\n2. x\n---\n"), ["Version 2. x"]);
        assert_eq!(titles("Version\n1. x\n---\n"), Vec::<String>::new());
    }

    #[test]
    fn sections_end_before_the_next_heading_of_the_same_or_higher_level() {
        let text = "intro\n# A\n## B\ntext\n\nC\n=\n### D\n## E\n";
        let lines = text.lines().collect::<Vec<_>>();
        let spans: Vec<_> = sections(&lines)
            .into_iter()
            .map(|section| (section.heading.title, section.heading.first, section.end))
            .collect();
        assert_eq!(
            spans,
            [
                ("A".to_owned(), 1, 5),
                ("B".to_owned(), 2, 5),
                ("C".to_owned(), 5, 9),
                ("D".to_owned(), 7, 8),
                ("E".to_owned(), 8, 9)
            ]
        );
    }
}
