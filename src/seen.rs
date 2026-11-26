//! What `context` and `pack` showed of each symbol in an agent session, so a
//! later view of the same symbol prints only what changed since.
//!
//! The store keeps the exact text of each symbol a view printed, not a digest
//! or a Git revision: the read, edit, read loop runs on an uncommitted working
//! tree, so Git cannot reproduce the earlier text, and only the text itself
//! makes the line diff exact. A record advances only when the agent was shown
//! every line it had seen that is gone now, so a later view never leans on a
//! line the agent still believes is there. Without a session the views print
//! whole, as before. The compaction handoff forgets the session's records in
//! every project, and every record in the project its hook runs in, because the
//! agent's context no longer holds what it was shown.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capture::source_digest;
use crate::context::Report;
use crate::workspace::{Workspace, display_path, lock_state, shell_text, write_json};

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
/// Files remembered per session for `read`/`expand` range and outline dedup.
const FILES: usize = 64;
/// Total cached file text remembered per session for range deltas.
const FILE_TOTAL_BYTES: usize = 1024 * 1024;
/// A larger file keeps its revision and seen ranges without caching full text
/// for line diffs, so unchanged re-reads still compact while edits print whole.
const FILE_TEXT_BYTES: usize = 128 * 1024;
/// Unchanged lines printed around each change.
const AROUND: usize = 2;
/// The largest line table the diff fills after trimming the common start and
/// end. A larger middle counts as replaced whole, which is still exact.
const DIFF_CELLS: usize = 4 << 20;

/// The session a view belongs to: `--session`, which the Claude Code hook adds
/// to `context` and `pack` commands from the hook's session and subagent IDs.
/// `CLAUDE_CODE_SESSION_ID` in the agent's shell is not used: a subagent has
/// the same one but a context of its own, and Claude Code marks top-level
/// shells as child sessions too, so the shell cannot tell them apart.
fn session(explicit: Option<&str>) -> Option<String> {
    explicit.filter(|name| !name.is_empty()).map(str::to_owned)
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    views: Vec<View>,
    /// The `pack` excerpts printed from each file, newest first.
    #[serde(default)]
    pack: Vec<PackFile>,
    /// File ranges and outlines shown by `read` / `expand`, newest first.
    #[serde(default)]
    files: Vec<FileView>,
}

/// What a session was shown of `file`: its text and seen 1-based line ranges
/// at `revision`, plus the revision of its full outline if shown.
#[derive(Clone, Serialize, Deserialize)]
struct FileView {
    file: String,
    revision: String,
    text: String,
    #[serde(default)]
    seen_ranges: Vec<(usize, usize)>,
    #[serde(default)]
    outlined_revision: Option<String>,
    ms: u64,
}

impl FileView {
    fn saw_line(&self, line: usize) -> bool {
        self.seen_ranges
            .iter()
            .any(|&(from, to)| from <= line && line <= to)
    }

