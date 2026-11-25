//! Structural outlines for TOML, YAML, and JSON/JSONC files.
//!
//! Each section carries exact byte ranges in the original text so `expand`
//! reproduces the exact source bytes and `edit` replaces verified sections.
//! Files without meaningful multi-line sections (such as minified JSON or flat
//! key-value TOML without table headers) return an empty list so `Snapshot`
//! falls back to normal text pages.

use std::ops::Range;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub kind: &'static str,
    pub bytes: Range<usize>,
    pub signature_start: usize,
    pub header_end: usize,
}

/// Extract structural sections for `.toml`, `.yaml`, `.yml`, `.json`, and
/// `.jsonc` files. `line_starts` holds the 0-based byte offset of each line.
pub fn sections(file: &str, text: &str, line_starts: &[usize]) -> Vec<Section> {
    let Some(ext) = Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return Vec::new();
    };
    match ext.as_str() {
        "toml" => toml_sections(text, line_starts),
        "yaml" | "yml" => yaml_sections(text, line_starts),
        "json" | "jsonc" => json_sections(text, line_starts),
        _ => Vec::new(),
    }
}

fn line_offset(line_starts: &[usize], text_len: usize, line: usize) -> usize {
    line_starts.get(line).copied().unwrap_or(text_len)
}

fn line_text<'a>(text: &'a str, line_starts: &[usize], line: usize) -> &'a str {
    let start = line_offset(line_starts, text.len(), line);
    let end = line_offset(line_starts, text.len(), line + 1);
    text[start..end]
        .strip_suffix('\n')
        .unwrap_or(&text[start..end])
        .strip_suffix('\r')
        .unwrap_or_else(|| text[start..end].trim_end_matches(['\n', '\r']))
}

fn line_of(line_starts: &[usize], offset: usize) -> usize {
    line_starts
        .partition_point(|&start| start <= offset)
        .saturating_sub(1)
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

// ---------------------------------------------------------------------------
// TOML
// ---------------------------------------------------------------------------

fn toml_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    if toml_edit::ImDocument::parse(text).is_err() {
        return Vec::new();
    }
    let mut headers: Vec<(usize, String, bool)> = Vec::new();
    let mut multiline: Option<&'static str> = None;

    for idx in 0..line_starts.len() {
        let line = line_text(text, line_starts, idx);
        if multiline.is_none() {
            let clean = strip_toml_comment(line).trim();
            if let Some(inner) = clean.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
                let name = inner.trim();
                if !name.is_empty() {
                    headers.push((idx, name.to_owned(), true));
                }
            } else if let Some(inner) = clean.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let name = inner.trim();
                if !name.is_empty() && !name.starts_with('[') && !name.ends_with(']') {
                    headers.push((idx, name.to_owned(), false));
                }
            }
        }
        advance_toml_multiline(line, &mut multiline);
    }

    if headers.len() < 2 {
        return Vec::new();
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::with_capacity(headers.len() + 1);

    // Keep top-level keys before the first table header visible.
    if headers[0].0 > 0
        && let Some(first_key) = (0..headers[0].0).find(|&i| {
            let t = line_text(text, line_starts, i).trim();
            !t.is_empty() && !t.starts_with('#')
        })
    {
        out.push(Section {
            name: "(root)".into(),
            kind: "section",
            bytes: offset(0)..offset(headers[0].0),
            signature_start: offset(first_key),
            header_end: offset(first_key + 1),
        });
    }

    for (pos, (first, raw_name, is_array)) in headers.iter().enumerate() {
        let end_line = headers.get(pos + 1).map_or(line_starts.len(), |h| h.0);
        let name = if *is_array {
            toml_array_label(text, line_starts, *first + 1, end_line)
                .map(|label| format!("{raw_name}:{label}"))
                .unwrap_or_else(|| raw_name.clone())
        } else {
            raw_name.clone()
        };
        out.push(Section {
            name,
            kind: "table",
            bytes: offset(*first)..offset(end_line),
            signature_start: offset(*first),
            header_end: offset(*first + 1),
        });
    }
    out
}

