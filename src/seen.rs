//! What `context` and `pack` showed of each symbol in an agent session, so a
//! later view of the same symbol prints only what changed since.
//!
//! The store keeps the exact text of each symbol a view printed, not a digest
//! or a Git revision: the read, edit, read loop runs on an uncommitted working
//! tree, so Git cannot reproduce the earlier text, and only the text itself
//! makes the line diff exact. A record advances only when the agent was shown
//! every line it had seen that is gone now, so a later view never leans on a
//! line the agent still believes is there. Without a session the views print
//! whole, as before. The compaction handoff forgets every record, because the
//! agent's context no longer holds what it was shown.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::capture::source_digest;
use crate::context::Report;
use crate::workspace::{
    ORIGIN_UNKNOWN, Workspace, display_path, lock_state, shell_text, write_json,
};

/// Session files under `.justless/`, one per session.
const DIRECTORY: &str = "seen";
/// Sessions kept; the least recently written go first.
const SESSIONS: usize = 16;
/// Symbols remembered per session, newest first.
const VIEWS: usize = 64;
/// Symbol text remembered per session.
const TEXT_BYTES: usize = 512 * 1024;
/// A larger symbol is not remembered, so its next view prints whole.
const VIEW_BYTES: usize = 64 * 1024;
/// Files whose `pack` excerpts a session remembers, and excerpts per file.
const PACK_FILES: usize = 64;
const PACK_EXCERPTS: usize = 64;
/// Unchanged lines printed around each change.
const AROUND: usize = 2;
/// The largest line table the diff fills after trimming the common start and
/// end. A larger middle counts as replaced whole, which is still exact.
const DIFF_CELLS: usize = 4 << 20;

/// The session a view belongs to: `--session`, else Claude Code's session ID
/// in the agent's shell, digested as hooks digest it for `run --session`. An
/// agent that Claude Code starts as a child session can share its parent's ID
/// but not its context, and an MCP server keeps the ID it started with after
/// `/clear`, so neither uses it.
fn session(explicit: Option<&str>) -> Option<String> {
    if let Some(name) = explicit.filter(|name| !name.is_empty()) {
        return Some(name.to_owned());
    }
    let child = std::env::var_os("CLAUDE_CODE_CHILD_SESSION")
        .is_some_and(|value| !value.is_empty() && value != "0");
    if child || std::env::var_os(ORIGIN_UNKNOWN).is_some() {
        return None;
    }
    let id = std::env::var("CLAUDE_CODE_SESSION_ID").ok()?;
    (!id.is_empty()).then(|| source_digest(id.as_bytes())[..24].to_owned())
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    views: Vec<View>,
    /// The `pack` excerpts printed from each file, newest first.
    #[serde(default)]
    pack: Vec<PackFile>,
}

/// Digests of the excerpts `pack` printed from `file` while it had
/// `revision`. A view of the file at another revision starts the list again,
/// so an excerpt that changed and changed back is printed again.
#[derive(Clone, Serialize, Deserialize)]
struct PackFile {
    file: String,
    revision: String,
    keys: Vec<String>,
    ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct View {
    file: String,
    symbol: String,
    /// The symbol's first line when it was shown.
    start: usize,
    /// The file's revision then, as `read --outline` prints it.
    revision: String,
    /// The symbol's lines then, line endings included.
    text: String,
    /// Inclusive ranges of `text`'s lines, from 0, that the agent was shown.
    seen: Vec<(usize, usize)>,
    ms: u64,
}

impl View {
    fn saw(&self, line: usize) -> bool {
        self.seen
            .iter()
            .any(|&(from, to)| from <= line && line <= to)
    }
}

/// One session's remembered views, loaded for a command and written back after
/// it prints.
pub(crate) struct Memory {
    path: PathBuf,
    fresh: bool,
    store: Store,
    /// Records to replace, by file and symbol, or to drop when `None`.
    changes: Vec<(String, String, Option<View>)>,
    /// Excerpts to add, or a file's list to start again at a new revision.
    pack: Vec<PackFile>,
}

/// A symbol `context` shows, with its current span, lines from 1.
pub(crate) struct Symbol<'a> {
    pub(crate) file: &'a str,
    pub(crate) name: &'a str,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Line {
    /// The agent saw this line, and it has not changed.
    Seen,
    /// The line is new or changed since the agent's view.
    Changed,
    /// The line has not changed, but the agent was never shown it.
    Unseen,
}

