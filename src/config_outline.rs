//! Structural outlines for config, DevOps, stylesheet, IDL, proxy, and markup files:
//! TOML, YAML (including GitHub Actions workflows and OpenAPI/Swagger specs),
//! JSON/JSONC (including OpenAPI/Swagger, root arrays, and large minified JSON),
//! Nix (`*.nix`), Makefile/Justfile, Dockerfile/Containerfile,
//! Nginx/Caddy/HAProxy configs, INI/Conf/EditorConfig/Properties,
//! CSS/SCSS/Less, Proto/GraphQL/Prisma IDL schemas, and XML/HTML/MSBuild files.
//!
//! Each section carries exact byte ranges in the original text so `expand`
//! reproduces the exact source bytes and `edit` replaces verified sections.
//! Malformed, unbalanced, or trivial files with fewer than two sections return
//! an empty list so `Snapshot` falls back to normal text pages.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

/// Byte threshold above which single-line/minified JSON, CSS, or XML files
/// still produce structural sections using exact byte spans.
const MINIFIED_THRESHOLD: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub kind: &'static str,
    pub bytes: Range<usize>,
    pub signature_start: usize,
    pub header_end: usize,
}

/// Extract structural sections for supported config, build, schema, stylesheet,
/// proxy, and markup files. `line_starts` holds the 0-based byte offset of each line.
pub fn sections(file: &str, text: &str, line_starts: &[usize]) -> Vec<Section> {
    let path = Path::new(file);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let lower_name = file_name.to_ascii_lowercase();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);

    if matches!(lower_name.as_str(), "makefile" | "gnumakefile")
        || lower_name.starts_with("makefile.")
        || ext.as_deref() == Some("mk")
    {
        return makefile_sections(text, line_starts);
    }
    if matches!(lower_name.as_str(), "justfile" | ".justfile") || ext.as_deref() == Some("just") {
        return justfile_sections(text, line_starts);
    }
    if matches!(lower_name.as_str(), "dockerfile" | "containerfile")
        || lower_name.starts_with("dockerfile.")
        || lower_name.starts_with("containerfile.")
        || matches!(ext.as_deref(), Some("dockerfile" | "containerfile"))
    {
        return dockerfile_sections(text, line_starts);
    }
    if lower_name == "caddyfile"
        || lower_name.starts_with("caddyfile.")
        || matches!(ext.as_deref(), Some("caddyfile" | "caddy"))
    {
        return caddy_sections(text, line_starts);
    }
    if lower_name == "haproxy.cfg"
        || (lower_name.starts_with("haproxy.") && matches!(ext.as_deref(), Some("cfg" | "conf")))
        || ext.as_deref() == Some("haproxy")
    {
        return haproxy_sections(text, line_starts, true);
    }
    if lower_name == "nginx.conf"
        || (lower_name.starts_with("nginx.") && ext.as_deref() == Some("conf"))
        || ext.as_deref() == Some("nginx")
    {
        return nginx_sections(text, line_starts);
    }
    if lower_name == ".editorconfig" {
        return ini_sections(text, line_starts);
    }

    match ext.as_deref() {
        Some("toml") => toml_sections(text, line_starts),
        Some("yaml" | "yml") => yaml_sections(file, text, line_starts),
        Some("json" | "jsonc") => json_sections(text, line_starts),
        Some("nix") => nix_sections(text, line_starts),
        Some("conf") => {
            let ini = ini_sections(text, line_starts);
            if !ini.is_empty() {
                ini
            } else {
                nginx_sections(text, line_starts)
            }
        }
        Some("cfg") => {
            let ini = ini_sections(text, line_starts);
            if !ini.is_empty() {
                ini
            } else {
                haproxy_sections(text, line_starts, false)
            }
        }
        Some("ini" | "properties") => ini_sections(text, line_starts),
        Some("css") => css_sections(text, line_starts, false),
        Some("scss" | "less") => css_sections(text, line_starts, true),
        Some("proto") => idl_sections(text, line_starts, IdlKind::Proto),
        Some("graphql" | "gql") => idl_sections(text, line_starts, IdlKind::GraphQl),
        Some("prisma") => idl_sections(text, line_starts, IdlKind::Prisma),
        Some("xml" | "csproj" | "fsproj" | "props" | "targets") => {
            xml_sections(text, line_starts, false)
        }
        Some("html" | "htm") => xml_sections(text, line_starts, true),
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

fn char_floor(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn compact_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip_label(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    if max < 4 {
        return String::new();
    }
    let end = char_floor(text, max - 3);
    format!("{}…", text[..end].trim_end())
}

/// Expand `start..end` to full lines when only whitespace precedes `start` on
/// its line and only whitespace follows `end` on its line; otherwise return the
/// exact byte range `start..end`.
fn whole_line_or_exact(
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
) -> Range<usize> {
    let first_line = line_of(line_starts, start);
    let last_line = line_of(line_starts, end.saturating_sub(1));
    let line_start = line_offset(line_starts, text.len(), first_line);
    let line_end = line_offset(line_starts, text.len(), last_line + 1);
    if text[line_start..start].trim().is_empty() && text[end..line_end].trim().is_empty() {
        line_start..line_end
    } else {
        start..end
    }
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
        let (name, header_end) = if *is_array {
            let label = toml_array_label(text, line_starts, *first + 1, end_line)
                .map(|label| format!("{raw_name}:{label}"))
                .unwrap_or_else(|| raw_name.clone());
            (label, offset(*first + 1))
        } else {
            // `[table]` header text already equals `name`; leave signature empty to avoid duplication.
            (raw_name.clone(), offset(*first))
        };
        out.push(Section {
            name,
            kind: "table",
            bytes: offset(*first)..offset(end_line),
            signature_start: offset(*first),
            header_end,
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

fn yaml_sections(file: &str, text: &str, line_starts: &[usize]) -> Vec<Section> {
    if line_starts.len() < 2 {
        return Vec::new();
    }
    let docs = split_yaml_docs(text, line_starts);
    if docs.len() >= 2 {
        return yaml_multi_doc_sections(text, line_starts, &docs);
    }
    let (start, end) = docs.first().copied().unwrap_or((0, line_starts.len()));
    if let Some(openapi) = openapi_yaml_sections(text, line_starts, start, end) {
        return openapi;
    }
    if is_github_workflow_path(file)
        && let Some(workflow) = github_actions_yaml_sections(text, line_starts, start, end)
    {
        return workflow;
    }
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
        let has_inline_value = yaml_mapping_key(line_text(text, line_starts, *first).trim())
            .is_some_and(|(_, rest)| !rest.is_empty());
        out.push(Section {
            name: full_name.clone(),
            kind: if prefix.is_some() { "section" } else { "key" },
            bytes: offset(*first)..offset(next),
            signature_start: offset(*first),
            header_end: if has_inline_value {
                offset(*first + 1)
            } else {
                offset(*first)
            },
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
                    let child_has_value =
                        yaml_mapping_key(line_text(text, line_starts, *cfirst).trim())
                            .is_some_and(|(_, rest)| !rest.is_empty());
                    out.push(Section {
                        name: format!("{full_name}.{ckey}"),
                        kind: "section",
                        bytes: offset(*cfirst)..offset(cnext),
                        signature_start: offset(*cfirst),
                        header_end: if child_has_value {
                            offset(*cfirst + 1)
                        } else {
                            offset(*cfirst)
                        },
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

fn yaml_block_scalar_parent_indent(indent: usize, trimmed: &str) -> Option<usize> {
    if yaml_opens_block_scalar(trimmed) {
        return Some(indent);
    }
    let after_dash = trimmed.strip_prefix("- ")?;
    let key_part = after_dash.trim_start();
    if yaml_opens_block_scalar(key_part) {
        let extra = trimmed.len() - key_part.len();
        Some(indent + extra)
    } else {
        None
    }
}

fn is_github_workflow_path(file: &str) -> bool {
    let normalized = file.replace('\\', "/").to_ascii_lowercase();
    (normalized.starts_with(".github/workflows/") || normalized.contains("/.github/workflows/"))
        && (normalized.ends_with(".yml") || normalized.ends_with(".yaml"))
}

struct WorkflowStep {
    first: usize,
    next: usize,
    label: String,
    has_named_or_uses: bool,
}

struct WorkflowJob {
    first: usize,
    next: usize,
    name: String,
    runs_on_line: Option<usize>,
    uses_line: Option<usize>,
    steps: Vec<WorkflowStep>,
}

fn github_actions_yaml_sections(
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
) -> Option<Vec<Section>> {
    let top_keys = yaml_keys_at_indent(text, line_starts, start, end, 0);
    let jobs_pos = top_keys.iter().position(|(_, k)| k == "jobs")?;
    let jobs_first = top_keys[jobs_pos].0;
    let jobs_next = top_keys.get(jobs_pos + 1).map_or(end, |k| k.0);
    let job_indent = yaml_first_child_indent(text, line_starts, jobs_first + 1, jobs_next)?;
    let job_keys = yaml_keys_at_indent(text, line_starts, jobs_first + 1, jobs_next, job_indent);
    if job_keys.is_empty() {
        return None;
    }

    let mut jobs = Vec::with_capacity(job_keys.len());
    for (jpos, (jfirst, jkey)) in job_keys.iter().enumerate() {
        let jnext = job_keys.get(jpos + 1).map_or(jobs_next, |j| j.0);
        let mut runs_on_line = None;
        let mut uses_line = None;
        let mut steps = Vec::new();
        if let Some(prop_indent) = yaml_first_child_indent(text, line_starts, *jfirst + 1, jnext) {
            let props = yaml_keys_at_indent(text, line_starts, *jfirst + 1, jnext, prop_indent);
            for (ppos, (pfirst, pkey)) in props.iter().enumerate() {
                let pnext = props.get(ppos + 1).map_or(jnext, |p| p.0);
                let trimmed = line_text(text, line_starts, *pfirst).trim();
                match pkey.as_str() {
                    "runs-on" => {
                        if let Some((_, val)) = yaml_mapping_key(trimmed) {
                            let clean = val.split_once(" #").map_or(val, |(h, _)| h).trim();
                            if !clean.is_empty() && runs_on_line.is_none() {
                                runs_on_line = Some(*pfirst);
                            }
                        }
                    }
                    "uses" => {
                        if let Some((_, val)) = yaml_mapping_key(trimmed) {
                            let clean = val.split_once(" #").map_or(val, |(h, _)| h).trim();
                            if !clean.is_empty() && uses_line.is_none() {
                                uses_line = Some(*pfirst);
                            }
                        }
                    }
                    "steps" => {
                        steps = parse_workflow_steps(
                            text,
                            line_starts,
                            *pfirst + 1,
                            pnext,
                            prop_indent,
                        );
                    }
                    _ => {}
                }
            }
        }
        jobs.push(WorkflowJob {
            first: *jfirst,
            next: jnext,
            name: jkey.clone(),
            runs_on_line,
            uses_line,
            steps,
        });
    }

    // Activate enriched job/step outlining for workflows with named or `uses:`
    // steps, multi-step jobs, reusable workflow calls, or substantial length so
    // tiny synthetic fixtures keep the compact generic YAML key/section view.
    let rich = line_starts.len() >= 25
        || jobs.iter().any(|j| {
            j.uses_line.is_some()
                || j.steps.len() >= 2
                || j.steps.iter().any(|s| s.has_named_or_uses)
        });
    if !rich {
        return None;
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::new();
    for (pos, (first, key)) in top_keys.iter().enumerate() {
        let next = top_keys.get(pos + 1).map_or(end, |k| k.0);
        let has_inline_value = yaml_mapping_key(line_text(text, line_starts, *first).trim())
            .is_some_and(|(_, rest)| !rest.is_empty());
        out.push(Section {
            name: key.clone(),
            kind: "key",
            bytes: offset(*first)..offset(next),
            signature_start: offset(*first),
            header_end: if has_inline_value {
                offset(*first + 1)
            } else {
                offset(*first)
            },
        });

        if key == "jobs" {
            for job in &jobs {
                let (sig_start, header_end) = match job.runs_on_line.or(job.uses_line) {
                    Some(sig_line) => (offset(sig_line), offset(sig_line + 1)),
                    None => (offset(job.first), offset(job.first)),
                };
                out.push(Section {
                    name: job.name.clone(),
                    kind: "job",
                    bytes: offset(job.first)..offset(job.next),
                    signature_start: sig_start,
                    header_end,
                });
                for step in &job.steps {
                    out.push(Section {
                        name: step.label.clone(),
                        kind: "step",
                        bytes: offset(step.first)..offset(step.next),
                        signature_start: offset(step.first),
                        header_end: offset(step.first),
                    });
                }
            }
        } else if next.saturating_sub(*first) >= 10
            && let Some(child_indent) = yaml_first_child_indent(text, line_starts, *first + 1, next)
        {
            let children = yaml_keys_at_indent(text, line_starts, *first + 1, next, child_indent);
            if should_expand_children(children.len(), next, &children) {
                for (cpos, (cfirst, ckey)) in children.iter().enumerate() {
                    let cnext = children.get(cpos + 1).map_or(next, |c| c.0);
                    let child_has_value =
                        yaml_mapping_key(line_text(text, line_starts, *cfirst).trim())
                            .is_some_and(|(_, rest)| !rest.is_empty());
                    out.push(Section {
                        name: format!("{key}.{ckey}"),
                        kind: "section",
                        bytes: offset(*cfirst)..offset(cnext),
                        signature_start: offset(*cfirst),
                        header_end: if child_has_value {
                            offset(*cfirst + 1)
                        } else {
                            offset(*cfirst)
                        },
                    });
                }
            }
        }
    }

    (out.len() >= 2).then_some(out)
}

fn parse_workflow_steps(
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
    min_indent: usize,
) -> Vec<WorkflowStep> {
    let mut step_indent: Option<usize> = None;
    for idx in start..end {
        let line = line_text(text, line_starts, idx);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if indent >= min_indent && (trimmed == "-" || trimmed.starts_with("- ")) {
            step_indent = Some(indent);
        }
        break;
    }
    let Some(step_indent) = step_indent else {
        return Vec::new();
    };

    let mut starts = Vec::new();
    let mut block_indent: Option<usize> = None;
    for idx in start..end {
        let line = line_text(text, line_starts, idx);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
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
        if indent == step_indent && (trimmed == "-" || trimmed.starts_with("- ")) {
            starts.push(idx);
        }
        if let Some(parent) = yaml_block_scalar_parent_indent(indent, trimmed) {
            block_indent = Some(parent);
        }
    }

    let mut steps = Vec::with_capacity(starts.len());
    for (pos, &sfirst) in starts.iter().enumerate() {
        let snext = starts.get(pos + 1).copied().unwrap_or(end);
        let (label, has_named_or_uses) =
            workflow_step_label(text, line_starts, sfirst, snext, pos + 1);
        steps.push(WorkflowStep {
            first: sfirst,
            next: snext,
            label,
            has_named_or_uses,
        });
    }
    steps
}

fn workflow_step_label(
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
    ordinal: usize,
) -> (String, bool) {
    let mut name: Option<String> = None;
    let mut uses: Option<String> = None;
    let mut id: Option<String> = None;
    let mut run_cmd: Option<String> = None;
    let mut block_indent: Option<usize> = None;
    let mut capture_run_block = false;
    let mut prop_indent: Option<usize> = None;

    let clean_val = |raw: &str| -> String {
        let stripped = raw.split_once(" #").map_or(raw, |(h, _)| h).trim();
        unquote(stripped).trim().to_owned()
    };

    for idx in start..end {
        let line = line_text(text, line_starts, idx);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if let Some(parent) = block_indent {
            if indent <= parent {
                block_indent = None;
                capture_run_block = false;
            } else {
                if capture_run_block && run_cmd.is_none() {
                    run_cmd = Some(trimmed.to_owned());
                }
                continue;
            }
        }

        let kv = if idx == start {
            trimmed
                .strip_prefix("- ")
                .and_then(|after| yaml_mapping_key(after.trim_start()))
        } else {
            let target = *prop_indent.get_or_insert(indent);
            if indent == target {
                yaml_mapping_key(trimmed)
            } else {
                None
            }
        };

        let block_parent = yaml_block_scalar_parent_indent(indent, trimmed);
        if let Some((k, v)) = kv {
            let opens_block = block_parent.is_some();
            match k {
                "name" if name.is_none() && !opens_block => {
                    let val = clean_val(v);
                    if !val.is_empty() {
                        name = Some(val);
                    }
                }
                "uses" if uses.is_none() && !opens_block => {
                    let val = clean_val(v);
                    if !val.is_empty() {
                        uses = Some(val);
                    }
                }
                "id" if id.is_none() && !opens_block => {
                    let val = clean_val(v);
                    if !val.is_empty() {
                        id = Some(val);
                    }
                }
                "run" if run_cmd.is_none() => {
                    if opens_block {
                        capture_run_block = true;
                    } else {
                        let val = clean_val(v);
                        if !val.is_empty() {
                            run_cmd = Some(val);
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some(parent) = block_parent {
            block_indent = Some(parent);
        }
    }

    let has_named_or_uses = name.is_some() || uses.is_some();
    let raw_label = if let Some(n) = name {
        n
    } else if let Some(u) = uses {
        u
    } else if let Some(i) = id {
        i
    } else if let Some(r) = run_cmd {
        format!("run: {r}")
    } else {
        format!("step-{ordinal}")
    };
    (clip_label(&compact_ws(&raw_label), 72), has_named_or_uses)
}

fn openapi_http_method(key: &str) -> Option<&'static str> {
    match key.to_ascii_lowercase().as_str() {
        "get" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "patch" => Some("PATCH"),
        "delete" => Some("DELETE"),
        "head" => Some("HEAD"),
        "options" => Some("OPTIONS"),
        "trace" => Some("TRACE"),
        "query" => Some("QUERY"),
        _ => None,
    }
}

fn openapi_yaml_sections(
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
) -> Option<Vec<Section>> {
    let top_keys = yaml_keys_at_indent(text, line_starts, start, end, 0);
    let has_spec_version = top_keys.iter().any(|(line, key)| {
        matches!(key.as_str(), "openapi" | "swagger")
            && yaml_mapping_key(line_text(text, line_starts, *line).trim())
                .is_some_and(|(_, v)| !v.is_empty())
    });
    let has_api_body = top_keys.iter().any(|(_, k)| {
        matches!(
            k.as_str(),
            "paths" | "components" | "definitions" | "webhooks"
        )
    });
    if !has_spec_version || !has_api_body {
        return None;
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::new();
    for (pos, (first, key)) in top_keys.iter().enumerate() {
        let next = top_keys.get(pos + 1).map_or(end, |k| k.0);
        let has_inline_value = yaml_mapping_key(line_text(text, line_starts, *first).trim())
            .is_some_and(|(_, rest)| !rest.is_empty());
        out.push(Section {
            name: key.clone(),
            kind: "key",
            bytes: offset(*first)..offset(next),
            signature_start: offset(*first),
            header_end: if has_inline_value {
                offset(*first + 1)
            } else {
                offset(*first)
            },
        });

        match key.as_str() {
            "paths" | "webhooks" => {
                if let Some(child_indent) =
                    yaml_first_child_indent(text, line_starts, *first + 1, next)
                {
                    let items =
                        yaml_keys_at_indent(text, line_starts, *first + 1, next, child_indent);
                    let kind = if key == "paths" { "path" } else { "webhook" };
                    for (ipos, (ifirst, ikey)) in items.iter().enumerate() {
                        let inext = items.get(ipos + 1).map_or(next, |c| c.0);
                        let mut methods = Vec::new();
                        if let Some(method_indent) =
                            yaml_first_child_indent(text, line_starts, *ifirst + 1, inext)
                        {
                            for (_, mkey) in yaml_keys_at_indent(
                                text,
                                line_starts,
                                *ifirst + 1,
                                inext,
                                method_indent,
                            ) {
                                if let Some(m) = openapi_http_method(&mkey)
                                    && !methods.contains(&m)
                                {
                                    methods.push(m);
                                }
                            }
                        }
                        let label = if methods.is_empty() {
                            ikey.clone()
                        } else {
                            format!("{ikey} [{}]", methods.join(", "))
                        };
                        out.push(Section {
                            name: clip_label(&label, 96),
                            kind,
                            bytes: offset(*ifirst)..offset(inext),
                            signature_start: offset(*ifirst),
                            header_end: offset(*ifirst),
                        });
                    }
                }
            }
            "components" => {
                if let Some(group_indent) =
                    yaml_first_child_indent(text, line_starts, *first + 1, next)
                {
                    let groups =
                        yaml_keys_at_indent(text, line_starts, *first + 1, next, group_indent);
                    for (gpos, (gfirst, gkey)) in groups.iter().enumerate() {
                        let gnext = groups.get(gpos + 1).map_or(next, |g| g.0);
                        out.push(Section {
                            name: format!("components.{gkey}"),
                            kind: "section",
                            bytes: offset(*gfirst)..offset(gnext),
                            signature_start: offset(*gfirst),
                            header_end: offset(*gfirst),
                        });
                        if let Some(item_indent) =
                            yaml_first_child_indent(text, line_starts, *gfirst + 1, gnext)
                        {
                            let items = yaml_keys_at_indent(
                                text,
                                line_starts,
                                *gfirst + 1,
                                gnext,
                                item_indent,
                            );
                            for (ipos, (ifirst, ikey)) in items.iter().enumerate() {
                                let inext = items.get(ipos + 1).map_or(gnext, |c| c.0);
                                let (kind, name) = if gkey == "schemas" {
                                    ("schema", ikey.clone())
                                } else {
                                    ("component", format!("{gkey}.{ikey}"))
                                };
                                out.push(Section {
                                    name: clip_label(&name, 96),
                                    kind,
                                    bytes: offset(*ifirst)..offset(inext),
                                    signature_start: offset(*ifirst),
                                    header_end: offset(*ifirst),
                                });
                            }
                        }
                    }
                }
            }
            "definitions" => {
                if let Some(item_indent) =
                    yaml_first_child_indent(text, line_starts, *first + 1, next)
                {
                    let items =
                        yaml_keys_at_indent(text, line_starts, *first + 1, next, item_indent);
                    for (ipos, (ifirst, ikey)) in items.iter().enumerate() {
                        let inext = items.get(ipos + 1).map_or(next, |c| c.0);
                        out.push(Section {
                            name: clip_label(ikey, 96),
                            kind: "schema",
                            bytes: offset(*ifirst)..offset(inext),
                            signature_start: offset(*ifirst),
                            header_end: offset(*ifirst),
                        });
                    }
                }
            }
            _ => {
                if next.saturating_sub(*first) >= 10
                    && let Some(child_indent) =
                        yaml_first_child_indent(text, line_starts, *first + 1, next)
                {
                    let children =
                        yaml_keys_at_indent(text, line_starts, *first + 1, next, child_indent);
                    if should_expand_children(children.len(), next, &children) {
                        for (cpos, (cfirst, ckey)) in children.iter().enumerate() {
                            let cnext = children.get(cpos + 1).map_or(next, |c| c.0);
                            let child_has_value =
                                yaml_mapping_key(line_text(text, line_starts, *cfirst).trim())
                                    .is_some_and(|(_, rest)| !rest.is_empty());
                            out.push(Section {
                                name: format!("{key}.{ckey}"),
                                kind: "section",
                                bytes: offset(*cfirst)..offset(cnext),
                                signature_start: offset(*cfirst),
                                header_end: if child_has_value {
                                    offset(*cfirst + 1)
                                } else {
                                    offset(*cfirst)
                                },
                            });
                        }
                    }
                }
            }
        }
    }

    (out.len() >= 2).then_some(out)
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

struct JsonElement {
    label: Option<String>,
    start: usize,
    end: usize,
    is_composite: bool,
}

enum JsonRoot {
    Object(Vec<JsonMember>),
    Array(Vec<JsonElement>),
}

fn json_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    let mut parser = JsonScanner::new(text);
    let Some(root) = parser.parse_root() else {
        return Vec::new();
    };
    match root {
        JsonRoot::Object(members) => json_object_sections(text, line_starts, &members),
        JsonRoot::Array(elements) => json_array_sections(text, line_starts, &elements),
    }
}

fn json_object_sections(text: &str, line_starts: &[usize], members: &[JsonMember]) -> Vec<Section> {
    if members.is_empty() {
        return Vec::new();
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let distinct_top_lines: BTreeSet<usize> = members
        .iter()
        .map(|m| line_of(line_starts, m.key_start))
        .collect();
    let minified = distinct_top_lines.len() < 2 && text.len() > MINIFIED_THRESHOLD;
    let expand_single_minified =
        minified && members.len() == 1 && (2..=64).contains(&members[0].children.len());
    if !minified
        && distinct_top_lines.len() < 2
        && !(members.len() == 1 && json_should_expand(&members[0], line_starts))
    {
        return Vec::new();
    }
    if minified && members.len() < 2 && !expand_single_minified {
        return Vec::new();
    }

    if let Some(openapi) = openapi_json_sections(text, line_starts, members, minified) {
        return openapi;
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

        let bytes = if !minified && !prev_ends_here && !next_starts_here {
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

        let expand_children =
            json_should_expand(member, line_starts) || (expand_single_minified && idx == 0);
        if expand_children {
            for child in &member.children {
                let cfirst = line_of(line_starts, child.key_start);
                let clast = line_of(line_starts, child.val_end.saturating_sub(1));
                let cbytes = if minified {
                    child.key_start..child.val_end
                } else {
                    offset(cfirst)..offset(clast + 1)
                };
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

fn parse_json_object_at(text: &str, start: usize, end: usize) -> Option<Vec<JsonMember>> {
    let slice = text.get(start..end)?;
    if !slice.starts_with('{') {
        return None;
    }
    let mut scanner = JsonScanner::new(slice);
    let mut members = scanner.parse_object(0)?;
    fn shift(members: &mut [JsonMember], delta: usize) {
        for member in members {
            member.key_start += delta;
            member.val_start += delta;
            member.val_end += delta;
            shift(&mut member.children, delta);
        }
    }
    shift(&mut members, start);
    Some(members)
}

fn json_member_bytes(
    text: &str,
    line_starts: &[usize],
    siblings: &[JsonMember],
    idx: usize,
    minified: bool,
) -> Range<usize> {
    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let member = &siblings[idx];
    let first_line = line_of(line_starts, member.key_start);
    let last_line = line_of(line_starts, member.val_end.saturating_sub(1));
    let prev_ends_here =
        idx > 0 && line_of(line_starts, siblings[idx - 1].val_end.saturating_sub(1)) == first_line;
    let next_starts_here = siblings
        .get(idx + 1)
        .is_some_and(|next| line_of(line_starts, next.key_start) == last_line);
    if !minified && !prev_ends_here && !next_starts_here {
        offset(first_line)..offset(last_line + 1)
    } else {
        member.key_start..member.val_end
    }
}

fn openapi_json_sections(
    text: &str,
    line_starts: &[usize],
    members: &[JsonMember],
    minified: bool,
) -> Option<Vec<Section>> {
    let has_spec_version = members.iter().any(|m| {
        matches!(m.name.as_str(), "openapi" | "swagger")
            && text.as_bytes().get(m.val_start) == Some(&b'"')
    });
    let has_api_body = members.iter().any(|m| {
        matches!(
            m.name.as_str(),
            "paths" | "components" | "definitions" | "webhooks"
        )
    });
    if !has_spec_version || !has_api_body {
        return None;
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::new();
    for (idx, member) in members.iter().enumerate() {
        let first_line = line_of(line_starts, member.key_start);
        let last_line = line_of(line_starts, member.val_end.saturating_sub(1));
        let bytes = json_member_bytes(text, line_starts, members, idx, minified);
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

        match member.name.as_str() {
            "paths" | "webhooks" => {
                let kind = if member.name == "paths" {
                    "path"
                } else {
                    "webhook"
                };
                for (cidx, child) in member.children.iter().enumerate() {
                    let mut methods = Vec::new();
                    if let Some(ops) = parse_json_object_at(text, child.val_start, child.val_end) {
                        for op in ops {
                            if let Some(m) = openapi_http_method(&op.name)
                                && !methods.contains(&m)
                            {
                                methods.push(m);
                            }
                        }
                    }
                    let label = if methods.is_empty() {
                        child.name.clone()
                    } else {
                        format!("{} [{}]", child.name, methods.join(", "))
                    };
                    let cbytes =
                        json_member_bytes(text, line_starts, &member.children, cidx, minified);
                    out.push(Section {
                        name: clip_label(&label, 96),
                        kind,
                        bytes: cbytes,
                        signature_start: child.key_start,
                        header_end: child.key_start,
                    });
                }
            }
            "components" => {
                for (gidx, group) in member.children.iter().enumerate() {
                    let gbytes =
                        json_member_bytes(text, line_starts, &member.children, gidx, minified);
                    out.push(Section {
                        name: format!("components.{}", group.name),
                        kind: "section",
                        bytes: gbytes,
                        signature_start: group.key_start,
                        header_end: group.key_start,
                    });
                    if let Some(items) = parse_json_object_at(text, group.val_start, group.val_end)
                    {
                        for (iidx, item) in items.iter().enumerate() {
                            let ibytes =
                                json_member_bytes(text, line_starts, &items, iidx, minified);
                            let (kind, name) = if group.name == "schemas" {
                                ("schema", item.name.clone())
                            } else {
                                ("component", format!("{}.{}", group.name, item.name))
                            };
                            out.push(Section {
                                name: clip_label(&name, 96),
                                kind,
                                bytes: ibytes,
                                signature_start: item.key_start,
                                header_end: item.key_start,
                            });
                        }
                    }
                }
            }
            "definitions" => {
                for (cidx, child) in member.children.iter().enumerate() {
                    let cbytes =
                        json_member_bytes(text, line_starts, &member.children, cidx, minified);
                    out.push(Section {
                        name: clip_label(&child.name, 96),
                        kind: "schema",
                        bytes: cbytes,
                        signature_start: child.key_start,
                        header_end: child.key_start,
                    });
                }
            }
            _ if json_should_expand(member, line_starts) => {
                for (cidx, child) in member.children.iter().enumerate() {
                    let cfirst = line_of(line_starts, child.key_start);
                    let clast = line_of(line_starts, child.val_end.saturating_sub(1));
                    let cbytes =
                        json_member_bytes(text, line_starts, &member.children, cidx, minified);
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
            _ => {}
        }
    }
    (out.len() >= 2).then_some(out)
}

fn json_array_sections(
    text: &str,
    line_starts: &[usize],
    elements: &[JsonElement],
) -> Vec<Section> {
    if elements.len() < 2 {
        return Vec::new();
    }

    let distinct_lines: BTreeSet<usize> = elements
        .iter()
        .map(|e| line_of(line_starts, e.start))
        .collect();
    let minified = distinct_lines.len() < 2;
    if minified {
        let has_composite = elements.iter().any(|e| e.is_composite);
        if text.len() <= MINIFIED_THRESHOLD || (!has_composite && text.len() / elements.len() < 16)
        {
            return Vec::new();
        }
    }

    let mut label_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for elem in elements {
        if let Some(label) = elem.label.as_deref() {
            *label_counts.entry(label).or_default() += 1;
        }
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::with_capacity(elements.len());
    for (idx, elem) in elements.iter().enumerate() {
        let first_line = line_of(line_starts, elem.start);
        let last_line = line_of(line_starts, elem.end.saturating_sub(1));
        let prev_ends_here =
            idx > 0 && line_of(line_starts, elements[idx - 1].end.saturating_sub(1)) == first_line;
        let next_starts_here = elements
            .get(idx + 1)
            .is_some_and(|next| line_of(line_starts, next.start) == last_line);

        let bytes = if !minified && !prev_ends_here && !next_starts_here {
            offset(first_line)..offset(last_line + 1)
        } else {
            elem.start..elem.end
        };
        let header_end = if first_line == last_line && elem.end - elem.start > 100 {
            char_floor(text, elem.start + 100).min(bytes.end)
        } else {
            offset(first_line + 1).min(bytes.end)
        };
        let name = match elem.label.as_deref() {
            Some(label) if label_counts.get(label).copied().unwrap_or(0) == 1 => label.to_owned(),
            Some(label) => format!("[{idx}] {label}"),
            None => format!("[{idx}]"),
        };
        out.push(Section {
            name,
            kind: "item",
            bytes,
            signature_start: elem.start,
            header_end,
        });
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

    fn parse_root(&mut self) -> Option<JsonRoot> {
        if self.src.starts_with('\u{feff}') {
            self.pos = '\u{feff}'.len_utf8();
        }
        self.skip_ws_and_comments()?;
        let root = match self.peek()? {
            b'{' => JsonRoot::Object(self.parse_object(0)?),
            b'[' => JsonRoot::Array(self.parse_root_array()?),
            _ => return None,
        };
        self.skip_ws_and_comments()?;
        (self.pos == self.bytes.len()).then_some(root)
    }

    fn parse_root_array(&mut self) -> Option<Vec<JsonElement>> {
        if self.peek() != Some(b'[') {
            return None;
        }
        self.pos += 1;
        let mut elements = Vec::new();
        loop {
            self.skip_ws_and_comments()?;
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Some(elements);
            }
            let start = self.pos;
            let is_composite = matches!(self.peek(), Some(b'{' | b'['));
            let label = if self.peek() == Some(b'{') {
                self.parse_object_label(1)?
            } else {
                self.skip_value(1)?;
                None
            };
            let end = self.pos;
            elements.push(JsonElement {
                label,
                start,
                end,
                is_composite,
            });
            self.skip_ws_and_comments()?;
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Some(elements);
                }
                _ => return None,
            }
        }
    }

    fn parse_object_label(&mut self, depth: usize) -> Option<Option<String>> {
        if depth > 128 || self.peek() != Some(b'{') {
            return None;
        }
        self.pos += 1;
        let mut best: Option<(u8, String)> = None;
        loop {
            self.skip_ws_and_comments()?;
            if self.peek() == Some(b'}') {
                self.pos += 1;
                return Some(best.map(|(_, v)| v));
            }
            let key = self.parse_string()?;
            self.skip_ws_and_comments()?;
            if self.peek() != Some(b':') {
                return None;
            }
            self.pos += 1;
            self.skip_ws_and_comments()?;
            let priority = match key.as_str() {
                "id" => Some(0),
                "name" => Some(1),
                "key" => Some(2),
                "type" => Some(3),
                _ => None,
            };
            if let Some(p) = priority
                && best.as_ref().is_none_or(|(cur, _)| p < *cur)
                && self.peek() == Some(b'"')
            {
                let value = self.parse_string()?;
                if !value.is_empty() && value.len() <= 80 && !value.contains('\n') {
                    best = Some((p, value));
                }
            } else {
                self.skip_value(depth + 1)?;
            }
            self.skip_ws_and_comments()?;
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Some(best.map(|(_, v)| v));
                }
                _ => return None,
            }
        }
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

// ---------------------------------------------------------------------------
// Makefile / Justfile
// ---------------------------------------------------------------------------

fn ends_with_odd_backslashes(line: &str) -> bool {
    line.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

fn strip_hash_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' && q == b'"' {
                    i += 2;
                    continue;
                }
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' | b'`' => quote = Some(b),
                b'#' => return &s[..i],
                _ => {}
            },
        }
        i += 1;
    }
    s
}

fn makefile_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    let mut targets: Vec<(usize, String, bool)> = Vec::new();
    let mut in_define = false;
    let mut continued = false;

    for idx in 0..line_starts.len() {
        let line = line_text(text, line_starts, idx);
        let next_continued = ends_with_odd_backslashes(line);
        if continued {
            continued = next_continued;
            continue;
        }
        continued = next_continued;

        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if in_define {
            if trimmed == "endef" || trimmed.starts_with("endef ") || trimmed.starts_with("endef\t")
            {
                in_define = false;
            }
            continue;
        }
        if line.starts_with([' ', '\t']) {
            continue;
        }
        if make_opens_define(trimmed) {
            in_define = true;
            continue;
        }
        if let Some((name, has_extra)) = parse_make_target(line) {
            targets.push((idx, name, has_extra));
        }
    }

    if in_define || targets.len() < 2 {
        return Vec::new();
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::with_capacity(targets.len() + 1);
    if targets[0].0 > 0
        && let Some(first_stmt) = (0..targets[0].0).find(|&i| {
            let t = line_text(text, line_starts, i).trim();
            !t.is_empty() && !t.starts_with('#')
        })
    {
        out.push(Section {
            name: "(root)".into(),
            kind: "section",
            bytes: offset(0)..offset(targets[0].0),
            signature_start: offset(first_stmt),
            header_end: offset(first_stmt + 1),
        });
    }

    for (pos, (first, name, has_extra)) in targets.iter().enumerate() {
        let end_line = targets.get(pos + 1).map_or(line_starts.len(), |t| t.0);
        out.push(Section {
            name: name.clone(),
            kind: "target",
            bytes: offset(*first)..offset(end_line),
            signature_start: offset(*first),
            header_end: if *has_extra {
                offset(*first + 1)
            } else {
                offset(*first)
            },
        });
    }
    out
}

fn make_opens_define(trimmed: &str) -> bool {
    let mut rest = trimmed;
    for prefix in ["override", "export", "private"] {
        if let Some(tail) = rest.strip_prefix(prefix)
            && tail.starts_with([' ', '\t'])
        {
            rest = tail.trim_start();
        }
    }
    rest.strip_prefix("define")
        .is_some_and(|tail| tail.starts_with([' ', '\t']))
}

fn make_directive(trimmed: &str) -> bool {
    let word = trimmed
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or_default();
    matches!(
        word,
        "ifeq"
            | "ifneq"
            | "ifdef"
            | "ifndef"
            | "else"
            | "endif"
            | "include"
            | "-include"
            | "sinclude"
            | "vpath"
            | "export"
            | "unexport"
            | "override"
            | "private"
            | "define"
            | "endef"
    )
}

fn parse_make_target(line: &str) -> Option<(String, bool)> {
    let trimmed = line.trim_start();
    if make_directive(trimmed) {
        return None;
    }
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    let mut colon = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'$' if matches!(bytes.get(i + 1), Some(b'(' | b'{')) => {
                depth += 1;
                i += 2;
                continue;
            }
            b')' | b'}' if depth > 0 => depth -= 1,
            b'#' if depth == 0 => return None,
            b'=' if depth == 0 => return None,
            b':' if depth == 0 => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let colon = colon?;
    // Reject `:=`, `::=`, `:::=` variable assignments.
    let mut after = colon + 1;
    while bytes.get(after) == Some(&b':') {
        after += 1;
    }
    if bytes.get(after) == Some(&b'=') || after - colon > 2 {
        return None;
    }
    let lhs = line[..colon].trim();
    if lhs.is_empty() || lhs.contains(['#', ';', '"', '\'']) {
        return None;
    }
    // Skip Make's built-in special targets like `.PHONY`, `.DEFAULT_GOAL`, `.SUFFIXES`.
    if lhs.starts_with('.')
        && lhs.len() > 1
        && lhs[1..]
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b'_')
    {
        return None;
    }
    let rhs = strip_hash_comment(&line[after..]).trim();
    // Skip target-specific variable assignments (`target: CFLAGS += -g`).
    if has_top_level_eq(rhs) {
        return None;
    }
    Some((lhs.to_owned(), !rhs.is_empty()))
}

fn has_top_level_eq(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'$' if matches!(bytes.get(i + 1), Some(b'(' | b'{')) => {
                depth += 1;
                i += 2;
                continue;
            }
            b')' | b'}' if depth > 0 => depth -= 1,
            b'=' if depth == 0 => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

fn justfile_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    // Each entry: (section_start_line, header_line, name, has_extra)
    let mut recipes: Vec<(usize, usize, String, bool)> = Vec::new();
    let mut multiline: Option<&'static str> = None;
    let mut continued = false;
    let mut pending_attr: Option<usize> = None;

    for idx in 0..line_starts.len() {
        let line = line_text(text, line_starts, idx);
        if multiline.is_some() {
            advance_toml_multiline(line, &mut multiline);
            continue;
        }
        let next_continued = ends_with_odd_backslashes(line);
        if continued {
            continued = next_continued;
            advance_toml_multiline(line, &mut multiline);
            continue;
        }
        continued = next_continued;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            pending_attr = None;
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            pending_attr.get_or_insert(idx);
            continue;
        }
        if just_directive(trimmed) {
            pending_attr = None;
            advance_toml_multiline(line, &mut multiline);
            continue;
        }
        if let Some((name, has_extra)) = parse_just_recipe(line) {
            let start_line = pending_attr.take().unwrap_or(idx);
            recipes.push((start_line, idx, name, has_extra));
        } else {
            pending_attr = None;
            advance_toml_multiline(line, &mut multiline);
        }
    }

    if multiline.is_some() || recipes.len() < 2 {
        return Vec::new();
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::with_capacity(recipes.len() + 1);
    if recipes[0].0 > 0
        && let Some(first_stmt) = (0..recipes[0].0).find(|&i| {
            let t = line_text(text, line_starts, i).trim();
            !t.is_empty() && !t.starts_with('#')
        })
    {
        out.push(Section {
            name: "(root)".into(),
            kind: "section",
            bytes: offset(0)..offset(recipes[0].0),
            signature_start: offset(first_stmt),
            header_end: offset(first_stmt + 1),
        });
    }

    for (pos, (start, header, name, has_extra)) in recipes.iter().enumerate() {
        let end_line = recipes.get(pos + 1).map_or(line_starts.len(), |r| r.0);
        out.push(Section {
            name: name.clone(),
            kind: "recipe",
            bytes: offset(*start)..offset(end_line),
            signature_start: offset(*header),
            header_end: if *has_extra {
                offset(*header + 1)
            } else {
                offset(*header)
            },
        });
    }
    out
}

fn just_directive(trimmed: &str) -> bool {
    let word = trimmed.split_whitespace().next().unwrap_or_default();
    matches!(
        word,
        "set" | "alias" | "export" | "unexport" | "import" | "import?" | "mod" | "mod?"
    )
}

fn parse_just_recipe(line: &str) -> Option<(String, bool)> {
    let (has_at, rest) = match line.strip_prefix('@') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let bytes = rest.as_bytes();
    let first = *bytes.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let name_end = bytes
        .iter()
        .position(|&b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        .unwrap_or(bytes.len());
    let name = &rest[..name_end];
    let tail = &rest[name_end..];
    // Scan for `:` (not `:=`) outside quotes and parens.
    let tbytes = tail.as_bytes();
    let mut quote: Option<u8> = None;
    let mut parens = 0usize;
    let mut colon = None;
    let mut i = 0;
    while i < tbytes.len() {
        let b = tbytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' && q == b'"' {
                    i += 2;
                    continue;
                }
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' | b'`' => quote = Some(b),
                b'(' => parens += 1,
                b')' => parens = parens.saturating_sub(1),
                b'#' if parens == 0 => return None,
                b':' if parens == 0 => {
                    if tbytes.get(i + 1) == Some(&b'=') {
                        return None;
                    }
                    colon = Some(i);
                    break;
                }
                _ => {}
            },
        }
        i += 1;
    }
    let colon = colon?;
    let params = &tail[..colon];
    if !params.is_empty() && !params.starts_with([' ', '\t']) {
        return None;
    }
    if params.contains(';') {
        return None;
    }
    let deps = strip_hash_comment(&tail[colon + 1..]).trim();
    let has_extra = has_at || !params.trim().is_empty() || !deps.is_empty();
    Some((name.to_owned(), has_extra))
}

// ---------------------------------------------------------------------------
// Dockerfile / Containerfile
// ---------------------------------------------------------------------------

fn dockerfile_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    let mut escape = b'\\';
    if let Some(first) = line_starts.first().map(|_| line_text(text, line_starts, 0))
        && let Some(rest) = first.trim().strip_prefix('#')
    {
        let directive = rest.trim();
        if let Some((k, v)) = directive.split_once('=')
            && k.trim().eq_ignore_ascii_case("escape")
            && let Some(&ch) = v.trim().as_bytes().first()
            && matches!(ch, b'\\' | b'`')
        {
            escape = ch;
        }
    }

    let mut stages: Vec<(usize, String)> = Vec::new();
    let mut continued = false;
    let mut heredoc: Option<(String, bool)> = None;

    for idx in 0..line_starts.len() {
        let line = line_text(text, line_starts, idx);
        if let Some((ref delim, strip_tabs)) = heredoc {
            let candidate = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if candidate == delim {
                heredoc = None;
            }
            continue;
        }
        let trimmed = line.trim();
        let next_continued = !trimmed.starts_with('#')
            && line
                .trim_end_matches([' ', '\t'])
                .as_bytes()
                .last()
                .is_some_and(|&b| b == escape);
        if continued {
            continued = next_continued;
            continue;
        }
        continued = next_continued;

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(hd) = dockerfile_heredoc_opener(trimmed) {
            heredoc = Some(hd);
            continued = false;
            continue;
        }
        let mut words = trimmed.split_whitespace();
        let Some(instruction) = words.next() else {
            continue;
        };
        if instruction.eq_ignore_ascii_case("FROM") {
            let Some(name) = parse_dockerfile_from(words) else {
                return Vec::new();
            };
            stages.push((idx, name));
        }
    }

    if heredoc.is_some() || stages.len() < 2 {
        return Vec::new();
    }

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, name) in &stages {
        *counts.entry(name.as_str()).or_default() += 1;
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::with_capacity(stages.len() + 1);
    if stages[0].0 > 0
        && let Some(first_stmt) = (0..stages[0].0).find(|&i| {
            let t = line_text(text, line_starts, i).trim();
            !t.is_empty() && !t.starts_with('#')
        })
    {
        out.push(Section {
            name: "(global)".into(),
            kind: "section",
            bytes: offset(0)..offset(stages[0].0),
            signature_start: offset(first_stmt),
            header_end: offset(first_stmt + 1),
        });
    }

    for (pos, (first, raw_name)) in stages.iter().enumerate() {
        let end_line = stages.get(pos + 1).map_or(line_starts.len(), |s| s.0);
        let name = if counts.get(raw_name.as_str()).copied().unwrap_or(0) > 1 {
            let n = seen.entry(raw_name.clone()).or_default();
            *n += 1;
            format!("{raw_name}:{n}")
        } else {
            raw_name.clone()
        };
        out.push(Section {
            name,
            kind: "stage",
            bytes: offset(*first)..offset(end_line),
            signature_start: offset(*first),
            header_end: offset(*first + 1),
        });
    }
    out
}

fn parse_dockerfile_from<'a>(words: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut image: Option<&str> = None;
    let mut expect_alias = false;
    for word in words {
        if expect_alias {
            return (!word.is_empty()).then(|| word.to_owned());
        }
        if image.is_none() {
            if word.starts_with("--") {
                continue;
            }
            image = Some(word);
        } else if word.eq_ignore_ascii_case("AS") {
            expect_alias = true;
        }
    }
    image.map(str::to_owned)
}

fn dockerfile_heredoc_opener(trimmed: &str) -> Option<(String, bool)> {
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        if &bytes[i..i + 2] == b"<<" && bytes.get(i + 2) != Some(&b'<') {
            let mut j = i + 2;
            let strip_tabs = if bytes.get(j) == Some(&b'-') {
                j += 1;
                true
            } else {
                false
            };
            let rest = &trimmed[j..];
            let token = rest.split_whitespace().next().unwrap_or_default();
            let delim = token.trim_matches(['"', '\'']);
            if !delim.is_empty()
                && delim
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Some((delim.to_owned(), strip_tabs));
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// INI / Conf / EditorConfig / Properties
// ---------------------------------------------------------------------------

fn ini_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    let mut headers: Vec<(usize, String)> = Vec::new();
    for idx in 0..line_starts.len() {
        let line = line_text(text, line_starts, idx);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
            continue;
        }
        if !line.starts_with([' ', '\t']) && trimmed.starts_with('[') {
            let Some(name) = parse_ini_header(trimmed) else {
                return Vec::new();
            };
            headers.push((idx, name));
        }
    }

    if headers.len() < 2 {
        return Vec::new();
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::with_capacity(headers.len() + 1);
    if headers[0].0 > 0
        && let Some(first_key) = (0..headers[0].0).find(|&i| {
            let t = line_text(text, line_starts, i).trim();
            !t.is_empty() && !t.starts_with(['#', ';'])
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

    for (pos, (first, name)) in headers.iter().enumerate() {
        let end_line = headers.get(pos + 1).map_or(line_starts.len(), |h| h.0);
        out.push(Section {
            name: name.clone(),
            kind: "section",
            bytes: offset(*first)..offset(end_line),
            signature_start: offset(*first),
            header_end: offset(*first),
        });
    }
    out
}

fn parse_ini_header(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix('[')?;
    // Allow trailing inline `#` or `;` comments after the closing `]`.
    let mut close = None;
    for (idx, ch) in rest.char_indices() {
        if ch == ']' {
            let after = rest[idx + 1..].trim_start();
            if after.is_empty() || after.starts_with(['#', ';']) {
                close = Some(idx);
            }
        }
    }
    let close = close?;
    let inner = rest[..close].trim();
    if inner.is_empty() || (inner.starts_with('[') && inner.ends_with(']')) {
        return None;
    }
    Some(inner.to_owned())
}

// ---------------------------------------------------------------------------
// CSS / SCSS / Less
// ---------------------------------------------------------------------------

fn css_sections(text: &str, line_starts: &[usize], allow_line_comments: bool) -> Vec<Section> {
    let bytes = text.as_bytes();
    let mut blocks: Vec<(usize, usize, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut stmt_start: Option<usize> = None;
    let mut open_brace = 0usize;
    let mut i = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };

    while i < bytes.len() {
        let b = bytes[i];
        // Block comment `/* ... */`
        if bytes[i..].starts_with(b"/*") {
            let Some(end) = text[i + 2..].find("*/") else {
                return Vec::new();
            };
            i += 2 + end + 2;
            continue;
        }
        // Single-line comment `// ...` in SCSS / Less
        if allow_line_comments && bytes[i..].starts_with(b"//") {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Quoted strings
        if matches!(b, b'"' | b'\'') {
            if depth == 0 {
                stmt_start.get_or_insert(i);
            }
            let Some(next) = skip_quoted(bytes, i, b) else {
                return Vec::new();
            };
            i = next;
            continue;
        }
        // `url(...)` unquoted argument so `https://` never triggers `//`
        if (b == b'u' || b == b'U')
            && bytes.len() >= i + 4
            && bytes[i..i + 4].eq_ignore_ascii_case(b"url(")
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'-'
                    || bytes[i - 1] == b'_'))
        {
            if depth == 0 {
                stmt_start.get_or_insert(i);
            }
            let Some(next) = skip_css_url(bytes, i + 4) else {
                return Vec::new();
            };
            i = next;
            continue;
        }
        // SCSS `#{...}` / Less `@{...}` interpolation inside a selector/prelude
        if allow_line_comments
            && depth == 0
            && matches!(b, b'#' | b'@')
            && bytes.get(i + 1) == Some(&b'{')
        {
            stmt_start.get_or_insert(i);
            let Some(next) = skip_balanced_braces(bytes, i + 1) else {
                return Vec::new();
            };
            i = next;
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match b {
            b';' if depth == 0 => {
                stmt_start = None;
                i += 1;
            }
            b'{' => {
                if depth == 0 {
                    if stmt_start.is_none() {
                        return Vec::new();
                    }
                    open_brace = i;
                }
                depth += 1;
                i += 1;
            }
            b'}' => {
                if depth == 0 {
                    return Vec::new();
                }
                depth -= 1;
                if depth == 0 {
                    let start = stmt_start.take().unwrap();
                    blocks.push((start, open_brace, i + 1));
                }
                i += 1;
            }
            _ => {
                if depth == 0 {
                    stmt_start.get_or_insert(i);
                }
                i += 1;
            }
        }
    }

    if depth != 0 || blocks.len() < 2 {
        return Vec::new();
    }

    let distinct_lines: BTreeSet<usize> = blocks
        .iter()
        .map(|&(start, _, _)| line_of(line_starts, start))
        .collect();
    if distinct_lines.len() < 2 && text.len() <= MINIFIED_THRESHOLD {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(blocks.len());
    for (start, brace, end) in blocks {
        let raw_prelude = text[start..brace].trim_end();
        let cleaned = strip_c_comments(raw_prelude, allow_line_comments);
        let compact = compact_ws(&cleaned);
        if compact.is_empty() {
            continue;
        }
        let (kind, name) = classify_css_prelude(&compact);
        let bytes = whole_line_or_exact(text, line_starts, start, end);
        let sig_end = (start + raw_prelude.len()).min(bytes.end);
        let header_end = if compact == name { start } else { sig_end };
        out.push(Section {
            name,
            kind,
            bytes,
            signature_start: start,
            header_end,
        });
    }
    if out.len() < 2 {
        return Vec::new();
    }
    out
}

fn classify_css_prelude(compact: &str) -> (&'static str, String) {
    if let Some(rest) = compact.strip_prefix('@') {
        let kw_end = rest
            .find(|c: char| c.is_whitespace() || c == '(')
            .unwrap_or(rest.len());
        let kw = rest[..kw_end].to_ascii_lowercase();
        let after = rest[kw_end..].trim();
        match kw.as_str() {
            "keyframes" | "-webkit-keyframes" | "-moz-keyframes" => {
                let name = after
                    .split_whitespace()
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("@keyframes");
                ("keyframes", name.to_owned())
            }
            "layer" => {
                let name = if after.is_empty() {
                    "@layer".to_owned()
                } else {
                    clip_label(after, 80)
                };
                ("layer", name)
            }
            "mixin" => {
                let name = after
                    .split(|c: char| c.is_whitespace() || c == '(')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("@mixin");
                ("mixin", name.to_owned())
            }
            "function" => {
                let name = after
                    .split(|c: char| c.is_whitespace() || c == '(')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("@function");
                ("function", name.to_owned())
            }
            _ => ("at-rule", clip_label(compact, 80)),
        }
    } else {
        ("rule", clip_label(compact, 80))
    }
}

fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 2;
                if i > bytes.len() {
                    return None;
                }
            }
            b if b == quote => return Some(i + 1),
            b'\n' | b'\r' => return None,
            _ => i += 1,
        }
    }
    None
}

fn skip_css_url(bytes: &[u8], mut i: usize) -> Option<usize> {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if matches!(bytes.get(i), Some(b'"' | b'\'')) {
        i = skip_quoted(bytes, i, bytes[i])?;
    }
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 2;
                if i > bytes.len() {
                    return None;
                }
            }
            b')' => return Some(i + 1),
            b'\n' | b'\r' => return None,
            _ => i += 1,
        }
    }
    None
}

fn skip_balanced_braces(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => i = skip_quoted(bytes, i, bytes[i])?,
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1)?;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn strip_c_comments(input: &str, allow_line_comments: bool) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            if let Some(end) = input[i + 2..].find("*/") {
                out.push(' ');
                i += 2 + end + 2;
                continue;
            }
            break;
        }
        if allow_line_comments && bytes[i..].starts_with(b"//") {
            out.push(' ');
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// ---------------------------------------------------------------------------
// IDL Schemas: Protobuf, GraphQL, Prisma
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Eq, PartialEq)]
enum IdlKind {
    Proto,
    GraphQl,
    Prisma,
}

struct IdlDecl {
    kind: &'static str,
    name: String,
    short_name: String,
    start: usize,
    sig_end: usize,
    end: usize,
}

struct IdlScope {
    decl: Option<(&'static str, String, String, usize, usize)>,
    container_name: Option<String>,
    capture_children: bool,
}

fn idl_sections(text: &str, line_starts: &[usize], idl: IdlKind) -> Vec<Section> {
    let bytes = text.as_bytes();
    let mut decls: Vec<IdlDecl> = Vec::new();
    let mut stack: Vec<IdlScope> = Vec::new();
    let mut stmt_start: Option<usize> = None;
    let mut i = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };

    let capture_here = |stack: &[IdlScope]| {
        stack.is_empty()
            || (idl == IdlKind::Proto && stack.last().is_some_and(|s| s.capture_children))
    };

    while i < bytes.len() {
        let b = bytes[i];
        // Comments
        match idl {
            IdlKind::Proto | IdlKind::Prisma => {
                if bytes[i..].starts_with(b"//") {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                if bytes[i..].starts_with(b"/*") {
                    let Some(end) = text[i + 2..].find("*/") else {
                        return Vec::new();
                    };
                    i += 2 + end + 2;
                    continue;
                }
            }
            IdlKind::GraphQl => {
                if b == b'#' {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                if bytes[i..].starts_with(b"\"\"\"") {
                    let Some(end) = skip_graphql_block_string(bytes, i) else {
                        return Vec::new();
                    };
                    i = end;
                    continue;
                }
            }
        }
        // Quoted strings
        if b == b'"' || (idl == IdlKind::Proto && b == b'\'') {
            if capture_here(&stack) {
                stmt_start.get_or_insert(i);
            }
            let Some(next) = skip_quoted(bytes, i, b) else {
                return Vec::new();
            };
            i = next;
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // GraphQL brace-less `union` (and `extend union`) at depth 0
        if idl == IdlKind::GraphQl && stack.is_empty() && stmt_start.is_none() {
            if let Some((decl, next_pos)) = parse_graphql_union(text, i) {
                decls.push(decl);
                i = next_pos;
                continue;
            }
            if let Some(next_pos) = skip_graphql_braceless_decl(text, i) {
                i = next_pos;
                continue;
            }
        }

        match b {
            b';' if capture_here(&stack) => {
                stmt_start = None;
                i += 1;
            }
            b'{' => {
                let mut scope = IdlScope {
                    decl: None,
                    container_name: None,
                    capture_children: false,
                };
                if capture_here(&stack)
                    && let Some(start) = stmt_start.take()
                {
                    let raw_head = text[start..i].trim_end();
                    let cleaned = match idl {
                        IdlKind::GraphQl => strip_graphql_trivia(raw_head),
                        IdlKind::Proto | IdlKind::Prisma => strip_c_comments(raw_head, true),
                    };
                    let compact = compact_ws(&cleaned);
                    if let Some((kind, short_name)) =
                        parse_idl_header(idl, &compact, !stack.is_empty())
                    {
                        let parent = stack.iter().rev().find_map(|s| s.container_name.as_deref());
                        let qualified = match parent {
                            Some(p) => format!("{p}.{short_name}"),
                            None => short_name.clone(),
                        };
                        let sig_end = start + raw_head.len();
                        let is_proto_message = idl == IdlKind::Proto && kind == "message";
                        scope = IdlScope {
                            decl: Some((kind, qualified.clone(), short_name, start, sig_end)),
                            container_name: Some(qualified),
                            capture_children: is_proto_message && stack.is_empty(),
                        };
                    }
                }
                stack.push(scope);
                stmt_start = None;
                i += 1;
            }
            b'}' => {
                let Some(scope) = stack.pop() else {
                    return Vec::new();
                };
                if let Some((kind, name, short_name, start, sig_end)) = scope.decl {
                    decls.push(IdlDecl {
                        kind,
                        name,
                        short_name,
                        start,
                        sig_end,
                        end: i + 1,
                    });
                }
                stmt_start = None;
                i += 1;
            }
            _ => {
                if capture_here(&stack) {
                    stmt_start.get_or_insert(i);
                }
                i += 1;
            }
        }
    }

    if !stack.is_empty() || decls.len() < 2 {
        return Vec::new();
    }

    decls.sort_by_key(|d| (d.start, std::cmp::Reverse(d.end)));
    let mut out = Vec::with_capacity(decls.len());
    for decl in decls {
        let bytes = whole_line_or_exact(text, line_starts, decl.start, decl.end);
        let sig_end = decl.sig_end.min(bytes.end);
        let compact = compact_ws(&text[decl.start..sig_end]);
        let trivial =
            compact == format!("{} {}", decl.kind, decl.short_name) || compact == decl.kind;
        let header_end = if trivial { decl.start } else { sig_end };
        out.push(Section {
            name: decl.name,
            kind: decl.kind,
            bytes,
            signature_start: decl.start,
            header_end,
        });
    }
    out
}

fn parse_idl_header(idl: IdlKind, compact: &str, nested: bool) -> Option<(&'static str, String)> {
    let mut parts = compact.split_whitespace();
    let first = parts.next()?;
    match idl {
        IdlKind::Proto => {
            let kind = match first {
                "message" => "message",
                "enum" => "enum",
                "service" if !nested => "service",
                "oneof" if nested => "oneof",
                "extend" if !nested => "extend",
                _ => return None,
            };
            let name = parts.next()?;
            is_idl_ident(name).then(|| (kind, name.to_owned()))
        }
        IdlKind::GraphQl => {
            let keyword = if first == "extend" {
                parts.next()?
            } else {
                first
            };
            if keyword == "schema" {
                return Some(("schema", "schema".to_owned()));
            }
            let kind = match keyword {
                "type" => "type",
                "input" => "input",
                "interface" => "interface",
                "enum" => "enum",
                _ => return None,
            };
            let name = parts.next()?;
            is_idl_ident(name).then(|| (kind, name.to_owned()))
        }
        IdlKind::Prisma => {
            let kind = match first {
                "model" => "model",
                "enum" => "enum",
                "type" => "type",
                "view" => "view",
                "datasource" => "datasource",
                "generator" => "generator",
                _ => return None,
            };
            let name = parts.next()?;
            is_idl_ident(name).then(|| (kind, name.to_owned()))
        }
    }
}

fn is_idl_ident(s: &str) -> bool {
    let mut bytes = s.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

fn skip_graphql_block_string(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 3;
    while i + 3 <= bytes.len() {
        if bytes[i] == b'\\' && bytes[i + 1..].starts_with(b"\"\"\"") {
            i += 4;
            continue;
        }
        if bytes[i..].starts_with(b"\"\"\"") {
            return Some(i + 3);
        }
        i += 1;
    }
    None
}

fn strip_graphql_trivia(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for line in input.lines() {
        let code = line.split('#').next().unwrap_or(line);
        out.push_str(code);
        out.push(' ');
    }
    out
}

fn graphql_starts_keyword(rest: &str, keyword: &str) -> bool {
    rest.strip_prefix(keyword).is_some_and(|after| {
        after.is_empty() || after.starts_with(|c: char| c.is_whitespace() || c == '{')
    })
}

fn graphql_starts_decl(rest: &str) -> bool {
    [
        "type",
        "input",
        "interface",
        "enum",
        "union",
        "scalar",
        "schema",
        "directive",
        "extend",
    ]
    .iter()
    .any(|kw| graphql_starts_keyword(rest, kw))
}

fn parse_graphql_union(text: &str, start: usize) -> Option<(IdlDecl, usize)> {
    let rest = &text[start..];
    let after_kw = if let Some(tail) = rest.strip_prefix("extend")
        && tail.starts_with(char::is_whitespace)
    {
        let tail = tail.trim_start();
        tail.strip_prefix("union")
            .filter(|t| t.starts_with(char::is_whitespace))?
    } else {
        rest.strip_prefix("union")
            .filter(|t| t.starts_with(char::is_whitespace))?
    };
    let name = after_kw
        .split_whitespace()
        .next()
        .filter(|n| is_idl_ident(n))?;
    // Advance line by line through the union definition (handling multi-line `| Member`).
    let mut pos = start;
    let mut last_content_end = start;
    let mut first_line_end = None;
    let mut line_idx = 0usize;
    while pos < text.len() {
        let line_end = text[pos..].find('\n').map_or(text.len(), |n| pos + n);
        let raw_line = text[pos..line_end].trim_end_matches('\r');
        let trimmed = raw_line.trim();
        if line_idx > 0 {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                // Stop if we already saw `=` on an earlier line; comments/blanks end the union.
                if text[start..last_content_end].contains('=') {
                    break;
                }
            } else if trimmed.starts_with("\"\"\"")
                || graphql_starts_decl(trimmed)
                || trimmed.starts_with('{')
            {
                break;
            }
        }
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            last_content_end = pos + raw_line.trim_end().len();
        }
        if first_line_end.is_none() {
            first_line_end = Some(pos + raw_line.trim_end().len());
        }
        pos = (line_end + 1).min(text.len());
        line_idx += 1;
        // Single-line `union X = A | B` ends unless it trails with `=` or `|` or next line starts with `|`.
        if text[start..last_content_end].contains('=') {
            let trimmed_end = text[start..last_content_end].trim_end();
            let next_trimmed = text[pos..].trim_start();
            if !trimmed_end.ends_with(['=', '|']) && !next_trimmed.starts_with('|') {
                break;
            }
        }
    }
    let sig_end = first_line_end
        .unwrap_or(last_content_end)
        .min(last_content_end);
    Some((
        IdlDecl {
            kind: "union",
            name: name.to_owned(),
            short_name: name.to_owned(),
            start,
            sig_end,
            end: last_content_end,
        },
        pos,
    ))
}

fn skip_graphql_braceless_decl(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    if graphql_starts_keyword(rest, "scalar") || graphql_starts_keyword(rest, "directive") {
        let end = text[start..]
            .find('\n')
            .map_or(text.len(), |n| start + n + 1);
        return Some(end);
    }
    None
}

// ---------------------------------------------------------------------------
// XML / HTML / MSBuild (.csproj, .fsproj, .props, .targets, pom.xml)
// ---------------------------------------------------------------------------

struct XmlElem {
    tag: String,
    label: String,
    start: usize,
    open_tag_end: usize,
    end: usize,
    has_attrs: bool,
    parent: Option<usize>,
}

struct XmlOpen {
    elem_idx: usize,
    tag: String,
}

fn is_html_void(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn xml_sections(text: &str, line_starts: &[usize], is_html: bool) -> Vec<Section> {
    let bytes = text.as_bytes();
    let mut elems: Vec<XmlElem> = Vec::new();
    let mut stack: Vec<XmlOpen> = Vec::new();
    let mut i = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };

    while i < bytes.len() {
        let Some(rel) = text[i..].find('<') else {
            break;
        };
        i += rel;
        if bytes[i..].starts_with(b"<!--") {
            let Some(end) = text[i + 4..].find("-->") else {
                return Vec::new();
            };
            i += 4 + end + 3;
            continue;
        }
        if bytes[i..].starts_with(b"<![CDATA[") {
            let Some(end) = text[i + 9..].find("]]>") else {
                return Vec::new();
            };
            i += 9 + end + 3;
            continue;
        }
        if bytes[i..].starts_with(b"<?") {
            let Some(end) = text[i + 2..].find("?>") else {
                return Vec::new();
            };
            i += 2 + end + 2;
            continue;
        }
        if bytes[i..].starts_with(b"<!") {
            let Some(next) = skip_xml_declaration(bytes, i + 2) else {
                return Vec::new();
            };
            i = next;
            continue;
        }
        if bytes[i..].starts_with(b"</") {
            let name_start = i + 2;
            let mut j = name_start;
            while j < bytes.len() && is_xml_name_char(bytes[j]) {
                j += 1;
            }
            if j == name_start {
                return Vec::new();
            }
            let close_name = &text[name_start..j];
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) != Some(&b'>') {
                return Vec::new();
            }
            let close_end = j + 1;
            if is_html {
                if let Some(pos) = stack
                    .iter()
                    .rposition(|open| open.tag.eq_ignore_ascii_case(close_name))
                {
                    // Close the matched element (and any unclosed inner elements).
                    while stack.len() > pos {
                        let open = stack.pop().unwrap();
                        elems[open.elem_idx].end = close_end;
                    }
                }
            } else {
                let Some(open) = stack.pop() else {
                    return Vec::new();
                };
                if open.tag != close_name {
                    return Vec::new();
                }
                elems[open.elem_idx].end = close_end;
            }
            i = close_end;
            continue;
        }

        // Opening tag `<tag ...>` or `<tag ... />`
        let tag_start = i;
        let name_start = i + 1;
        if !bytes
            .get(name_start)
            .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_' || b == b':')
        {
            if is_html {
                i += 1;
                continue;
            }
            return Vec::new();
        }
        let mut j = name_start + 1;
        while j < bytes.len() && is_xml_name_char(bytes[j]) {
            j += 1;
        }
        let tag = text[name_start..j].to_owned();
        let Some((open_tag_end, self_closing, has_attrs, label_attr)) =
            scan_xml_open_tag(text, bytes, j)
        else {
            return Vec::new();
        };

        let label = match label_attr {
            Some(attr) => format!("{tag}:{attr}"),
            None => tag.clone(),
        };
        let parent = stack.last().map(|o| o.elem_idx);
        let elem_idx = elems.len();
        let void_or_self = self_closing || (is_html && is_html_void(&tag));

        if is_html
            && !void_or_self
            && (tag.eq_ignore_ascii_case("script") || tag.eq_ignore_ascii_case("style"))
        {
            let Some(close_end) = find_html_raw_close(text, open_tag_end, &tag) else {
                return Vec::new();
            };
            elems.push(XmlElem {
                tag,
                label,
                start: tag_start,
                open_tag_end,
                end: close_end,
                has_attrs,
                parent,
            });
            i = close_end;
            continue;
        }

        elems.push(XmlElem {
            tag: tag.clone(),
            label,
            start: tag_start,
            open_tag_end,
            end: if void_or_self { open_tag_end } else { 0 },
            has_attrs,
            parent,
        });
        if !void_or_self {
            stack.push(XmlOpen { elem_idx, tag });
        }
        i = open_tag_end;
    }

    if !is_html && !stack.is_empty() {
        return Vec::new();
    }

    // Build parent -> completed children map.
    let mut roots = Vec::new();
    let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); elems.len()];
    for (idx, elem) in elems.iter().enumerate() {
        if elem.end <= elem.start {
            continue;
        }
        match elem.parent {
            None => roots.push(idx),
            Some(p) => children_of[p].push(idx),
        }
    }
    if roots.is_empty() {
        return Vec::new();
    }
    if roots.len() == 1 && children_of[roots[0]].len() < 2 {
        return Vec::new();
    }

    let mut chosen: Vec<(usize, String)> = Vec::new();
    for &root_idx in &roots {
        chosen.push((root_idx, elems[root_idx].label.clone()));
        for &d1_idx in &children_of[root_idx] {
            let d1 = &elems[d1_idx];
            chosen.push((d1_idx, d1.label.clone()));
            let d1_span = line_of(line_starts, d1.end.saturating_sub(1))
                .saturating_sub(line_of(line_starts, d1.start))
                + 1;
            let kids = &children_of[d1_idx];
            if d1_span >= 10 && should_expand_xml_children(d1, kids, &elems, line_starts) {
                for &d2_idx in kids {
                    chosen.push((d2_idx, format!("{}.{}", d1.label, elems[d2_idx].label)));
                }
            }
        }
    }

    if chosen.len() < 2 {
        return Vec::new();
    }
    let distinct_lines: BTreeSet<usize> = chosen
        .iter()
        .map(|&(idx, _)| line_of(line_starts, elems[idx].start))
        .collect();
    if distinct_lines.len() < 2 && text.len() <= MINIFIED_THRESHOLD {
        return Vec::new();
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::with_capacity(chosen.len());
    for (idx, name) in chosen {
        let elem = &elems[idx];
        let bytes = whole_line_or_exact(text, line_starts, elem.start, elem.end);
        let first_line = line_of(line_starts, elem.start);
        let last_line = line_of(line_starts, elem.end.saturating_sub(1));
        let header_end = if first_line == last_line {
            char_floor(text, elem.start + 120)
                .min(offset(first_line + 1))
                .min(bytes.end)
        } else if elem.has_attrs {
            elem.open_tag_end.min(offset(first_line + 1)).min(bytes.end)
        } else {
            elem.start
        };
        out.push(Section {
            name,
            kind: "element",
            bytes,
            signature_start: elem.start,
            header_end,
        });
    }
    out
}

fn should_expand_xml_children(
    parent: &XmlElem,
    kids: &[usize],
    elems: &[XmlElem],
    line_starts: &[usize],
) -> bool {
    if kids.is_empty() || kids.len() > 64 {
        return false;
    }
    if kids.len() == 1 {
        let child = &elems[kids[0]];
        let span = line_of(line_starts, child.end.saturating_sub(1))
            .saturating_sub(line_of(line_starts, child.start))
            + 1;
        return span >= 4 && !child.tag.eq_ignore_ascii_case(&parent.tag);
    }
    let distinct_labels: BTreeSet<&str> = kids.iter().map(|&i| elems[i].label.as_str()).collect();
    if distinct_labels.len() < 2 {
        return false;
    }
    let mut multiline = 0usize;
    let mut any_large = false;
    for &kid in kids {
        let elem = &elems[kid];
        let span = line_of(line_starts, elem.end.saturating_sub(1))
            .saturating_sub(line_of(line_starts, elem.start))
            + 1;
        if span >= 2 {
            multiline += 1;
        }
        if span >= 4 {
            any_large = true;
        }
    }
    multiline >= 2 || any_large
}

fn is_xml_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.')
}

fn skip_xml_declaration(bytes: &[u8], mut i: usize) -> Option<usize> {
    let mut bracket_depth = 0usize;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'[' => bracket_depth += 1,
                b']' => bracket_depth = bracket_depth.saturating_sub(1),
                b'>' if bracket_depth == 0 => return Some(i + 1),
                _ => {}
            },
        }
        i += 1;
    }
    None
}

fn scan_xml_open_tag(
    text: &str,
    bytes: &[u8],
    mut i: usize,
) -> Option<(usize, bool, bool, Option<String>)> {
    let mut has_attrs = false;
    let mut best_label: Option<(u8, String)> = None;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'>' {
            return Some((i + 1, false, has_attrs, best_label.map(|(_, v)| v)));
        }
        if bytes[i..].starts_with(b"/>") {
            return Some((i + 2, true, has_attrs, best_label.map(|(_, v)| v)));
        }
        // Attribute name
        let attr_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && !matches!(bytes[i], b'=' | b'>' | b'/')
        {
            i += 1;
        }
        if i == attr_start {
            return None;
        }
        has_attrs = true;
        let attr_name = &text[attr_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
            // Boolean HTML attribute (e.g. `disabled`, `defer`)
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let val = if let Some(&q @ (b'"' | b'\'')) = bytes.get(i) {
            let val_start = i + 1;
            i += 1;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            if i >= bytes.len() {
                return None;
            }
            let val = &text[val_start..i];
            i += 1;
            val
        } else {
            let val_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && !matches!(bytes[i], b'>' | b'/')
            {
                i += 1;
            }
            &text[val_start..i]
        };
        let priority = if attr_name.eq_ignore_ascii_case("id") {
            Some(0)
        } else if attr_name.eq_ignore_ascii_case("name") {
            Some(1)
        } else if attr_name.eq_ignore_ascii_case("label") {
            Some(2)
        } else if attr_name.eq_ignore_ascii_case("key") {
            Some(3)
        } else {
            None
        };
        if let Some(p) = priority
            && best_label.as_ref().is_none_or(|(cur, _)| p < *cur)
        {
            let trimmed = val.trim();
            if !trimmed.is_empty() && trimmed.len() <= 60 && !trimmed.contains(['\n', '\r']) {
                best_label = Some((p, trimmed.to_owned()));
            }
        }
    }
    None
}

fn find_html_raw_close(text: &str, start: usize, tag: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        let rel = text[i..].find("</")?;
        let close_start = i + rel;
        let name_start = close_start + 2;
        let name_end = name_start + tag.len();
        if name_end <= bytes.len() && text[name_start..name_end].eq_ignore_ascii_case(tag) {
            let mut j = name_end;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'>') {
                return Some(j + 1);
            }
        }
        i = close_start + 2;
    }
    None
}

// ---------------------------------------------------------------------------
// Nix (*.nix flakes, modules, derivations, libraries)
// ---------------------------------------------------------------------------

struct NixScan<'a> {
    src: &'a str,
    bytes: &'a [u8],
    /// Maps the opening byte of every `{`, `[`, `(`, `"`, `''`, `#`, `/*` to
    /// its exclusive closing byte (for strings/comments) or exact closing token
    /// byte index (for `{`, `[`, `(`).
    pair_end: BTreeMap<usize, usize>,
    trivia_end: BTreeMap<usize, usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NixFrame {
    Brace(usize),
    Bracket(usize),
    Paren(usize),
    DoubleQuote(usize),
    IndentedString(usize),
    InterpBrace,
}

impl<'a> NixScan<'a> {
    fn parse(src: &'a str) -> Option<Self> {
        let bytes = src.as_bytes();
        let mut pair_end = BTreeMap::new();
        let mut trivia_end = BTreeMap::new();
        let mut stack: Vec<NixFrame> = Vec::new();
        let mut i = if src.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };

        while i < bytes.len() {
            match stack.last().copied() {
                Some(NixFrame::DoubleQuote(str_start)) => match bytes[i] {
                    b'\\' => {
                        i += 2;
                        if i > bytes.len() {
                            return None;
                        }
                    }
                    b'$' if bytes.get(i + 1) == Some(&b'{') => {
                        stack.push(NixFrame::InterpBrace);
                        i += 2;
                    }
                    b'"' => {
                        stack.pop();
                        i += 1;
                        if stack.is_empty()
                            || matches!(
                                stack.last(),
                                Some(
                                    NixFrame::Brace(_)
                                        | NixFrame::Bracket(_)
                                        | NixFrame::Paren(_)
                                        | NixFrame::InterpBrace
                                )
                            )
                        {
                            pair_end.insert(str_start, i);
                        }
                    }
                    _ => i += 1,
                },
                Some(NixFrame::IndentedString(str_start)) => {
                    if bytes[i..].starts_with(b"'''") || bytes[i..].starts_with(b"''$") {
                        i += 3;
                    } else if bytes[i..].starts_with(b"''\\") {
                        i += 3;
                        if i < bytes.len() {
                            i += 1;
                        } else {
                            return None;
                        }
                    } else if bytes[i..].starts_with(b"${") {
                        stack.push(NixFrame::InterpBrace);
                        i += 2;
                    } else if bytes[i..].starts_with(b"''") {
                        stack.pop();
                        i += 2;
                        pair_end.insert(str_start, i);
                    } else {
                        i += 1;
                    }
                }
                _ => {
                    if bytes[i] == b'#' {
                        let start = i;
                        i += 1;
                        while i < bytes.len() && bytes[i] != b'\n' {
                            i += 1;
                        }
                        trivia_end.insert(start, i);
                        continue;
                    }
                    if bytes[i..].starts_with(b"/*") {
                        let start = i;
                        let end = src[i + 2..].find("*/")?;
                        i += 2 + end + 2;
                        trivia_end.insert(start, i);
                        continue;
                    }
                    if bytes[i] == b'"' {
                        stack.push(NixFrame::DoubleQuote(i));
                        i += 1;
                        continue;
                    }
                    if bytes[i..].starts_with(b"''") {
                        stack.push(NixFrame::IndentedString(i));
                        i += 2;
                        continue;
                    }
                    match bytes[i] {
                        b'{' => {
                            stack.push(NixFrame::Brace(i));
                            i += 1;
                        }
                        b'}' => match stack.pop()? {
                            NixFrame::Brace(open) => {
                                pair_end.insert(open, i);
                                i += 1;
                            }
                            NixFrame::InterpBrace => {
                                i += 1;
                            }
                            _ => return None,
                        },
                        b'[' => {
                            stack.push(NixFrame::Bracket(i));
                            i += 1;
                        }
                        b']' => match stack.pop()? {
                            NixFrame::Bracket(open) => {
                                pair_end.insert(open, i);
                                i += 1;
                            }
                            _ => return None,
                        },
                        b'(' => {
                            stack.push(NixFrame::Paren(i));
                            i += 1;
                        }
                        b')' => match stack.pop()? {
                            NixFrame::Paren(open) => {
                                pair_end.insert(open, i);
                                i += 1;
                            }
                            _ => return None,
                        },
                        _ => i += 1,
                    }
                }
            }
        }

        if !stack.is_empty() {
            return None;
        }
        Some(Self {
            src,
            bytes,
            pair_end,
            trivia_end,
        })
    }

    fn skip_trivia(&self, mut pos: usize, limit: usize) -> usize {
        while pos < limit {
            if self.bytes[pos].is_ascii_whitespace() {
                pos += 1;
            } else if let Some(&end) = self.trivia_end.get(&pos) {
                pos = end.min(limit);
            } else {
                break;
            }
        }
        pos
    }

    fn keyword_at(&self, pos: usize, limit: usize, kw: &str) -> Option<usize> {
        let end = pos + kw.len();
        if end > limit || !self.bytes[pos..end].eq(kw.as_bytes()) {
            return None;
        }
        if pos > 0 && is_nix_ident_cont(self.bytes[pos - 1]) {
            return None;
        }
        if end < limit && is_nix_ident_cont(self.bytes[end]) {
            return None;
        }
        Some(end)
    }

    fn read_ident(&self, pos: usize, limit: usize) -> Option<(&'a str, usize)> {
        if pos >= limit || !is_nix_ident_start(self.bytes[pos]) {
            return None;
        }
        let mut end = pos + 1;
        while end < limit && is_nix_ident_cont(self.bytes[end]) {
            end += 1;
        }
        Some((&self.src[pos..end], end))
    }

    /// Advance past one balanced token (comment, string, `{...}`, `[...]`, `(...)`,
    /// identifier, or single byte).
    fn step_balanced(&self, pos: usize, limit: usize) -> usize {
        if pos >= limit {
            return limit;
        }
        if let Some(&end) = self.trivia_end.get(&pos) {
            return end.min(limit);
        }
        match self.bytes[pos] {
            b'{' | b'[' | b'(' => {
                if let Some(&close) = self.pair_end.get(&pos) {
                    return (close + 1).min(limit);
                }
            }
            b'"' => {
                if let Some(&end) = self.pair_end.get(&pos) {
                    return end.min(limit);
                }
            }
            b'\'' if self.bytes[pos..].starts_with(b"''") => {
                if let Some(&end) = self.pair_end.get(&pos) {
                    return end.min(limit);
                }
            }
            _ => {}
        }
        if is_nix_ident_start(self.bytes[pos]) {
            let mut end = pos + 1;
            while end < limit && is_nix_ident_cont(self.bytes[end]) {
                end += 1;
            }
            return end;
        }
        pos + 1
    }

    /// Peel one Nix lambda parameter header (`x:`, `{ a, b, ... }:`,
    /// `{ a, ... }@inputs:`, `inputs@{ a, ... }:`) at `pos`.
    fn peel_lambda_header(&self, pos: usize, limit: usize) -> Option<usize> {
        let p = self.skip_trivia(pos, limit);
        if p >= limit {
            return None;
        }
        if self.bytes[p] == b'{' {
            let close = *self.pair_end.get(&p)?;
            if close >= limit {
                return None;
            }
            let mut after = self.skip_trivia(close + 1, limit);
            if after < limit && self.bytes[after] == b'@' {
                after = self.skip_trivia(after + 1, limit);
                let (_, next) = self.read_ident(after, limit)?;
                after = self.skip_trivia(next, limit);
            }
            if after < limit && self.bytes[after] == b':' {
                return Some(self.skip_trivia(after + 1, limit));
            }
            return None;
        }
        if let Some((ident, next)) = self.read_ident(p, limit) {
            if matches!(
                ident,
                "let" | "in" | "rec" | "with" | "if" | "then" | "else" | "assert" | "inherit"
            ) {
                return None;
            }
            let after = self.skip_trivia(next, limit);
            if after < limit && self.bytes[after] == b':' {
                // Reject URIs like `https://...` or `github:...` where `:` is
                // immediately followed by a non-whitespace path character.
                let next_byte = self.bytes.get(after + 1).copied();
                if matches!(
                    next_byte,
                    None | Some(b' ' | b'\t' | b'\r' | b'\n' | b'#' | b'/' | b'{' | b'(' | b'[')
                ) && !self.bytes[after..].starts_with(b"://")
                {
                    return Some(self.skip_trivia(after + 1, limit));
                }
            }
            if after < limit && self.bytes[after] == b'@' {
                let brace = self.skip_trivia(after + 1, limit);
                if brace < limit && self.bytes[brace] == b'{' {
                    let close = *self.pair_end.get(&brace)?;
                    if close >= limit {
                        return None;
                    }
                    let colon = self.skip_trivia(close + 1, limit);
                    if colon < limit && self.bytes[colon] == b':' {
                        return Some(self.skip_trivia(colon + 1, limit));
                    }
                }
            }
        }
        None
    }

    fn is_lambda(&self, start: usize, end: usize) -> bool {
        self.peel_lambda_header(start, end).is_some()
    }
}

fn is_nix_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_nix_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'\'' | b'-')
}

struct NixBinding {
    name: String,
    start: usize,
    val_start: usize,
    val_end: usize,
    end: usize,
}

fn parse_nix_attrpath(scan: &NixScan<'_>, mut pos: usize, limit: usize) -> Option<(String, usize)> {
    let mut parts = Vec::new();
    loop {
        pos = scan.skip_trivia(pos, limit);
        if pos >= limit {
            return None;
        }
        if scan.bytes[pos] == b'"' {
            let end = *scan.pair_end.get(&pos)?;
            if end > limit {
                return None;
            }
            parts.push(scan.src[pos + 1..end - 1].to_owned());
            pos = end;
        } else if scan.bytes[pos..].starts_with(b"${") {
            let close = *scan.pair_end.get(&(pos + 1))?;
            if close >= limit {
                return None;
            }
            parts.push(compact_ws(&scan.src[pos..=close]));
            pos = close + 1;
        } else if let Some((ident, next)) = scan.read_ident(pos, limit) {
            if parts.is_empty() && matches!(ident, "inherit" | "in" | "let") {
                return None;
            }
            parts.push(ident.to_owned());
            pos = next;
        } else {
            return None;
        }
        let after = scan.skip_trivia(pos, limit);
        if after < limit && scan.bytes[after] == b'.' {
            pos = after + 1;
        } else {
            return Some((parts.join("."), pos));
        }
    }
}

fn find_nix_binding_semi(scan: &NixScan<'_>, mut pos: usize, limit: usize) -> Option<usize> {
    let mut let_depth = 0usize;
    while pos < limit {
        pos = scan.skip_trivia(pos, limit);
        if pos >= limit {
            break;
        }
        if let Some(after) = scan.keyword_at(pos, limit, "let") {
            let_depth += 1;
            pos = after;
            continue;
        }
        if let Some(after) = scan.keyword_at(pos, limit, "in") {
            if let_depth == 0 {
                return None;
            }
            let_depth -= 1;
            pos = after;
            continue;
        }
        if scan.bytes[pos] == b';' && let_depth == 0 {
            return Some(pos);
        }
        pos = scan.step_balanced(pos, limit);
    }
    None
}

fn parse_nix_bindings_until(
    scan: &NixScan<'_>,
    mut pos: usize,
    limit: usize,
    stop_on_in: bool,
) -> Option<(Vec<NixBinding>, usize)> {
    let mut bindings = Vec::new();
    loop {
        pos = scan.skip_trivia(pos, limit);
        if pos >= limit {
            return if stop_on_in {
                None
            } else {
                Some((bindings, pos))
            };
        }
        if stop_on_in && let Some(after_in) = scan.keyword_at(pos, limit, "in") {
            return Some((bindings, after_in));
        }
        if scan.bytes[pos] == b';' {
            pos += 1;
            continue;
        }
        if let Some(after_inherit) = scan.keyword_at(pos, limit, "inherit") {
            let semi = find_nix_binding_semi(scan, after_inherit, limit)?;
            pos = semi + 1;
            continue;
        }
        let binding_start = pos;
        let Some((name, after_path)) = parse_nix_attrpath(scan, pos, limit) else {
            // Skip unrecognized statement up to next `;` or bail cleanly on `let`.
            let semi = find_nix_binding_semi(scan, pos, limit)?;
            pos = semi + 1;
            continue;
        };
        let eq = scan.skip_trivia(after_path, limit);
        if eq >= limit || scan.bytes[eq] != b'=' || scan.bytes.get(eq + 1) == Some(&b'=') {
            return None;
        }
        let val_start = scan.skip_trivia(eq + 1, limit);
        let semi = find_nix_binding_semi(scan, val_start, limit)?;
        let val_end = scan.src[val_start..semi].trim_end().len() + val_start;
        bindings.push(NixBinding {
            name,
            start: binding_start,
            val_start,
            val_end,
            end: semi + 1,
        });
        pos = semi + 1;
    }
}

fn is_nix_major_section(name: &str) -> bool {
    let root = name.split('.').next().unwrap_or(name);
    matches!(
        root,
        "description"
            | "inputs"
            | "outputs"
            | "nixConfig"
            | "options"
            | "config"
            | "imports"
            | "packages"
            | "devShells"
            | "nixosConfigurations"
            | "darwinConfigurations"
            | "homeConfigurations"
            | "nixosModules"
            | "homeModules"
            | "overlays"
            | "checks"
            | "apps"
            | "formatter"
            | "lib"
            | "perSystem"
    )
}

fn is_nix_expandable_parent(name: &str) -> bool {
    if matches!(
        name,
        "packages"
            | "devShells"
            | "nixosConfigurations"
            | "darwinConfigurations"
            | "homeConfigurations"
            | "nixosModules"
            | "homeModules"
            | "overlays"
            | "checks"
            | "apps"
            | "lib"
            | "options"
            | "config"
            | "perSystem"
            | "services"
            | "programs"
    ) {
        return true;
    }
    if let Some((head, tail)) = name.split_once('.') {
        return matches!(head, "packages" | "devShells" | "checks" | "apps")
            && !tail.contains('.')
            && tail.contains('-');
    }
    false
}

fn nix_split_merge_operands(scan: &NixScan<'_>, start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut parts = Vec::new();
    let mut seg_start = start;
    let mut pos = start;
    let mut let_depth = 0usize;
    while pos < end {
        pos = scan.skip_trivia(pos, end);
        if pos >= end {
            break;
        }
        if let Some(after) = scan.keyword_at(pos, end, "let") {
            let_depth += 1;
            pos = after;
            continue;
        }
        if let Some(after) = scan.keyword_at(pos, end, "in") {
            let_depth = let_depth.saturating_sub(1);
            pos = after;
            continue;
        }
        if let_depth == 0 && pos + 1 < end && &scan.bytes[pos..pos + 2] == b"//" {
            parts.push((seg_start, pos));
            pos += 2;
            seg_start = pos;
            continue;
        }
        pos = scan.step_balanced(pos, end);
    }
    if !parts.is_empty() {
        parts.push((seg_start, end));
    }
    parts
}

fn nix_find_body_expr(scan: &NixScan<'_>, start: usize, end: usize) -> Option<(usize, usize)> {
    let mut pos = scan.skip_trivia(start, end);
    let mut last_brace: Option<(usize, usize)> = None;
    let mut last_paren: Option<(usize, usize)> = None;
    while pos < end {
        pos = scan.skip_trivia(pos, end);
        if pos >= end {
            break;
        }
        if scan.bytes[pos] == b'{'
            && let Some(&close) = scan.pair_end.get(&pos)
            && close < end
        {
            last_brace = Some((pos, close + 1));
            pos = close + 1;
            continue;
        }
        if scan.bytes[pos] == b'('
            && let Some(&close) = scan.pair_end.get(&pos)
            && close < end
        {
            let inner = scan.skip_trivia(pos + 1, close);
            if scan.is_lambda(inner, close)
                || scan.keyword_at(inner, close, "let").is_some()
                || scan.bytes.get(inner) == Some(&b'{')
            {
                last_paren = Some((pos + 1, close));
            }
            pos = close + 1;
            continue;
        }
        pos = scan.step_balanced(pos, end);
    }
    last_paren.or(last_brace)
}

fn emit_nix_binding(
    text: &str,
    line_starts: &[usize],
    b: &NixBinding,
    name: String,
    kind: &'static str,
    out: &mut Vec<Section>,
) {
    let bytes = whole_line_or_exact(text, line_starts, b.start, b.end);
    let first_line = line_of(line_starts, b.start);
    let line_end = line_offset(line_starts, text.len(), first_line + 1).min(bytes.end);
    let first_text = text[b.start..line_end].trim();
    let redundant_open = first_text == format!("{} = {{", b.name)
        || first_text == format!("{} = rec {{", b.name)
        || first_text == format!("{} =", b.name);
    let header_end = if redundant_open { b.start } else { line_end };
    out.push(Section {
        name: clip_label(&name, 96),
        kind,
        bytes,
        signature_start: b.start,
        header_end,
    });
}

fn extract_nix_expr(
    scan: &NixScan<'_>,
    line_starts: &[usize],
    span: Range<usize>,
    prefix: Option<&str>,
    depth: usize,
    emit_lets: bool,
    out: &mut Vec<Section>,
) -> Option<()> {
    if depth > 4 {
        return Some(());
    }
    let (mut start, mut end) = (span.start, span.end);
    loop {
        start = scan.skip_trivia(start, end);
        if start >= end {
            return Some(());
        }
        if let Some(after) = scan.peel_lambda_header(start, end) {
            start = after;
            continue;
        }
        if let Some(after_let) = scan.keyword_at(start, end, "let") {
            let (lets, after_in) = parse_nix_bindings_until(scan, after_let, end, true)?;
            if emit_lets {
                for b in &lets {
                    emit_nix_binding(scan.src, line_starts, b, b.name.clone(), "let", out);
                }
            }
            start = after_in;
            continue;
        }
        if let Some(after_with) = scan.keyword_at(start, end, "with") {
            let semi = find_nix_binding_semi(scan, after_with, end)?;
            start = semi + 1;
            continue;
        }
        if scan.bytes[start] == b'('
            && let Some(&close) = scan.pair_end.get(&start)
            && close < end
            && scan.skip_trivia(close + 1, end) >= end
        {
            start += 1;
            end = close;
            continue;
        }
        break;
    }

    let merge_parts = nix_split_merge_operands(scan, start, end);
    if !merge_parts.is_empty() {
        for (pstart, pend) in merge_parts {
            extract_nix_expr(
                scan,
                line_starts,
                pstart..pend,
                prefix,
                depth,
                emit_lets,
                out,
            )?;
        }
        return Some(());
    }

    let brace_pos = if let Some(after_rec) = scan.keyword_at(start, end, "rec") {
        let p = scan.skip_trivia(after_rec, end);
        (p < end && scan.bytes[p] == b'{').then_some(p)
    } else if start < end && scan.bytes[start] == b'{' {
        Some(start)
    } else {
        None
    };

    let Some(open_brace) = brace_pos else {
        if let Some((inner_start, inner_end)) = nix_find_body_expr(scan, start, end) {
            extract_nix_expr(
                scan,
                line_starts,
                inner_start..inner_end,
                prefix,
                depth,
                emit_lets,
                out,
            )?;
        }
        return Some(());
    };

    let close_brace = *scan.pair_end.get(&open_brace)?;
    if close_brace >= end {
        return None;
    }
    let (bindings, _) = parse_nix_bindings_until(scan, open_brace + 1, close_brace, false)?;
    for b in &bindings {
        let full_name = match prefix {
            Some(p) => format!("{p}.{}", b.name),
            None => b.name.clone(),
        };
        let is_fn = scan.is_lambda(b.val_start, b.val_end);
        let kind = if b.name == "outputs" && prefix.is_none() {
            "section"
        } else if is_fn {
            "function"
        } else if prefix.is_none() && is_nix_major_section(&b.name) && !b.name.contains('.') {
            "section"
        } else {
            "attr"
        };
        emit_nix_binding(scan.src, line_starts, b, full_name.clone(), kind, out);

        if b.name == "outputs" && prefix.is_none() {
            extract_nix_expr(
                scan,
                line_starts,
                b.val_start..b.val_end,
                None,
                depth + 1,
                true,
                out,
            )?;
        } else if depth < 2 && !is_fn {
            let span_lines = line_of(line_starts, b.end.saturating_sub(1))
                .saturating_sub(line_of(line_starts, b.start))
                + 1;
            let expandable = is_nix_expandable_parent(&full_name) || span_lines >= 8;
            if expandable {
                let mut child_out = Vec::new();
                if extract_nix_expr(
                    scan,
                    line_starts,
                    b.val_start..b.val_end,
                    Some(&full_name),
                    depth + 1,
                    false,
                    &mut child_out,
                )
                .is_some()
                {
                    let keep = if is_nix_expandable_parent(&full_name) {
                        !child_out.is_empty() && child_out.len() <= 64
                    } else {
                        (2..=64).contains(&child_out.len())
                    };
                    if keep {
                        out.extend(child_out);
                    }
                }
            }
        }
    }
    Some(())
}

fn nix_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    if line_starts.len() < 2 {
        return Vec::new();
    }
    let Some(scan) = NixScan::parse(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if extract_nix_expr(&scan, line_starts, 0..text.len(), None, 0, true, &mut out).is_none() {
        return Vec::new();
    }
    if out.len() < 2 {
        return Vec::new();
    }
    out
}

// ---------------------------------------------------------------------------
// Nginx (nginx.conf, *.nginx, *.conf fallback)
// ---------------------------------------------------------------------------

struct BraceBlock {
    start: usize,
    open_brace: usize,
    end: usize,
    children: Vec<BraceBlock>,
}

fn parse_nginx_blocks(text: &str) -> Option<Vec<BraceBlock>> {
    let bytes = text.as_bytes();
    let mut i = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let mut stack: Vec<(usize, usize, Vec<BraceBlock>)> = Vec::new();
    let mut roots: Vec<BraceBlock> = Vec::new();
    let mut stmt_start: Option<usize> = None;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if matches!(b, b'"' | b'\'') {
            stmt_start.get_or_insert(i);
            i = skip_quoted(bytes, i, b)?;
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match b {
            b';' => {
                stmt_start = None;
                i += 1;
            }
            b'{' => {
                let start = stmt_start.take()?;
                stack.push((start, i, Vec::new()));
                i += 1;
            }
            b'}' => {
                let (start, open_brace, children) = stack.pop()?;
                let block = BraceBlock {
                    start,
                    open_brace,
                    end: i + 1,
                    children,
                };
                if let Some(parent) = stack.last_mut() {
                    parent.2.push(block);
                } else {
                    roots.push(block);
                }
                stmt_start = None;
                i += 1;
            }
            _ => {
                stmt_start.get_or_insert(i);
                i += 1;
            }
        }
    }
    if !stack.is_empty() {
        return None;
    }
    Some(roots)
}

fn nginx_clean_prelude(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let stripped = strip_hash_comment(line);
        out.push_str(stripped);
        out.push(' ');
    }
    compact_ws(&out)
}

/// Scan direct (non-nested) simple directives inside an Nginx block body.
fn nginx_direct_directives(text: &str, block: &BraceBlock) -> Vec<(usize, String, String)> {
    let bytes = text.as_bytes();
    let body_start = block.open_brace + 1;
    let body_end = block.end.saturating_sub(1);
    let mut directives = Vec::new();
    let mut i = body_start;
    let mut depth = 0usize;
    let mut stmt_start: Option<usize> = None;

    while i < body_end {
        let b = bytes[i];
        if b == b'#' {
            while i < body_end && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if matches!(b, b'"' | b'\'') {
            if depth == 0 {
                stmt_start.get_or_insert(i);
            }
            let Some(next) = skip_quoted(bytes, i, b) else {
                break;
            };
            i = next.min(body_end);
            continue;
        }
        match b {
            b'{' => {
                if depth == 0 {
                    stmt_start = None;
                }
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b';' if depth == 0 => {
                if let Some(start) = stmt_start.take() {
                    let clean = nginx_clean_prelude(&text[start..i]);
                    let mut parts = clean.splitn(2, char::is_whitespace);
                    if let Some(kw) = parts.next()
                        && !kw.is_empty()
                    {
                        let args = parts.next().unwrap_or_default().trim().to_owned();
                        directives.push((start, kw.to_ascii_lowercase(), args));
                    }
                }
                i += 1;
            }
            _ => {
                if depth == 0 && !b.is_ascii_whitespace() {
                    stmt_start.get_or_insert(i);
                }
                i += 1;
            }
        }
    }
    directives
}

fn emit_nginx_blocks(
    text: &str,
    line_starts: &[usize],
    blocks: &[BraceBlock],
    server_count: &mut usize,
    out: &mut Vec<Section>,
) {
    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    for block in blocks {
        let prelude = nginx_clean_prelude(&text[block.start..block.open_brace]);
        let mut parts = prelude.splitn(2, char::is_whitespace);
        let kw = parts.next().unwrap_or_default().to_ascii_lowercase();
        let args = parts.next().unwrap_or_default().trim();

        let bytes = whole_line_or_exact(text, line_starts, block.start, block.end);
        let mut sig_start = block.start;
        let mut header_end = block.start;

        let emitted = match kw.as_str() {
            "http" | "events" | "stream" | "mail" | "types" => Some(("block", kw.clone())),
            "upstream" if !args.is_empty() => Some(("upstream", clip_label(args, 80))),
            "server" => {
                *server_count += 1;
                let directives = nginx_direct_directives(text, block);
                let server_name = directives
                    .iter()
                    .find(|(_, d, a)| d == "server_name" && !a.is_empty())
                    .map(|(_, _, a)| a.clone());
                let listen = directives
                    .iter()
                    .find(|(_, d, a)| d == "listen" && !a.is_empty());
                let name = match (server_name, listen) {
                    (Some(sn), Some((lpos, _, _))) => {
                        let lline = line_of(line_starts, *lpos);
                        let ls = offset(lline);
                        let le = offset(lline + 1).min(bytes.end);
                        if ls >= bytes.start && le >= ls {
                            sig_start = ls;
                            header_end = le;
                        }
                        clip_label(&sn, 72)
                    }
                    (Some(sn), None) => clip_label(&sn, 72),
                    (None, Some((_, _, largs))) => clip_label(largs, 72),
                    (None, None) => format!("server-{server_count}"),
                };
                Some(("server", name))
            }
            "location" if !args.is_empty() => {
                let directives = nginx_direct_directives(text, block);
                let target = directives
                    .iter()
                    .find(|(_, d, _)| {
                        matches!(
                            d.as_str(),
                            "proxy_pass" | "grpc_pass" | "fastcgi_pass" | "uwsgi_pass"
                        )
                    })
                    .or_else(|| directives.iter().find(|(_, d, _)| d == "return"));
                if let Some((pos, _, _)) = target {
                    let line = line_of(line_starts, *pos);
                    let ls = offset(line);
                    let le = offset(line + 1).min(bytes.end);
                    if ls >= bytes.start && le >= ls {
                        sig_start = ls;
                        header_end = le;
                    }
                }
                Some(("location", clip_label(args, 80)))
            }
            "map" | "geo" | "split_clients" | "limit_except" if !args.is_empty() => {
                Some(("block", clip_label(&prelude, 80)))
            }
            _ => None,
        };

        if let Some((kind, name)) = emitted {
            out.push(Section {
                name,
                kind,
                bytes,
                signature_start: sig_start,
                header_end,
            });
        }
        if !block.children.is_empty() {
            emit_nginx_blocks(text, line_starts, &block.children, server_count, out);
        }
    }
}

fn nginx_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    if line_starts.len() < 2 {
        return Vec::new();
    }
    let Some(blocks) = parse_nginx_blocks(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut server_count = 0usize;
    emit_nginx_blocks(text, line_starts, &blocks, &mut server_count, &mut out);
    if out.len() < 2 {
        return Vec::new();
    }
    out
}

// ---------------------------------------------------------------------------
// Caddy (Caddyfile, Caddyfile.*, *.caddyfile, *.caddy)
// ---------------------------------------------------------------------------

fn skip_caddy_heredoc(text: &str, start: usize) -> Option<usize> {
    // `<<MARKER\n...\n<indent>MARKER`
    let after = start + 2;
    let line_end = text[after..].find('\n').map_or(text.len(), |n| after + n);
    let marker = text[after..line_end].trim();
    if marker.is_empty()
        || !marker
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    let mut pos = (line_end + 1).min(text.len());
    while pos < text.len() {
        let next_nl = text[pos..].find('\n').map_or(text.len(), |n| pos + n);
        let line = text[pos..next_nl].trim_end_matches('\r');
        if line
            .trim()
            .strip_prefix(marker)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return Some((next_nl + 1).min(text.len()));
        }
        pos = (next_nl + 1).min(text.len());
    }
    None
}

fn is_caddy_block_brace(bytes: &[u8], idx: usize) -> bool {
    let prev_ok = idx == 0 || bytes[idx - 1].is_ascii_whitespace();
    let next_ok = bytes
        .get(idx + 1)
        .is_none_or(|&b| b.is_ascii_whitespace() || b == b'#');
    prev_ok && next_ok
}

fn parse_caddy_blocks(text: &str) -> Option<Vec<BraceBlock>> {
    let bytes = text.as_bytes();
    let mut i = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let mut stack: Vec<(usize, usize, Vec<BraceBlock>)> = Vec::new();
    let mut roots: Vec<BraceBlock> = Vec::new();
    let mut stmt_start: Option<usize> = None;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'"' {
            stmt_start.get_or_insert(i);
            i = skip_quoted(bytes, i, b'"')?;
            continue;
        }
        if b == b'`' {
            stmt_start.get_or_insert(i);
            let close = text[i + 1..].find('`')?;
            i += 1 + close + 1;
            continue;
        }
        if bytes[i..].starts_with(b"<<") {
            stmt_start.get_or_insert(i);
            i = skip_caddy_heredoc(text, i)?;
            stmt_start = None;
            continue;
        }
        if b == b'\n' {
            // In Caddyfile, top-level site lists may continue across lines with a
            // trailing comma; otherwise a newline without `{` finishes a single-line
            // directive inside a block.
            if !stack.is_empty() {
                stmt_start = None;
            } else if let Some(start) = stmt_start {
                let prev = text[start..i].trim_end();
                if !prev.ends_with(',') && !prev.ends_with('\\') {
                    // Single-line directive at top level (not a block header) resets
                    // unless the next non-empty line is `{` (not typical in Caddy).
                    stmt_start = None;
                }
            }
            i += 1;
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Inline Caddy placeholder like `{http.request.uri}` or `{$PORT}`:
        if b == b'{' && !is_caddy_block_brace(bytes, i) {
            stmt_start.get_or_insert(i);
            i += 1;
            while i < bytes.len() && bytes[i] != b'}' && bytes[i] != b'\n' {
                i += 1;
            }
            if bytes.get(i) == Some(&b'}') {
                i += 1;
            }
            continue;
        }
        if b == b'{' && is_caddy_block_brace(bytes, i) {
            let start = stmt_start.take().unwrap_or(i);
            stack.push((start, i, Vec::new()));
            i += 1;
            continue;
        }
        if b == b'}' && is_caddy_block_brace(bytes, i) {
            let (start, open_brace, children) = stack.pop()?;
            let block = BraceBlock {
                start,
                open_brace,
                end: i + 1,
                children,
            };
            if let Some(parent) = stack.last_mut() {
                parent.2.push(block);
            } else {
                roots.push(block);
            }
            stmt_start = None;
            i += 1;
            continue;
        }
        stmt_start.get_or_insert(i);
        i += 1;
    }

    if !stack.is_empty() {
        return None;
    }
    Some(roots)
}

fn caddy_sections(text: &str, line_starts: &[usize]) -> Vec<Section> {
    if line_starts.len() < 2 {
        return Vec::new();
    }
    let Some(blocks) = parse_caddy_blocks(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in &blocks {
        let prelude = nginx_clean_prelude(&text[block.start..block.open_brace]);
        let bytes = whole_line_or_exact(text, line_starts, block.start, block.end);
        let (kind, name, expand_children) = if prelude.is_empty() {
            ("section", "global".to_owned(), false)
        } else if let Some(inner) = prelude.strip_prefix('(').and_then(|s| s.strip_suffix(')'))
            && !inner.trim().is_empty()
        {
            ("snippet", clip_label(inner.trim(), 72), false)
        } else if let Some(inner) = prelude.strip_prefix("&(").and_then(|s| s.strip_suffix(')'))
            && !inner.trim().is_empty()
        {
            ("route", clip_label(inner.trim(), 72), false)
        } else {
            ("site", clip_label(&prelude, 80), true)
        };
        out.push(Section {
            name,
            kind,
            bytes,
            signature_start: block.start,
            header_end: block.start,
        });

        let span_lines = line_of(line_starts, block.end.saturating_sub(1))
            .saturating_sub(line_of(line_starts, block.start))
            + 1;
        if expand_children && span_lines >= 6 {
            for child in &block.children {
                let cprelude = nginx_clean_prelude(&text[child.start..child.open_brace]);
                let mut parts = cprelude.splitn(2, char::is_whitespace);
                let head = parts.next().unwrap_or_default();
                let args = parts.next().unwrap_or_default().trim();
                let classified = if head.starts_with('@') && head.len() > 1 {
                    Some(("matcher", head.to_owned()))
                } else {
                    match head {
                        "handle" | "handle_path" => Some((
                            "handle",
                            if args.is_empty() {
                                "(default)".to_owned()
                            } else {
                                clip_label(args, 72)
                            },
                        )),
                        "handle_errors" => Some((
                            "handle",
                            if args.is_empty() {
                                "errors".to_owned()
                            } else {
                                clip_label(&format!("errors {args}"), 72)
                            },
                        )),
                        "route" => Some((
                            "route",
                            if args.is_empty() {
                                "(default)".to_owned()
                            } else {
                                clip_label(args, 72)
                            },
                        )),
                        "reverse_proxy" if !args.is_empty() => {
                            Some(("proxy", clip_label(args, 72)))
                        }
                        _ => None,
                    }
                };
                if let Some((ckind, cname)) = classified {
                    let cbytes = whole_line_or_exact(text, line_starts, child.start, child.end);
                    out.push(Section {
                        name: cname,
                        kind: ckind,
                        bytes: cbytes,
                        signature_start: child.start,
                        header_end: child.start,
                    });
                }
            }
        }
    }
    if out.len() < 2 {
        return Vec::new();
    }
    out
}

// ---------------------------------------------------------------------------
// HAProxy (haproxy.cfg, *.haproxy, *.cfg fallback)
// ---------------------------------------------------------------------------

fn parse_haproxy_header(trimmed: &str) -> Option<(&'static str, String, bool)> {
    let clean = strip_hash_comment(trimmed).trim();
    let mut parts = clean.splitn(2, char::is_whitespace);
    let kw = parts.next()?;
    let rest = parts.next().unwrap_or_default().trim();
    match kw {
        "global" if rest.is_empty() => Some(("section", "global".to_owned(), true)),
        "defaults" => {
            if rest.is_empty() {
                Some(("section", "defaults".to_owned(), true))
            } else {
                Some(("defaults", clip_label(rest, 72), true))
            }
        }
        "frontend" if !rest.is_empty() => Some(("frontend", clip_label(rest, 72), true)),
        "backend" if !rest.is_empty() => Some(("backend", clip_label(rest, 72), true)),
        "listen" if !rest.is_empty() => Some(("listen", clip_label(rest, 72), true)),
        "resolvers" | "peers" | "mailers" | "userlist" | "cache" | "ring" | "fcgi-app"
        | "program" | "http-errors" | "crt-store"
            if !rest.is_empty() =>
        {
            Some(("section", clip_label(&format!("{kw} {rest}"), 72), false))
        }
        "traces" if rest.is_empty() => Some(("section", "traces".to_owned(), false)),
        _ => None,
    }
}

fn haproxy_sections(text: &str, line_starts: &[usize], explicit: bool) -> Vec<Section> {
    if line_starts.len() < 2 {
        return Vec::new();
    }
    let mut headers: Vec<(usize, &'static str, String, bool)> = Vec::new();
    let mut saw_first_content = false;

    for idx in 0..line_starts.len() {
        let line = line_text(text, line_starts, idx);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let is_first = !saw_first_content;
        saw_first_content = true;

        if !explicit && trimmed.starts_with('[') {
            return Vec::new();
        }
        if !line.starts_with([' ', '\t']) {
            if let Some((kind, name, is_core)) = parse_haproxy_header(trimmed) {
                headers.push((idx, kind, name, is_core));
            } else if !explicit && is_first {
                return Vec::new();
            }
        } else if !explicit && is_first {
            return Vec::new();
        }
    }

    if headers.len() < 2 || !headers.iter().any(|h| h.3) {
        return Vec::new();
    }

    let offset = |line: usize| line_offset(line_starts, text.len(), line);
    let mut out = Vec::with_capacity(headers.len());
    for (pos, (first, kind, name, _)) in headers.iter().enumerate() {
        let end_line = headers.get(pos + 1).map_or(line_starts.len(), |h| h.0);
        let sig_line = match *kind {
            "frontend" | "listen" => (*first + 1..end_line).find(|&i| {
                let t = line_text(text, line_starts, i).trim();
                t.starts_with("bind ") || t.starts_with("bind\t")
            }),
            "backend" => (*first + 1..end_line)
                .find(|&i| {
                    let t = line_text(text, line_starts, i).trim();
                    t.starts_with("balance ") || t.starts_with("balance\t")
                })
                .or_else(|| {
                    (*first + 1..end_line).find(|&i| {
                        let t = line_text(text, line_starts, i).trim();
                        t.starts_with("mode ") || t.starts_with("mode\t")
                    })
                }),
            _ => None,
        };
        let (sig_start, header_end) = match sig_line {
            Some(l) => (offset(l), offset(l + 1)),
            None => (offset(*first), offset(*first)),
        };
        out.push(Section {
            name: name.clone(),
            kind,
            bytes: offset(*first)..offset(end_line),
            signature_start: sig_start,
            header_end,
        });
    }
    out
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

        // Minified 1-line JSON (small) and broken JSON stay empty.
        let minified = "{\"a\":1,\"b\":2}\n";
        assert!(sections("min.json", minified, &line_starts(minified)).is_empty());
        let broken = "{\n  \"a\": 1,\n  \"b\": [\n";
        assert!(sections("bad.json", broken, &line_starts(broken)).is_empty());
    }

    #[test]
    fn json_root_arrays_and_large_minified_json_outline_with_exact_spans() {
        let arr = r#"[
  {
    "type": "widget",
    "id": "alpha",
    "value": 1
  },
  {
    "name": "beta",
    "value": 2
  },
  {
    "key": "gamma",
    "value": 3
  },
  {
    "value": 4
  }
]
"#;
        let found = sections("items.json", arr, &line_starts(arr));
        let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert_eq!(
            names,
            [
                ("item", "alpha"),
                ("item", "beta"),
                ("item", "gamma"),
                ("item", "[3]"),
            ]
        );
        assert!(arr[found[0].bytes.clone()].contains("\"id\": \"alpha\""));
        assert!(arr[found[3].bytes.clone()].contains("\"value\": 4"));

        // Duplicate labels disambiguate cleanly with `[i] label`.
        let dup = "[\n  {\"type\": \"row\", \"v\": 1},\n  {\"type\": \"row\", \"v\": 2}\n]\n";
        let dup_found = sections("dup.json", dup, &line_starts(dup));
        assert_eq!(
            dup_found
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["[0] row", "[1] row"]
        );

        // Large single-line minified JSON object uses exact `key_start..val_end` spans.
        let big_val = "x".repeat(1200);
        let min_obj =
            format!("{{\"alpha\":{{\"data\":\"{big_val}\"}},\"beta\":{{\"data\":\"{big_val}\"}}}}");
        assert!(min_obj.len() > MINIFIED_THRESHOLD);
        let min_found = sections("bundle.json", &min_obj, &line_starts(&min_obj));
        assert_eq!(
            min_found
                .iter()
                .map(|s| (s.kind, s.name.as_str()))
                .collect::<Vec<_>>(),
            [("key", "alpha"), ("key", "beta")]
        );
        assert_eq!(
            &min_obj[min_found[0].bytes.clone()],
            format!("\"alpha\":{{\"data\":\"{big_val}\"}}")
        );
        assert_eq!(
            &min_obj[min_found[1].bytes.clone()],
            format!("\"beta\":{{\"data\":\"{big_val}\"}}")
        );

        // Large single-line minified JSON root array uses exact element spans.
        let min_arr = format!(
            "[{{\"id\":\"first\",\"payload\":\"{big_val}\"}},{{\"id\":\"second\",\"payload\":\"{big_val}\"}}]"
        );
        let min_arr_found = sections("records.json", &min_arr, &line_starts(&min_arr));
        assert_eq!(
            min_arr_found
                .iter()
                .map(|s| (s.kind, s.name.as_str()))
                .collect::<Vec<_>>(),
            [("item", "first"), ("item", "second")]
        );
        assert_eq!(
            &min_arr[min_arr_found[0].bytes.clone()],
            format!("{{\"id\":\"first\",\"payload\":\"{big_val}\"}}")
        );
        assert_eq!(
            &min_arr[min_arr_found[1].bytes.clone()],
            format!("{{\"id\":\"second\",\"payload\":\"{big_val}\"}}")
        );

        // Tiny 1-line array and malformed array fail closed.
        let tiny_arr = "[{\"id\":\"a\"},{\"id\":\"b\"}]";
        assert!(sections("tiny.json", tiny_arr, &line_starts(tiny_arr)).is_empty());
        let bad_arr = "[\n  {\"id\": \"a\"},\n  {\"id\":\n";
        assert!(sections("bad_arr.json", bad_arr, &line_starts(bad_arr)).is_empty());
    }

    #[test]
    fn makefile_and_justfile_sections_outline_targets_and_recipes() {
        let mk = concat!(
            "CC := gcc\n",
            "CFLAGS = -O2\n\n",
            ".PHONY: all build test clean\n\n",
            "define HELPER\n",
            "not_a_target: foo\n",
            "endef\n\n",
            "build: CFLAGS += -g\n",
            "build: src/main.c\n",
            "\t$(CC) $(CFLAGS) -o app src/main.c\n\n",
            "test: build\n",
            "\t./app --test\n\n",
            "clean:\n",
            "\trm -f app\n",
        );
        for name in ["Makefile", "GNUmakefile", "rules.mk"] {
            let found = sections(name, mk, &line_starts(mk));
            let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
            assert_eq!(
                names,
                [
                    ("section", "(root)"),
                    ("target", "build"),
                    ("target", "test"),
                    ("target", "clean"),
                ],
                "{name}"
            );
            assert_eq!(&mk[found[3].bytes.clone()], "clean:\n\trm -f app\n");
            // Deduplicated signature for bare `clean:` is empty.
            assert_eq!(found[3].signature_start, found[3].header_end);
            // Target with prerequisites keeps header signature.
            assert!(found[1].header_end > found[1].signature_start);
        }
        // Unclosed `define` and single target fail closed.
        assert!(
            sections(
                "Makefile",
                "define X\na:\n\ttrue\nb:\n\ttrue\n",
                &line_starts("define X\na:\n\ttrue\nb:\n\ttrue\n")
            )
            .is_empty()
        );
        assert!(
            sections(
                "Makefile",
                "only:\n\ttrue\n",
                &line_starts("only:\n\ttrue\n")
            )
            .is_empty()
        );

        let just = concat!(
            "set shell := [\"bash\", \"-cu\"]\n",
            "version := \"1.0\"\n\n",
            "[group('dev')]\n",
            "@test *args: build\n",
            "    cargo test {{args}}\n\n",
            "build target=\"debug\":\n",
            "    cargo build\n\n",
            "clean:\n",
            "    rm -rf target\n",
        );
        for name in ["justfile", "Justfile", "ci.just"] {
            let found = sections(name, just, &line_starts(just));
            let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
            assert_eq!(
                names,
                [
                    ("section", "(root)"),
                    ("recipe", "test"),
                    ("recipe", "build"),
                    ("recipe", "clean"),
                ],
                "{name}"
            );
            assert!(
                just[found[1].bytes.clone()].starts_with("[group('dev')]\n@test *args: build\n")
            );
            assert_eq!(&just[found[3].bytes.clone()], "clean:\n    rm -rf target\n");
            assert_eq!(found[3].signature_start, found[3].header_end);
        }
    }

    #[test]
    fn dockerfile_and_containerfile_sections_outline_stages() {
        let df = concat!(
            "# syntax=docker/dockerfile:1\n",
            "ARG RUST_VERSION=1.91\n\n",
            "FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-slim AS builder\n",
            "WORKDIR /app\n",
            "RUN <<EOF\n",
            "FROM not_a_stage\n",
            "EOF\n",
            "RUN cargo build --release\n\n",
            "FROM debian:bookworm-slim AS runtime\n",
            "COPY --from=builder /app/target/release/app /usr/local/bin/app\n",
            "ENTRYPOINT [\"/usr/local/bin/app\"]\n",
        );
        for name in [
            "Dockerfile",
            "Dockerfile.prod",
            "app.dockerfile",
            "Containerfile",
            "Containerfile.ubi",
        ] {
            let found = sections(name, df, &line_starts(df));
            let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
            assert_eq!(
                names,
                [
                    ("section", "(global)"),
                    ("stage", "builder"),
                    ("stage", "runtime"),
                ],
                "{name}"
            );
            assert!(df[found[1].bytes.clone()].contains("FROM not_a_stage"));
            assert!(
                df[found[2].bytes.clone()].starts_with("FROM debian:bookworm-slim AS runtime\n")
            );
        }
        // Single-stage Dockerfile fails closed.
        let single = "FROM alpine:3.20\nRUN echo hi\n";
        assert!(sections("Dockerfile", single, &line_starts(single)).is_empty());
    }

    #[test]
    fn ini_conf_editorconfig_and_properties_sections() {
        let ec = concat!(
            "# EditorConfig\n",
            "root = true\n\n",
            "[*]\n",
            "indent_style = space\n",
            "indent_size = 4\n\n",
            "[*.rs] # Rust files\n",
            "max_line_length = 100\n",
        );
        for name in [
            ".editorconfig",
            "setup.cfg",
            "app.ini",
            "service.conf",
            "app.properties",
        ] {
            let found = sections(name, ec, &line_starts(ec));
            let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
            assert_eq!(
                names,
                [("section", "(root)"), ("section", "*"), ("section", "*.rs"),],
                "{name}"
            );
            assert_eq!(found[1].signature_start, found[1].header_end);
        }
        // Flat properties and unclosed header fail closed.
        let flat = "db.host=localhost\ndb.port=5432\n";
        assert!(sections("app.properties", flat, &line_starts(flat)).is_empty());
        let broken = "[unclosed\na = 1\n[ok]\nb = 2\n";
        assert!(sections("bad.ini", broken, &line_starts(broken)).is_empty());
    }

    #[test]
    fn css_scss_and_less_sections_outline_rules_and_at_rules() {
        let scss = concat!(
            "@use 'sass:math';\n",
            "$primary: #09f;\n\n",
            "/* comment with { brace } */\n",
            "@layer base {\n",
            "  body { background: url(https://example.com/bg.png); }\n",
            "}\n\n",
            "@mixin button($bg) {\n",
            "  // line comment\n",
            "  background: $bg;\n",
            "}\n\n",
            "@keyframes fadeIn {\n",
            "  from { opacity: 0; }\n",
            "  to { opacity: 1; }\n",
            "}\n\n",
            ".col-#{$i} {\n",
            "  width: 100%;\n",
            "}\n\n",
            "@media (max-width: 768px) {\n",
            "  .card { display: block; }\n",
            "}\n",
        );
        let found = sections("styles/app.scss", scss, &line_starts(scss));
        let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert_eq!(
            names,
            [
                ("layer", "base"),
                ("mixin", "button"),
                ("keyframes", "fadeIn"),
                ("rule", ".col-#{$i}"),
                ("at-rule", "@media (max-width: 768px)"),
            ]
        );
        assert!(scss[found[1].bytes.clone()].starts_with("@mixin button($bg) {\n"));
        assert!(scss[found[3].bytes.clone()].contains("width: 100%;"));

        // Plain CSS and Less extensions work, unbalanced CSS fails closed.
        let css = ":root {\n  --a: 1;\n}\n.btn {\n  color: red;\n}\n";
        assert_eq!(sections("app.css", css, &line_starts(css)).len(), 2);
        assert_eq!(sections("app.less", css, &line_starts(css)).len(), 2);
        let bad = ".a {\n  color: red;\n.b {\n  color: blue;\n}\n";
        assert!(sections("bad.css", bad, &line_starts(bad)).is_empty());
    }

    #[test]
    fn idl_schemas_proto_graphql_and_prisma() {
        let proto = concat!(
            "syntax = \"proto3\";\n",
            "package billing.v1;\n\n",
            "enum Currency {\n",
            "  CURRENCY_UNSPECIFIED = 0;\n",
            "}\n\n",
            "message Order {\n",
            "  string id = 1;\n",
            "  oneof payment {\n",
            "    string card_id = 2;\n",
            "    string bank_id = 3;\n",
            "  }\n",
            "  message LineItem {\n",
            "    string sku = 1;\n",
            "  }\n",
            "}\n\n",
            "service BillingService {\n",
            "  rpc CreateOrder(Order) returns (Order);\n",
            "}\n",
        );
        let p_found = sections("billing.proto", proto, &line_starts(proto));
        assert_eq!(
            p_found
                .iter()
                .map(|s| (s.kind, s.name.as_str()))
                .collect::<Vec<_>>(),
            [
                ("enum", "Currency"),
                ("message", "Order"),
                ("oneof", "Order.payment"),
                ("message", "Order.LineItem"),
                ("service", "BillingService"),
            ]
        );
        assert!(proto[p_found[2].bytes.clone()].contains("card_id"));

        let gql = concat!(
            "\"\"\"\n",
            "type FakeInsideDocstring { x: Int }\n",
            "\"\"\"\n",
            "type Order implements Node {\n",
            "  id: ID!\n",
            "}\n\n",
            "input CreateOrderInput {\n",
            "  sku: String!\n",
            "}\n\n",
            "interface Node {\n",
            "  id: ID!\n",
            "}\n\n",
            "union SearchResult =\n",
            "  | Order\n",
            "  | Customer\n\n",
            "enum OrderStatus {\n",
            "  OPEN\n",
            "}\n",
        );
        for ext in ["schema.graphql", "schema.gql"] {
            let g_found = sections(ext, gql, &line_starts(gql));
            assert_eq!(
                g_found
                    .iter()
                    .map(|s| (s.kind, s.name.as_str()))
                    .collect::<Vec<_>>(),
                [
                    ("type", "Order"),
                    ("input", "CreateOrderInput"),
                    ("interface", "Node"),
                    ("union", "SearchResult"),
                    ("enum", "OrderStatus"),
                ],
                "{ext}"
            );
            assert!(gql[g_found[3].bytes.clone()].contains("| Customer"));
        }

        let prisma = concat!(
            "datasource db {\n",
            "  provider = \"postgresql\"\n",
            "}\n\n",
            "generator client {\n",
            "  provider = \"prisma-client-js\"\n",
            "}\n\n",
            "model User {\n",
            "  id Int @id\n",
            "}\n",
        );
        let pr_found = sections("schema.prisma", prisma, &line_starts(prisma));
        assert_eq!(
            pr_found
                .iter()
                .map(|s| (s.kind, s.name.as_str()))
                .collect::<Vec<_>>(),
            [
                ("datasource", "db"),
                ("generator", "client"),
                ("model", "User"),
            ]
        );
        // Unbalanced IDL fails closed.
        assert!(
            sections(
                "bad.proto",
                "message A {\n  string x = 1;\n",
                &line_starts("message A {\n  string x = 1;\n")
            )
            .is_empty()
        );
    }

    #[test]
    fn xml_html_csproj_and_pom_sections() {
        let pom = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n",
            "  <modelVersion>4.0.0</modelVersion>\n",
            "  <dependencies>\n",
            "    <dependency>\n",
            "      <groupId>a</groupId>\n",
            "    </dependency>\n",
            "    <dependency>\n",
            "      <groupId>b</groupId>\n",
            "    </dependency>\n",
            "  </dependencies>\n",
            "  <build>\n",
            "    <plugins>\n",
            "      <plugin>\n",
            "        <artifactId>maven-compiler-plugin</artifactId>\n",
            "      </plugin>\n",
            "      <plugin>\n",
            "        <artifactId>maven-surefire-plugin</artifactId>\n",
            "      </plugin>\n",
            "      <plugin>\n",
            "        <artifactId>maven-jar-plugin</artifactId>\n",
            "      </plugin>\n",
            "    </plugins>\n",
            "  </build>\n",
            "</project>\n",
        );
        let pom_found = sections("pom.xml", pom, &line_starts(pom));
        assert_eq!(
            pom_found
                .iter()
                .map(|s| (s.kind, s.name.as_str()))
                .collect::<Vec<_>>(),
            [
                ("element", "project"),
                ("element", "modelVersion"),
                ("element", "dependencies"),
                ("element", "build"),
                ("element", "build.plugins"),
            ]
        );
        assert!(pom[pom_found[4].bytes.clone()].contains("maven-compiler-plugin"));

        let csproj = concat!(
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n",
            "  <PropertyGroup>\n",
            "    <TargetFramework>net8.0</TargetFramework>\n",
            "  </PropertyGroup>\n",
            "  <ItemGroup Label=\"Dependencies\">\n",
            "    <PackageReference Include=\"Serilog\" Version=\"4.0.0\" />\n",
            "  </ItemGroup>\n",
            "  <Target Name=\"PostBuild\">\n",
            "    <Exec Command=\"echo ok\" />\n",
            "  </Target>\n",
            "</Project>\n",
        );
        for name in [
            "App.csproj",
            "Lib.fsproj",
            "Directory.Build.props",
            "Custom.targets",
        ] {
            let found = sections(name, csproj, &line_starts(csproj));
            assert_eq!(
                found.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                [
                    "Project",
                    "PropertyGroup",
                    "ItemGroup:Dependencies",
                    "Target:PostBuild"
                ],
                "{name}"
            );
        }

        let html = concat!(
            "<!DOCTYPE html>\n",
            "<html lang=\"en\">\n",
            "<head>\n",
            "  <meta charset=\"utf-8\">\n",
            "  <title>Demo</title>\n",
            "  <link rel=\"stylesheet\" href=\"app.css\">\n",
            "</head>\n",
            "<body>\n",
            "  <header id=\"top\">\n",
            "    <h1>Demo</h1>\n",
            "  </header>\n",
            "  <main>\n",
            "    <p>Line 1</p>\n",
            "    <p>Line 2</p>\n",
            "  </main>\n",
            "  <script>\n",
            "    if (a < b && c > d) { console.log('<not-a-tag>'); }\n",
            "  </script>\n",
            "  <footer>\n",
            "    <small>End</small>\n",
            "  </footer>\n",
            "</body>\n",
            "</html>\n",
        );
        for name in ["index.html", "page.htm"] {
            let found = sections(name, html, &line_starts(html));
            assert_eq!(
                found.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                [
                    "html",
                    "head",
                    "body",
                    "body.header:top",
                    "body.main",
                    "body.script",
                    "body.footer",
                ],
                "{name}"
            );
            assert!(html[found[5].bytes.clone()].contains("if (a < b && c > d)"));
        }

        // Strict XML fails closed on mismatched or unclosed tags, and single-child root.
        let bad_xml = "<project>\n  <a>\n  </b>\n</project>\n";
        assert!(sections("bad.xml", bad_xml, &line_starts(bad_xml)).is_empty());
        let single_xml = "<root>\n  <only/>\n</root>\n";
        assert!(sections("one.xml", single_xml, &line_starts(single_xml)).is_empty());
    }

    #[test]
    fn mdx_routes_to_markdown_sections_and_snapshot_expands_exact_bytes() {
        let mdx = concat!(
            "import { Callout } from './components';\n\n",
            "# Overview\n\n",
            "<Callout>Welcome</Callout>\n\n",
            "## Install\n\n",
            "Run `cargo install`.\n",
        );
        let mut snap = crate::source::Snapshot::from_text("docs/guide.mdx", mdx.to_owned());
        snap.index("docs/guide.mdx").unwrap();
        let syms = snap.symbols();
        assert_eq!(
            syms.iter()
                .map(|(n, k, ..)| (k.as_str(), n.as_str()))
                .collect::<Vec<_>>(),
            [("h1", "Overview"), ("h2", "Install")]
        );
        let page = snap.page("", None, 40, 4096).unwrap().display();
        assert!(page.contains("h1 Overview\n"), "{page}");
        assert!(page.contains("h2 Install\n"), "{page}");
    }

    fn assert_section_invariants(text: &str, secs: &[Section]) {
        for s in secs {
            assert!(!s.name.is_empty(), "empty section name: {s:?}");
            assert!(
                s.bytes.start <= s.bytes.end && s.bytes.end <= text.len(),
                "invalid bytes span: {s:?}"
            );
            assert!(
                s.bytes.start <= s.header_end && s.header_end <= text.len(),
                "header_end out of range for --match: {s:?}"
            );
            assert!(
                s.signature_start <= s.header_end,
                "signature_start > header_end: {s:?}"
            );
            assert!(text.is_char_boundary(s.bytes.start));
            assert!(text.is_char_boundary(s.bytes.end));
            assert!(text.is_char_boundary(s.signature_start));
            assert!(text.is_char_boundary(s.header_end));
        }
    }

    #[test]
    fn nix_flake_and_module_outlines_preserve_exact_balanced_spans() {
        let flake = concat!(
            "{\n",
            "  description = \"Order service flake\";\n\n",
            "  inputs = {\n",
            "    nixpkgs.url = \"github:NixOS/nixpkgs/nixos-unstable\";\n",
            "    flake-utils.url = \"github:numtide/flake-utils\";\n",
            "  };\n\n",
            "  outputs = { self, nixpkgs, flake-utils, ... }@inputs:\n",
            "    let\n",
            "      supportedSystems = [ \"x86_64-linux\" \"aarch64-darwin\" ];\n",
            "      forAllSystems = f: nixpkgs.lib.genAttrs supportedSystems (system: f system);\n",
            "      scriptText = ''\n",
            "        echo \"not a { brace } binding = 1;\"\n",
            "        echo '''escaped quotes'''\n",
            "      '';\n",
            "    in\n",
            "    {\n",
            "      packages = forAllSystems (system: {\n",
            "        default = nixpkgs.legacyPackages.${system}.hello;\n",
            "        cli = nixpkgs.legacyPackages.${system}.ripgrep;\n",
            "      });\n\n",
            "      devShells = forAllSystems (system: {\n",
            "        default = nixpkgs.legacyPackages.${system}.mkShell {\n",
            "          packages = [ nixpkgs.legacyPackages.${system}.rustc ];\n",
            "        };\n",
            "      });\n\n",
            "      nixosConfigurations.prod = nixpkgs.lib.nixosSystem {\n",
            "        system = \"x86_64-linux\";\n",
            "        modules = [ ./configuration.nix ];\n",
            "      };\n\n",
            "      lib.mkService = { name, port }: {\n",
            "        systemd.services.${name} = {\n",
            "          description = \"Service ${name} on ${toString port}\";\n",
            "        };\n",
            "      };\n",
            "    };\n",
            "}\n",
        );

        let found = sections("flake.nix", flake, &line_starts(flake));
        assert_section_invariants(flake, &found);
        let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert_eq!(
            names,
            [
                ("section", "description"),
                ("section", "inputs"),
                ("section", "outputs"),
                ("let", "supportedSystems"),
                ("let", "forAllSystems"),
                ("let", "scriptText"),
                ("section", "packages"),
                ("attr", "packages.default"),
                ("attr", "packages.cli"),
                ("section", "devShells"),
                ("attr", "devShells.default"),
                ("attr", "nixosConfigurations.prod"),
                ("function", "lib.mkService"),
            ]
        );
        let prod = found
            .iter()
            .find(|s| s.name == "nixosConfigurations.prod")
            .unwrap();
        assert!(flake[prod.bytes.clone()].contains("./configuration.nix"));
        assert!(flake[prod.bytes.clone()].trim_end().ends_with("};"));
        let mk_svc = found.iter().find(|s| s.name == "lib.mkService").unwrap();
        assert!(flake[mk_svc.bytes.clone()].contains("systemd.services.${name}"));
        assert!(!flake[mk_svc.bytes.clone()].contains("./configuration.nix"));

        // NixOS module with top-level lambda + `let ... in { options; config; }`
        let module = concat!(
            "{ config, lib, pkgs, ... }:\n",
            "let\n",
            "  cfg = config.services.demo;\n",
            "  mkPort = p: toString p;\n",
            "in\n",
            "{\n",
            "  options.services.demo = {\n",
            "    enable = lib.mkEnableOption \"demo\";\n",
            "    port = lib.mkOption { default = 8080; };\n",
            "  };\n\n",
            "  config = lib.mkIf cfg.enable {\n",
            "    services.demo.port = 9090;\n",
            "  };\n",
            "}\n",
        );
        let mod_found = sections("modules/demo.nix", module, &line_starts(module));
        assert_section_invariants(module, &mod_found);
        let mod_names: Vec<_> = mod_found
            .iter()
            .map(|s| (s.kind, s.name.as_str()))
            .collect();
        assert!(mod_names.contains(&("let", "cfg")), "{mod_names:?}");
        assert!(mod_names.contains(&("let", "mkPort")), "{mod_names:?}");
        assert!(
            mod_names.contains(&("attr", "options.services.demo")),
            "{mod_names:?}"
        );
        assert!(mod_names.contains(&("section", "config")), "{mod_names:?}");

        // Snapshot outline + `--match` works cleanly.
        let mut snap = crate::source::Snapshot::from_text("flake.nix", flake.to_owned());
        snap.index("flake.nix").unwrap();
        let page = snap.page("mkService", None, 40, 4096).unwrap().display();
        assert!(page.contains("function lib.mkService"), "{page}");

        // Unbalanced Nix fails closed.
        let bad = "{\n  description = \"broken\";\n  inputs = {\n    nixpkgs.url = \"a\";\n";
        assert!(sections("flake.nix", bad, &line_starts(bad)).is_empty());
        let bad_str = "{\n  a = ''unclosed\n  b = 2;\n}\n";
        assert!(sections("bad.nix", bad_str, &line_starts(bad_str)).is_empty());
    }

    #[test]
    fn github_actions_workflow_outlines_jobs_and_steps() {
        let wf = concat!(
            "name: Build and Test\n",
            "on:\n",
            "  push:\n",
            "    branches: [main]\n",
            "  pull_request:\n",
            "jobs:\n",
            "  build:\n",
            "    runs-on: ubuntu-latest\n",
            "    steps:\n",
            "      - name: Checkout\n",
            "        uses: actions/checkout@v4\n",
            "      - name: Run tests\n",
            "        run: |\n",
            "          # - name: fake step inside script\n",
            "          cargo test --locked\n",
            "      - uses: actions/upload-artifact@v4\n",
            "        with:\n",
            "          name: logs\n",
            "      - run: |\n",
            "          cargo fmt --all -- --check\n",
            "        name: Format check\n",
            "  deploy:\n",
            "    needs: build\n",
            "    uses: ./.github/workflows/release.yml\n",
        );

        let mut snap =
            crate::source::Snapshot::from_text(".github/workflows/ci.yml", wf.to_owned());
        snap.index(".github/workflows/ci.yml").unwrap();
        let page = snap.page("", None, 40, 4096).unwrap().display();
        assert!(
            page.contains("job build  runs-on: ubuntu-latest\n"),
            "{page}"
        );
        assert!(page.contains("step Checkout\n"), "{page}");
        assert!(page.contains("step Run tests\n"), "{page}");
        assert!(page.contains("step actions/upload-artifact@v4\n"), "{page}");
        assert!(page.contains("step Format check\n"), "{page}");
        assert!(
            page.contains("job deploy  uses: ./.github/workflows/release.yml\n"),
            "{page}"
        );
        assert!(!page.contains("fake step inside script"), "{page}");

        let found = sections(".github/workflows/ci.yml", wf, &line_starts(wf));
        assert_section_invariants(wf, &found);
        let run_step = found.iter().find(|s| s.name == "Run tests").unwrap();
        assert!(wf[run_step.bytes.clone()].contains("cargo test --locked"));
        assert!(!wf[run_step.bytes.clone()].contains("upload-artifact"));

        // Also supports `.yaml` workflow extension and `--match`.
        let matched = snap.page("Run tests", None, 40, 4096).unwrap().display();
        assert!(matched.contains("step Run tests\n"), "{matched}");
        assert!(!sections(".github/workflows/deploy.yaml", wf, &line_starts(wf)).is_empty());
    }

    #[test]
    fn openapi_yaml_and_json_outline_paths_methods_and_schemas() {
        let spec_yaml = concat!(
            "openapi: 3.1.0\n",
            "info:\n",
            "  title: Orders API\n",
            "  version: 1.0.0\n",
            "paths:\n",
            "  /v1/orders:\n",
            "    get:\n",
            "      summary: List orders\n",
            "    post:\n",
            "      summary: Create order\n",
            "  /v1/orders/{id}:\n",
            "    parameters:\n",
            "      - name: id\n",
            "        in: path\n",
            "    get:\n",
            "      summary: Fetch order\n",
            "    delete:\n",
            "      summary: Cancel order\n",
            "components:\n",
            "  schemas:\n",
            "    OrderResponse:\n",
            "      type: object\n",
            "      properties:\n",
            "        id:\n",
            "          type: string\n",
            "    CreateOrderRequest:\n",
            "      type: object\n",
            "  responses:\n",
            "    NotFound:\n",
            "      description: Missing resource\n",
        );

        let y_found = sections("api/openapi.yaml", spec_yaml, &line_starts(spec_yaml));
        assert_section_invariants(spec_yaml, &y_found);
        let y_names: Vec<_> = y_found.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert_eq!(
            y_names,
            [
                ("key", "openapi"),
                ("key", "info"),
                ("key", "paths"),
                ("path", "/v1/orders [GET, POST]"),
                ("path", "/v1/orders/{id} [GET, DELETE]"),
                ("key", "components"),
                ("section", "components.schemas"),
                ("schema", "OrderResponse"),
                ("schema", "CreateOrderRequest"),
                ("section", "components.responses"),
                ("component", "responses.NotFound"),
            ]
        );
        let orders_path = y_found
            .iter()
            .find(|s| s.name == "/v1/orders [GET, POST]")
            .unwrap();
        assert!(spec_yaml[orders_path.bytes.clone()].contains("Create order"));
        assert!(!spec_yaml[orders_path.bytes.clone()].contains("Cancel order"));

        let mut y_snap =
            crate::source::Snapshot::from_text("api/openapi.yaml", spec_yaml.to_owned());
        y_snap.index("api/openapi.yaml").unwrap();
        let y_page = y_snap.page("", None, 40, 4096).unwrap().display();
        assert!(y_page.contains("path /v1/orders [GET, POST]\n"), "{y_page}");
        assert!(y_page.contains("schema OrderResponse\n"), "{y_page}");

        // OpenAPI / Swagger JSON (including Swagger 2.0 `definitions`)
        let spec_json = r#"{
  "openapi": "3.0.3",
  "info": {
    "title": "Orders API",
    "version": "1.0.0"
  },
  "paths": {
    "/v1/orders": {
      "get": { "summary": "List" },
      "post": { "summary": "Create" }
    },
    "/v1/health": {
      "get": { "summary": "Health" }
    }
  },
  "components": {
    "schemas": {
      "OrderResponse": {
        "type": "object",
        "properties": { "id": { "type": "string" } }
      },
      "HealthStatus": {
        "type": "object"
      }
    }
  }
}
"#;
        let j_found = sections("api/openapi.json", spec_json, &line_starts(spec_json));
        assert_section_invariants(spec_json, &j_found);
        let j_names: Vec<_> = j_found.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert_eq!(
            j_names,
            [
                ("key", "openapi"),
                ("key", "info"),
                ("key", "paths"),
                ("path", "/v1/orders [GET, POST]"),
                ("path", "/v1/health [GET]"),
                ("key", "components"),
                ("section", "components.schemas"),
                ("schema", "OrderResponse"),
                ("schema", "HealthStatus"),
            ]
        );
        let order_schema = j_found.iter().find(|s| s.name == "OrderResponse").unwrap();
        assert!(spec_json[order_schema.bytes.clone()].contains("\"OrderResponse\""));
        assert!(!spec_json[order_schema.bytes.clone()].contains("\"HealthStatus\""));

        // Swagger 2.0 YAML with `definitions`
        let swagger = concat!(
            "swagger: \"2.0\"\n",
            "paths:\n",
            "  /v1/items:\n",
            "    get:\n",
            "      summary: List items\n",
            "definitions:\n",
            "  Item:\n",
            "    type: object\n",
        );
        let sw_found = sections("swagger.yml", swagger, &line_starts(swagger));
        assert_section_invariants(swagger, &sw_found);
        assert!(
            sw_found
                .iter()
                .any(|s| s.kind == "path" && s.name == "/v1/items [GET]")
        );
        assert!(
            sw_found
                .iter()
                .any(|s| s.kind == "schema" && s.name == "Item")
        );
    }

    #[test]
    fn nginx_caddy_and_haproxy_config_outlines() {
        let nginx = concat!(
            "worker_processes auto;\n\n",
            "events {\n",
            "    worker_connections 1024;\n",
            "}\n\n",
            "http {\n",
            "    upstream api_backend {\n",
            "        server 127.0.0.1:8080;\n",
            "        server 127.0.0.1:8081;\n",
            "    }\n\n",
            "    server {\n",
            "        listen 443 ssl;\n",
            "        server_name api.example.com;\n\n",
            "        location = /health {\n",
            "            return 200 \"ok\";\n",
            "        }\n\n",
            "        location /v1/ {\n",
            "            # comment with { brace }\n",
            "            proxy_pass http://api_backend;\n",
            "        }\n",
            "    }\n",
            "}\n",
        );
        for file in ["nginx.conf", "conf.d/api.nginx", "sites-enabled/api.conf"] {
            let found = sections(file, nginx, &line_starts(nginx));
            assert_section_invariants(nginx, &found);
            let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
            assert_eq!(
                names,
                [
                    ("block", "events"),
                    ("block", "http"),
                    ("upstream", "api_backend"),
                    ("server", "api.example.com"),
                    ("location", "= /health"),
                    ("location", "/v1/"),
                ],
                "{file}"
            );
            assert!(nginx[found[5].bytes.clone()].contains("proxy_pass http://api_backend;"));
        }
        let mut n_snap = crate::source::Snapshot::from_text("nginx.conf", nginx.to_owned());
        n_snap.index("nginx.conf").unwrap();
        let n_page = n_snap.page("", None, 40, 4096).unwrap().display();
        assert!(
            n_page.contains("server api.example.com  listen 443 ssl;\n"),
            "{n_page}"
        );
        assert!(
            n_page.contains("location /v1/  proxy_pass http://api_backend;\n"),
            "{n_page}"
        );
        // Unbalanced nginx fails closed.
        assert!(
            sections(
                "nginx.conf",
                "http {\n  server {\n    listen 80;\n}\n",
                &line_starts("http {\n  server {\n    listen 80;\n}\n")
            )
            .is_empty()
        );

        let caddy = concat!(
            "{\n",
            "    admin off\n",
            "}\n\n",
            "(cors) {\n",
            "    header Access-Control-Allow-Origin *\n",
            "}\n\n",
            "api.example.com {\n",
            "    import cors\n",
            "    @health {\n",
            "        path /health\n",
            "    }\n",
            "    handle /v1/* {\n",
            "        reverse_proxy {$API_UPSTREAM}:8080\n",
            "    }\n",
            "    handle {\n",
            "        respond <<HTML\n",
            "        <h1>{not a block}</h1>\n",
            "        HTML 200\n",
            "    }\n",
            "}\n",
        );
        for file in ["Caddyfile", "Caddyfile.prod", "edge.caddy"] {
            let found = sections(file, caddy, &line_starts(caddy));
            assert_section_invariants(caddy, &found);
            let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
            assert_eq!(
                names,
                [
                    ("section", "global"),
                    ("snippet", "cors"),
                    ("site", "api.example.com"),
                    ("matcher", "@health"),
                    ("handle", "/v1/*"),
                    ("handle", "(default)"),
                ],
                "{file}"
            );
            assert!(caddy[found[4].bytes.clone()].contains("reverse_proxy {$API_UPSTREAM}:8080"));
        }
        // Unbalanced Caddyfile fails closed.
        assert!(
            sections(
                "Caddyfile",
                "example.com {\n  handle {\n    respond 200\n}\n",
                &line_starts("example.com {\n  handle {\n    respond 200\n}\n")
            )
            .is_empty()
        );

        let haproxy = concat!(
            "global\n",
            "    maxconn 4096\n\n",
            "defaults\n",
            "    mode http\n",
            "    timeout connect 5s\n\n",
            "frontend https_in\n",
            "    bind *:443\n",
            "    default_backend api_nodes\n\n",
            "backend api_nodes\n",
            "    balance roundrobin\n",
            "    server app1 10.0.0.1:8080 check\n\n",
            "listen stats\n",
            "    bind *:8404\n",
            "    stats enable\n",
        );
        for file in ["haproxy.cfg", "haproxy.prod.cfg", "lb.cfg"] {
            let found = sections(file, haproxy, &line_starts(haproxy));
            assert_section_invariants(haproxy, &found);
            let names: Vec<_> = found.iter().map(|s| (s.kind, s.name.as_str())).collect();
            assert_eq!(
                names,
                [
                    ("section", "global"),
                    ("section", "defaults"),
                    ("frontend", "https_in"),
                    ("backend", "api_nodes"),
                    ("listen", "stats"),
                ],
                "{file}"
            );
            assert!(haproxy[found[3].bytes.clone()].contains("server app1 10.0.0.1:8080"));
        }
        let mut h_snap = crate::source::Snapshot::from_text("haproxy.cfg", haproxy.to_owned());
        h_snap.index("haproxy.cfg").unwrap();
        let h_page = h_snap.page("", None, 40, 4096).unwrap().display();
        assert!(
            h_page.contains("frontend https_in  bind *:443\n"),
            "{h_page}"
        );
        assert!(
            h_page.contains("backend api_nodes  balance roundrobin\n"),
            "{h_page}"
        );
        assert!(h_page.contains("listen stats  bind *:8404\n"), "{h_page}");
    }
}
