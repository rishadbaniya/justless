//! The learned block in project instructions: which files receive it, how written
//! rules stay put, and the per-clone record of what was written and dismissed.
//!
//! Written lines keep their place and wording, new rules are appended, and every byte
//! outside the block is preserved, so diffs stay small and the instruction prefix
//! stays stable for prompt caching.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::workspace::{Workspace, write_json, write_text};

pub const BEGIN: &str = "<!-- justless:learned:begin -->";
pub const END: &str = "<!-- justless:learned:end -->";
const HEADING: &str = "## Learned command rules";
const INIT_BEGIN: &str = "<!-- justless:begin -->";
const INIT_END: &str = "<!-- justless:end -->";
const MAX_BYTES: usize = 1200;
const AGENTS: &str = "AGENTS.md";
const CLAUDE: &str = "CLAUDE.md";
pub const FILES: [(&str, &str); 3] = [
    ("agents", AGENTS),
    ("claude", CLAUDE),
    ("gemini", "GEMINI.md"),
];

/// Removed rules remembered for their place, so a rule that comes back returns there.
const TOMBSTONES: usize = 32;

/// `.justless/learned.json`: rules this clone wrote and rules a person dismissed.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub rules: BTreeMap<String, Applied>,
    pub dismissed: BTreeSet<String>,
    /// Rules removed from a file, with the line they followed there.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub removed: BTreeMap<String, Removed>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub class: String,
    pub scope: String,
    pub text: String,
    pub slots: BTreeMap<String, String>,
    pub sessions: u64,
    pub failures: u64,
    pub last_seen_ms: u64,
    pub files: BTreeSet<String>,
    /// When `--apply` first found a `PATH` or `$SHELL` check failing; cleared when it
    /// holds again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_since_ms: Option<u64>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Removed {
    pub at_ms: u64,
    /// Per file, the block line the rule followed, or `None` at the top.
    pub after: BTreeMap<String, Option<String>>,
}

fn state_path(workspace: &Workspace) -> PathBuf {
    workspace.state.join("learned.json")
}

pub fn load(workspace: &Workspace) -> Result<State> {
    match fs::read_to_string(state_path(workspace)) {
        Ok(text) => serde_json::from_str(&text).context("decode .justless/learned.json"),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(State {
            version: 1,
            ..State::default()
        }),
        Err(error) => Err(error).context("read .justless/learned.json"),
    }
}

/// A rule proposed in this run, in rank order.
pub struct Proposal {
    pub id: String,
    pub text: String,
    pub applied: Applied,
    /// Facts about this machine, which stay out of files Git shares by default.
    pub machine: bool,
}

pub struct Targets {
    pub files: Vec<String>,
    /// Files Git shares: tracked, or not ignored.
    pub shared: BTreeSet<String>,
    pub skipped: Vec<(String, String)>,
    pub notes: Vec<String>,
    /// A CLAUDE.md that keeps Claude Code from reading the only target, AGENTS.md.
    pub shadow: Option<PathBuf>,
}