    fn covers(&self, first: usize, last: usize) -> bool {
        if first == 0 || first > last || self.seen_ranges.is_empty() {
            return false;
        }
        if self
            .seen_ranges
            .iter()
            .any(|&(from, to)| from <= first && last <= to)
        {
            return true;
        }
        if self.seen_ranges.len() == 1 {
            return false;
        }
        // Fallback for unmerged adjacent/overlapping ranges (e.g. loaded from disk).
        let mut cursor = first;
        loop {
            let mut extended = false;
            for &(from, to) in &self.seen_ranges {
                if from <= cursor && to >= cursor {
                    if to >= last {
                        return true;
                    }
                    cursor = to + 1;
                    extended = true;
                }
            }
            if !extended {
                return false;
            }
        }
    }
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
    /// File range/outline records to replace, by workspace-relative file.
    file_changes: Vec<(String, Option<FileView>)>,
    /// Views printed as changes, with the bytes their whole views would have
    /// printed and the bytes they did, for `stats`.
    saved: (u64, u64, u64),
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
        let path = workspace.state.join(DIRECTORY).join(file_name(&key));
        Some(Self {
            store: load(&path),
            path,
            fresh,
            changes: Vec::new(),
            pack: Vec::new(),
            file_changes: Vec::new(),
            saved: (0, 0, 0),
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
        let width = decimal_digits(number(wb));
        let modified = !window_removed.is_empty() || (wa..=wb).any(|i| status[i] == Line::Changed);
        let whole_view = full(
            since
                .as_ref()
                .filter(|_| modified)
                .map(|since| format!("changed since you read it at {since}"))
                .as_deref(),
        );
        // What the view prints without a session, for `stats`.
        let whole_bytes: usize = whole_view
            .iter()
            .map(|row| match row {
                Row::Text(_) => location.len() + 1,
                Row::Line(i) => 2 + decimal_digits(number(*i)) + 3 + shown(lines[*i]).len() + 1,
                Row::Removed(_) => 0,
            })
            .sum();
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
        let room = report.left();
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
        if delta {
            self.count_saved(whole_bytes.min(room), room.saturating_sub(report.left()));
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
        let seen_ranges = ranges_from_iter((0..lines.len()).filter(|&i| known(i)));
        if seen_ranges.is_empty() {
            return;
        }
        let text_len: usize = lines.iter().map(|line| line.len()).sum();
        let view = (text_len <= VIEW_BYTES).then(|| {
            let mut text = String::with_capacity(text_len);
            for line in lines {
                text.push_str(line);
            }
            View {
                file: symbol.file.to_owned(),
                symbol: symbol.name.to_owned(),
                start: symbol.start,
                revision,
                text,
                seen: seen_ranges,
                ms: crate::history::now_ms(),
            }
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

    fn current_file(&self, file: &str) -> Option<&FileView> {
        if let Some((_, updated)) = self
            .file_changes
            .iter()
            .rev()
            .find(|(other, _)| other == file)
        {
            updated.as_ref()
        } else {
            self.store.files.iter().find(|entry| entry.file == file)
        }
    }

    fn set_file(&mut self, file: &str, view: FileView) {
        if let Some((_, slot)) = self
            .file_changes
            .iter_mut()
            .rev()
            .find(|(other, _)| other == file)
        {
            *slot = Some(view);
        } else {
            self.file_changes.push((file.to_owned(), Some(view)));
        }
    }

    fn touch_file(&mut self, file: &str, ms: u64) {
        if let Some((_, slot)) = self
            .file_changes
            .iter_mut()
            .rev()
            .find(|(other, _)| other == file)
        {
            if let Some(view) = slot.as_mut() {
                view.ms = ms;
            }
            return;
        }
        if let Some(view) = self.store.files.iter().find(|entry| entry.file == file) {
            let mut touched = view.clone();
            touched.ms = ms;
            self.file_changes.push((file.to_owned(), Some(touched)));
        }
    }

    /// Records that this session was shown inclusive 1-based lines `first..=last`
    /// of `file` at `revision`.
    pub(crate) fn record_range(
        &mut self,
        file: &str,
        revision: &str,
        text: &str,
        first: usize,
        last: usize,
    ) {
        if first == 0 || last < first {
            return;
        }
        let ms = crate::history::now_ms();
        // Fast path: mutate an already-staged FileView in place without cloning `text`.
        if let Some((_, Some(view))) = self
            .file_changes
            .iter_mut()
            .rev()
            .find(|(other, _)| other == file)
        {
            if view.revision == revision {
                view.seen_ranges.push((first, last));
                view.seen_ranges = merge_line_ranges(std::mem::take(&mut view.seen_ranges));
                if view.text.is_empty() && text.len() <= FILE_TEXT_BYTES {
                    view.text = text.to_owned();
                }
                view.ms = ms;
                return;
            }
            let kept_outline = view.outlined_revision.take().filter(|rev| rev == revision);
            view.revision.clear();
            view.revision.push_str(revision);
            view.text = if text.len() <= FILE_TEXT_BYTES {
                text.to_owned()
            } else {
                String::new()
            };
            view.seen_ranges.clear();
            view.seen_ranges.push((first, last));
            view.outlined_revision = kept_outline;
            view.ms = ms;
            return;
        }
        let updated = match self.current_file(file) {
            Some(view) if view.revision == revision => {
                let mut view = view.clone();
                view.seen_ranges.push((first, last));
                view.seen_ranges = merge_line_ranges(view.seen_ranges);
                if view.text.is_empty() && text.len() <= FILE_TEXT_BYTES {
                    view.text = text.to_owned();
                }
                view.ms = ms;
                view
            }
            Some(view) => FileView {
                file: file.to_owned(),
                revision: revision.to_owned(),
                text: if text.len() <= FILE_TEXT_BYTES {
                    text.to_owned()
                } else {
                    String::new()
                },
                seen_ranges: vec![(first, last)],
                outlined_revision: view
                    .outlined_revision
                    .as_deref()
                    .filter(|&rev| rev == revision)
                    .map(str::to_owned),
                ms,
            },
            None => FileView {
                file: file.to_owned(),
                revision: revision.to_owned(),
                text: if text.len() <= FILE_TEXT_BYTES {
                    text.to_owned()
                } else {
                    String::new()
                },
                seen_ranges: vec![(first, last)],
                outlined_revision: None,
                ms,
            },
        };
        self.set_file(file, updated);
    }

    /// Compares inclusive 1-based lines `first..=last` of `file` (whose full
    /// bytes are `text` at `revision`, and whose caller-rendered slice is `whole`)
    /// with what this session already saw. Returns a 1-line unchanged notice or a
    /// line-level exact delta when strictly smaller than `whole`, or `None` when
    /// `whole` should be printed as-is.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn show_range(
        &mut self,
        file: &str,
        revision: &str,
        text: &str,
        first: usize,
        last: usize,
        whole: &str,
        fresh: bool,
        full_hint: &str,
    ) -> Option<String> {
        let total_lines = line_count(text);
        let last = last.min(total_lines);
        if first == 0 || first > last {
            return None;
        }
        let effective_fresh = self.fresh || fresh;
        let path = display_path(file);
        let location = format!("{path}:{first}-{last}");

        // Fast path: unchanged file at the same revision. Inspect by reference
        // first so unchanged re-reads avoid cloning cached file text and line vecs.
        let same_rev = self
            .current_file(file)
            .is_some_and(|view| view.revision == revision);
        if same_rev {
            let covers = self
                .current_file(file)
                .is_some_and(|view| view.covers(first, last));
            if covers {
                if !effective_fresh {
                    let rev_short = &revision[..revision.len().min(12)];
                    let notice = format!(
                        "{location} · unchanged since you read it at {rev_short} · {full_hint}\n"
                    );
                    if notice.len() < whole.len() {
                        self.count_saved(whole.len(), notice.len());
                        self.touch_file(file, crate::history::now_ms());
                        return Some(notice);
                    }
                }
                self.record_range(file, revision, text, first, last);
                return None;
            }
            // Partial overlap at the same revision: print only the unseen lines
            // with surrounding context when smaller than `whole`.
            if !effective_fresh {
                let wa = first - 1;
                let wb = last - 1;
                let rendered = {
                    let old_view = self.current_file(file)?;
                    let status: Vec<Line> = (0..total_lines)
                        .map(|i| {
                            if old_view.saw_line(i + 1) {
                                Line::Seen
                            } else {
                                Line::Unseen
                            }
                        })
                        .collect();
                    let count = wb - wa + 1;
                    let seen = (wa..=wb).filter(|&i| status[i] == Line::Seen).count();
                    if seen > 0 {
                        let new_lines: Vec<&str> = text.split_inclusive('\n').collect();
                        let rev_short = &revision[..revision.len().min(12)];
                        let summary = format!(
                            "{location} · {seen} of {count} lines unchanged since you read them at {rev_short} · {full_hint}"
                        );
                        let rows = hunks(summary, &status, &[], wa, wb);
                        Some(render_range_rows(&rows, &status, &new_lines, &[], last))
                    } else {
                        None
                    }
                };
                if let Some(rendered) = rendered
                    && rendered.len() < whole.len()
                {
                    self.count_saved(whole.len(), rendered.len());
                    self.record_range(file, revision, text, first, last);
                    return Some(rendered);
                }
            }
            self.record_range(file, revision, text, first, last);
            return None;
        }

        // Different revision (or first read of this file).
        let can_diff = self
            .current_file(file)
            .is_some_and(|view| !view.text.is_empty() && !view.seen_ranges.is_empty());
        if !can_diff {
            self.record_range(file, revision, text, first, last);
            return None;
        }

        let wa = first - 1;
        let wb = last - 1;
        let new_lines: Vec<&str> = text.split_inclusive('\n').collect();
        let (delta, all_removed_covered, showed_anything_new, new_seen_ranges, kept_outline) = {
            let old_view = self.current_file(file)?;
            let old_lines: Vec<&str> = old_view.text.split_inclusive('\n').collect();
            let alignment = align(&old_lines, &new_lines);
            let status: Vec<Line> = alignment
                .from
                .iter()
                .map(|from| match from {
                    Some(j) if old_view.saw_line(*j + 1) => Line::Seen,
                    None => Line::Changed,
                    Some(_) => Line::Unseen,
                })
                .collect();
            let removed: Vec<(usize, usize)> = alignment
                .removed
                .iter()
                .copied()
                .filter(|&(j, _)| old_view.saw_line(j + 1))
                .collect();
            let window_removed: Vec<(usize, usize)> = removed
                .iter()
                .copied()
                .filter(|&(_, anchor)| (wa..=wb + 1).contains(&anchor))
                .collect();
            let count = wb - wa + 1;
            let seen = (wa..=wb).filter(|&i| status[i] == Line::Seen).count();
            let changed = !window_removed.is_empty() || seen < count;
            let rev_short = &old_view.revision[..old_view.revision.len().min(12)];

            let candidate = if effective_fresh {
                None
            } else if !changed {
                Some(format!(
                    "{location} · unchanged since you read it at {rev_short} · {full_hint}\n"
                ))
            } else if seen > 0 || !window_removed.is_empty() {
                let summary = format!(
                    "{location} · {seen} of {count} lines unchanged since you read them at {rev_short} · {full_hint}"
                );
                let rows = hunks(summary, &status, &window_removed, wa, wb);
                Some(render_range_rows(
                    &rows, &status, &new_lines, &old_lines, last,
                ))
            } else {
                None
            };

            let delta = candidate.filter(|rendered| rendered.len() < whole.len());
            let whole_file = wa == 0 && wb + 1 == new_lines.len();
            let shown_removed: BTreeSet<usize> = if delta.is_some() {
                window_removed.iter().map(|&(j, _)| j).collect()
            } else {
                BTreeSet::new()
            };
            let all_removed_covered = removed.iter().all(|&(j, anchor)| {
                shown_removed.contains(&j)
                    || (delta.is_none() && (whole_file || (wa < anchor && anchor <= wb)))
            });
            let showed_anything_new =
                !window_removed.is_empty() || (wa..=wb).any(|i| status[i] != Line::Seen);
            let new_seen_ranges = if all_removed_covered {
                ranges_from_iter(
                    (0..new_lines.len())
                        .filter(|&i| status[i] == Line::Seen || (wa..=wb).contains(&i))
                        .map(|i| i + 1),
                )
            } else {
                Vec::new()
            };
            let kept_outline = old_view
                .outlined_revision
                .as_deref()
                .filter(|&rev| rev == revision)
                .map(str::to_owned);
            (
                delta,
                all_removed_covered,
                showed_anything_new,
                new_seen_ranges,
                kept_outline,
            )
        };

        if all_removed_covered || showed_anything_new {
            let stored_text = if text.len() <= FILE_TEXT_BYTES {
                text.to_owned()
            } else {
                String::new()
            };
            let ms = crate::history::now_ms();
            let seen_ranges = if all_removed_covered {
                new_seen_ranges
            } else {
                // Unshown removals exist outside this window, while this view
                // revealed new lines at `revision`. Reset to only `first..=last`
                // so a revert or a later read of the other lines is never hidden.
                vec![(first, last)]
            };
            let updated = FileView {
                file: file.to_owned(),
                revision: revision.to_owned(),
                text: stored_text,
                seen_ranges,
                outlined_revision: kept_outline,
                ms,
            };
            self.set_file(file, updated);
        }
        // Otherwise: unchanged notice with unshown edits outside this window —
        // keep `old_view` intact so those edits are still shown when read.

        if let Some(ref rendered) = delta {
            self.count_saved(whole.len(), rendered.len());
        }
        delta
    }

    /// Checks whether this session already saw the full outline of `file` at
    /// `revision`. Returns a 1-line unchanged notice when smaller than `whole`
    /// and `--fresh` was not given; otherwise records `outlined_revision` while
    /// preserving any seen source ranges for future line diffs.
    pub(crate) fn saw_outline(
        &mut self,
        file: &str,
        revision: &str,
        whole: &str,
        fresh: bool,
        full_hint: &str,
    ) -> Option<String> {
        let effective_fresh = self.fresh || fresh;
        let ms = crate::history::now_ms();
        let already_outlined = self
            .current_file(file)
            .is_some_and(|view| view.outlined_revision.as_deref() == Some(revision));
        if already_outlined {
            if !effective_fresh {
                let path = display_path(file);
                let rev_short = &revision[..revision.len().min(12)];
                let notice = format!("{path} · outline unchanged at {rev_short} · {full_hint}\n");
                if notice.len() < whole.len() {
                    self.count_saved(whole.len(), notice.len());
                    self.touch_file(file, ms);
                    return Some(notice);
                }
            }
            return None;
        }
        if let Some((_, Some(view))) = self
            .file_changes
            .iter_mut()
            .rev()
            .find(|(other, _)| other == file)
        {
            view.outlined_revision = Some(revision.to_owned());
            view.ms = ms;
            return None;
        }
        let updated = match self.current_file(file) {
            Some(view) => {
                let mut view = view.clone();
                view.outlined_revision = Some(revision.to_owned());
                view.ms = ms;
                view
            }
            None => FileView {
                file: file.to_owned(),
                revision: revision.to_owned(),
                text: String::new(),
                seen_ranges: Vec::new(),
                outlined_revision: Some(revision.to_owned()),
                ms,
            },
        };
        self.set_file(file, updated);
        None
    }

    /// Writes the records this command changed back to the session's file,
    /// over what other commands wrote meanwhile, within the store's bounds.
    /// Counts a view that printed `shown` bytes where the whole view would
    /// have printed `whole`.
    pub(crate) fn count_saved(&mut self, whole: usize, shown: usize) {
        self.saved.0 += 1;
        self.saved.1 += whole as u64;
        self.saved.2 += shown as u64;
    }

    pub(crate) fn save(self, workspace: &Workspace) {
        let (views, whole, shown) = self.saved;
        if views > 0 {
            crate::history::saw_delta(workspace, views, whole, shown);
        }
        if self.changes.is_empty() && self.pack.is_empty() && self.file_changes.is_empty() {
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
        for (file, updated) in self.file_changes {
            store.files.retain(|other| other.file != file);
            if let Some(view) = updated {
                store.files.push(view);
            }
        }
        store.files.sort_by_key(|entry| Reverse(entry.ms));
        store.files.truncate(FILES);
        let mut file_text_total = 0;
        for entry in &mut store.files {
            if file_text_total + entry.text.len() > FILE_TOTAL_BYTES {
                entry.text.clear();
            } else {
                file_text_total += entry.text.len();
            }
        }
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

/// The file under [`DIRECTORY`] that holds session `key`'s records.
fn file_name(key: &str) -> String {
    format!("{}.json", &source_digest(key.as_bytes())[..24])
}

/// Forgets what the agent was shown. The compaction handoff calls it: after
/// compaction the agent no longer holds the source, so views print whole again.
/// Every session's records in `workspace`, where the hook runs, are forgotten,
/// and with `session`, the `--session` value the hook gives the compacted
/// session's commands, its records in every other project the registry lists,
/// such as a nested repository or another `--root` its commands read. Other
/// sessions' records there stay, since their agents still hold what they read.
pub(crate) fn forget(workspace: &Workspace, session: Option<&str>) {
    clear(&workspace.state, None);
    let Some(session) = session.filter(|session| !session.is_empty()) else {
        return;
    };
    let name = file_name(session);
    for project in crate::registry::projects() {
        if project.root != workspace.root {
            clear(&project.root.join(".justless"), Some(&name));
        }
    }
}

/// Removes session file `name` from the records under `state`, or all of them.
/// The lock is taken only when there is something to remove.
fn clear(state: &Path, name: Option<&str>) {
    let directory = state.join(DIRECTORY);
    let target = match name {
        Some(name) => directory.join(name),
        None => directory,
    };
    if fs::symlink_metadata(&target).is_err() {
        return;
    }
    let _guard = lock_state(&state.join("seen.lock"));
    let _ = match name {
        Some(_) => fs::remove_file(&target),
        None => fs::remove_dir_all(&target),
    };
}

fn load(path: &std::path::Path) -> Store {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn excerpt_key(file: &str, lines: &[String]) -> String {
    let capacity = file.len() + 1 + lines.iter().map(|line| line.len() + 1).sum::<usize>();
    let mut text = String::with_capacity(capacity);
    text.push_str(file);
    text.push('\0');
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    source_digest(text.as_bytes())[..24].to_owned()
}

#[inline]
fn decimal_digits(mut n: usize) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// Exact line count matching `text.split_inclusive('\n').count()` without allocating.
#[inline]
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.as_bytes().iter().filter(|&&b| b == b'\n').count() + usize::from(!text.ends_with('\n'))
}

/// A line as `str::lines` yields it, without its line ending.
fn shown(line: &str) -> &str {
    match line.strip_suffix('\n') {
        Some(line) => line.strip_suffix('\r').unwrap_or(line),
        None => line,
    }
}

fn ranges_from_iter(lines: impl IntoIterator<Item = usize>) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for line in lines {
        match ranges.last_mut() {
            Some((_, to)) if *to + 1 == line => *to = line,
            _ => ranges.push((line, line)),
        }
    }
    ranges
}

fn merge_line_ranges(mut input: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    input.retain(|&(from, to)| from > 0 && from <= to);
    input.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(input.len());
    for (from, to) in input {
        match merged.last_mut() {
            Some((_, prev_to)) if from <= prev_to.saturating_add(1) => {
                *prev_to = (*prev_to).max(to);
            }
            _ => merged.push((from, to)),
        }
    }
    merged
}

fn render_range_rows(
    rows: &[Row],
    status: &[Line],
    new_lines: &[&str],
    old_lines: &[&str],
    last_line: usize,
) -> String {
    let width = decimal_digits(last_line);
    let capacity: usize = rows
        .iter()
        .map(|row| match row {
            Row::Text(text) => text.len() + 1,
            Row::Line(i) => width + 6 + new_lines[*i].len(),
            Row::Removed(j) => width + 6 + old_lines[*j].len(),
        })
        .sum();
    let mut out = String::with_capacity(capacity);
    for row in rows {
        match row {
            Row::Text(text) => {
                out.push_str(text);
                out.push('\n');
            }
            Row::Line(i) => {
                let mark = if status[*i] == Line::Changed {
                    '+'
                } else {
                    ' '
                };
                let _ = writeln!(out, "{mark} {:>width$} | {}", i + 1, shown(new_lines[*i]));
            }
            Row::Removed(j) => {
                let _ = writeln!(out, "- {:width$} | {}", "", shown(old_lines[*j]));
            }
        }
    }
    out
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

fn align<'a>(old: &[&'a str], new: &[&'a str]) -> Alignment {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];
    let mut pairs: Vec<(usize, usize)> =
        Vec::with_capacity(prefix + suffix + old_mid.len().min(new_mid.len()));
    pairs.extend((0..prefix).map(|i| (i, i)));
    if !old_mid.is_empty() && !new_mid.is_empty() {
        let mut ids: HashMap<&'a str, usize> =
            HashMap::with_capacity(old_mid.len().max(new_mid.len()));
        let mut id = |line: &'a str| {
            let next = ids.len();
            *ids.entry(line).or_insert(next)
        };
        let old_ids: Vec<usize> = old_mid.iter().map(|&line| id(line)).collect();
        let new_ids: Vec<usize> = new_mid.iter().map(|&line| id(line)).collect();
        pairs.extend(
            common(&old_ids, &new_ids)
                .into_iter()
                .map(|(a, b)| (a + prefix, b + prefix)),
        );
    }
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

    fn make_lines(count: usize) -> String {
        (1..=count)
            .map(|i| format!("    let item_{i:02} = compute_value({i});\n"))
            .collect()
    }

    fn test_memory(dir: &tempfile::TempDir, session: &str, fresh: bool) -> (Workspace, Memory) {
        let ws = Workspace::discover(Some(dir.path())).unwrap();
        let mem = Memory::open(&ws, Some(session), fresh).unwrap();
        (ws, mem)
    }

    #[test]
    fn legacy_seen_store_without_files_deserializes_cleanly() {
        let legacy = r#"{"views":[],"pack":[]}"#;
        let store: Store = serde_json::from_str(legacy).unwrap();
        assert!(store.files.is_empty());
        assert!(store.views.is_empty());
        assert!(store.pack.is_empty());
    }

    #[test]
    fn show_range_emits_unchanged_delta_fresh_and_handles_reverts() {
        let dir = tempfile::tempdir().unwrap();
        let text1 = make_lines(40);
        let rev1 = crate::source::revision_for("src/lib.rs", text1.as_bytes());
        let hint = "full: justless expand src/lib.rs L1-40 --fresh";

        // First read prints whole and records the seen range.
        let (ws, mut mem) = test_memory(&dir, "s-range", false);
        assert!(
            mem.show_range("src/lib.rs", &rev1, &text1, 1, 40, &text1, false, hint)
                .is_none()
        );
        mem.save(&ws);

        // Second unchanged read returns a 1-line notice.
        let (ws, mut mem) = test_memory(&dir, "s-range", false);
        let unchanged = mem
            .show_range("src/lib.rs", &rev1, &text1, 1, 40, &text1, false, hint)
            .expect("unchanged notice");
        assert_eq!(unchanged.lines().count(), 1, "{unchanged}");
        assert!(
            unchanged.contains("src/lib.rs:1-40 · unchanged since you read it at "),
            "{unchanged}"
        );
        assert!(unchanged.contains(hint), "{unchanged}");
        // `--fresh` bypasses dedup.
        assert!(
            mem.show_range("src/lib.rs", &rev1, &text1, 1, 40, &text1, true, hint)
                .is_none()
        );
        mem.save(&ws);

        // Small edit produces a line-level delta with AROUND = 2 context.
        let text2 = text1.replace("compute_value(20)", "compute_value(200)");
        let rev2 = crate::source::revision_for("src/lib.rs", text2.as_bytes());
        let (ws, mut mem) = test_memory(&dir, "s-range", false);
        let delta = mem
            .show_range("src/lib.rs", &rev2, &text2, 1, 40, &text2, false, hint)
            .expect("edit delta");
        assert!(delta.len() < text2.len());
        assert!(delta.contains("39 of 40 lines unchanged"), "{delta}");
        assert!(
            delta.contains("+ 20 |     let item_20 = compute_value(200);\n"),
            "{delta}"
        );
        assert!(
            delta.contains("-    |     let item_20 = compute_value(20);\n"),
            "{delta}"
        );
        assert!(
            delta.contains("  18 |     let item_18 = compute_value(18);\n"),
            "{delta}"
        );
        assert!(
            delta.contains("  22 |     let item_22 = compute_value(22);\n"),
            "{delta}"
        );
        assert!(!delta.contains("item_05"), "{delta}");
        mem.save(&ws);

        // Edit two lines, view only the top window, then revert the top line:
        // reading the whole file must still show BOTH the reverted top line and
        // the unshown bottom edit.
        let text3 = text2
            .replace("compute_value(5)", "compute_value(55)")
            .replace("compute_value(35)", "compute_value(355)");
        let rev3 = crate::source::revision_for("src/lib.rs", text3.as_bytes());
        let top_slice: String = text3.split_inclusive('\n').take(15).collect();
        let (ws, mut mem) = test_memory(&dir, "s-range", false);
        let partial = mem
            .show_range("src/lib.rs", &rev3, &text3, 1, 15, &top_slice, false, hint)
            .expect("partial delta");
        assert!(partial.contains("compute_value(55)"), "{partial}");
        assert!(!partial.contains("compute_value(355)"), "{partial}");
        mem.save(&ws);

        let text4 = text3.replace("compute_value(55)", "compute_value(5)");
        let rev4 = crate::source::revision_for("src/lib.rs", text4.as_bytes());
        let (ws, mut mem) = test_memory(&dir, "s-range", false);
        let after_revert = mem.show_range("src/lib.rs", &rev4, &text4, 1, 40, &text4, false, hint);
        // Either printed whole (None) or a delta that includes both lines 5 and 35.
        if let Some(view) = after_revert {
            assert!(view.contains("compute_value(5)"), "{view}");
            assert!(view.contains("compute_value(355)"), "{view}");
            assert!(!view.contains("· unchanged since you read it"), "{view}");
        }
        mem.save(&ws);
    }

    #[test]
    fn record_range_merges_and_saw_outline_preserves_range_history() {
        let dir = tempfile::tempdir().unwrap();
        let text1 = make_lines(30);
        let rev1 = crate::source::revision_for("src/a.rs", text1.as_bytes());
        let outline_text = "revision abc · 30 lines\n".to_owned()
            + &"#1 L1-30 function demo  pub fn demo()\n".repeat(6);
        let outline_hint = "full outline: justless read src/a.rs --outline --fresh";
        let range_hint = "full: justless expand src/a.rs L1-30 --fresh";

        let (ws, mut mem) = test_memory(&dir, "s-outline", false);
        mem.record_range("src/a.rs", &rev1, &text1, 1, 15);
        mem.record_range("src/a.rs", &rev1, &text1, 16, 30);
        // Merged range covers 1..=30 so show_range emits unchanged.
        let hit = mem
            .show_range("src/a.rs", &rev1, &text1, 1, 30, &text1, false, range_hint)
            .expect("merged range unchanged");
        assert!(hit.contains("unchanged since you read it"), "{hit}");
        // First outline returns None, second returns 1-line notice.
        assert!(
            mem.saw_outline("src/a.rs", &rev1, &outline_text, false, outline_hint)
                .is_none()
        );
        let notice = mem
            .saw_outline("src/a.rs", &rev1, &outline_text, false, outline_hint)
            .expect("outline unchanged");
        assert_eq!(notice.lines().count(), 1, "{notice}");
        assert!(notice.contains("outline unchanged at"), "{notice}");
        assert!(
            mem.saw_outline("src/a.rs", &rev1, &outline_text, true, outline_hint)
                .is_none()
        );
        mem.save(&ws);

        // Editing the file invalidates the outline, and re-outlining at rev2
        // still preserves rev1's seen text so a follow-up range read diffs cleanly.
        let text2 = text1.replace("compute_value(12)", "compute_value(120)");
        let rev2 = crate::source::revision_for("src/a.rs", text2.as_bytes());
        let (ws, mut mem) = test_memory(&dir, "s-outline", false);
        assert!(
            mem.saw_outline("src/a.rs", &rev2, &outline_text, false, outline_hint)
                .is_none()
        );
        let delta = mem
            .show_range("src/a.rs", &rev2, &text2, 1, 30, &text2, false, range_hint)
            .expect("delta survives outline refresh");
        assert!(delta.contains("compute_value(120)"), "{delta}");
        assert!(delta.contains("29 of 30 lines unchanged"), "{delta}");
        mem.save(&ws);
    }

    #[test]
    fn dedup_fast_paths_handle_unmerged_ranges_partial_overlaps_and_large_files() {
        // 1. `FileView::covers` handles single, unsorted adjacent, overlapping, and gapped ranges.
        let view = FileView {
            file: "a.rs".into(),
            revision: "rev1".into(),
            text: String::new(),
            seen_ranges: vec![(11, 20), (1, 10), (25, 30)],
            outlined_revision: None,
            ms: 1,
        };
        assert!(view.covers(1, 20));
        assert!(view.covers(5, 15));
        assert!(!view.covers(1, 25));
        assert!(!view.covers(0, 10));
        assert!(!view.covers(15, 10));

        // 2. `align` fast paths: identical, pure insertion, pure deletion, and no-trailing-newline.
        let base = ["a\n", "b\n", "c\n"];
        assert_eq!(apply(&base, &base), base);
        let inserted = ["a\n", "x\n", "y\n", "b\n", "c\n"];
        assert_eq!(apply(&base, &inserted), inserted);
        let deleted = ["a\n", "c\n"];
        assert_eq!(apply(&base, &deleted), deleted);
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("only"), 1);
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count("a\nb"), 2);

        // 3. Same-revision partial overlap emits delta for unseen lines only,
        //    and coalesced in-memory `record_range` updates don't duplicate entries.
        let dir = tempfile::tempdir().unwrap();
        let text = make_lines(50);
        let rev = crate::source::revision_for("src/big.rs", text.as_bytes());
        let hint = "full: justless expand src/big.rs L1-50 --fresh";
        let (ws, mut mem) = test_memory(&dir, "s-edge", false);
        mem.record_range("src/big.rs", &rev, &text, 1, 35);
        mem.record_range("src/big.rs", &rev, &text, 36, 40);
        assert_eq!(mem.file_changes.len(), 1);
        let partial = mem
            .show_range("src/big.rs", &rev, &text, 1, 50, &text, false, hint)
            .expect("partial same-rev delta");
        assert!(partial.contains("40 of 50 lines unchanged"), "{partial}");
        assert!(partial.contains("item_45"), "{partial}");
        assert!(!partial.contains("item_05"), "{partial}");
        // Now 1..=50 is fully covered; re-reading emits 1-line unchanged notice.
        let unchanged = mem
            .show_range("src/big.rs", &rev, &text, 1, 50, &text, false, hint)
            .expect("now fully covered");
        assert!(
            unchanged.contains("unchanged since you read it"),
            "{unchanged}"
        );
        mem.save(&ws);

        // 4. Large file (> FILE_TEXT_BYTES): caches revision + ranges without text,
        //    still emits unchanged notice on repeat read and prints whole after edit.
        let large_text = make_lines(3500);
        assert!(large_text.len() > FILE_TEXT_BYTES);
        let large_rev = crate::source::revision_for("src/huge.rs", large_text.as_bytes());
        let (ws, mut mem) = test_memory(&dir, "s-large", false);
        assert!(
            mem.show_range(
                "src/huge.rs",
                &large_rev,
                &large_text,
                1,
                3500,
                &large_text,
                false,
                hint,
            )
            .is_none()
        );
        let hit = mem
            .show_range(
                "src/huge.rs",
                &large_rev,
                &large_text,
                1,
                3500,
                &large_text,
                false,
                hint,
            )
            .expect("large unchanged notice");
        assert!(hit.contains("unchanged since you read it"), "{hit}");
        let large_edited = large_text.replacen("compute_value(1)", "compute_value(999)", 1);
        let large_rev2 = crate::source::revision_for("src/huge.rs", large_edited.as_bytes());
        assert!(
            mem.show_range(
                "src/huge.rs",
                &large_rev2,
                &large_edited,
                1,
                3500,
                &large_edited,
                false,
                hint,
            )
            .is_none()
        );
        mem.save(&ws);
    }
}