fn toml_array_label(text: &str, line_starts: &[usize], start: usize, end: usize) -> Option<String> {
    for idx in start..end.min(start + 12) {
        let line = strip_toml_comment(line_text(text, line_starts, idx)).trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if matches!(k.trim(), "name" | "id") {
            let label = unquote(v);
            if !label.is_empty() && !label.contains('\n') {
                return Some(label.to_owned());
            }
        }
    }
    None
}

fn strip_toml_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(b'"') => {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    quote = None;
                }
            }
            Some(b'\'') => {
                if b == b'\'' {
                    quote = None;
                }
            }
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'#' => return &line[..i],
                _ => {}
            },
        }
        i += 1;
    }
    line
}

fn advance_toml_multiline(line: &str, state: &mut Option<&'static str>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match *state {
            Some("\"\"\"") => {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i..].starts_with(b"\"\"\"") {
                    *state = None;
                    i += 3;
                    continue;
                }
                i += 1;
            }
            Some("'''") => {
                if bytes[i..].starts_with(b"'''") {
                    *state = None;
                    i += 3;
                    continue;
                }
                i += 1;
            }
            Some(_) => i += 1,
            None => match bytes[i] {
                b'#' => break,
                b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                    *state = Some("\"\"\"");
                    i += 3;
                }
                b'\'' if bytes[i..].starts_with(b"'''") => {
                    *state = Some("'''");
                    i += 3;
                }
                b'"' => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == b'\\' {
                            i += 2;
                        } else if bytes[i] == b'"' {
                            i += 1;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                }
                b'\'' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                _ => i += 1,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// YAML
// ---------------------------------------------------------------------------

fn yaml_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    if line_starts.len() < 2 {
        return Vec::new();
    }
    let docs = split_yaml_docs(text, line_starts);
    if docs.len() >= 2 {
        return yaml_multi_doc_sections(text, line_starts, &docs);
    }
    let (start, end) = docs.first().copied().unwrap_or((0, line_starts.len()));
    yaml_single_doc_sections(text, line_starts, start, end, None)
}

fn split_yaml_docs(text: &str, line_starts: &[usize]) -> Vec<(usize, usize)> {
    let mut markers = Vec::new();
    let mut block_indent: Option<usize> = None;

    for idx in 0..line_starts.len() {
        let line = line_text(text, line_starts, idx);
        let trimmed = line.trim();
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if let Some(parent) = block_indent {
            if !trimmed.is_empty() && indent <= parent {
                block_indent = None;
            } else {
                continue;
            }
        }
        if indent == 0 && (trimmed == "---" || trimmed.starts_with("--- ")) {
            markers.push(idx);
            continue;
        }
        if yaml_opens_block_scalar(trimmed) {
            block_indent = Some(indent);
        }
    }

    if markers.is_empty() {
        return vec![(0, line_starts.len())];
    }

    let has_content = |a: usize, b: usize| {
        (a..b).any(|i| {
            let t = line_text(text, line_starts, i).trim();
            !t.is_empty() && !t.starts_with('#') && t != "---" && t != "..."
        })
    };

    let mut docs = Vec::new();
    if markers[0] > 0 && has_content(0, markers[0]) {
        docs.push((0, markers[0]));
    }
    for (pos, &start) in markers.iter().enumerate() {
        let end = markers.get(pos + 1).copied().unwrap_or(line_starts.len());
        if has_content(start, end) {
            docs.push((start, end));
        }
    }
    docs
}