/// Choose the instructions files. Explicit names select exactly those files; without
/// them, each existing regular AGENTS.md, CLAUDE.md and GEMINI.md that does not import
/// another is chosen once. Unless `strict`, a file that cannot be read is left out
/// with a note instead of stopping the run.
pub fn targets(
    workspace: &Workspace,
    explicit: &[String],
    transcripts: bool,
    strict: bool,
) -> Result<Targets> {
    let root = workspace.root.as_path();
    let mut targets = Targets {
        files: Vec::new(),
        shared: BTreeSet::new(),
        skipped: Vec::new(),
        notes: Vec::new(),
        shadow: None,
    };
    let readable = |file: &str, notes: &mut Vec<String>| -> Result<Option<String>> {
        match read(root, file) {
            Ok(text) => Ok(Some(text)),
            Err(error) if !strict => {
                notes.push(format!("{error:#}; `--apply` stops on it."));
                Ok(None)
            }
            Err(error) => Err(error),
        }
    };
    if !explicit.is_empty() {
        for name in explicit {
            let Some((_, file)) = FILES.iter().find(|(key, _)| key == name) else {
                bail!("unknown target {name}");
            };
            if targets.files.iter().any(|f| f == file) {
                continue;
            }
            match fs::symlink_metadata(root.join(file)) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("{file} is a symbolic link; Justless does not write through links")
                }
                Ok(metadata) => {
                    ensure!(metadata.is_file(), "{file} must be a regular file");
                    if readable(file, &mut targets.notes)?.is_none() {
                        continue;
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    if *file == CLAUDE && fs::symlink_metadata(root.join(AGENTS)).is_ok() {
                        bail!(
                            "creating CLAUDE.md would stop Claude Code from reading AGENTS.md; start CLAUDE.md with the line @AGENTS.md, or use --target agents"
                        );
                    }
                }
                Err(error) => return Err(error).with_context(|| format!("read {file}")),
            }
            targets.files.push((*file).to_owned());
        }
    } else {
        let mut present = Vec::new();
        for (_, file) in FILES {
            match fs::symlink_metadata(root.join(file)) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    targets
                        .skipped
                        .push((file.to_owned(), "symbolic link".into()));
                }
                Ok(metadata) if metadata.is_file() => {
                    // An unreadable file is still present: it must not be recreated.
                    let text = readable(file, &mut targets.notes)?;
                    present.push((file, text));
                }
                Ok(_) => targets
                    .skipped
                    .push((file.to_owned(), "not a regular file".into())),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error).with_context(|| format!("read {file}")),
            }
        }
        for (file, text) in &present {
            let Some(text) = text else {
                continue;
            };
            // An importer already shows the imported file's rules to its client.
            let imported = present.iter().find(|(other, other_text)| {
                other != file
                    && imports(text, other)
                    && !other_text
                        .as_deref()
                        .is_some_and(|other_text| imports(other_text, file))
            });
            match imported {
                Some((other, _)) => targets
                    .skipped
                    .push(((*file).to_owned(), format!("imports {other}"))),
                None => targets.files.push((*file).to_owned()),
            }
        }
        if present.is_empty() {
            let file = if transcripts { CLAUDE } else { AGENTS };
            let exists = |name: &str| fs::symlink_metadata(root.join(name)).is_ok();
            // Never replace a link, and never create CLAUDE.md next to an AGENTS.md.
            if !exists(file) && (file != CLAUDE || !exists(AGENTS)) {
                targets.files.push(file.to_owned());
            } else {
                targets.notes.push(
                    "No instructions file can take learned rules here: the existing ones are links or not regular files.".into(),
                );
            }
        }
    }
    if targets.files == [AGENTS] {
        targets.shadow = shadow(root);
    }
    targets.shared = targets
        .files
        .iter()
        .filter(|file| shared(workspace, file))
        .cloned()
        .collect();
    Ok(targets)
}

/// Whether Git shares a file: `git check-ignore` exits 1 for a tracked file and for one
/// that is not ignored, including one not created yet. Outside a repository, or
/// without Git, nothing is shared. check-ignore rejects `--literal-pathspecs`, and the
/// names here are fixed, so `Workspace::git` is not used.
fn shared(workspace: &Workspace, file: &str) -> bool {
    std::process::Command::new("git")
        .args(["check-ignore", "-q", "--", file])
        .current_dir(&workspace.root)
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|output| output.status.code() == Some(1))
}

/// A file that makes Claude Code ignore this AGENTS.md: an independent CLAUDE.md or a
/// CLAUDE.local.md here, `.claude/CLAUDE.md`, or either name in a parent directory.
fn shadow(root: &Path) -> Option<PathBuf> {
    let own = root.join(CLAUDE);
    match fs::symlink_metadata(&own) {
        Ok(metadata) if metadata.file_type().is_symlink() => return None,
        Ok(_) => {
            return fs::read_to_string(&own)
                .ok()
                .filter(|text| !imports(text, AGENTS))
                .map(|_| own);
        }
        Err(_) => {}
    }
    let user =
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude").join(CLAUDE));
    [
        root.join("CLAUDE.local.md"),
        root.join(".claude").join(CLAUDE),
    ]
    .into_iter()
    .chain(
        root.ancestors()
            .skip(1)
            .flat_map(|dir| [dir.join(CLAUDE), dir.join("CLAUDE.local.md")]),
    )
    .find(|path| path.is_file() && Some(path) != user.as_ref())
}

fn read(root: &Path, file: &str) -> Result<String> {
    match fs::read(root.join(file)) {
        Ok(bytes) => String::from_utf8(bytes).with_context(|| format!("{file} is not UTF-8")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("read {file}")),
    }
}

