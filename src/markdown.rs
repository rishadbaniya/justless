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

const MIN_CODE_LINES: usize = 15;
const MIN_TABLE_ROWS: usize = 8;

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
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub struct Section {
    pub heading: Heading,
    /// The line after the section: the next such heading's first line, or the
    /// line count.
    pub end: usize,
}

/// The kind of structural block emitted in a Markdown outline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    FrontMatter,
    Heading(usize),
    Code,
    Table,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FrontMatter => "frontmatter",
            Self::Heading(1) => "h1",
            Self::Heading(2) => "h2",
            Self::Heading(3) => "h3",
            Self::Heading(4) => "h4",
            Self::Heading(5) => "h5",
            Self::Heading(6) | Self::Heading(_) => "h6",
            Self::Code => "code",
            Self::Table => "table",
        }
    }
}

/// A structural block in a Markdown outline. Line indices are 0-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineBlock {
    pub kind: BlockKind,
    /// Compact label for the outline row (heading title, frontmatter keys,
    /// fenced code language, or table header columns and row count).
    pub name: String,
    /// First 0-based line of the block.
    pub first: usize,
    /// Last 0-based line of the block's header (for `--match` searching).
    pub header_last: usize,
    /// Exclusive 0-based end line of the block (`first < end <= lines.len()`).
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

fn section_ends(headings: &[Heading], total_lines: usize) -> Vec<usize> {
    let mut ends = vec![total_lines; headings.len()];
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
    ends
}

/// The sections of a Markdown file, in heading order.
#[allow(dead_code)]
pub fn sections<S: AsRef<str>>(lines: &[S]) -> Vec<Section> {
    let headings = headings(lines);
    let ends = section_ends(&headings, lines.len());
    headings
        .into_iter()
        .zip(ends)
        .map(|(heading, end)| Section { heading, end })
        .collect()
}