fn yaml_multi_doc_sections(
    text: &str,
    line_starts: &[usize],
    docs: &[(usize, usize)],
) -> Vec<Section> {
    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::new();
    for (idx, &(start, end)) in docs.iter().enumerate() {
        let label = yaml_doc_label(text, line_starts, start, end)
            .unwrap_or_else(|| format!("doc-{}", idx + 1));
        let sig_line = (start..end)
            .find(|&i| {
                let t = line_text(text, line_starts, i).trim();
                !t.is_empty() && !t.starts_with('#') && t != "---"
            })
            .unwrap_or(start);
        out.push(Section {
            name: label.clone(),
            kind: "document",
            bytes: offset(start)..offset(end),
            signature_start: offset(sig_line),
            header_end: offset(sig_line + 1),
        });
        if end.saturating_sub(start) >= 20 {
            let children = yaml_single_doc_sections(text, line_starts, start, end, Some(&label));
            for child in children {
                let span = line_of(line_starts, child.bytes.end.saturating_sub(1))
                    .saturating_sub(line_of(line_starts, child.bytes.start))
                    + 1;
                if span >= 4 {
                    out.push(child);
                }
            }
        }
    }
    out
}

fn yaml_doc_label(text: &str, line_starts: &[usize], start: usize, end: usize) -> Option<String> {
    let mut kind: Option<String> = None;
    let mut meta_name: Option<String> = None;
    let mut top_name: Option<String> = None;
    let mut in_metadata = false;
    let mut block_indent: Option<usize> = None;

    for idx in start..end {
        let line = line_text(text, line_starts, idx);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" || trimmed == "..." {
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if let Some(parent) = block_indent {
            if indent <= parent {
                block_indent = None;
            } else {
                continue;
            }
        }
        if indent == 0 {
            in_metadata = false;
            if let Some((k, v)) = yaml_mapping_key(trimmed) {
                match k {
                    "kind" if !v.is_empty() => kind = Some(unquote(v).to_owned()),
                    "name" if !v.is_empty() => top_name = Some(unquote(v).to_owned()),
                    "metadata" => in_metadata = true,
                    _ => {}
                }
            }
        } else if in_metadata
            && indent == 2
            && let Some(("name", v)) = yaml_mapping_key(trimmed)
            && !v.is_empty()
        {
            meta_name = Some(unquote(v).to_owned());
        }
        if yaml_opens_block_scalar(trimmed) {
            block_indent = Some(indent);
        }
    }
    match (kind, meta_name, top_name) {
        (Some(k), Some(n), _) => Some(format!("{k}/{n}")),
        (Some(k), None, _) => Some(k),
        (None, _, Some(n)) => Some(n),
        _ => None,
    }
}