/// `@NAME` or `@./NAME` at the start of a line or after whitespace, up to the next
/// whitespace, outside fenced code and backtick spans, as Claude Code imports files.
/// A trailing period or other character makes a different path, which is not taken
/// as an import, so the worst case is a rule shown twice rather than never.
fn imports(text: &str, name: &str) -> bool {
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // Drop backtick spans: each run of backticks opens or closes one.
        let mut plain = String::with_capacity(line.len());
        let mut code = false;
        let mut previous = None;
        for c in line.chars() {
            if c == '`' {
                if previous != Some('`') {
                    code = !code;
                }
                plain.push(' ');
            } else {
                plain.push(if code { ' ' } else { c });
            }
            previous = Some(c);
        }
        if plain
            .split_whitespace()
            .filter_map(|word| word.strip_prefix('@'))
            .any(|path| path == name || path.strip_prefix("./") == Some(name))
        {
            return true;
        }
    }
    false
}

/// The byte range of the learned block, after checking every marker.
fn locate(text: &str, file: &str) -> Result<Option<(usize, usize)>> {
    let begins: Vec<usize> = text.match_indices(BEGIN).map(|(at, _)| at).collect();
    let ends: Vec<usize> = text.match_indices(END).map(|(at, _)| at).collect();
    let (begin, end) = match (begins.as_slice(), ends.as_slice()) {
        ([], []) => return Ok(None),
        ([begin], [end]) => (*begin, *end),
        ([], _) | (_, []) => bail!("{file} has an incomplete learned block"),
        _ => bail!("{file} has more than one learned block"),
    };
    ensure!(
        begin < end,
        "{file}: the learned block ends before it begins"
    );
    if let (Some(start), Some(stop)) = (text.find(INIT_BEGIN), text.find(INIT_END))
        && begin < stop
        && end > start
    {
        bail!(
            "{file}: the learned block is inside the Justless instructions block, which `justless init` rewrites; move it after {INIT_END}"
        );
    }
    Ok(Some((begin, end + END.len())))
}

#[derive(Clone)]
enum Line {
    Generated(String),
    User(String),
}