/// The structural blocks of a Markdown file for `read --outline`: YAML
/// frontmatter (when present), top-level headings (`h1`..`h6`) with their full
/// section spans, large top-level fenced code blocks (`>= 15` lines), and large
/// top-level GFM pipe tables (`>= 8` data rows), sorted in document order.
pub fn outline_blocks<S: AsRef<str>>(lines: &[S]) -> Vec<OutlineBlock> {
    let fm_end = front_matter(lines);
    let mut scanner = Scanner {
        collect_blocks: true,
        ..Scanner::default()
    };
    for (number, line) in lines.iter().enumerate().skip(fm_end) {
        let mut line = line.as_ref();
        if number == 0 {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        scanner.line(number, line);
    }
    scanner.finish(lines.len());

    let mut blocks =
        Vec::with_capacity(usize::from(fm_end > 0) + scanner.headings.len() + scanner.blocks.len());
    if fm_end > 0 {
        blocks.push(OutlineBlock {
            kind: BlockKind::FrontMatter,
            name: front_matter_label(&lines[..fm_end]),
            first: 0,
            header_last: fm_end - 1,
            end: fm_end,
        });
    }
    let ends = section_ends(&scanner.headings, lines.len());
    for (heading, end) in scanner.headings.into_iter().zip(ends) {
        blocks.push(OutlineBlock {
            kind: BlockKind::Heading(heading.level),
            name: heading.title,
            first: heading.first,
            header_last: heading.last,
            end,
        });
    }
    blocks.extend(scanner.blocks);
    blocks.sort_by_key(|block| (block.first, std::cmp::Reverse(block.end)));
    blocks
}

/// The lines a YAML front matter block takes at the top of a file: `---`
/// directly followed by YAML, through a closing `---` or `...`. Without a
/// closing line, or with a blank line after the opening one, the `---` is a
/// thematic break, so a file that opens with a rule keeps its headings.
fn front_matter<S: AsRef<str>>(lines: &[S]) -> usize {
    fn trimmed(line: &impl AsRef<str>) -> &str {
        line.as_ref().trim_end_matches([' ', '\t', '\r'])
    }
    let opens = lines
        .first()
        .is_some_and(|first| trimmed(first).trim_start_matches('\u{feff}') == "---")
        && lines.get(1).is_some_and(|next| !trimmed(next).is_empty());
    if !opens {
        return 0;
    }
    lines[1..]
        .iter()
        .position(|line| matches!(trimmed(line), "---" | "..."))
        .map_or(0, |closing| closing + 2)
}

#[derive(Default)]
struct Scanner<'a> {
    /// Open block quotes and list items, outermost first.
    containers: Vec<Container>,
    /// The open leaf block, inside the innermost container.
    leaf: Leaf,
    /// Reusable line buffer for the currently open `Leaf::Paragraph`.
    paragraph_lines: Vec<(usize, &'a str)>,
    headings: Vec<Heading>,
    /// Whether to also collect large top-level fenced code blocks and GFM tables.
    collect_blocks: bool,
    blocks: Vec<OutlineBlock>,
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
enum Leaf {
    #[default]
    None,
    /// Open paragraph whose `(line, text)` entries live in `Scanner::paragraph_lines`,
    /// tracking how many leading lines are link reference definitions.
    Paragraph {
        definitions: usize,
    },
    /// A fenced code block opened by `width` backticks or tildes.
    Fence {
        marker: u8,
        width: usize,
        first: usize,
        label: String,
        top_level: bool,
    },
    /// A top-level GFM pipe table.
    Table {
        header_line: usize,
        headers: Vec<String>,
        data_rows: usize,
        last_line: usize,
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

/// Fast check for closing a top-level fenced code block directly on raw bytes
/// (before tab expansion): at most 3 leading spaces (a leading tab expands to
/// column 4 and cannot close), at least `width` marker bytes, then only spaces
/// or tabs.
fn closes_fence_raw(raw: &str, marker: u8, width: usize) -> bool {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
        if i > 3 {
            return false;
        }
    }
    if bytes.get(i) != Some(&marker) {
        return false;
    }
    let start = i;
    while i < bytes.len() && bytes[i] == marker {
        i += 1;
    }
    i - start >= width && bytes[i..].iter().all(|&b| b == b' ' || b == b'\t')
}

impl<'a> Scanner<'a> {
    fn line(&mut self, number: usize, line: &'a str) {
        let raw = line.strip_suffix('\r').unwrap_or(line);
        // Top-level fences and HTML blocks can often be decided on raw bytes
        // without expanding tabs (important for Go/Makefile/TSV code fences).
        if self.containers.is_empty() {
            match &self.leaf {
                Leaf::Fence { marker, width, .. } => {
                    if closes_fence_raw(raw, *marker, *width)
                        && let Leaf::Fence {
                            first,
                            label,
                            top_level,
                            ..
                        } = std::mem::take(&mut self.leaf)
                    {
                        let end = number + 1;
                        if self.collect_blocks && top_level && end - first >= MIN_CODE_LINES {
                            self.blocks.push(OutlineBlock {
                                kind: BlockKind::Code,
                                name: label,
                                first,
                                header_last: first,
                                end,
                            });
                        }
                    }
                    return;
                }
                Leaf::Html(HtmlEnd::Blank) => {
                    if raw.bytes().all(|b| b == b' ' || b == b'\t') {
                        self.leaf = Leaf::None;
                    }
                    return;
                }
                Leaf::Html(HtmlEnd::Contains(ends)) => {
                    if contains(raw, ends) {
                        self.leaf = Leaf::None;
                    }
                    return;
                }
                _ => {}
            }
        }
        let line = Expanded::new(raw);
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
        // The open code, table or HTML block takes the line when its containers do.
        if continued {
            match self.leaf {
                Leaf::Fence { marker, width, .. } => {
                    let (indent, next) = indentation(text, at);
                    if indent <= 3
                        && closes_fence(&text[next..], marker, width)
                        && let Leaf::Fence {
                            first,
                            label,
                            top_level,
                            ..
                        } = std::mem::take(&mut self.leaf)
                    {
                        let end = number + 1;
                        if self.collect_blocks && top_level && end - first >= MIN_CODE_LINES {
                            self.blocks.push(OutlineBlock {
                                kind: BlockKind::Code,
                                name: label,
                                first,
                                header_last: first,
                                end,
                            });
                        }
                    }
                    return;
                }
                Leaf::Table { .. } => {
                    if blank {
                        self.finish_table();
                        return;
                    }
                    let (indent, next) = indentation(text, at);
                    let rest = &text[next..];
                    let interrupts = indent <= 3
                        && match rest.as_bytes().first().copied() {
                            Some(b'>') => true,
                            Some(b'#') => atx(rest).is_some(),
                            Some(b'`' | b'~') => fence(rest).is_some(),
                            Some(b'<') => html(rest, true).is_some(),
                            Some(b'-' | b'*') => {
                                thematic_break(rest) || list_marker(rest, true).is_some()
                            }
                            Some(b'_') => thematic_break(rest),
                            Some(b'+' | b'0'..=b'9') => list_marker(rest, true).is_some(),
                            _ => false,
                        };
                    if !interrupts && has_unescaped_pipe(rest) {
                        if let Leaf::Table {
                            data_rows,
                            last_line,
                            ..
                        } = &mut self.leaf
                        {
                            *data_rows += 1;
                            *last_line = number;
                        }
                        return;
                    }
                    self.finish_table();
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
            // Fast first-byte dispatch: normal prose lines skip all block checks in one branch.
            match rest.as_bytes()[0] {
                b'>' => {
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
                b'#' => {
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
                }
                b'`' | b'~' => {
                    if let Some((marker, width)) = fence(rest) {
                        self.start(&mut matched, Leaf::None);
                        let top_level = self.containers.is_empty();
                        let label = if self.collect_blocks && top_level {
                            fence_label(line.slice(next + width..text.len()))
                        } else {
                            String::new()
                        };
                        self.leaf = Leaf::Fence {
                            marker,
                            width,
                            first: number,
                            label,
                            top_level,
                        };
                        return;
                    }
                }
                b'<' => {
                    if let Some(end) = html(rest, paragraph || lazy) {
                        let leaf = match end {
                            HtmlEnd::Contains(ends) if contains(rest, ends) => Leaf::None,
                            end => Leaf::Html(end),
                        };
                        self.start(&mut matched, leaf);
                        return;
                    }
                }
                b'=' => {
                    if paragraph
                        && let Some(level) = setext(rest)
                        && self.underline(number, level)
                    {
                        return;
                    }
                }
                b'-' => {
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
                    if self.collect_blocks
                        && paragraph
                        && self.containers.is_empty()
                        && let Some(cols) = table_delimiter_columns(rest)
                        && self.start_table(number, cols)
                    {
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
                }
                b'*' => {
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
                }
                b'_' => {
                    if thematic_break(rest) {
                        self.start(&mut matched, Leaf::None);
                        return;
                    }
                }
                b'|' | b':' => {
                    if self.collect_blocks
                        && paragraph
                        && self.containers.is_empty()
                        && let Some(cols) = table_delimiter_columns(rest)
                        && self.start_table(number, cols)
                    {
                        return;
                    }
                }
                b'+' | b'0'..=b'9' => {
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
                }
                _ => {}
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
            self.paragraph_lines.clear();
            self.start(&mut matched, Leaf::Paragraph { definitions: 0 });
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
    fn start(&mut self, matched: &mut usize, leaf: Leaf) {
        self.close(matched);
        if let Some(Container::Item { filled, .. }) = self.containers.last_mut() {
            *filled = true;
        }
        self.leaf = leaf;
    }

    /// Adds a line of text to the open paragraph.
    fn add(&mut self, number: usize, content: &'a str) {
        if let Leaf::Paragraph { definitions } = &mut self.leaf {
            if *definitions == self.paragraph_lines.len() && definition(content) {
                *definitions += 1;
            }
            self.paragraph_lines.push((number, content));
        }
    }

    /// Turns the open paragraph into a setext heading, unless it holds only
    /// link reference definitions.
    fn underline(&mut self, number: usize, level: usize) -> bool {
        let Leaf::Paragraph { definitions } = self.leaf else {
            return false;
        };
        let Some(&(first, _)) = self.paragraph_lines.get(definitions) else {
            return false;
        };
        if self.containers.is_empty() {
            let slice = &self.paragraph_lines[definitions..];
            let title = if slice.len() == 1 {
                slice[0].1.to_owned()
            } else {
                let cap: usize =
                    slice.iter().map(|(_, text)| text.len()).sum::<usize>() + slice.len() - 1;
                let mut title = String::with_capacity(cap);
                for (index, &(_, text)) in slice.iter().enumerate() {
                    if index > 0 {
                        title.push(' ');
                    }
                    title.push_str(text);
                }
                title
            };
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

    /// Promotes the open paragraph's last line into a top-level GFM pipe table
    /// when its cell count matches the delimiter row's `cols`.
    fn start_table(&mut self, number: usize, cols: usize) -> bool {
        let Leaf::Paragraph { definitions } = self.leaf else {
            return false;
        };
        if self.paragraph_lines.len() <= definitions {
            return false;
        }
        let Some(&(header_line, header_text)) = self.paragraph_lines.last() else {
            return false;
        };
        let Some(headers) = split_table_cells(header_text) else {
            return false;
        };
        if headers.len() != cols {
            return false;
        }
        self.leaf = Leaf::Table {
            header_line,
            headers,
            data_rows: 0,
            last_line: number,
        };
        true
    }

    fn finish_table(&mut self) {
        if let Leaf::Table {
            header_line,
            headers,
            data_rows,
            last_line,
        } = std::mem::take(&mut self.leaf)
            && self.collect_blocks
            && data_rows >= MIN_TABLE_ROWS
        {
            self.blocks.push(OutlineBlock {
                kind: BlockKind::Table,
                name: table_label(&headers, data_rows),
                first: header_line,
                header_last: header_line,
                end: last_line + 1,
            });
        }
    }

    /// Flushes any top-level fenced code block or table that reaches EOF.
    fn finish(&mut self, total_lines: usize) {
        match std::mem::take(&mut self.leaf) {
            Leaf::Fence {
                first,
                label,
                top_level,
                ..
            } if self.collect_blocks
                && top_level
                && total_lines.saturating_sub(first) >= MIN_CODE_LINES =>
            {
                self.blocks.push(OutlineBlock {
                    kind: BlockKind::Code,
                    name: label,
                    first,
                    header_last: first,
                    end: total_lines,
                });
            }
            Leaf::Table {
                header_line,
                headers,
                data_rows,
                last_line,
            } if self.collect_blocks && data_rows >= MIN_TABLE_ROWS => {
                self.blocks.push(OutlineBlock {
                    kind: BlockKind::Table,
                    name: table_label(&headers, data_rows),
                    first: header_line,
                    header_last: header_line,
                    end: last_line + 1,
                });
            }
            _ => {}
        }
    }
}

fn clip_label(text: &str, max: usize) -> String {
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

/// Extracts a top-level (column-0) YAML mapping key from a frontmatter line.
fn yaml_top_level_key(line: &str) -> Option<&str> {
    let line = line.trim_end_matches('\r');
    let first = *line.as_bytes().first()?;
    if matches!(first, b' ' | b'\t' | b'#' | b'-' | b'?' | b':') {
        return None;
    }
    if first == b'"' || first == b'\'' {
        let bytes = line.as_bytes();
        let mut i = 1;
        while i < bytes.len() {
            if first == b'"' && bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == first {
                if first == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                let after = line[i + 1..].trim_start_matches([' ', '\t']);
                if after
                    .strip_prefix(':')
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
                {
                    let key = line[1..i].trim();
                    return (!key.is_empty()).then_some(key);
                }
                return None;
            }
            i += 1;
        }
        return None;
    }
    for (idx, _) in line.match_indices(':') {
        let after = line.as_bytes().get(idx + 1);
        if after.is_none_or(|&byte| byte == b' ' || byte == b'\t') {
            let key = line[..idx].trim();
            return (!key.is_empty()).then_some(key);
        }
    }
    None
}

fn front_matter_label<S: AsRef<str>>(lines: &[S]) -> String {
    let body = lines
        .get(1..lines.len().saturating_sub(1))
        .unwrap_or_default();
    let mut keys: Vec<&str> = Vec::new();
    for line in body {
        if let Some(key) = yaml_top_level_key(line.as_ref())
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    if keys.is_empty() {
        return "yaml".to_owned();
    }
    let mut shown: Vec<String> = Vec::new();
    let mut len = 0usize;
    for (idx, key) in keys.iter().enumerate() {
        if idx == 0 {
            let first = clip_label(key, 48);
            len = first.len();
            shown.push(first);
            continue;
        }
        if shown.len() >= 6 || len + 2 + key.len() > 72 {
            break;
        }
        len += 2 + key.len();
        shown.push((*key).to_owned());
    }
    let remaining = keys.len().saturating_sub(shown.len());
    if remaining == 0 {
        format!("keys: {}", shown.join(", "))
    } else {
        format!("keys: {}, +{remaining} more", shown.join(", "))
    }
}

fn fence_label(info: &str) -> String {
    let token = info.split_whitespace().next().unwrap_or("");
    let lang = if let Some(inner) = token.strip_prefix('{') {
        inner
            .trim_end_matches('}')
            .strip_prefix('.')
            .and_then(|rest| rest.split([',', ' ', '\t']).next())
            .unwrap_or("")
    } else {
        token.split([',', '{']).next().unwrap_or("")
    };
    let lang = lang.trim();
    if lang.is_empty() {
        "block".to_owned()
    } else {
        clip_label(lang, 32)
    }
}

fn has_unescaped_pipe(line: &str) -> bool {
    if !line.contains('|') {
        return false;
    }
    if !line.contains('\\') {
        return true;
    }
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && matches!(bytes[i + 1], b'\\' | b'|') {
            i += 2;
            continue;
        }
        if bytes[i] == b'|' {
            return true;
        }
        i += 1;
    }
    false
}

fn unescape_table_cell(raw: &str) -> String {
    let trimmed = raw.trim_matches([' ', '\t']);
    if !trimmed.contains(r"\|") {
        return trimmed.to_owned();
    }
    trimmed.replace(r"\|", "|")
}

/// Splits a GFM table row on unescaped `|`, stripping optional outer pipes.
fn split_table_cells(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim_matches([' ', '\t', '\r']);
    let bytes = trimmed.as_bytes();
    let mut pipes = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && matches!(bytes[i + 1], b'\\' | b'|') {
            i += 2;
            continue;
        }
        if bytes[i] == b'|' {
            pipes.push(i);
        }
        i += 1;
    }
    if pipes.is_empty() || trimmed == "|" {
        return None;
    }
    let leading_pipe = pipes.first() == Some(&0);
    let trailing_pipe =
        pipes.last() == Some(&(bytes.len() - 1)) && !(leading_pipe && pipes.len() == 1);
    let start = usize::from(leading_pipe);
    let end = if trailing_pipe {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    let inner_pipes = &pipes[usize::from(leading_pipe)..pipes.len() - usize::from(trailing_pipe)];
    let mut cells = Vec::with_capacity(inner_pipes.len() + 1);
    let mut pos = start;
    for &pipe in inner_pipes {
        cells.push(unescape_table_cell(&trimmed[pos..pipe]));
        pos = pipe + 1;
    }
    cells.push(unescape_table_cell(&trimmed[pos..end]));
    Some(cells)
}

/// Column count of a GFM table delimiter row (e.g. `| --- | ---: |`), or `None`.
fn table_delimiter_columns(line: &str) -> Option<usize> {
    let trimmed = line.trim_matches([' ', '\t', '\r']);
    let bytes = trimmed.as_bytes();
    let &first = bytes.first()?;
    if !matches!(first, b'|' | b'-' | b':') || trimmed == "|" {
        return None;
    }
    let mut has_pipe = false;
    for &byte in bytes {
        match byte {
            b'|' => has_pipe = true,
            b'-' | b':' | b' ' | b'\t' => {}
            _ => return None,
        }
    }
    if !has_pipe {
        return None;
    }
    let leading_pipe = bytes[0] == b'|';
    let trailing_pipe = bytes[bytes.len() - 1] == b'|' && bytes.len() > 1;
    let inner = &trimmed[usize::from(leading_pipe)..bytes.len() - usize::from(trailing_pipe)];
    let mut cols = 0usize;
    for cell in inner.split('|') {
        let cell = cell.trim_matches([' ', '\t']);
        let cell = cell.strip_prefix(':').unwrap_or(cell);
        let cell = cell.strip_suffix(':').unwrap_or(cell);
        if cell.is_empty() || !cell.bytes().all(|byte| byte == b'-') {
            return None;
        }
        cols += 1;
    }
    (cols > 0).then_some(cols)
}

fn table_label(headers: &[String], data_rows: usize) -> String {
    let non_empty: Vec<String> = headers
        .iter()
        .map(|cell| cell.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|cell| !cell.is_empty())
        .collect();
    if non_empty.is_empty() {
        let noun = if headers.len() == 1 { "col" } else { "cols" };
        return format!("{} {noun} ({data_rows} rows)", headers.len());
    }
    let mut shown: Vec<String> = Vec::new();
    let mut len = 0usize;
    for (idx, cell) in non_empty.iter().enumerate() {
        if idx == 0 {
            let first = clip_label(cell, 48);
            len = first.len();
            shown.push(first);
            continue;
        }
        if shown.len() >= 5 || len + 3 + cell.len() > 64 {
            break;
        }
        len += 3 + cell.len();
        shown.push(cell.clone());
    }
    let remaining = non_empty.len().saturating_sub(shown.len());
    if remaining == 0 {
        format!("{} ({data_rows} rows)", shown.join(" | "))
    } else {
        format!("{} | +{remaining} ({data_rows} rows)", shown.join(" | "))
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
    let (marker, level) = match rest.as_bytes().first()? {
        b'=' => (b'=', 1),
        b'-' => (b'-', 2),
        _ => return None,
    };
    let underline = rest.trim_end_matches(' ');
    underline
        .bytes()
        .all(|byte| byte == marker)
        .then_some(level)
}

/// Three or more `*`, `-` or `_`, alone or with spaces between.
fn thematic_break(rest: &str) -> bool {
    let Some(&marker @ (b'*' | b'-' | b'_')) = rest.as_bytes().first() else {
        return false;
    };
    let mut count = 0usize;
    for byte in rest.bytes() {
        if byte == marker {
            count += 1;
        } else if byte != b' ' {
            return false;
        }
    }
    count >= 3
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
        && content.contains("]:")
        && DEFINITION
            .captures(content)
            .is_some_and(|definition| !definition[1].trim().is_empty())
}

fn is_block_tag(name: &str) -> bool {
    if name.is_empty() || name.len() > 16 {
        return false;
    }
    let mut buf = [0u8; 16];
    for (slot, byte) in buf.iter_mut().zip(name.bytes()) {
        *slot = byte.to_ascii_lowercase();
    }
    let lower = std::str::from_utf8(&buf[..name.len()]).unwrap_or("");
    BLOCK_TAGS.binary_search(&lower).is_ok()
}

/// The end condition of an HTML block that starts `rest`. A lone tag of
/// another name cannot interrupt a paragraph.
fn html(rest: &str, interrupts: bool) -> Option<HtmlEnd> {
    if !rest.starts_with('<') {
        return None;
    }
    let after_lt = &rest[1..];
    let ends_name = |name: &str| {
        after_lt.len() >= name.len()
            && after_lt.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes())
            && after_lt
                .as_bytes()
                .get(name.len())
                .is_none_or(|&next| next == b' ' || next == b'>')
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
    if rest.starts_with("<!--") {
        return Some(HtmlEnd::Contains(&["-->"]));
    }
    if rest.starts_with("<?") {
        return Some(HtmlEnd::Contains(&["?>"]));
    }
    if rest.as_bytes().get(1) == Some(&b'!')
        && rest.as_bytes().get(2).is_some_and(u8::is_ascii_alphabetic)
    {
        return Some(HtmlEnd::Contains(&[">"]));
    }
    if rest.len() >= 9 && rest.as_bytes()[..9].eq_ignore_ascii_case(b"<![cdata[") {
        return Some(HtmlEnd::Contains(&["]]>"]));
    }
    let tag = after_lt.strip_prefix('/').unwrap_or(after_lt);
    let name_len = tag
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric())
        .count();
    let name = &tag[..name_len];
    let after = &tag[name_len..];
    if is_block_tag(name)
        && (after.is_empty() || after.starts_with([' ', '>']) || after.starts_with("/>"))
    {
        return Some(HtmlEnd::Blank);
    }
    if interrupts || !rest.trim_end().ends_with('>') {
        return None;
    }
    let lone = LONE_TAG.captures(rest)?;
    let name = lone.get(1).or(lone.get(2))?.as_str();
    let excluded = name.eq_ignore_ascii_case("pre")
        || name.eq_ignore_ascii_case("script")
        || name.eq_ignore_ascii_case("style")
        || name.eq_ignore_ascii_case("textarea");
    (!excluded).then_some(HtmlEnd::Blank)
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    let Some(&first) = needle.first() else {
        return true;
    };
    let len = needle.len();
    if haystack.len() < len {
        return false;
    }
    let first_lower = first.to_ascii_lowercase();
    let first_upper = first.to_ascii_uppercase();
    let limit = haystack.len() - len;
    let mut i = 0;
    while i <= limit {
        let byte = haystack[i];
        if (byte == first_lower || byte == first_upper)
            && haystack[i..i + len].eq_ignore_ascii_case(needle)
        {
            return true;
        }
        i += 1;
    }
    false
}

fn contains(text: &str, ends: &[&str]) -> bool {
    let haystack = text.as_bytes();
    ends.iter()
        .any(|end| contains_ascii_case_insensitive(haystack, end.as_bytes()))
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
        // So is one followed by a blank line, as front matter starts with YAML.
        assert_eq!(
            titles("---\n\n# Project\n\nIntro\n\n---\n\n## Install\n"),
            ["Project", "Install"]
        );
        assert_eq!(titles("---\n---\n# after\n"), ["after"]);
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

    fn blocks(text: &str) -> Vec<(&'static str, String, usize, usize)> {
        let lines: Vec<&str> = text.lines().collect();
        outline_blocks(&lines)
            .into_iter()
            .map(|b| (b.kind.as_str(), b.name, b.first + 1, b.end))
            .collect()
    }

    #[test]
    fn outline_blocks_extracts_frontmatter_keys_and_spans() {
        let doc = concat!(
            "---\n",
            "title: Guide\n",
            "# ignored: comment\n",
            "description: |\n",
            "  indented: ignored\n",
            "tags:\n",
            "  - rust\n",
            "\"og:title\": Social\n",
            "'author': Ada\n",
            "---\n",
            "# Overview\n",
            "Body.\n",
        );
        assert_eq!(
            blocks(doc),
            [
                (
                    "frontmatter",
                    "keys: title, description, tags, og:title, author".to_owned(),
                    1,
                    10
                ),
                ("h1", "Overview".to_owned(), 11, 12),
            ]
        );

        // `...` closes frontmatter, and empty frontmatter falls back to `yaml`.
        assert_eq!(
            blocks("---\n---\n# After\n"),
            [
                ("frontmatter", "yaml".to_owned(), 1, 2),
                ("h1", "After".to_owned(), 3, 3),
            ]
        );
        assert_eq!(
            blocks("---\na: 1\nb: 2\nc: 3\nd: 4\ne: 5\nf: 6\ng: 7\nh: 8\n...\n# Done\n"),
            [
                (
                    "frontmatter",
                    "keys: a, b, c, d, e, f, +2 more".to_owned(),
                    1,
                    10
                ),
                ("h1", "Done".to_owned(), 11, 11),
            ]
        );
        // Thematic break followed by blank line is not frontmatter.
        assert_eq!(
            blocks("---\n\n# Project\n"),
            [("h1", "Project".to_owned(), 3, 3)]
        );
    }

    #[test]
    fn outline_blocks_emits_heading_levels_for_atx_and_setext() {
        let doc = concat!(
            "Setext One\n",
            "==========\n",
            "Intro.\n",
            "\n",
            "Setext Two\n",
            "----------\n",
            "### Three\n",
            "#### Four\n",
            "##### Five\n",
            "###### Six\n",
        );
        assert_eq!(
            blocks(doc),
            [
                ("h1", "Setext One".to_owned(), 1, 10),
                ("h2", "Setext Two".to_owned(), 5, 10),
                ("h3", "Three".to_owned(), 7, 10),
                ("h4", "Four".to_owned(), 8, 10),
                ("h5", "Five".to_owned(), 9, 10),
                ("h6", "Six".to_owned(), 10, 10),
            ]
        );
    }

    #[test]
    fn outline_blocks_emits_large_fenced_code_blocks_at_least_15_lines() {
        let make_fence = |open: &str, close: &str, body_lines: usize| {
            let mut out = format!("{open}\n");
            for i in 1..=body_lines {
                out.push_str(&format!("line_{i}\n"));
            }
            out.push_str(&format!("{close}\n"));
            out
        };
        // 14 total lines (12 body + 2 fences) is omitted; 15 total lines is emitted.
        let doc = format!(
            "# Code\n{}\n{}\n{}\n{}\n{}\n{}\n- item\n  ```rust\n{}\n  ```\n\n> ```yaml\n{}\n> ```\n",
            make_fence("```rust", "```", 12),
            make_fence("```yaml title=\"cfg\"", "```", 13),
            make_fence("~~~rust,no_run", "~~~", 14),
            make_fence("```ts{1,3-5}", "```", 13),
            make_fence("```{.python}", "```", 13),
            make_fence("```", "```", 13),
            (1..=16)
                .map(|i| format!("  let x_{i} = {i};"))
                .collect::<Vec<_>>()
                .join("\n"),
            (1..=16)
                .map(|i| format!("> k{i}: v{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let found = blocks(&doc);
        let codes: Vec<_> = found
            .iter()
            .filter(|(kind, ..)| *kind == "code")
            .map(|(_, name, first, end)| (name.as_str(), end - first + 1))
            .collect();
        assert_eq!(
            codes,
            [
                ("yaml", 15),
                ("rust", 16),
                ("ts", 15),
                ("python", 15),
                ("block", 15),
            ]
        );

        // Unclosed >= 15-line fence at EOF and `{1-3}` highlight-only info string.
        let unclosed = format!(
            "# EOF\n```{{1-3}}\n{}",
            (1..=15).map(|i| format!("x{i}\n")).collect::<String>()
        );
        assert_eq!(
            blocks(&unclosed),
            [
                ("h1", "EOF".to_owned(), 1, 17),
                ("code", "block".to_owned(), 2, 17),
            ]
        );
    }

    #[test]
    fn outline_blocks_emits_large_gfm_tables_at_least_8_rows() {
        let make_rows = |n: usize, piped: bool| {
            (1..=n)
                .map(|i| {
                    if piped {
                        format!("| k{i} | v{i} |\n")
                    } else {
                        format!("k{i} | v{i}\n")
                    }
                })
                .collect::<String>()
        };
        let doc = format!(
            concat!(
                "# Tables\n",
                "| Short | Val |\n",
                "| --- | --- |\n",
                "{short}",
                "\n",
                "| | Flag | Effect |\n",
                "| --- | :--- | ---: |\n",
                "{eight}",
                "## Next Without Blank\n",
                "Col A\\|1 | Col B\n",
                ":---: | ---\n",
                "{nine}",
                "Trailing prose ends table.\n",
                "```md\n",
                "| Inside | Fence |\n",
                "| --- | --- |\n",
                "{nine}",
                "```\n",
                "| C1 | C2 | C3 | C4 | C5 | C6 | C7 |\n",
                "| - | - | - | - | - | - | - |\n",
                "{wide}",
            ),
            short = make_rows(7, true),
            eight = (1..=8)
                .map(|i| format!("| {i} | --f{i} | desc {i} |\n"))
                .collect::<String>(),
            nine = make_rows(9, false),
            wide = (1..=8)
                .map(|i| format!("| {i} | b | c | d | e | f | g |\n"))
                .collect::<String>(),
        );
        let found = blocks(&doc);
        let tables: Vec<_> = found
            .iter()
            .filter(|(kind, ..)| *kind == "table")
            .map(|(_, name, first, end)| (name.as_str(), *first, *end))
            .collect();
        assert_eq!(
            tables,
            [
                ("Flag | Effect (8 rows)", 12, 21),
                ("Col A|1 | Col B (9 rows)", 23, 33),
                ("C1 | C2 | C3 | C4 | C5 | +2 (8 rows)", 48, 57),
            ]
        );
        // The heading immediately following the 8-row table without a blank line is preserved.
        assert!(
            found.iter().any(|(kind, name, first, _)| *kind == "h2"
                && name == "Next Without Blank"
                && *first == 22),
            "{found:?}"
        );
    }

    #[test]
    fn headings_and_sections_stay_unchanged_with_tables_fences_and_frontmatter() {
        let mut doc = String::from("---\ntitle: Doc\n---\n# One\n| A | B |\n| --- | --- |\n");
        for i in 1..=9 {
            doc.push_str(&format!("| {i} | x |\n"));
        }
        doc.push_str("## Two\n```rust\n");
        for i in 1..=15 {
            doc.push_str(&format!("let v_{i} = {i};\n"));
        }
        doc.push_str("```\n");
        let lines: Vec<&str> = doc.lines().collect();
        assert_eq!(
            headings(&lines)
                .into_iter()
                .map(|h| (h.level, h.title, h.first + 1))
                .collect::<Vec<_>>(),
            [(1, "One".to_owned(), 4), (2, "Two".to_owned(), 16)]
        );
        assert_eq!(
            sections(&lines)
                .into_iter()
                .map(|s| (s.heading.title, s.heading.first + 1, s.end))
                .collect::<Vec<_>>(),
            [("One".to_owned(), 4, 33), ("Two".to_owned(), 16, 33)]
        );
    }

    #[test]
    fn outline_blocks_preserve_exact_byte_spans_for_frontmatter_fences_and_tables() {
        let mut doc = String::from(concat!(
            "\u{feff}---\n",
            "title: Fast Path\n",
            "\"schema:v1\": true\n",
            "---\n",
            "# Spec\n",
            "```go\n",
        ));
        for i in 1..=13 {
            // Tabs inside a top-level fence must not close it or corrupt its byte span.
            doc.push_str(&format!("\tfmt.Println({i})\n"));
        }
        doc.push_str("```\t\n");
        doc.push_str("| Key\\|Alias | Value |\n| :--- | ---: |\n");
        for i in 1..=8 {
            doc.push_str(&format!("| k{i} | v{i} |\n"));
        }
        doc.push_str("## After\nTrailing text.\n");

        // Reconstruct exact line start byte offsets (matching `source::Snapshot`).
        let mut starts = vec![0usize];
        for (idx, byte) in doc.bytes().enumerate() {
            if byte == b'\n' && idx + 1 < doc.len() {
                starts.push(idx + 1);
            }
        }
        let offset = |line: usize| starts.get(line).copied().unwrap_or(doc.len());
        let lines: Vec<&str> = (0..starts.len())
            .map(|line| {
                let raw = &doc[offset(line)..offset(line + 1)];
                raw.strip_suffix('\n').unwrap_or(raw)
            })
            .collect();

        let blocks = outline_blocks(&lines);
        assert_eq!(blocks.len(), 5, "{blocks:?}");

        let slice_of = |b: &OutlineBlock| &doc[offset(b.first)..offset(b.end)];

        assert_eq!(blocks[0].kind, BlockKind::FrontMatter);
        assert_eq!(blocks[0].name, "keys: title, schema:v1");
        assert_eq!(
            slice_of(&blocks[0]),
            "\u{feff}---\ntitle: Fast Path\n\"schema:v1\": true\n---\n"
        );

        assert_eq!(blocks[1].kind, BlockKind::Heading(1));
        assert_eq!(blocks[1].name, "Spec");
        assert!(slice_of(&blocks[1]).starts_with("# Spec\n```go\n"));
        assert!(slice_of(&blocks[1]).ends_with("Trailing text.\n"));

        assert_eq!(blocks[2].kind, BlockKind::Code);
        assert_eq!(blocks[2].name, "go");
        assert!(slice_of(&blocks[2]).starts_with("```go\n\tfmt.Println(1)\n"));
        assert!(slice_of(&blocks[2]).ends_with("```\t\n"));

        assert_eq!(blocks[3].kind, BlockKind::Table);
        assert_eq!(blocks[3].name, "Key|Alias | Value (8 rows)");
        assert!(slice_of(&blocks[3]).starts_with("| Key\\|Alias | Value |\n| :--- | ---: |\n"));
        assert!(slice_of(&blocks[3]).ends_with("| k8 | v8 |\n"));

        assert_eq!(blocks[4].kind, BlockKind::Heading(2));
        assert_eq!(blocks[4].name, "After");
        assert_eq!(slice_of(&blocks[4]), "## After\nTrailing text.\n");
    }

    #[test]
    fn contains_ascii_case_insensitive_handles_uppercase_needles() {
        assert!(contains_ascii_case_insensitive(
            b"closing </pre> tag",
            b"</PRE>"
        ));
        assert!(contains_ascii_case_insensitive(b"hello world", b"WORLD"));
        assert!(contains_ascii_case_insensitive(b"HELLO WORLD", b"world"));
        assert!(!contains_ascii_case_insensitive(b"hello", b"WORLD"));
    }
}