fn yaml_single_doc_sections(
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
    prefix: Option<&str>,
) -> Vec<Section> {
    let top_keys = yaml_keys_at_indent(text, line_starts, start, end, 0);
    if top_keys.is_empty() {
        return Vec::new();
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::new();

    for (pos, (first, key)) in top_keys.iter().enumerate() {
        let next = top_keys.get(pos + 1).map_or(end, |k| k.0);
        let full_name = match prefix {
            Some(p) => format!("{p}.{key}"),
            None => key.clone(),
        };
        out.push(Section {
            name: full_name.clone(),
            kind: if prefix.is_some() { "section" } else { "key" },
            bytes: offset(*first)..offset(next),
            signature_start: offset(*first),
            header_end: offset(*first + 1),
        });

        // Expand nested mapping sections (such as GitHub Actions `jobs.*`,
        // docker-compose `services.*`, or OpenAPI `paths.*`) when they hold
        // real multi-line subsections rather than a flat scalar map.
        if prefix.is_none()
            && next.saturating_sub(*first) >= 10
            && let Some(child_indent) = yaml_first_child_indent(text, line_starts, *first + 1, next)
        {
            let children = yaml_keys_at_indent(text, line_starts, *first + 1, next, child_indent);
            if should_expand_children(children.len(), next, &children) {
                for (cpos, (cfirst, ckey)) in children.iter().enumerate() {
                    let cnext = children.get(cpos + 1).map_or(next, |c| c.0);
                    out.push(Section {
                        name: format!("{full_name}.{ckey}"),
                        kind: "section",
                        bytes: offset(*cfirst)..offset(cnext),
                        signature_start: offset(*cfirst),
                        header_end: offset(*cfirst + 1),
                    });
                }
            }
        }
    }

    if prefix.is_none() && out.len() < 2 {
        return Vec::new();
    }
    out
}

fn should_expand_children(count: usize, parent_end: usize, children: &[(usize, String)]) -> bool {
    if !(2..=64).contains(&count) {
        return false;
    }
    let mut multiline = 0;
    let mut any_large = false;
    for (pos, (first, _)) in children.iter().enumerate() {
        let next = children.get(pos + 1).map_or(parent_end, |c| c.0);
        let span = next.saturating_sub(*first);
        if span >= 2 {
            multiline += 1;
        }
        if span >= 4 {
            any_large = true;
        }
    }
    multiline >= 2 || any_large
}

fn yaml_first_child_indent(
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
) -> Option<usize> {
    for idx in start..end {
        let line = line_text(text, line_starts, idx);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        return (indent > 0 && !trimmed.starts_with("- ")).then_some(indent);
    }
    None
}

fn yaml_keys_at_indent(
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
    target_indent: usize,
) -> Vec<(usize, String)> {
    let mut keys = Vec::new();
    let mut block_indent: Option<usize> = None;

    for idx in start..end {
        let line = line_text(text, line_starts, idx);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" || trimmed == "..." {
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if let Some(parent) = block_indent {
            if indent <= parent {
                block_indent = None;
            } else {
                continue;
            }
        }
        if indent == target_indent
            && let Some((key, _)) = yaml_mapping_key(trimmed)
        {
            keys.push((idx, key.to_owned()));
        }
        if yaml_opens_block_scalar(trimmed) {
            block_indent = Some(indent);
        }
    }
    keys
}

fn yaml_mapping_key(trimmed: &str) -> Option<(&str, &str)> {
    if trimmed.starts_with(['#', '-', '?', '{', '[']) || trimmed == "---" || trimmed == "..." {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for (idx, &b) in bytes.iter().enumerate() {
        match quote {
            Some(b'"') => {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    quote = None;
                }
            }
            Some(b'\'') => {
                if b == b'\'' {
                    quote = None;
                }
            }
            Some(_) => {}
            None => match b {
                b'"' | b'\'' if idx == 0 => quote = Some(b),
                b':' => {
                    let next = bytes.get(idx + 1).copied();
                    if matches!(next, None | Some(b' ' | b'\t')) {
                        let key = unquote(&trimmed[..idx]);
                        if key.is_empty() {
                            return None;
                        }
                        let rest = trimmed[idx + 1..].trim();
                        return Some((key, rest));
                    }
                }
                _ => {}
            },
        }
    }
    None
}

fn yaml_opens_block_scalar(trimmed: &str) -> bool {
    let Some((_, rest)) = yaml_mapping_key(trimmed) else {
        return false;
    };
    let rest = rest.split_once(" #").map_or(rest, |(head, _)| head).trim();
    let Some(tail) = rest.strip_prefix(['|', '>']) else {
        return false;
    };
    tail.len() <= 2 && tail.bytes().all(|b| matches!(b, b'+' | b'-' | b'1'..=b'9'))
}

// ---------------------------------------------------------------------------
// JSON / JSONC
// ---------------------------------------------------------------------------

struct JsonMember {
    name: String,
    key_start: usize,
    val_start: usize,
    val_end: usize,
    children: Vec<JsonMember>,
}

fn json_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    let mut parser = JsonScanner::new(text);
    let Some(members) = parser.parse_root_object() else {
        return Vec::new();
    };
    if members.is_empty() {
        return Vec::new();
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let distinct_top_lines: std::collections::BTreeSet<usize> = members
        .iter()
        .map(|m| line_of(line_starts, m.key_start))
        .collect();
    if distinct_top_lines.len() < 2
        && !(members.len() == 1 && json_should_expand(&members[0], line_starts))
    {
        return Vec::new();
    }

    let mut out = Vec::new();
    for (idx, member) in members.iter().enumerate() {
        let first_line = line_of(line_starts, member.key_start);
        let last_line = line_of(line_starts, member.val_end.saturating_sub(1));
        let prev_ends_here = idx > 0
            && line_of(line_starts, members[idx - 1].val_end.saturating_sub(1)) == first_line;
        let next_starts_here = members
            .get(idx + 1)
            .is_some_and(|next| line_of(line_starts, next.key_start) == last_line);

        let bytes = if !prev_ends_here && !next_starts_here {
            offset(first_line)..offset(last_line + 1)
        } else {
            member.key_start..member.val_end
        };
        let header_end = if first_line == last_line && member.val_end - member.key_start > 80 {
            member.val_start.min(bytes.end)
        } else {
            offset(first_line + 1).min(bytes.end)
        };
        out.push(Section {
            name: member.name.clone(),
            kind: "key",
            bytes,
            signature_start: member.key_start,
            header_end,
        });

        if json_should_expand(member, line_starts) {
            for child in &member.children {
                let cfirst = line_of(line_starts, child.key_start);
                let clast = line_of(line_starts, child.val_end.saturating_sub(1));
                let cbytes = offset(cfirst)..offset(clast + 1);
                let cheader = if cfirst == clast && child.val_end - child.key_start > 80 {
                    child.val_start.min(cbytes.end)
                } else {
                    offset(cfirst + 1).min(cbytes.end)
                };
                out.push(Section {
                    name: format!("{}.{}", member.name, child.name),
                    kind: "section",
                    bytes: cbytes,
                    signature_start: child.key_start,
                    header_end: cheader,
                });
            }
        }
    }
    if out.len() < 2 {
        return Vec::new();
    }
    out
}

fn json_should_expand(member: &JsonMember, line_starts: &[usize]) -> bool {
    let count = member.children.len();
    if !(2..=64).contains(&count) {
        return false;
    }
    let mut prev_end = None;
    let mut multiline = 0;
    let mut any_large = false;
    for child in &member.children {
        let first = line_of(line_starts, child.key_start);
        let last = line_of(line_starts, child.val_end.saturating_sub(1));
        if prev_end.is_some_and(|prev| prev >= first) {
            return false;
        }
        prev_end = Some(last);
        let span = last - first + 1;
        if span >= 2 {
            multiline += 1;
        }
        if span >= 4 {
            any_large = true;
        }
    }
    multiline >= 2 || any_large
}

struct JsonScanner<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonScanner<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    fn parse_root_object(&mut self) -> Option<Vec<JsonMember>> {
        if self.src.starts_with('\u{feff}') {
            self.pos = '\u{feff}'.len_utf8();
        }
        self.skip_ws_and_comments()?;
        if self.peek() != Some(b'{') {
            return None;
        }
        let members = self.parse_object(0)?;
        self.skip_ws_and_comments()?;
        (self.pos == self.bytes.len()).then_some(members)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws_and_comments(&mut self) -> Option<()> {
        loop {
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            if self.bytes[self.pos..].starts_with(b"//") {
                self.pos += 2;
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            if self.bytes[self.pos..].starts_with(b"/*") {
                self.pos += 2;
                let end = self.src[self.pos..].find("*/")?;
                self.pos += end + 2;
                continue;
            }
            return Some(());
        }
    }

    fn parse_object(&mut self, depth: usize) -> Option<Vec<JsonMember>> {
        if depth > 128 || self.peek() != Some(b'{') {
            return None;
        }
        self.pos += 1;
        let mut members = Vec::new();
        loop {
            self.skip_ws_and_comments()?;
            if self.peek() == Some(b'}') {
                self.pos += 1;
                return Some(members);
            }
            let key_start = self.pos;
            let name = self.parse_string()?;
            self.skip_ws_and_comments()?;
            if self.peek() != Some(b':') {
                return None;
            }
            self.pos += 1;
            self.skip_ws_and_comments()?;
            let val_start = self.pos;
            let children = if depth == 0 && self.peek() == Some(b'{') {
                self.parse_object(depth + 1)?
            } else {
                self.skip_value(depth + 1)?;
                Vec::new()
            };
            let val_end = self.pos;
            if depth <= 1 {
                members.push(JsonMember {
                    name,
                    key_start,
                    val_start,
                    val_end,
                    children,
                });
            }
            self.skip_ws_and_comments()?;
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Some(members);
                }
                _ => return None,
            }
        }
    }

    fn skip_value(&mut self, depth: usize) -> Option<()> {
        if depth > 128 {
            return None;
        }
        self.skip_ws_and_comments()?;
        match self.peek()? {
            b'{' => {
                self.parse_object(depth)?;
                Some(())
            }
            b'[' => {
                self.pos += 1;
                loop {
                    self.skip_ws_and_comments()?;
                    if self.peek() == Some(b']') {
                        self.pos += 1;
                        return Some(());
                    }
                    self.skip_value(depth + 1)?;
                    self.skip_ws_and_comments()?;
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => {
                self.skip_string()?;
                Some(())
            }
            b't' if self.bytes[self.pos..].starts_with(b"true") => {
                self.pos += 4;
                Some(())
            }
            b'f' if self.bytes[self.pos..].starts_with(b"false") => {
                self.pos += 5;
                Some(())
            }
            b'n' if self.bytes[self.pos..].starts_with(b"null") => {
                self.pos += 4;
                Some(())
            }
            b'-' | b'0'..=b'9' => {
                let start = self.pos;
                if self.peek() == Some(b'-') {
                    self.pos += 1;
                }
                let digits_start = self.pos;
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.pos += 1;
                }
                if self.pos == digits_start {
                    return None;
                }
                if self.peek() == Some(b'.') {
                    self.pos += 1;
                    let frac = self.pos;
                    while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                        self.pos += 1;
                    }
                    if self.pos == frac {
                        return None;
                    }
                }
                if matches!(self.peek(), Some(b'e' | b'E')) {
                    self.pos += 1;
                    if matches!(self.peek(), Some(b'+' | b'-')) {
                        self.pos += 1;
                    }
                    let exp = self.pos;
                    while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                        self.pos += 1;
                    }
                    if self.pos == exp {
                        return None;
                    }
                }
                (self.pos > start).then_some(())
            }
            _ => None,
        }
    }

    fn skip_string(&mut self) -> Option<Range<usize>> {
        if self.peek() != Some(b'"') {
            return None;
        }
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b'\\' => {
                    self.pos += 1;
                    if self.pos >= self.bytes.len() {
                        return None;
                    }
                    self.pos += 1;
                }
                b'"' => {
                    self.pos += 1;
                    return Some(start..self.pos);
                }
                b'\n' | b'\r' => return None,
                _ => self.pos += 1,
            }
        }
        None
    }

    fn parse_string(&mut self) -> Option<String> {
        let range = self.skip_string()?;
        let raw = &self.src[range];
        serde_json::from_str::<String>(raw).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_starts(text: &str) -> Vec<usize> {
        let mut lines = vec![0];
        lines.extend(
            text.match_indices('\n')
                .map(|(i, _)| i + 1)
                .filter(|i| *i < text.len()),
        );
        lines
    }

    #[test]
    fn toml_sections_outline_tables_arrays_and_skip_multiline_strings() {
        let text = concat!(
            "# comment\n",
            "edition = \"2024\"\n\n",
            "[package]\n",
            "name = \"demo\"\n",
            "description = \"\"\"\n",
            "[not_a_table]\n",
            "\"\"\"\n\n",
            "[dependencies]\n",
            "serde = \"1\"\n\n",
            "[[bin]]\n",
            "name = \"app\"\n",
            "path = \"src/main.rs\"\n",
        );
        let found = sections("Cargo.toml", text, &line_starts(text));
        let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert_eq!(
            names,
            [
                ("section", "(root)"),
                ("table", "package"),
                ("table", "dependencies"),
                ("table", "bin:app"),
            ]
        );
        assert!(text[found[1].bytes.clone()].starts_with("[package]\n"));
        assert!(text[found[1].bytes.clone()].contains("[not_a_table]"));
        assert_eq!(
            &text[found[3].bytes.clone()],
            "[[bin]]\nname = \"app\"\npath = \"src/main.rs\"\n"
        );

        // Flat TOML with no tables stays empty so normal text/pages apply.
        let flat = "a = 1\nb = 2\n";
        assert!(sections("notes.toml", flat, &line_starts(flat)).is_empty());
        // Invalid TOML fails closed.
        let broken = "[package\nname = \"x\"\n[deps]\n";
        assert!(sections("Cargo.toml", broken, &line_starts(broken)).is_empty());
    }

    #[test]
    fn yaml_sections_expand_nested_blocks_handle_block_scalars_and_multi_doc() {
        let ci = concat!(
            "name: CI\n",
            "on:\n",
            "  push:\n",
            "    branches: [main]\n",
            "jobs:\n",
            "  lint:\n",
            "    runs-on: ubuntu-latest\n",
            "    steps:\n",
            "      - run: |\n",
            "          echo \"fake_key: not a section\"\n",
            "          ---\n",
            "          cargo clippy\n",
            "  test:\n",
            "    runs-on: ubuntu-latest\n",
            "    steps:\n",
            "      - run: cargo test\n",
        );
        let found = sections(".github/workflows/ci.yml", ci, &line_starts(ci));
        let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert_eq!(
            names,
            [
                ("key", "name"),
                ("key", "on"),
                ("key", "jobs"),
                ("section", "jobs.lint"),
                ("section", "jobs.test"),
            ]
        );
        assert!(ci[found[3].bytes.clone()].contains("cargo clippy"));
        assert!(!ci[found[3].bytes.clone()].contains("cargo test"));
        assert!(ci[found[4].bytes.clone()].starts_with("  test:\n"));

        // Multi-document Kubernetes manifest labels each resource by kind/name.
        let k8s = concat!(
            "---\n",
            "apiVersion: apps/v1\n",
            "kind: Deployment\n",
            "metadata:\n",
            "  name: web\n",
            "spec:\n",
            "  replicas: 3\n",
            "---\n",
            "apiVersion: v1\n",
            "kind: Service\n",
            "metadata:\n",
            "  name: web-svc\n",
        );
        let docs = sections("k8s.yaml", k8s, &line_starts(k8s));
        let doc_names: Vec<_> = docs.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert_eq!(
            doc_names,
            [
                ("document", "Deployment/web"),
                ("document", "Service/web-svc")
            ]
        );
    }

    #[test]
    fn json_and_jsonc_sections_outline_keys_and_expand_nested_objects_only_when_multiline() {
        let pkg = r#"{
  // JSONC comment supported for tsconfig/settings
  "name": "demo",
  "dependencies": {
    "a": "1.0.0",
    "b": "2.0.0",
    "c": "3.0.0",
  },
  "targets": {
    "build": {
      "executor": "vite:build",
      "options": {}
    },
    "test": {
      "executor": "vitest:run",
      "options": {}
    }
  }
}
"#;
        let found = sections("project.json", pkg, &line_starts(pkg));
        let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
        // Flat 1-line `dependencies` stays one row; multi-line `targets` expands.
        assert_eq!(
            names,
            [
                ("key", "name"),
                ("key", "dependencies"),
                ("key", "targets"),
                ("section", "targets.build"),
                ("section", "targets.test"),
            ]
        );
        assert!(pkg[found[3].bytes.clone()].contains("vite:build"));
        assert!(pkg[found[4].bytes.clone()].contains("vitest:run"));

        // Minified 1-line JSON and broken JSON stay empty.
        let minified = "{\"a\":1,\"b\":2}\n";
        assert!(sections("min.json", minified, &line_starts(minified)).is_empty());
        let broken = "{\n  \"a\": 1,\n  \"b\": [\n";
        assert!(sections("bad.json", broken, &line_starts(broken)).is_empty());
    }
}