#[derive(Default)]
pub struct Change {
    pub file: String,
    pub content: Option<String>,
    pub lines: usize,
    /// The written rules in block order after the merge.
    pub present: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<(String, &'static str)>,
    pub dismissed: Vec<String>,
    /// Proposals the block had no room for because of the rules already in it.
    pub full: Vec<String>,
    /// Proposals that lost their room to higher-ranked rules added in this run.
    pub over: Vec<String>,
    /// Machine rules kept out of a file Git shares.
    pub machine: Vec<String>,
}

fn render(state: &State, line: &Line) -> String {
    match line {
        Line::Generated(id) => format!("- {}", state.rules[id].text),
        Line::User(text) => text.clone(),
    }
}

/// Merge proposals into one file. `removals` maps an applied rule to the reason it no
/// longer holds; such a line is removed only where this clone wrote it. Machine rules
/// are added only when `machine` allows them here.
#[allow(clippy::too_many_arguments)]
fn merge(
    file: &str,
    text: &str,
    state: &mut State,
    proposals: &[Proposal],
    removals: &BTreeMap<String, &'static str>,
    limit: usize,
    machine: bool,
    now: u64,
) -> Result<Change> {
    let span = locate(text, file)?;
    let eol = if text
        .split('\n')
        .next()
        .is_some_and(|line| line.ends_with('\r'))
        && text.contains('\n')
    {
        "\r\n"
    } else {
        "\n"
    };
    let applied: BTreeMap<String, String> = state
        .rules
        .iter()
        .filter(|(_, rule)| rule.files.contains(file))
        .map(|(id, rule)| (id.clone(), rule.text.clone()))
        .collect();
    let mut lines = Vec::new();
    if let Some((begin, end)) = span {
        let inner = &text[begin + BEGIN.len()..end - END.len()];
        let mut body: Vec<&str> = inner
            .split('\n')
            .map(|line| line.trim_end_matches('\r'))
            .collect();
        if body.first() == Some(&"") {
            body.remove(0);
        }
        if body.last() == Some(&"") {
            body.pop();
        }
        if body.first() == Some(&HEADING) {
            body.remove(0);
            if body.first() == Some(&"") {
                body.remove(0);
            }
        }
        for line in body {
            let generated = line.strip_prefix("- ").and_then(|rule| {
                applied.iter().find(|(id, text)| {
                    *text == rule
                        && !lines
                            .iter()
                            .any(|seen| matches!(seen, Line::Generated(other) if other == *id))
                })
            });
            lines.push(match generated {
                Some((id, _)) => Line::Generated(id.clone()),
                None => Line::User(line.to_owned()),
            });
        }
    }
    let mut change = Change {
        file: file.to_owned(),
        ..Change::default()
    };
    let present = |lines: &[Line], id: &str| {
        lines
            .iter()
            .any(|line| matches!(line, Line::Generated(other) if other == id))
    };
    // A missing line in a present block is a dismissal; a missing block or file (a
    // revert, another branch) only forgets what was written here.
    for id in applied.keys() {
        if !present(&lines, id) {
            if span.is_some() {
                state.dismissed.insert(id.clone());
                change.dismissed.push(id.clone());
            }
            unlink(state, id, file);
        }
    }
    // A removed rule remembers the line it followed, so it can return to its place.
    let mut kept: Vec<Line> = Vec::with_capacity(lines.len());
    for line in lines {
        if let Line::Generated(id) = &line
            && let Some(reason) = removals.get(id)
        {
            change.removed.push((id.clone(), reason));
            let after = kept.last().map(|line| render(state, line));
            let removed = state.removed.entry(id.clone()).or_insert_with(|| Removed {
                at_ms: now,
                after: BTreeMap::new(),
            });
            removed.at_ms = now;
            removed.after.insert(file.to_owned(), after);
            unlink(state, id, file);
            continue;
        }
        kept.push(line);
    }
    let mut lines = kept;
    // On a clone without learned.json, a line with a proposal's exact text is adopted.
    for line in &mut lines {
        if let Line::User(text) = line
            && let Some(proposal) = text.strip_prefix("- ").and_then(|rule| {
                proposals
                    .iter()
                    .find(|p| p.text == rule && !state.dismissed.contains(&p.id))
            })
            && !state
                .rules
                .get(&proposal.id)
                .is_some_and(|rule| rule.files.contains(file))
        {
            link(state, proposal, file);
            *line = Line::Generated(proposal.id.clone());
        }
    }
    let text_of =
        |state: &State, id: &str| state.rules.get(id).map_or(0, |rule| rule.text.len() + 3);
    let mut count = lines
        .iter()
        .filter(|line| matches!(line, Line::Generated(_)))
        .count();
    let mut bytes: usize = lines
        .iter()
        .map(|line| match line {
            Line::Generated(id) => text_of(state, id),
            Line::User(_) => 0,
        })
        .sum();
    let (count_before, bytes_before) = (count, bytes);
    for proposal in proposals {
        if present(&lines, &proposal.id) {
            // Refresh the counts of a rule that is still proposed; its text stays.
            if let Some(rule) = state.rules.get_mut(&proposal.id) {
                rule.sessions = proposal.applied.sessions;
                rule.failures = proposal.applied.failures;
                rule.last_seen_ms = proposal.applied.last_seen_ms;
            }
            continue;
        }
        if state.dismissed.contains(&proposal.id) {
            continue;
        }
        if proposal.machine && !machine {
            change.machine.push(proposal.id.clone());
            continue;
        }
        let width = proposal.text.len() + 3;
        if count < limit && bytes + width <= MAX_BYTES {
            count += 1;
            bytes += width;
            // A rule that was removed here returns after the line it followed.
            let place = state
                .removed
                .get(&proposal.id)
                .and_then(|removed| removed.after.get(file))
                .and_then(|after| match after {
                    None => Some(0),
                    Some(text) => lines
                        .iter()
                        .position(|line| render(state, line) == *text)
                        .map(|at| at + 1),
                })
                .unwrap_or(lines.len());
            link(state, proposal, file);
            lines.insert(place, Line::Generated(proposal.id.clone()));
            change.added.push(proposal.id.clone());
            if let Some(removed) = state.removed.get_mut(&proposal.id) {
                removed.after.remove(file);
                if removed.after.is_empty() {
                    state.removed.remove(&proposal.id);
                }
            }
        } else if count_before < limit && bytes_before + width <= MAX_BYTES {
            change.over.push(proposal.id.clone());
        } else {
            change.full.push(proposal.id.clone());
        }
    }
    change.lines = count;
    change.present = lines
        .iter()
        .filter_map(|line| match line {
            Line::Generated(id) => Some(id.clone()),
            Line::User(_) => None,
        })
        .collect();
    let rendered: Vec<String> = lines.iter().map(|line| render(state, line)).collect();
    let block = (!rendered.is_empty()).then(|| {
        [BEGIN, HEADING, ""]
            .into_iter()
            .map(str::to_owned)
            .chain(rendered)
            .chain([END.to_owned()])
            .collect::<Vec<_>>()
            .join(eol)
    });
    // Without an added or removed line, the block keeps the bytes a person gave it.
    if change.added.is_empty() && change.removed.is_empty() {
        return Ok(change);
    }
    let updated = place(text, span, block.as_deref(), eol);
    change.content = (updated != text).then_some(updated);
    Ok(change)
}

fn link(state: &mut State, proposal: &Proposal, file: &str) {
    let rule = state
        .rules
        .entry(proposal.id.clone())
        .or_insert_with(|| Applied {
            files: BTreeSet::new(),
            ..proposal.applied.clone()
        });
    rule.files.insert(file.to_owned());
}

fn unlink(state: &mut State, id: &str, file: &str) {
    if let Some(rule) = state.rules.get_mut(id) {
        rule.files.remove(file);
        if rule.files.is_empty() {
            state.rules.remove(id);
        }
    }
}

/// Replace, insert or remove the block, keeping every other byte.
fn place(text: &str, span: Option<(usize, usize)>, block: Option<&str>, eol: &str) -> String {
    match (span, block) {
        (Some((begin, end)), Some(block)) => format!("{}{block}{}", &text[..begin], &text[end..]),
        (Some((begin, end)), None) => {
            let (before, after) = (&text[..begin], &text[end..]);
            let blank = format!("{eol}{eol}");
            if before.ends_with(&blank) && after.starts_with(eol) {
                format!(
                    "{}{}",
                    &before[..before.len() - eol.len()],
                    &after[eol.len()..]
                )
            } else if before.ends_with(&blank) && after.is_empty() {
                before[..before.len() - blank.len()].to_owned()
            } else if let Some(after) = after.strip_prefix(eol) {
                format!("{before}{after}")
            } else {
                format!("{before}{after}")
            }
        }
        (None, Some(block)) => match text.find(INIT_END) {
            Some(at) => {
                let at = at + INIT_END.len();
                format!("{}{eol}{eol}{block}{}", &text[..at], &text[at..])
            }
            None if text.is_empty() => format!("{block}{eol}"),
            None if text.ends_with('\n') => format!("{text}{eol}{block}{eol}"),
            None => format!("{text}{eol}{eol}{block}{eol}"),
        },
        (None, None) => text.to_owned(),
    }
}

pub struct Outcome {
    pub changes: Vec<Change>,
}

/// What `--apply` would change: merge every target in memory. Unless `strict`, a
/// file whose block cannot be merged is left out with a note.
pub struct Plan<'a> {
    pub targets: &'a Targets,
    pub proposals: &'a [Proposal],
    pub removals: &'a BTreeMap<String, &'static str>,
    pub limit: usize,
    pub include_machine: bool,
    pub strict: bool,
    pub now: u64,
}

