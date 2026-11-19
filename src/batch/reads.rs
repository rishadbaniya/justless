//! Reproduce the bytes downstream of line selections and column cuts while
//! retaining the source line of each surviving row.
use std::collections::HashMap;
use std::path::{Component, Path};

use super::{MAX_BYTES, parse::Filter};
use crate::{source::Snapshot, workspace::Workspace};

pub(super) struct Files {
    files: HashMap<String, Snapshot>,
    bytes: usize,
}

pub(super) struct SourceLines {
    /// The loaded text: a workspace path, or a blob id and path.
    key: String,
    /// What the outline header calls it, such as `HEAD~1:src/a.rs`.
    name: String,
    /// The path that picks the parser.
    path: String,
    spans: Vec<(usize, usize)>,
    cut: bool,
}

struct Row {
    source_line: usize,
    text: String,
}

impl Files {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            bytes: 0,
        }
    }

    pub fn read(
        &mut self,
        ws: &Workspace,
        cwd: &Path,
        file: &str,
        select: &Filter,
        numbered: bool,
        filters: &[Filter],
    ) -> Option<(String, SourceLines)> {
        let path = cwd.join(file);
        let relative = path.strip_prefix(&ws.root).ok()?;
        let name: std::path::PathBuf = relative
            .components()
            .filter(|c| *c != Component::CurDir)
            .collect();
        let name = name.to_str()?;
        if !self.files.contains_key(name) {
            // max_file_bytes may be configured above the post-hook work limit.
            if std::fs::metadata(ws.source_path(name).ok()?).ok()?.len() > MAX_BYTES as u64 {
                return None;
            }
            let snap = Snapshot::open(ws, name).ok()?;
            self.bytes += snap.text.len();
            if self.bytes > MAX_BYTES * 4 {
                return None;
            }
            self.files.insert(name.to_owned(), snap);
        }
        self.select(name, name, name, select, numbered, filters)
    }

    /// The blob `git show SPEC` prints in `cwd`, which outlines as the text at
    /// that revision. Its key cannot collide with a workspace path.
    pub fn object(
        &mut self,
        cwd: &Path,
        spec: &str,
        filters: &[Filter],
    ) -> Option<(String, SourceLines)> {
        let path = crate::shellread::blob_path(spec)?;
        let (id, text) = crate::shellread::object(cwd, spec, MAX_BYTES as u64)?;
        let key = format!("\0{id}\0{path}");
        if !self.files.contains_key(&key) {
            self.bytes += text.len();
            if self.bytes > MAX_BYTES * 4 {
                return None;
            }
            self.files
                .insert(key.clone(), Snapshot::from_text(path, text));
        }
        self.select(&key, spec, path, &Filter::All, false, filters)
    }

    fn select(
        &self,
        key: &str,
        name: &str,
        path: &str,
        select: &Filter,
        numbered: bool,
        filters: &[Filter],
    ) -> Option<(String, SourceLines)> {
        let snap = self.files.get(key)?;
        let mut rows = Vec::new();
        for (i, line) in snap.text.split_inclusive('\n').enumerate() {
            if i >= 100_000 {
                return None;
            }
            if numbered && line.starts_with("\\:") {
                return None;
            }
            let text = if numbered {
                format!(
                    "{:>6}\t{}\n",
                    i + 1,
                    line.strip_suffix('\n').unwrap_or(line)
                )
            } else {
                line.to_owned()
            };
            rows.push(Row {
                source_line: i + 1,
                text,
            });
        }
        rows = apply(rows, select)?;
        for filter in filters {
            rows = apply(rows, filter)?;
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut text = String::new();
        for row in rows {
            if let Some((_, end)) = spans.last_mut()
                && row.source_line <= *end + 1
            {
                *end = (*end).max(row.source_line);
            } else {
                spans.push((row.source_line, row.source_line));
            }
            text.push_str(&row.text);
        }
        Some((
            text,
            SourceLines {
                key: key.to_owned(),
                name: name.to_owned(),
                path: path.to_owned(),
                spans,
                cut: filters.iter().any(|f| matches!(f, Filter::Columns(_))),
            },
        ))
    }

    pub fn outline(
        &mut self,
        source: &SourceLines,
        visible: &str,
        budget: usize,
    ) -> Option<String> {
        if source.spans.is_empty() {
            return None;
        }
        let snap = self.files.get_mut(&source.key)?;
        if snap.symbols().is_empty() {
            snap.index(&source.path).ok()?;
        }
        if crate::agents::uncovered_code(&source.path, snap)?
            .iter()
            .any(|&i| source.spans.iter().any(|&(a, b)| a - 1 <= i && i < b))
        {
            return None;
        }
        let ranges = source
            .spans
            .iter()
            .map(|(a, b)| format!("{a}-{b}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut out = format!("{} lines {ranges}\n", source.name);
        let mut count = 0;
        let mut shown = 0;
        for (name, kind, a, b) in snap.symbols() {
            if kind == "text" {
                return None;
            }
            if kind == "import"
                || !source
                    .spans
                    .iter()
                    .any(|&(first, last)| a <= last && b >= first && (a >= first || b <= last))
            {
                continue;
            }
            if source.cut && !visible.contains(name.rsplit("::").next()?) {
                continue;
            }
            let row = format!("  L{a}-{b} {kind} {name}\n");
            count += 1;
            if count == shown + 1 && out.len() + row.len() + 32 <= budget {
                out.push_str(&row);
                shown += 1;
            }
        }
        if shown < 2 {
            return None;
        }
        if count > shown {
            out.push_str(&format!("  … {} more symbols\n", count - shown));
        }
        Some(out)
    }
}

pub(super) fn filter(text: String, filters: &[Filter]) -> Option<String> {
    if filters.is_empty() {
        return Some(text);
    }
    let mut rows = text
        .split_inclusive('\n')
        .map(|line| Row {
            source_line: 0,
            text: line.to_owned(),
        })
        .collect::<Vec<_>>();
    for filter in filters {
        rows = apply(rows, filter)?;
    }
    Some(rows.into_iter().map(|r| r.text).collect())
}

fn apply(mut rows: Vec<Row>, filter: &Filter) -> Option<Vec<Row>> {
    match filter {
        Filter::All => {}
        Filter::First(n) => rows.truncate(*n),
        Filter::Last(n) => {
            rows.drain(..rows.len().saturating_sub(*n));
        }
        Filter::From(n) => {
            rows.drain(..(*n - 1).min(rows.len()));
        }
        Filter::Sed(ranges) => {
            let mut selected = Vec::new();
            let mut size = 0;
            for (i, mut row) in rows.into_iter().enumerate() {
                let n = i + 1;
                let copies = ranges.iter().filter(|&&(a, b)| a <= n && n <= b).count();
                if copies == 0 {
                    continue;
                }
                if cfg!(any(
                    target_vendor = "apple",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd"
                )) && !row.text.ends_with('\n')
                {
                    row.text.push('\n');
                }
                size += row.text.len() * copies;
                if size > MAX_BYTES || selected.len() + copies > 100_000 {
                    return None;
                }
                for _ in 1..copies {
                    selected.push(Row {
                        source_line: row.source_line,
                        text: row.text.clone(),
                    });
                }
                selected.push(row);
            }
            rows = selected;
        }
        Filter::Columns(ranges) => {
            for row in &mut rows {
                let mut line = String::new();
                for (i, c) in row
                    .text
                    .strip_suffix('\n')
                    .unwrap_or(&row.text)
                    .chars()
                    .enumerate()
                {
                    if ranges.iter().any(|&(a, b)| a <= i + 1 && i < b) {
                        line.push(c);
                    }
                }
                line.push('\n');
                row.text = line;
            }
        }
    }
    Some(rows)
}
