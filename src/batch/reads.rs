//! Reproduce the bytes downstream of line selections and column cuts while
//! retaining the source line of each surviving row.
use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path};

use super::{
    MAX_BYTES,
    parse::{Filter, Numbers},
};
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
    /// A `git show` blob, which shows a revision rather than the working file.
    object: bool,
    /// A file outside the workspace, which is never recorded as read.
    outside: bool,
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

    /// The lines a read of `file` prints in the resolved directory `cwd`, which
    /// the shell names `named`.
    pub fn read(
        &mut self,
        ws: &Workspace,
        (cwd, named): (&Path, &Path),
        file: &str,
        select: &Filter,
        numbered: Numbers,
        filters: &[Filter],
    ) -> Option<(String, SourceLines)> {
        let path = cwd.join(file);
        let Ok(relative) = path.strip_prefix(&ws.root) else {
            return self.outside(ws, (&path, &named.join(file)), select, numbered, filters);
        };
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

    /// A regular file outside the workspace, reread where the shell read it:
    /// `path` under its resolved directory, which the shell names `named`.
    /// Neither spelling, nor the file they resolve to, may be one the outline of
    /// an outside Read leaves alone, and the name itself may not be a link. The
    /// text is outlined by its own path and never recorded as read, since
    /// `resume`, `expand` and `edit` take workspace files only.
    fn outside(
        &mut self,
        ws: &Workspace,
        (path, named): (&Path, &Path),
        select: &Filter,
        numbered: Numbers,
        filters: &[Filter],
    ) -> Option<(String, SourceLines)> {
        let named = crate::find::lexical(named);
        let real = path.canonicalize().ok()?;
        let allowed = |path: &Path| crate::agents::outside_file(&ws.root, path);
        if !allowed(&named)
            || !allowed(path)
            || !allowed(&real)
            || !std::fs::symlink_metadata(path).ok()?.file_type().is_file()
        {
            return None;
        }
        let (name, real) = (named.to_str()?, real.to_str()?);
        let key = format!("\0\0{real}");
        if !self.files.contains_key(&key) {
            let cap = (MAX_BYTES as u64).min(ws.settings().ok()?.max_file_bytes);
            let mut bytes = Vec::new();
            std::fs::File::open(real)
                .ok()?
                .take(cap.saturating_add(1))
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 > cap {
                return None;
            }
            let text = String::from_utf8(bytes).ok()?;
            if text.contains('\0') {
                return None;
            }
            self.bytes += text.len();
            if self.bytes > MAX_BYTES * 4 {
                return None;
            }
            self.files
                .insert(key.clone(), Snapshot::from_text(real, text));
        }
        let (text, mut source) = self.select(&key, name, real, select, numbered, filters)?;
        source.outside = true;
        Some((text, source))
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
        let (text, mut source) =
            self.select(&key, spec, path, &Filter::All, Numbers::None, filters)?;
        source.object = true;
        Some((text, source))
    }

    fn select(
        &self,
        key: &str,
        name: &str,
        path: &str,
        select: &Filter,
        numbered: Numbers,
        filters: &[Filter],
    ) -> Option<(String, SourceLines)> {
        let snap = self.files.get(key)?;
        let mut rows = Vec::new();
        for (i, line) in snap.text.split_inclusive('\n').enumerate() {
            if i >= 100_000 {
                return None;
            }
            let text = match numbered {
                // `nl` treats these lines as section delimiters.
                Numbers::Nl if line.starts_with("\\:") => return None,
                Numbers::Nl => format!(
                    "{:>6}\t{}\n",
                    i + 1,
                    line.strip_suffix('\n').unwrap_or(line)
                ),
                Numbers::Cat => format!("{:>6}\t{line}", i + 1),
                Numbers::None => line.to_owned(),
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
                object: false,
                outside: false,
            },
        ))
    }

    /// Records the lines of a workspace file a replaced list showed, for
    /// `resume` and its `[source changed]` notice, as a single-file read does.
    /// Lines shown in full count as read; an outline, an excerpt or a column
    /// cut counts as an outline of the same lines.
    pub fn observe(&self, ws: &Workspace, source: &SourceLines, verbatim: bool) {
        let recorded = !source.object && !source.outside;
        let Some(snap) = self.files.get(&source.key).filter(|_| recorded) else {
            return;
        };
        let digest = crate::capture::source_digest(snap.text.as_bytes());
        for &(first, last) in &source.spans {
            crate::history::saw_range(
                ws,
                &source.path,
                first,
                last,
                digest.clone(),
                !verbatim || source.cut,
            );
        }
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