pub fn plan(
    root: &Path,
    state: &mut State,
    plan: &Plan,
    notes: &mut Vec<String>,
) -> Result<Outcome> {
    let mut changes = Vec::new();
    for file in &plan.targets.files {
        let machine = plan.include_machine || !plan.targets.shared.contains(file);
        let merged = read(root, file).and_then(|text| {
            merge(
                file,
                &text,
                state,
                plan.proposals,
                plan.removals,
                plan.limit,
                machine,
                plan.now,
            )
        });
        match merged {
            Ok(change) => changes.push(change),
            Err(error) if !plan.strict => notes.push(format!("{error:#}; `--apply` stops on it.")),
            Err(error) => return Err(error),
        }
    }
    while state.removed.len() > TOMBSTONES {
        let oldest = state
            .removed
            .iter()
            .min_by_key(|(id, removed)| (removed.at_ms, (*id).clone()))
            .map(|(id, _)| id.clone())
            .expect("a tombstone");
        state.removed.remove(&oldest);
    }
    Ok(Outcome { changes })
}

/// Write the changed files and the state after a strict plan, so a marker error in any
/// file writes nothing. Callers hold `.justless/setup.lock`.
pub fn write(
    workspace: &Workspace,
    state: &mut State,
    original: &State,
    outcome: &Outcome,
) -> Result<()> {
    for change in &outcome.changes {
        if let Some(content) = &change.content {
            write_text(&workspace.root.join(&change.file), content)
                .with_context(|| format!("write {}", change.file))?;
        }
    }
    state.version = 1;
    if state != original {
        write_json(&state_path(workspace), state).context("write .justless/learned.json")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(id: &str, text: &str) -> Proposal {
        Proposal {
            id: id.into(),
            text: text.into(),
            applied: Applied {
                class: "zsh_glob".into(),
                scope: "machine".into(),
                text: text.into(),
                slots: BTreeMap::new(),
                sessions: 2,
                failures: 3,
                last_seen_ms: 0,
                files: BTreeSet::new(),
                stale_since_ms: None,
            },
            machine: false,
        }
    }

    /// The text `merge` writes to AGENTS.md, with a limit of 8 rules.
    fn merged(text: &str, state: &mut State, proposals: &[Proposal], removed: &[&str]) -> String {
        let removals = removed.iter().map(|id| (id.to_string(), "stale")).collect();
        super::merge("AGENTS.md", text, state, proposals, &removals, 8, true, 0)
            .unwrap()
            .content
            .unwrap()
    }

    #[test]
    fn removed_rule_returns_to_its_line() {
        let proposals = [
            proposal("a", "Rule A."),
            proposal("b", "Rule B."),
            proposal("c", "Rule C."),
        ];
        let mut state = State::default();
        let written = merged("# A\n", &mut state, &proposals, &[]);
        for id in ["a", "b", "c"] {
            let without = merged(&written, &mut state, &[], &[id]);
            assert!(!without.contains(&format!("Rule {}.", id.to_uppercase())));
            let back = merged(&without, &mut state, &proposals, &[]);
            assert_eq!(back, written, "{id}");
            assert!(state.removed.is_empty());
        }
    }

    #[test]
    fn insertion_and_removal_restore_bytes() {
        for original in [
            "",
            "# Notes\n",
            "# Notes",
            "intro\n<!-- justless:begin -->\nx\n<!-- justless:end -->\nafter\n",
            "intro\n<!-- justless:begin -->\nx\n<!-- justless:end -->",
            "# Notes\r\nmore\r\n",
        ] {
            let mut state = State::default();
            let written = merged(original, &mut state, &[proposal("a", "Rule A.")], &[]);
            assert!(written.contains("\n- Rule A."), "{written:?}");
            if original.contains("\r\n") {
                assert!(
                    written.contains("- Rule A.\r\n<!-- justless:learned:end -->"),
                    "{written:?}"
                );
            }
            let restored = merged(&written, &mut state, &[], &["a"]);
            let expected = if original == "# Notes" {
                "# Notes\n"
            } else {
                original
            };
            assert_eq!(restored, expected);
        }
    }

    #[test]
    fn rejects_bad_markers() {
        for text in [
            "<!-- justless:learned:begin -->\n",
            "<!-- justless:learned:end -->\n<!-- justless:learned:begin -->",
            "<!-- justless:learned:begin -->\n<!-- justless:learned:end -->\n<!-- justless:learned:begin -->\n<!-- justless:learned:end -->",
            "<!-- justless:begin -->\n<!-- justless:learned:begin -->\n<!-- justless:learned:end -->\n<!-- justless:end -->",
        ] {
            assert!(locate(text, "AGENTS.md").is_err(), "{text}");
        }
    }

    #[test]
    fn finds_imports_outside_fences() {
        assert!(imports("@AGENTS.md\n", "AGENTS.md"));
        assert!(imports("intro\n  @./AGENTS.md\n", "AGENTS.md"));
        assert!(imports("Follow @AGENTS.md for everything.\n", "AGENTS.md"));
        assert!(imports("See @README and\t@./AGENTS.md\n", "AGENTS.md"));
        assert!(!imports("See `@AGENTS.md`.\n", "AGENTS.md"));
        assert!(!imports("Run ``x @AGENTS.md y`` here\n", "AGENTS.md"));
        assert!(!imports("```\n@AGENTS.md\n```\n", "AGENTS.md"));
        assert!(!imports("mail me@AGENTS.md or @AGENTS.mdx\n", "AGENTS.md"));
        assert!(!imports("See @AGENTS.md.\n", "AGENTS.md"));
    }
}