enum Row {
    Text(String),
    Line(usize),
    Removed(usize),
}

impl Memory {
    /// The current session's memory, or `None` without a session, which
    /// leaves every view as it was.
    pub(crate) fn open(workspace: &Workspace, explicit: Option<&str>, fresh: bool) -> Option<Self> {
        let key = session(explicit)?;
        let name = format!("{}.json", &source_digest(key.as_bytes())[..24]);
        let path = workspace.state.join(DIRECTORY).join(name);
        Some(Self {
            store: load(&path),
            path,
            fresh,
            changes: Vec::new(),
            pack: Vec::new(),
        })
    }

    /// Prints lines `window` of `symbol`, from the file's `text`: whole the
    /// first time, then as the lines that changed since this session saw them,
    /// with a few unchanged lines around each change and a command that prints
    /// the whole symbol. With `header`, as for `context --symbol`, the view
    /// starts with the symbol's location. `overlapping` names the other
    /// symbols that share lines with this one, such as its methods or the
    /// block around it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn show(
        &mut self,
        workspace: &Workspace,
        text: &str,
        symbol: &Symbol<'_>,
        overlapping: &[String],
        window: (usize, usize),
        header: bool,
        report: &mut Report,
    ) {
        let file_lines: Vec<&str> = text.split_inclusive('\n').collect();
        let end = symbol.end.min(file_lines.len());
        if symbol.start == 0 || symbol.start > end {
            return;
        }
        let lines = &file_lines[symbol.start - 1..end];
        let whole = window.0 <= symbol.start && window.1 >= end;
        let (wa, wb) = (
            window.0.clamp(symbol.start, end) - symbol.start,
            window.1.clamp(symbol.start, end) - symbol.start,
        );
        let old = self
            .store
            .views
            .iter()
            .find(|view| view.file == symbol.file && view.symbol == symbol.name);
        let old_lines: Vec<&str> =
            old.map_or_else(Vec::new, |view| view.text.split_inclusive('\n').collect());
        let alignment = align(&old_lines, lines);
        let status: Vec<Line> = alignment
            .from
            .iter()
            .map(|from| match (old, from) {
                (Some(view), Some(j)) if view.saw(*j) => Line::Seen,
                (Some(_), None) => Line::Changed,
                _ => Line::Unseen,
            })
            .collect();
        // The lines the agent saw that are gone now, each with the new line it
        // would precede.
        let removed: Vec<(usize, usize)> = alignment
            .removed
            .iter()
            .copied()
            .filter(|&(j, _)| old.is_some_and(|view| view.saw(j)))
            .collect();
        let window_removed: Vec<(usize, usize)> = removed
            .iter()
            .copied()
            .filter(|&(_, anchor)| (wa..=wb + 1).contains(&anchor))
            .collect();
        let count = wb - wa + 1;
        let seen = (wa..=wb).filter(|&i| status[i] == Line::Seen).count();
        let changed = !window_removed.is_empty() || seen < count;
        let revision = crate::source::revision_for(symbol.file, text.as_bytes());
        let path = display_path(symbol.file);
        let location = format!("{path}:{}-{end}", symbol.start);
        let whole_symbol = format!(
            "whole symbol: {} expand {} '{revision}#L{}-{end}'",
            workspace.invocation().files,
            shell_text(symbol.file),
            symbol.start,
        );
        let since = old.map(|view| {
            let at = &view.revision[..view.revision.len().min(12)];
            if view.start == symbol.start {
                at.to_owned()
            } else {
                let last = view.start + old_lines.len().saturating_sub(1);
                format!("{at}, then at lines {}-{last}", view.start)
            }
        });
        let number = |i: usize| symbol.start + i;
        let full = |note: Option<&str>| {
            let mut rows = Vec::new();
            if header {
                rows.push(Row::Text(match note {
                    Some(note) => format!("{location} · {note}"),
                    None => location.clone(),
                }));
            }
            rows.extend((wa..=wb).map(Row::Line));
            rows
        };
        let width = number(wb).to_string().len();
        let modified = !window_removed.is_empty() || (wa..=wb).any(|i| status[i] == Line::Changed);
        let whole_view = full(
            since
                .as_ref()
                .filter(|_| modified)
                .map(|since| format!("changed since you read it at {since}"))
                .as_deref(),
        );
        let shorter = match &since {
            Some(since) if !self.fresh && !changed => Some(vec![Row::Text(if header {
                format!("{location} · unchanged since you read it at {since} · {whole_symbol}")
            } else {
                format!(
                    "  lines {}-{} unchanged since you read them at {since} · {whole_symbol}",
                    number(wa),
                    number(wb)
                )
            })]),
            Some(since) if !self.fresh && (seen > 0 || !window_removed.is_empty()) => {
                let summary = if header {
                    format!(
                        "{location} · {seen} of {count} lines unchanged since you read it at {since} · {whole_symbol}"
                    )
                } else {
                    format!(
                        "  {seen} of {count} lines unchanged since you read them at {since} · {whole_symbol}"
                    )
                };
                Some(hunks(summary, &status, &window_removed, wa, wb))
            }
            _ => None,
        };
        // The whole view wins whenever it is no longer, as for a rewrite or a
        // window of a few short lines.
        let bytes = |rows: &[Row]| -> usize {
            rows.iter()
                .map(|row| match row {
                    Row::Text(text) => text.len() + 1,
                    Row::Line(i) => lines[*i].len() + width + 5,
                    Row::Removed(j) => old_lines[*j].len() + width + 5,
                })
                .sum()
        };
        let (rows, delta) = match shorter {
            Some(rows) if bytes(&rows) < bytes(&whole_view) => (rows, true),
            _ => (whole_view, false),
        };
        // What the agent was shown completely.
        let mut printed = BTreeSet::new();
        let mut printed_removed = BTreeSet::new();
        for row in &rows {
            if report.clipped {
                break;
            }
            match row {
                Row::Text(text) => report.line(text),
                Row::Line(i) => {
                    let content = shown(lines[*i]);
                    if delta {
                        let mark = if status[*i] == Line::Changed {
                            '+'
                        } else {
                            ' '
                        };
                        report.line(format!("{mark} {:>width$} | {content}", number(*i)));
                    } else {
                        report.line(format!("  {} | {content}", number(*i)));
                    }
                    if !report.clipped {
                        printed.insert(*i);
                    }
                }
                Row::Removed(j) => {
                    report.line(format!("- {:width$} | {}", "", shown(old_lines[*j])));
                    if !report.clipped {
                        printed_removed.insert(*j);
                    }
                }
            }
        }
        let known = |i: usize| status[i] == Line::Seen || printed.contains(&i);
        if let (Some(&first), Some(&last)) = (printed.first(), printed.last()) {
            let (from, to) = if (wa..=wb).all(known) {
                (wa, wb)
            } else {
                (first, last)
            };
            crate::history::saw_source(
                workspace,
                symbol.file,
                number(from),
                number(to),
                source_digest(text.as_bytes()),
            );
        }
        if !printed.is_empty() || !printed_removed.is_empty() {
            self.showed(symbol.file, &revision, overlapping);
        }
        // A view printed whole and in order shows which lines are gone up to
        // its last printed line; a delta names each one. Until the agent has
        // been shown every line it saw that is gone, the record cannot advance.
        // It stays when this view printed nothing new, and otherwise goes, so
        // a line changed back to the record's text is never called unchanged.
        let last = printed.last().copied();
        let covered = |&(j, anchor): &(usize, usize)| {
            printed_removed.contains(&j)
                || (!delta
                    && last.is_some_and(|last| {
                        if whole {
                            anchor.min(lines.len() - 1) <= last
                        } else {
                            wa < anchor && anchor <= last
                        }
                    }))
        };
        if !removed.iter().all(covered) {
            if !printed_removed.is_empty() || printed.iter().any(|&i| status[i] != Line::Seen) {
                self.changes
                    .push((symbol.file.to_owned(), symbol.name.to_owned(), None));
            }
            return;
        }
        let saw: BTreeSet<usize> = (0..lines.len()).filter(|&i| known(i)).collect();
        if saw.is_empty() {
            return;
        }
        let text: String = lines.concat();
        let view = (text.len() <= VIEW_BYTES).then(|| View {
            file: symbol.file.to_owned(),
            symbol: symbol.name.to_owned(),
            start: symbol.start,
            revision,
            text,
            seen: ranges(&saw),
            ms: crate::history::now_ms(),
        });
        self.changes
            .push((symbol.file.to_owned(), symbol.name.to_owned(), view));
    }

    /// Records that the agent was just shown lines of `file` at `revision`,
    /// which may differ from what it saw earlier of the symbols in `names`. A
    /// record made at another revision of one of them, and the file's `pack`
    /// excerpts from another revision, are dropped, so a line changed back
    /// later is printed again.
    pub(crate) fn showed(&mut self, file: &str, revision: &str, names: &[String]) {
        for name in names {
            let stale =
                self.store.views.iter().any(|view| {
                    view.file == file && view.symbol == *name && view.revision != revision
                });
            if stale {
                self.changes.push((file.to_owned(), name.clone(), None));
            }
        }
        if self
            .pack_revision(file)
            .is_some_and(|known| known != revision)
        {
            self.pack.push(PackFile {
                file: file.to_owned(),
                revision: revision.to_owned(),
                keys: Vec::new(),
                ms: crate::history::now_ms(),
            });
        }
    }

    /// The revision of `file` that its remembered `pack` excerpts belong to.
    fn pack_revision(&self, file: &str) -> Option<&str> {
        self.pack
            .iter()
            .rev()
            .chain(&self.store.pack)
            .find(|entry| entry.file == file)
            .map(|entry| entry.revision.as_str())
    }

    /// Whether this session was already shown `lines` of `file`, exactly as
    /// `pack` prints them now, with no other view of the file at another
    /// revision since, and `--fresh` was not given.
    pub(crate) fn saw_excerpt(&self, file: &str, revision: &str, lines: &[String]) -> bool {
        if self.fresh || self.pack_revision(file) != Some(revision) {
            return false;
        }
        let key = excerpt_key(file, lines);
        self.pack.iter().chain(&self.store.pack).any(|entry| {
            entry.file == file && entry.revision == revision && entry.keys.contains(&key)
        })
    }

    /// Remembers that `pack` printed `lines` of `file` at `revision`.
    pub(crate) fn remember_excerpt(&mut self, file: &str, revision: &str, lines: &[String]) {
        self.pack.push(PackFile {
            file: file.to_owned(),
            revision: revision.to_owned(),
            keys: vec![excerpt_key(file, lines)],
            ms: crate::history::now_ms(),
        });
    }

    /// Writes the records this command changed back to the session's file,
    /// over what other commands wrote meanwhile, within the store's bounds.
    pub(crate) fn save(self, workspace: &Workspace) {
        if self.changes.is_empty() && self.pack.is_empty() {
            return;
        }
        let Some(directory) = self.path.parent() else {
            return;
        };
        if workspace.prepare_state().is_err() || fs::create_dir_all(directory).is_err() {
            return;
        }
        let Ok(_guard) = lock_state(&workspace.state.join("seen.lock")) else {
            return;
        };
        let mut store = load(&self.path);
        for (file, symbol, view) in self.changes {
            store
                .views
                .retain(|other| other.file != file || other.symbol != symbol);
            store.views.extend(view);
        }
        store.views.sort_by_key(|view| Reverse(view.ms));
        store.views.truncate(VIEWS);
        let mut total = 0;
        store.views.retain(|view| {
            total += view.text.len();
            total <= TEXT_BYTES
        });
        for update in self.pack {
            match store
                .pack
                .iter_mut()
                .find(|entry| entry.file == update.file)
            {
                Some(entry) if entry.revision == update.revision => {
                    for key in update.keys {
                        entry.keys.retain(|other| *other != key);
                        entry.keys.insert(0, key);
                    }
                    entry.keys.truncate(PACK_EXCERPTS);
                    entry.ms = update.ms;
                }
                Some(entry) => *entry = update,
                None => store.pack.push(update),
            }
        }
        store.pack.sort_by_key(|entry| Reverse(entry.ms));
        store.pack.truncate(PACK_FILES);
        if write_json(&self.path, &store).is_err() {
            return;
        }
        // Keep the most recently written sessions.
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        let mut sessions: Vec<_> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let modified = entry.metadata().ok()?.modified().ok()?;
                (entry.path().extension()? == "json").then(|| (modified, entry.path()))
            })
            .collect();
        if sessions.len() > SESSIONS {
            sessions.sort_by_key(|(modified, _)| Reverse(*modified));
            for (_, path) in sessions.drain(SESSIONS..) {
                if path != self.path {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }
}

/// Forgets what every session was shown. The compaction handoff calls it: after
/// compaction the agent no longer holds the source, so views print whole again.
pub(crate) fn forget(workspace: &Workspace) {
    let directory = workspace.state.join(DIRECTORY);
    if !directory.is_dir() {
        return;
    }
    let _guard = lock_state(&workspace.state.join("seen.lock"));
    let _ = fs::remove_dir_all(directory);
}

fn load(path: &std::path::Path) -> Store {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn excerpt_key(file: &str, lines: &[String]) -> String {
    let mut text = format!("{file}\0");
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    source_digest(text.as_bytes())[..24].to_owned()
}

/// A line as `str::lines` yields it, without its line ending.
fn shown(line: &str) -> &str {
    match line.strip_suffix('\n') {
        Some(line) => line.strip_suffix('\r').unwrap_or(line),
        None => line,
    }
}

fn ranges(lines: &BTreeSet<usize>) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &line in lines {
        match ranges.last_mut() {
            Some((_, to)) if *to + 1 == line => *to = line,
            _ => ranges.push((line, line)),
        }
    }
    ranges
}

/// The summary, then each changed or unseen line in `wa..=wb` and each removed
/// line, with up to `AROUND` seen lines on either side and `…` between groups.
fn hunks(
    summary: String,
    status: &[Line],
    removed: &[(usize, usize)],
    wa: usize,
    wb: usize,
) -> Vec<Row> {
    // Source order: the lines removed before a line come first.
    let mut order = Vec::new();
    let mut gone = removed.iter().peekable();
    for i in wa..=wb + 1 {
        while let Some(&&(j, _)) = gone.peek().filter(|&&&(_, anchor)| anchor <= i) {
            order.push(Row::Removed(j));
            gone.next();
        }
        if i <= wb {
            order.push(Row::Line(i));
        }
    }
    order.extend(gone.map(|&(j, _)| Row::Removed(j)));
    let interesting: Vec<bool> = order
        .iter()
        .map(|row| match row {
            Row::Line(i) => status[*i] != Line::Seen,
            _ => true,
        })
        .collect();
    let mut keep = vec![false; order.len()];
    for at in (0..order.len()).filter(|&at| interesting[at]) {
        let from = at.saturating_sub(AROUND);
        let to = (at + AROUND).min(order.len() - 1);
        keep[from..=to].fill(true);
    }
    let mut rows = vec![Row::Text(summary)];
    let mut skipped = false;
    for (row, keep) in order.into_iter().zip(keep) {
        if !keep {
            skipped = true;
            continue;
        }
        if skipped && rows.len() > 1 {
            rows.push(Row::Text("  …".to_owned()));
        }
        skipped = false;
        rows.push(row);
    }
    rows
}

/// How `new`'s lines descend from `old`'s: each new line's equal old line, if
/// any, and each old line with no equal, with the new line it would precede.
struct Alignment {
    from: Vec<Option<usize>>,
    removed: Vec<(usize, usize)>,
}

fn align(old: &[&str], new: &[&str]) -> Alignment {
    let mut ids = HashMap::new();
    let mut id = |line: &str| {
        let next = ids.len();
        *ids.entry(line.to_owned()).or_insert(next)
    };
    let old: Vec<usize> = old.iter().map(|line| id(line)).collect();
    let new: Vec<usize> = new.iter().map(|line| id(line)).collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut pairs: Vec<(usize, usize)> = (0..prefix).map(|i| (i, i)).collect();
    pairs.extend(
        common(
            &old[prefix..old.len() - suffix],
            &new[prefix..new.len() - suffix],
        )
        .into_iter()
        .map(|(a, b)| (a + prefix, b + prefix)),
    );
    pairs.extend((0..suffix).map(|k| (old.len() - suffix + k, new.len() - suffix + k)));
    let mut from = vec![None; new.len()];
    for &(a, b) in &pairs {
        from[b] = Some(a);
    }
    let mut removed = Vec::new();
    let mut matched = pairs.iter().peekable();
    // The new line after the last matched pair.
    let mut anchor = 0;
    for j in 0..old.len() {
        match matched.peek() {
            Some(&&(a, b)) if a == j => {
                anchor = b + 1;
                matched.next();
            }
            _ => removed.push((j, anchor)),
        }
    }
    Alignment { from, removed }
}

/// A longest common subsequence of `old` and `new` as index pairs, or none
/// when the table would be too large.
fn common(old: &[usize], new: &[usize]) -> Vec<(usize, usize)> {
    let (m, n) = (old.len(), new.len());
    if m == 0 || n == 0 || (m + 1).saturating_mul(n + 1) > DIFF_CELLS {
        return Vec::new();
    }
    // `table[i * w + j]` is the length of the longest common subsequence of
    // `old[i..]` and `new[j..]`; it fits in 16 bits because the smaller side
    // has at most 2,048 lines.
    let w = n + 1;
    let mut table = vec![0u16; (m + 1) * w];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            table[i * w + j] = if old[i] == new[j] {
                table[(i + 1) * w + j + 1] + 1
            } else {
                table[(i + 1) * w + j].max(table[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j, mut pairs) = (0, 0, Vec::new());
    while i < m && j < n {
        if old[i] == new[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if table[(i + 1) * w + j] >= table[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(old: &[&str], new: &[&str]) -> Vec<String> {
        // Rebuild `new` from `old` and the alignment: keep matched lines, drop
        // removed ones, and take every unmatched new line from `new`.
        let alignment = align(old, new);
        let removed: BTreeSet<usize> = alignment.removed.iter().map(|&(j, _)| j).collect();
        let kept: Vec<usize> = (0..old.len()).filter(|j| !removed.contains(j)).collect();
        let matched: Vec<usize> = alignment.from.iter().flatten().copied().collect();
        assert_eq!(kept, matched, "matched lines keep their order");
        alignment
            .from
            .iter()
            .enumerate()
            .map(|(i, from)| from.map_or(new[i], |j| old[j]).to_owned())
            .collect()
    }

    #[test]
    fn alignment_rebuilds_the_new_text_and_anchors_removals() {
        let old = ["a\n", "b\n", "c\n", "d\n", "e\n"];
        let new = ["a\n", "B\n", "c\n", "e\n", "f\n"];
        assert_eq!(apply(&old, &new), new);
        let alignment = align(&old, &new);
        assert_eq!(alignment.from, [Some(0), None, Some(2), Some(4), None]);
        assert_eq!(alignment.removed, [(1, 1), (3, 3)]);
        // A line ending is part of the line.
        let crlf = align(&["a\r\n"], &["a\n"]);
        assert_eq!(crlf.from, [None]);
        assert_eq!(crlf.removed, [(0, 0)]);
        // Moving a block shows as removed and inserted lines.
        let moved = ["x\n", "1\n", "2\n", "y\n"];
        let back = ["1\n", "2\n", "x\n", "y\n"];
        assert_eq!(apply(&moved, &back), back);
    }

    #[test]
    fn hunks_keep_changes_with_their_neighbors() {
        let status = [
            Line::Seen,
            Line::Seen,
            Line::Seen,
            Line::Seen,
            Line::Changed,
            Line::Seen,
            Line::Seen,
            Line::Seen,
            Line::Seen,
            Line::Seen,
            Line::Seen,
            Line::Unseen,
        ];
        let rows = hunks(String::new(), &status, &[(4, 4)], 0, 11);
        let shape: Vec<String> = rows
            .iter()
            .skip(1)
            .map(|row| match row {
                Row::Text(text) => text.trim().to_owned(),
                Row::Line(i) => i.to_string(),
                Row::Removed(j) => format!("-{j}"),
            })
            .collect();
        assert_eq!(shape, ["2", "3", "-4", "4", "5", "6", "…", "9", "10", "11"]);
    }
}
