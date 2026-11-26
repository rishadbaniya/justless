//! Adaptive preview budgets. When `run` has no `--max-bytes`, each command
//! family's budget follows how often agents replayed in full a stream that the
//! budget had clipped.
//!
//! The advisor is deterministic and local. It reads the project's run records,
//! which streams agents replayed in full, and `.justless/budgets.json`, which
//! holds each family's current step and when it last changed. Only runs completed
//! since that change count, so every step is judged by the runs it produced and
//! the budget moves one step at a time. `.justless/budget-runs.json` caches the
//! few facts it needs from each finished run, so a `run` reads only records it
//! has not seen.
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::history::{human_bytes, now_ms};
use crate::workspace::{Settings, Workspace, lock_state, write_json};

/// Clipped runs considered at the current budget, newest first.
const WINDOW: usize = 20;
/// Clipped runs needed at the current budget before it can be raised. Lowering
/// needs a full window: a smaller preview can hide evidence, while a larger one
/// costs only bytes.
const MIN_SAMPLE: usize = 5;
/// A budget lowered and then raised back to where it was has relapsed. Each
/// relapse doubles the runs needed to lower it again, up to `WINDOW` times 8.
const MAX_RELAPSES: u32 = 3;
/// Runs cached per family: twice the longest window needed to lower.
const KEEP: usize = (WINDOW << MAX_RELAPSES) * 2;
/// No budget is smaller, so a run whose streams both fit in it is never limited
/// by one and is not cached.
const SMALLEST: u64 = 512;
const STATE: &str = "budgets.json";
/// Per run, how often each stream was replayed in full.
const REPLAYS: &str = "replays.json";

/// The configured default and the range the budget may move within.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Limits {
    default: usize,
    floor: usize,
    cap: usize,
}

impl Limits {
    fn new(settings: &Settings) -> Self {
        let default = settings.max_output_bytes;
        Self {
            default,
            floor: settings
                .adaptive_floor_bytes
                .unwrap_or((default / 2).max(512))
                .min(default),
            cap: settings
                .adaptive_cap_bytes
                .unwrap_or(default.saturating_mul(4))
                .max(default),
        }
    }

    /// The budget `level` steps from the default: each step doubles or halves it.
    fn budget(self, level: i32) -> usize {
        if level >= 0 {
            let factor = 1usize
                .checked_shl(level.unsigned_abs())
                .unwrap_or(usize::MAX);
            self.default.saturating_mul(factor).min(self.cap)
        } else {
            let reduced = self.default.checked_shr(level.unsigned_abs()).unwrap_or(0);
            reduced.max(self.floor)
        }
    }

    /// The level nearest the default with the same budget, so a step saved under a
    /// wider range comes back in one step after the cap or floor moves in.
    fn normalize(self, level: i32) -> i32 {
        let mut level = level.clamp(-32, 32);
        while level > 0 && self.budget(level - 1) == self.budget(level) {
            level -= 1;
        }
        while level < 0 && self.budget(level + 1) == self.budget(level) {
            level += 1;
        }
        level
    }
}

/// What replaced a run's plain preview: nothing, a summary or grouped report of
/// stdout, or a lint report of both streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Replaced {
    Nothing,
    Stdout,
    Both,
}

/// What the advisor keeps of a finished run. Records never change once written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Brief {
    id: String,
    completed_ms: u64,
    /// Sizes of the stdout and stderr logs.
    stdout: u64,
    stderr: u64,
    /// Preview bytes shown for both streams, without the footer.
    shown: u64,
    replaced: Replaced,
    /// Preview byte budget recorded for this run, when present in `record.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget: Option<u64>,
    /// Whether the run used an explicit budget (such as `--max-bytes`).
    #[serde(default, skip_serializing_if = "is_false")]
    explicit: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Brief {
    /// Which streams a budget of `at` limits, for a run judged at the family's
    /// `budget`: a plain stream larger than `at` is clipped. A summary or grouped
    /// report that took at least a third of the budget was cut to fit, as a
    /// grouped search is. A smaller one, such as a failing `cargo test` reported
    /// in a few hundred bytes, reads the same at any budget, so its stream is not
    /// limited.
    fn limited(&self, at: u64, budget: u64) -> [bool; 2] {
        match self.replaced {
            Replaced::Nothing => [self.stdout > at, self.stderr > at],
            Replaced::Stdout => {
                let report = self.shown.saturating_sub(self.stderr.min(budget));
                [
                    self.stdout > at && report.saturating_mul(3) >= budget,
                    self.stderr > at,
                ]
            }
            Replaced::Both => {
                let filled = self.shown.saturating_mul(3) >= budget;
                [self.stdout > at && filled, self.stderr > at && filled]
            }
        }
    }

    /// Whether the run's preview came from `budget`. Newer records persist
    /// `budget_bytes` and `explicit_budget`, so explicit `--max-bytes` overrides
    /// and runs made at another step are excluded exactly, while well-compacted
    /// reports are never falsely dropped. Older records without those fields
    /// fall back to checking whether `shown` fits what `budget` could produce.
    fn made_at(&self, budget: u64) -> bool {
        if self.explicit {
            return false;
        }
        if let Some(recorded) = self.budget {
            return recorded == budget;
        }
        let fits = |size: u64| size.min(budget);
        let clipped = |size: u64| u64::from(size > budget);
        let (most, clipped) = match self.replaced {
            Replaced::Nothing => (
                fits(self.stdout).saturating_add(fits(self.stderr)),
                clipped(self.stdout) + clipped(self.stderr),
            ),
            Replaced::Stdout => (
                budget.saturating_add(fits(self.stderr)),
                clipped(self.stderr),
            ),
            Replaced::Both => (
                budget.max(fits(self.stdout).saturating_add(fits(self.stderr))),
                0,
            ),
        };
        (clipped.saturating_mul(budget) / 4..=most).contains(&self.shown)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Replays {
    #[serde(default)]
    stdout: u64,
    #[serde(default)]
    stderr: u64,
}

#[derive(Deserialize)]
struct Recalls {
    #[serde(default)]
    full: u64,
}

fn read_into(path: &Path, buf: &mut Vec<u8>) -> bool {
    buf.clear();
    File::open(path)
        .and_then(|mut file| file.read_to_end(buf))
        .is_ok()
}

/// Which streams of run `id` under `runs_dir` the agent replayed in full,
/// reusing `path` and `buf` to avoid per-run allocations. A run replayed before
/// streams were recorded counts both when `recalls.json` has a full replay.
fn replayed_in(path: &mut PathBuf, buf: &mut Vec<u8>, id: &str) -> [bool; 2] {
    path.push(id);
    path.push(REPLAYS);
    if read_into(path, buf) {
        path.pop();
        path.pop();
        let replays: Replays = serde_json::from_slice(buf).unwrap_or_default();
        return [replays.stdout > 0, replays.stderr > 0];
    }
    path.set_file_name("recalls.json");
    let full = read_into(path, buf)
        && serde_json::from_slice::<Recalls>(buf).is_ok_and(|recalls| recalls.full > 0);
    path.pop();
    path.pop();
    [full, full]
}

/// Records that `justless log` replayed `stream` (`stdout`, `stderr` or `both`) of
/// the run in `directory` in full, so a replay counts only against a budget that
/// limited that stream.
pub fn note_replay(directory: &Path, stream: &str) {
    let path = directory.join(REPLAYS);
    let Ok(_guard) = lock_state(&path.with_extension("lock")) else {
        return;
    };
    let mut replays: Replays = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if stream != "stderr" {
        replays.stdout = replays.stdout.saturating_add(1);
    }
    if stream != "stdout" {
        replays.stderr = replays.stderr.saturating_add(1);
    }
    let _ = write_json(&path, &replays);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Step {
    #[default]
    Hold,
    Raise,
    Lower,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Decision {
    step: Step,
    /// Runs the current budget clipped, at most `WINDOW`.
    clipped: usize,
    /// How many of them were replayed in full where it clipped them.
    reopened: usize,
    /// Runs the lowering threshold limits, at most `window`, and how many of them
    /// were replayed in full where it does.
    over: usize,
    over_reopened: usize,
    /// The runs needed to lower.
    window: usize,
    /// Among the newest runs, those replayed in full only where nothing was
    /// limited, such as a summarized stdout.
    uncounted: usize,
}

/// The size a run's stream must exceed to count toward lowering the budget at
/// `level`. Above the default it is the budget one step down, so a stream shown
/// whole that a lower step would clip counts, and a raise that is no longer needed
/// wears off. At or below the default only clipped streams count: a stream shown
/// whole says nothing about a budget below the configured one.
fn lowering_threshold(limits: Limits, level: i32) -> u64 {
    let budget = limits.budget(level);
    if budget > limits.default {
        limits.budget(level.saturating_sub(1)) as u64
    } else {
        budget as u64
    }
}

/// Decides the next step from the runs made since the last change, newest first.
/// Raise when at least a quarter of five or more clipped runs were replayed in
/// full where they were clipped. Lower after `window` runs over the lowering
/// threshold that nobody replayed in full. Targeted lookups count toward
/// neither: they mean the preview's line markers led to what was needed. Runs
/// reported as unchanged, and runs whose preview did not come from this budget,
/// are left out.
#[cfg(test)]
fn decide(
    limits: Limits,
    level: i32,
    window: usize,
    runs: &[Brief],
    replayed: impl FnMut(&Brief) -> [bool; 2],
) -> Decision {
    decide_inner(limits, level, window, runs, replayed, true)
}

fn decide_inner(
    limits: Limits,
    level: i32,
    window: usize,
    runs: &[Brief],
    mut replayed: impl FnMut(&Brief) -> [bool; 2],
    need_details: bool,
) -> Decision {
    let budget = limits.budget(level) as u64;
    let threshold = lowering_threshold(limits, level);
    let can_lower = limits.budget(level.saturating_sub(1)) < budget as usize;
    let mut decision = Decision {
        window,
        ..Decision::default()
    };
    let made_here = runs.iter().filter(|run| run.made_at(budget));
    for (index, run) in made_here.enumerate() {
        let here = run.limited(budget, budget);
        let below = run.limited(threshold, budget);
        let count_here = here.contains(&true) && decision.clipped < WINDOW;
        let check_below = can_lower && (need_details || decision.over_reopened == 0);
        let count_below = check_below && below.contains(&true) && decision.over < window;
        let recent = need_details && index < WINDOW;
        if !(count_here || count_below || recent) {
            if decision.clipped == WINDOW && (!check_below || decision.over == window) {
                break;
            }
            continue;
        }
        let replay = replayed(run);
        let hit = |limited: [bool; 2]| (limited[0] && replay[0]) || (limited[1] && replay[1]);
        if count_here {
            decision.clipped += 1;
            decision.reopened += usize::from(hit(here));
        }
        if count_below {
            decision.over += 1;
            decision.over_reopened += usize::from(hit(below));
        }
        if recent && replay.contains(&true) && !hit(here) && !hit(below) {
            decision.uncounted += 1;
        }
    }
    decision.step = if wants_raise(decision.clipped, decision.reopened)
        && limits.budget(level.saturating_add(1)) > budget as usize
    {
        Step::Raise
    } else if can_lower && decision.over >= window && decision.over_reopened == 0 {
        Step::Lower
    } else {
        Step::Hold
    };
    decision
}

fn wants_raise(clipped: usize, reopened: usize) -> bool {
    clipped >= MIN_SAMPLE && reopened.saturating_mul(4) >= clipped
}

/// The family `capture` will record for `args`, and the command label beside it.
fn family(args: &[OsString]) -> String {
    use crate::filters::{Profile, Selection};
    // Project filters never change the profile, so none are needed here.
    let profile = Selection::for_run(args, &[])
        .ok()
        .and_then(|selection| selection.profile);
    match profile {
        Some(Profile::Lint | Profile::Search | Profile::Listing) => {
            crate::filters::tool_label(args)
        }
        Some(profile) => profile.label().to_owned(),
        None => crate::summary::family(args),
    }
}

/// The budget key for a run as a borrowed slice: its family, or its command
/// label when that names the command more precisely, as `git log` does within
/// `git` and `make test` within `other`. Runs that only the catch-all `other`
/// or `legacy` describe mix unrelated commands, as runners do, so they keep
/// the default.
fn key_str<'a>(family: &'a str, label: &'a str) -> Option<&'a str> {
    let specific = family == "other"
        || label
            .strip_prefix(family)
            .is_some_and(|rest| rest.starts_with(' '));
    let candidate = if specific { label } else { family };
    (!matches!(candidate, "" | "other" | "legacy") && !runner(candidate)).then_some(candidate)
}

fn key(family: &str, label: &str) -> Option<String> {
    key_str(family, label).map(str::to_owned)
}

/// Whether a key names a runner rather than the work it runs: a shell or
/// interpreter given a script, a `run` or `exec` subcommand such as `npm run` or
/// `cargo run`, or a program that takes subcommands labeled without one because
/// options came first, such as `make -j8 lint` or `git -C dir log`. Its runs mix
/// unrelated scripts and targets. A key that selects a command profile, such as
/// the linter `golangci-lint run`, names the work.
fn runner(key: &str) -> bool {
    let looks_like_runner = match key.split_once(' ') {
        Some((_, subcommand)) => matches!(subcommand, "run" | "exec" | "dlx" | "x"),
        None => {
            key.starts_with("python")
                || key.starts_with("pypy")
                || matches!(
                    key,
                    "sh" | "bash"
                        | "zsh"
                        | "dash"
                        | "ksh"
                        | "fish"
                        | "pwsh"
                        | "powershell"
                        | "env"
                        | "time"
                        | "nice"
                        | "nohup"
                        | "timeout"
                        | "xargs"
                        | "sudo"
                        | "doas"
                        | "watch"
                        | "ssh"
                        | "node"
                        | "tsx"
                        | "ts-node"
                        | "ruby"
                        | "perl"
                        | "php"
                        | "lua"
                        | "java"
                        | "julia"
                        | "Rscript"
                        | "osascript"
                        | "just"
                        | "task"
                )
                || crate::history::label(&[key, "build"]) != key
        }
    };
    if !looks_like_runner {
        return false;
    }
    let words: Vec<OsString> = key.split(' ').map(OsString::from).collect();
    crate::filters::Selection::for_run(&words, &[])
        .ok()
        .and_then(|selection| selection.profile)
        .is_none()
}

fn command_key(args: &[OsString]) -> Option<String> {
    let words: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
    key(&family(args), &crate::history::label(&words))
}

#[derive(Default)]
struct HasElements(bool);

impl<'de> Deserialize<'de> for HasElements {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = HasElements;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a sequence")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut any = false;
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    any = true;
                }
                Ok(HasElements(any))
            }
        }
        deserializer.deserialize_seq(Visitor)
    }
}

#[derive(Default, Deserialize)]
struct RecordSummary<'a> {
    #[serde(default, borrow)]
    kind: Cow<'a, str>,
    #[serde(default)]
    items: HasElements,
    #[serde(default)]
    outcomes: HasElements,
}

/// The fields of a run record the advisor reads.
#[derive(Deserialize)]
struct Record<'a> {
    #[serde(default, borrow)]
    family: Cow<'a, str>,
    #[serde(default, borrow)]
    label: Cow<'a, str>,
    #[serde(default)]
    status: i32,
    #[serde(default)]
    raw_bytes: u64,
    #[serde(default)]
    preview_bytes: u64,
    #[serde(default)]
    completed_ms: u64,
    #[serde(default)]
    unchanged: bool,
    #[serde(default, borrow)]
    summary: RecordSummary<'a>,
    #[serde(default, borrow)]
    replaced: Option<Cow<'a, str>>,
    #[serde(default)]
    budget_bytes: Option<u64>,
    #[serde(default)]
    explicit_budget: bool,
}

impl Record<'_> {
    /// What replaced the plain preview: persisted directly by `capture` when
    /// available, falling back to the legacy summary heuristic for older
    /// records written before `replaced` and `budget_bytes` existed.
    fn replaced(&self) -> Replaced {
        match self.replaced.as_deref() {
            Some("both") => return Replaced::Both,
            Some("stdout") => return Replaced::Stdout,
            Some("nothing") => return Replaced::Nothing,
            _ => {}
        }
        if self.budget_bytes.is_some() {
            return Replaced::Nothing;
        }
        let summary = &self.summary;
        let renders = !summary.kind.is_empty()
            && (summary.items.0 || summary.outcomes.0)
            && !(self.status != 0
                && (summary.kind == "inventory" || summary.kind == "tests" && !summary.items.0));
        match summary.kind.as_ref() {
            _ if !renders => Replaced::Nothing,
            "lint" => Replaced::Both,
            _ => Replaced::Stdout,
        }
    }
}

/// A finished run's budget key and brief. `None` marks a run outside every
/// budget, one reported as unchanged, one too small for any budget to limit, or
/// a damaged record.
#[cfg(test)]
fn brief(id: &str, directory: &Path, record: &[u8]) -> Option<(String, Brief)> {
    let mut path = directory.to_path_buf();
    brief_in(id, &mut path, record)
}

fn brief_in(id: &str, directory: &mut PathBuf, record: &[u8]) -> Option<(String, Brief)> {
    let run: Record<'_> = serde_json::from_slice(record).ok()?;
    if run.unchanged {
        return None;
    }
    let key = key_str(&run.family, &run.label)?;
    directory.push("stdout");
    let stdout = fs::metadata(&*directory).ok().map(|m| m.len());
    directory.set_file_name("stderr");
    let stderr = fs::metadata(&*directory).ok().map(|m| m.len());
    directory.pop();
    let (stdout, stderr) = match (stdout, stderr) {
        (Some(stdout), Some(stderr)) => (stdout, stderr),
        _ => (run.raw_bytes, 0),
    };
    if stdout.max(stderr) <= SMALLEST {
        return None;
    }
    let replaced = run.replaced();
    Some((
        key.to_owned(),
        Brief {
            id: id.to_owned(),
            completed_ms: run.completed_ms,
            stdout,
            stderr,
            shown: run.preview_bytes,
            replaced,
            budget: run.budget_bytes,
            explicit: run.explicit_budget,
        },
    ))
}

/// Briefs of finished runs, cached in `.justless/budget-runs.json`. `seen` lists
/// every run already read, one ID per line, so a `run` lists the runs directory
/// and reads only new records. `runs` keeps the newest `KEEP` briefs of each key.
/// A cache written by another version, or one that lists a deleted run, is
/// rebuilt.
#[derive(Default, Serialize, Deserialize)]
struct Index {
    version: u32,
    #[serde(default)]
    seen: String,
    #[serde(default)]
    runs: BTreeMap<String, Vec<Brief>>,
}

const INDEX: &str = "budget-runs.json";
const INDEX_VERSION: u32 = 3;

/// Loads and refreshes the cached run index. An unfinished run is skipped until
/// its record exists, and a damaged record silently: this runs before every
/// command, and a warning there would reach the agent with the command's output.
fn load_index(workspace: &Workspace, save: bool) -> Option<Index> {
    let directory = workspace.state.join("runs");
    let path = workspace.state.join(INDEX);
    let cached = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Index>(&bytes).ok())
        .filter(|index| index.version == INDEX_VERSION);
    let (mut index, changed) = match cached.map(|index| refresh(&directory, index)) {
        Some(Some(refreshed)) => refreshed,
        // A run was deleted, or there was no usable cache.
        _ => match refresh(&directory, Index::default()) {
            Some((index, _)) => (index, true),
            None => return None,
        },
    };
    index.version = INDEX_VERSION;
    if save && changed {
        let _ = write_json(&path, &index);
    }
    Some(index)
}

fn recorded_for(workspace: &Workspace, key: &str, save: bool) -> Vec<Brief> {
    load_index(workspace, save)
        .and_then(|mut index| index.runs.remove(key))
        .unwrap_or_default()
}

fn recorded_all(workspace: &Workspace, save: bool) -> BTreeMap<String, Vec<Brief>> {
    load_index(workspace, save)
        .map(|index| index.runs)
        .unwrap_or_default()
}

/// Adds the runs in `directory` that `index` has not seen, and whether any were
/// added or cleaned up. `None` when the directory cannot be listed or a seen
/// run is gone. Corrupt or empty lines in `index.seen` are skipped cleanly.
fn refresh(directory: &Path, mut index: Index) -> Option<(Index, bool)> {
    let entries = fs::read_dir(directory).ok()?;
    index.runs.retain(|key, _| !key.trim().is_empty());
    let seen_text = std::mem::take(&mut index.seen);
    let mut seen = HashSet::new();
    let mut seen_dirty = !seen_text.is_empty() && !seen_text.ends_with('\n');
    for line in seen_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("run-") && trimmed.len() > "run-".len() {
            if trimmed.len() != line.len() || !seen.insert(trimmed) {
                seen_dirty = true;
            }
        } else {
            seen_dirty = true;
        }
    }
    let mut found = 0usize;
    let mut added = Vec::new();
    let mut path = directory.to_path_buf();
    let mut record_buf = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str() else {
            continue;
        };
        if !id.starts_with("run-") {
            continue;
        }
        if !seen.is_empty() && seen.contains(id) {
            found += 1;
            continue;
        }
        path.push(id);
        path.push("record.json");
        let has_record = read_into(&path, &mut record_buf);
        path.pop();
        if has_record {
            let brief = brief_in(id, &mut path, &record_buf);
            added.push((id.to_owned(), brief));
        }
        path.pop();
    }
    if found < seen.len() {
        return None;
    }
    let mut seen_text = if seen_dirty {
        let mut rebuilt = String::with_capacity(seen_text.len());
        for id in &seen {
            rebuilt.push_str(id);
            rebuilt.push('\n');
        }
        drop(seen);
        rebuilt
    } else {
        drop(seen);
        seen_text
    };
    let changed = seen_dirty || !added.is_empty();
    let mut keys = HashSet::new();
    for (id, brief) in added {
        seen_text.push_str(&id);
        seen_text.push('\n');
        if let Some((key, brief)) = brief {
            if let Some(list) = index.runs.get_mut(&key) {
                if !list.iter().any(|known| known.id == brief.id) {
                    list.push(brief);
                    if !keys.contains(&key) {
                        keys.insert(key);
                    }
                }
            } else {
                index.runs.insert(key.clone(), vec![brief]);
                keys.insert(key);
            }
        }
    }
    for key in keys {
        let list = index.runs.get_mut(&key).expect("key was just added");
        list.sort_by_key(|a| std::cmp::Reverse(a.completed_ms));
        list.truncate(KEEP);
    }
    index.seen = seen_text;
    Some((index, changed))
}

/// A family's saved step.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    level: i32,
    /// When the step last changed. Runs completed before it ran at another budget.
    since_ms: u64,
    /// Why it changed, as `stats` shows it.
    #[serde(default)]
    reason: String,
    /// The level the family was last lowered from, and how often evidence raised
    /// it back there since. Each return doubles the runs needed to lower it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lowered_from: Option<i32>,
    #[serde(default, skip_serializing_if = "is_zero")]
    relapses: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl Entry {
    /// The runs over the lowering threshold needed to lower from `level`.
    fn window(entry: Option<&Self>, limits: Limits, level: i32) -> usize {
        let relapses = entry
            .filter(|entry| entry.lowered_from.map(|from| limits.normalize(from)) == Some(level))
            .map_or(0, |entry| entry.relapses.min(MAX_RELAPSES));
        WINDOW << relapses
    }

    /// The entry after a step from `from` to `to`. Raising back to the level the
    /// family was last lowered from is a relapse; lowering from it again keeps the
    /// count, and any other step starts over.
    fn step(old: Option<&Self>, limits: Limits, from: i32, to: i32, since_ms: u64) -> Self {
        let lowered = old.and_then(|old| {
            old.lowered_from
                .map(|level| (limits.normalize(level), old.relapses))
        });
        let (lowered_from, relapses) = match lowered {
            Some((level, relapses)) if to > from && to == level => {
                (Some(level), relapses.saturating_add(1))
            }
            Some((level, relapses)) if to < from && from == level => (Some(level), relapses),
            _ if to < from => (Some(from), 0),
            _ => (None, 0),
        };
        Self {
            level: to,
            since_ms,
            reason: String::new(),
            lowered_from,
            relapses,
        }
    }
}

type State = BTreeMap<String, Entry>;

fn parse_state(bytes: &[u8]) -> State {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return State::new();
    }
    if let Ok(mut state) = serde_json::from_slice::<State>(bytes) {
        state.retain(|key, _| !key.trim().is_empty());
        return state;
    }
    let Ok(serde_json::Value::Object(map)) = serde_json::from_slice(bytes) else {
        return State::new();
    };
    let mut state = State::new();
    for (key, value) in map {
        if key.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_value::<Entry>(value) {
            state.insert(key, entry);
        }
    }
    state
}

fn read_state(workspace: &Workspace) -> State {
    fs::read(workspace.state.join(STATE))
        .ok()
        .map_or_else(State::new, |bytes| parse_state(&bytes))
}

/// The budget the next run of a family gets, and why.
#[derive(Debug)]
struct Advice {
    bytes: usize,
    /// The budget of the saved level, kept when a change cannot be saved.
    saved_bytes: usize,
    /// The entry to save when the level changes.
    change: Option<Entry>,
    reason: String,
    decision: Decision,
}

fn advise(
    limits: Limits,
    entry: Option<&Entry>,
    runs: &[Brief],
    replayed: impl FnMut(&Brief) -> [bool; 2],
    now: u64,
) -> Advice {
    advise_inner(limits, entry, runs, replayed, now, true)
}

fn advise_inner(
    limits: Limits,
    entry: Option<&Entry>,
    runs: &[Brief],
    replayed: impl FnMut(&Brief) -> [bool; 2],
    now: u64,
    need_details: bool,
) -> Advice {
    let saved = entry.map_or(0, |entry| limits.normalize(entry.level));
    let since = entry.map_or(0, |entry| entry.since_ms);
    let recent = runs
        .iter()
        .take_while(|run| run.completed_ms > since)
        .count();
    let window = Entry::window(entry, limits, saved);
    let decision = decide_inner(
        limits,
        saved,
        window,
        &runs[..recent],
        replayed,
        need_details,
    );
    let level = match decision.step {
        Step::Raise => saved + 1,
        Step::Lower => saved - 1,
        Step::Hold => saved,
    };
    let change = (level != saved).then(|| {
        let budget = human_bytes(limits.budget(saved) as u64);
        let mut next = Entry::step(entry, limits, saved, level, now);
        next.reason = match decision.step {
            Step::Raise => {
                let mut reason = format!(
                    "raised from {budget}: {}",
                    reopened_text(decision.clipped, decision.reopened)
                );
                if next.relapses > 0 {
                    let _ = write!(
                        reason,
                        " · raised back here {}, so lowering it again waits for {} runs",
                        times(next.relapses),
                        Entry::window(Some(&next), limits, level)
                    );
                }
                reason
            }
            _ if lowering_threshold(limits, saved) < limits.budget(saved) as u64 => format!(
                "lowered from {budget}: none of the last {} runs over {} was reopened in full",
                decision.over,
                human_bytes(lowering_threshold(limits, saved))
            ),
            _ => format!(
                "lowered from {budget}: none of the last {} clipped runs was reopened in full",
                decision.over
            ),
        };
        next
    });
    let reason = if !need_details {
        String::new()
    } else {
        match &change {
            Some(next) => next.reason.clone(),
            None => hold_reason(limits, saved, entry, &decision),
        }
    };
    Advice {
        bytes: limits.budget(level),
        saved_bytes: limits.budget(saved),
        change,
        reason,
        decision,
    }
}

fn times(count: u32) -> Cow<'static, str> {
    match count {
        1 => Cow::Borrowed("once"),
        2 => Cow::Borrowed("twice"),
        count => Cow::Owned(format!("{count} times")),
    }
}

fn reopened_text(clipped: usize, reopened: usize) -> String {
    format!(
        "{reopened} of the last {clipped} clipped runs {} reopened in full",
        if reopened == 1 { "was" } else { "were" }
    )
}

/// Why a family's budget holds: the last change, if any, and what the runs since
/// then show, distinguishing evidence still accruing from runs that cannot count.
fn hold_reason(limits: Limits, level: i32, entry: Option<&Entry>, decision: &Decision) -> String {
    let budget = limits.budget(level);
    let can_raise = limits.budget(level.saturating_add(1)) > budget;
    let can_lower = limits.budget(level.saturating_sub(1)) < budget;
    let threshold = lowering_threshold(limits, level);
    let mut text = String::with_capacity(96);
    match entry.filter(|entry| !entry.reason.is_empty()) {
        Some(entry) => {
            text.push_str(&entry.reason);
            text.push_str(" · since then ");
        }
        None => text.push_str("default · "),
    }
    let Decision {
        clipped,
        reopened,
        over,
        over_reopened,
        window,
        uncounted,
        ..
    } = *decision;
    if clipped > 0 {
        if clipped < MIN_SAMPLE {
            let _ = write!(
                text,
                "{reopened} of {clipped} clipped run{} reopened in full",
                if clipped == 1 { "" } else { "s" }
            );
            if can_raise {
                let _ = write!(text, ", {MIN_SAMPLE} needed");
            }
        } else {
            let _ = write!(
                text,
                "{reopened} of the last {clipped} clipped runs {} reopened in full",
                if reopened == 1 { "was" } else { "were" }
            );
        }
        if !can_raise && wants_raise(clipped, reopened) {
            text.push_str(" · at the cap");
        } else if !can_lower && clipped >= WINDOW && reopened == 0 {
            text.push_str(" · at the floor");
        } else if can_lower && over_reopened == 0 && window > WINDOW {
            let _ = write!(
                text,
                " · {over} of the {window} runs needed to lower, none reopened in full"
            );
        }
    } else if can_lower && over > 0 {
        let over_text = human_bytes(threshold);
        if over_reopened == 0 {
            let _ = write!(
                text,
                "no run was clipped · {over} of the {window} runs over {over_text} \
                 needed to lower, none reopened in full"
            );
        } else {
            let _ = write!(
                text,
                "no run was clipped · {over_reopened} of {over} runs over {over_text} \
                 reopened in full"
            );
        }
    } else if threshold < budget as u64 {
        let _ = write!(
            text,
            "no run was over {}, so none counts",
            human_bytes(threshold)
        );
    } else {
        text.push_str("no run was clipped");
    }
    if uncounted > 0 {
        let _ = write!(
            text,
            " · {uncounted} run{} reopened in full where the stream was summarized or \
             shown whole, which a larger budget would not change",
            if uncounted == 1 { " was" } else { "s were" }
        );
    }
    text
}

/// Forgets the rerun baselines of `key`'s commands. A rerun is compared with the
/// last run of its command at the same budget, so after a change the next run of
/// each command, in every session, starts fresh instead of being compared with a
/// run made before the change.
fn forget_baselines(workspace: &Workspace, key: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct RecordKey<'a> {
        #[serde(default, borrow)]
        family: Cow<'a, str>,
        #[serde(default, borrow)]
        label: Cow<'a, str>,
    }
    let baselines = workspace.state.join("baselines");
    if !baselines.is_dir() {
        return Ok(());
    }
    let _guard = lock_state(&workspace.state.join("capture.lock"))?;
    let mut record_path = workspace.state.join("runs");
    let mut id_buf = String::new();
    let mut record_buf = Vec::new();
    for entry in fs::read_dir(&baselines)? {
        let path = entry?.path();
        id_buf.clear();
        let Ok(mut file) = File::open(&path) else {
            continue;
        };
        if file.read_to_string(&mut id_buf).is_err() {
            continue;
        }
        let id = id_buf.trim();
        let valid = id.strip_prefix("run-").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric())
        });
        if !valid {
            continue;
        }
        record_path.push(id);
        record_path.push("record.json");
        let ok = read_into(&record_path, &mut record_buf);
        record_path.pop();
        record_path.pop();
        if !ok {
            continue;
        }
        let Ok(run) = serde_json::from_slice::<RecordKey<'_>>(&record_buf) else {
            continue;
        };
        if key_str(&run.family, &run.label) == Some(key)
            && let Err(error) = fs::remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error.into());
        }
    }
    Ok(())
}

struct Advised {
    state: PathBuf,
    args: Vec<OsString>,
    budget: usize,
}

thread_local! {
    static LAST_ADVISED: RefCell<Option<Advised>> = const { RefCell::new(None) };
}

/// Returns `true` when `preview_budget` just chose `budget` for this workspace
/// and command on this thread, consuming the handoff. Callers that passed an
/// explicit budget (such as `run --max-bytes`) bypass `preview_budget` and get
/// `false`.
pub(crate) fn take_advised(workspace: &Workspace, args: &[OsString], budget: usize) -> bool {
    LAST_ADVISED.with(|cell| {
        cell.borrow_mut().take().is_some_and(|advised| {
            advised.state == workspace.state && advised.args == args && advised.budget == budget
        })
    })
}

/// The preview budget for a `run` without `--max-bytes`. A settings error stops
/// the run as before. Any other failure falls back to the saved budget.
pub fn preview_budget(workspace: &Workspace, args: &[OsString]) -> Result<usize> {
    let budget = preview_budget_inner(workspace, args)?;
    LAST_ADVISED.with(|cell| {
        *cell.borrow_mut() = Some(Advised {
            state: workspace.state.clone(),
            args: args.to_vec(),
            budget,
        });
    });
    Ok(budget)
}

fn preview_budget_inner(workspace: &Workspace, args: &[OsString]) -> Result<usize> {
    let settings = workspace.settings()?;
    let limits = Limits::new(&settings);
    if !settings.adaptive_previews {
        return Ok(limits.default);
    }
    let Some(key) = command_key(args) else {
        return Ok(limits.default);
    };
    let runs = recorded_for(workspace, &key, true);
    let mut state = read_state(workspace);
    let mut path_buf = workspace.state.join("runs");
    let mut read_buf = Vec::new();
    let advice = advise_inner(
        limits,
        state.get(&key),
        &runs,
        |run| replayed_in(&mut path_buf, &mut read_buf, &run.id),
        now_ms(),
        false,
    );
    if let Some(entry) = advice.change {
        if forget_baselines(workspace, &key).is_err() {
            return Ok(advice.saved_bytes);
        }
        // Written without a lock: a concurrent change to another family can be
        // lost, and the next run of that family makes the same decision again.
        state.insert(key, entry);
        let _ = write_json(&workspace.state.join(STATE), &state);
    }
    Ok(advice.bytes)
}

#[derive(Serialize)]
struct Report {
    adaptive: bool,
    default_bytes: usize,
    floor_bytes: usize,
    cap_bytes: usize,
    families: Vec<Row>,
}

#[derive(Serialize)]
struct Row {
    family: String,
    budget_bytes: usize,
    adjustment: &'static str,
    reason: String,
    /// The clipped runs the next raise is judged on, and how many of them were
    /// replayed in full where they were clipped.
    clipped_runs: usize,
    reopened_runs: usize,
    /// The runs the next lowering is judged on, those over its threshold, and how
    /// many it needs.
    lowering_runs: usize,
    lowering_window: usize,
    /// Recent runs replayed in full only where a summary or a whole stream was
    /// shown, which a larger budget would not change.
    summaries_reopened: usize,
}

/// What `stats` reports: the budget each family's next run gets, without saving.
fn report(workspace: &Workspace) -> Result<Report> {
    let settings = workspace.settings()?;
    let limits = Limits::new(&settings);
    let mut families = Vec::new();
    if settings.adaptive_previews {
        let state = read_state(workspace);
        let mut runs = recorded_all(workspace, false);
        for key in state.keys() {
            if !runs.contains_key(key) {
                runs.insert(key.clone(), Vec::new());
            }
        }
        let mut path_buf = workspace.state.join("runs");
        let mut read_buf = Vec::new();
        let now = now_ms();
        for (key, runs) in runs {
            let advice = advise(
                limits,
                state.get(&key),
                &runs,
                |run| replayed_in(&mut path_buf, &mut read_buf, &run.id),
                now,
            );
            families.push(Row {
                family: key,
                budget_bytes: advice.bytes,
                adjustment: match advice.bytes.cmp(&limits.default) {
                    std::cmp::Ordering::Greater => "raised",
                    std::cmp::Ordering::Less => "lowered",
                    std::cmp::Ordering::Equal => "default",
                },
                reason: advice.reason,
                clipped_runs: advice.decision.clipped,
                reopened_runs: advice.decision.reopened,
                lowering_runs: advice.decision.over,
                lowering_window: advice.decision.window,
                summaries_reopened: advice.decision.uncounted,
            });
        }
    }
    Ok(Report {
        adaptive: settings.adaptive_previews,
        default_bytes: limits.default,
        floor_bytes: limits.floor,
        cap_bytes: limits.cap,
        families,
    })
}

/// The `preview_budgets` value of `stats --json`.
pub fn stats_json(workspace: &Workspace) -> serde_json::Value {
    report(workspace)
        .and_then(|report| Ok(serde_json::to_value(report)?))
        .unwrap_or_else(|error| serde_json::json!({ "error": format!("{error:#}") }))
}

/// The budget section of `stats`: families whose budget moved, whose previews
/// were clipped at it, or whose summarized runs were reopened in full. The
/// section is left out while there are none.
pub fn print_stats(workspace: &Workspace) {
    let report = match report(workspace) {
        Ok(report) => report,
        Err(error) => {
            println!("\nPreview budgets unavailable: {error:#}");
            return;
        }
    };
    if !report.adaptive {
        println!(
            "\nPreview budgets: adaptive_previews is off, so every run uses {} per stream",
            human_bytes(report.default_bytes as u64)
        );
        return;
    }
    let rows: Vec<_> = report
        .families
        .iter()
        .filter(|row| {
            row.adjustment != "default" || row.clipped_runs > 0 || row.summaries_reopened > 0
        })
        .collect();
    if rows.is_empty() {
        return;
    }
    println!(
        "\nPreview budgets per stream (default {}, adapting from {} to {})",
        human_bytes(report.default_bytes as u64),
        human_bytes(report.floor_bytes as u64),
        human_bytes(report.cap_bytes as u64)
    );
    let wide = rows
        .iter()
        .map(|row| row.family.chars().count())
        .max()
        .unwrap_or(0)
        .min(28);
    for row in rows {
        println!(
            "  {:<wide$}  {:>8} · {}",
            crate::summary::short(&row.family, 28),
            human_bytes(row.budget_bytes as u64),
            row.reason
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        default: 4096,
        floor: 2048,
        cap: 16384,
    };

    /// A plain run whose stdout log has `size` bytes, of which `shown` were shown.
    fn plain(index: usize, size: u64, shown: u64) -> Brief {
        Brief {
            id: format!("run-{index}"),
            completed_ms: 1_000_000 - index as u64,
            stdout: size,
            stderr: 0,
            shown,
            replaced: Replaced::Nothing,
            budget: None,
            explicit: false,
        }
    }

    /// `count` plain runs clipped at `budget`, which show about half of it.
    fn runs(count: usize, budget: usize) -> Vec<Brief> {
        (0..count)
            .map(|i| plain(i, 40_000, budget as u64 / 2 - 30))
            .collect()
    }

    /// The first `count` runs were replayed in full, both streams.
    fn first(count: usize) -> impl Fn(&Brief) -> [bool; 2] {
        move |run| {
            let index: usize = run.id["run-".len()..].parse().unwrap();
            [index < count; 2]
        }
    }

    fn step(level: i32, sample: &[Brief], reopened: usize) -> Step {
        decide(LIMITS, level, WINDOW, sample, first(reopened)).step
    }

    #[test]
    fn steps_double_and_halve_within_the_range() {
        let budgets: Vec<_> = (-2..=4).map(|level| LIMITS.budget(level)).collect();
        assert_eq!(budgets, [2048, 2048, 4096, 8192, 16384, 16384, 16384]);
        assert_eq!(LIMITS.normalize(4), 2);
        assert_eq!(LIMITS.normalize(-3), -1);
        let odd = Limits {
            default: 4096,
            floor: 3000,
            cap: 6000,
        };
        assert_eq!((odd.budget(1), odd.budget(-1)), (6000, 3000));
    }

    #[test]
    fn raises_when_a_quarter_of_clipped_runs_were_reopened() {
        let raise = decide(LIMITS, 0, WINDOW, &runs(9, 4096), first(4));
        assert_eq!(
            (raise.step, raise.clipped, raise.reopened),
            (Step::Raise, 9, 4)
        );
        assert_eq!(step(0, &runs(9, 4096), 2), Step::Hold);
        assert_eq!(step(0, &runs(8, 4096), 2), Step::Raise);
        // Four reopened runs are not a sample yet.
        assert_eq!(step(0, &runs(4, 4096), 4), Step::Hold);
        // At the cap the budget holds.
        assert_eq!(step(2, &runs(9, 16384), 9), Step::Hold);
    }

    #[test]
    fn lowers_only_after_a_full_window_without_reopens() {
        assert_eq!(step(0, &runs(19, 4096), 0), Step::Hold);
        assert_eq!(step(0, &runs(20, 4096), 0), Step::Lower);
        assert_eq!(step(0, &runs(30, 4096), 1), Step::Hold);
        assert_eq!(step(-1, &runs(20, 2048), 0), Step::Hold);
        // A raise that worked holds through 19 clean runs and is tested one step
        // down only after a full window.
        assert_eq!(step(1, &runs(19, 8192), 0), Step::Hold);
        assert_eq!(step(1, &runs(20, 8192), 0), Step::Lower);
    }

    #[test]
    fn replays_count_only_where_the_budget_limited_the_stream() {
        // A failing `cargo test`: stdout of 1,370 bytes summarized in about 400,
        // stderr of 16 KB of warnings clipped to about half of the budget.
        let sample: Vec<_> = (0..16)
            .map(|i| Brief {
                stdout: 1370,
                stderr: 16_000,
                shown: 400 + 2048,
                replaced: Replaced::Stdout,
                ..plain(i, 0, 0)
            })
            .collect();
        let stdout_only = |_: &Brief| [true, false];
        let decision = decide(LIMITS, 0, WINDOW, &sample, stdout_only);
        assert_eq!(
            (decision.step, decision.clipped, decision.reopened),
            (Step::Hold, 16, 0)
        );
        assert_eq!(decision.uncounted, 16);
        // Replaying the clipped stderr is evidence.
        let both = |_: &Brief| [true, true];
        assert_eq!(decide(LIMITS, 0, WINDOW, &sample, both).step, Step::Raise);
        // A grouped search that filled the budget was cut to fit, so replaying
        // its stdout is evidence; a summary well under the budget is not.
        let search = Brief {
            stdout: 60_000,
            shown: 3_300,
            replaced: Replaced::Stdout,
            ..plain(0, 0, 0)
        };
        assert_eq!(search.limited(4096, 4096), [true, false]);
        let summary = Brief {
            shown: 634,
            ..search.clone()
        };
        assert_eq!(summary.limited(4096, 4096), [false, false]);
        // A lint report replaces both streams.
        let lint = Brief {
            stdout: 30_000,
            stderr: 9_000,
            shown: 2_000,
            replaced: Replaced::Both,
            ..plain(0, 0, 0)
        };
        assert_eq!(lint.limited(4096, 4096), [true, true]);
    }

    #[test]
    fn runs_made_at_another_budget_are_left_out() {
        // No stream shows more than the budget.
        assert!(plain(0, 108_920, 2_040).made_at(4096));
        assert!(!plain(0, 108_920, 15_007).made_at(4096));
        assert!(plain(0, 3_000, 3_000).made_at(4096));
        assert!(!plain(0, 6_000, 6_000).made_at(4096));
        // A clipped stream shows about half of the budget, so a quarter or less
        // came from a much smaller --max-bytes.
        assert!(!plain(0, 108_920, 500).made_at(4096));
        // Twenty clean runs at 30,000 bytes leave the budget alone.
        let explicit: Vec<_> = (0..20).map(|i| plain(i, 108_920, 15_007)).collect();
        let decision = decide(LIMITS, 0, WINDOW, &explicit, first(0));
        assert_eq!((decision.step, decision.clipped), (Step::Hold, 0));
    }

    #[test]
    fn raised_budgets_wear_off_when_output_fits_one_step_down() {
        // At 16 KB a 9,919-byte output is shown whole and never reopened. Since
        // 8 KB would clip it, 20 such runs lower the budget one step.
        let fits: Vec<_> = (0..20).map(|i| plain(i, 9_919, 9_919)).collect();
        assert_eq!(step(2, &fits[..19], 0), Step::Hold);
        let decision = decide(LIMITS, 2, WINDOW, &fits, first(0));
        assert_eq!(
            (decision.step, decision.clipped, decision.over),
            (Step::Lower, 0, 20)
        );
        // A full replay of such a run blocks it.
        assert_eq!(step(2, &fits, 1), Step::Hold);
        let entry = Entry {
            level: 2,
            reason: "raised from 8.0 KB".into(),
            ..Entry::default()
        };
        let advice = advise(LIMITS, Some(&entry), &fits[..12], first(0), 9);
        assert_eq!(
            advice.reason,
            "raised from 8.0 KB · since then no run was clipped · 12 of the 20 runs \
             over 8.0 KB needed to lower, none reopened in full"
        );
        let small: Vec<_> = (0..30).map(|i| plain(i, 5_000, 5_000)).collect();
        let advice = advise(LIMITS, Some(&entry), &small, first(0), 9);
        assert_eq!(advice.bytes, 16384);
        assert_eq!(
            advice.reason,
            "raised from 8.0 KB · since then no run was over 8.0 KB, so none counts"
        );
        // At the default, output shown whole says nothing about a smaller budget.
        let whole: Vec<_> = (0..30).map(|i| plain(i, 3_000, 3_000)).collect();
        assert_eq!(step(0, &whole, 0), Step::Hold);
    }

    #[test]
    fn a_budget_raised_back_after_lowering_waits_longer_to_lower_again() {
        let raised = Entry::step(None, LIMITS, 1, 2, 10);
        assert_eq!((raised.lowered_from, raised.relapses), (None, 0));
        assert_eq!(Entry::window(Some(&raised), LIMITS, 2), 20);
        let lowered = Entry::step(Some(&raised), LIMITS, 2, 1, 20);
        assert_eq!((lowered.lowered_from, lowered.relapses), (Some(2), 0));
        let relapsed = Entry::step(Some(&lowered), LIMITS, 1, 2, 30);
        assert_eq!((relapsed.lowered_from, relapsed.relapses), (Some(2), 1));
        // Twenty clean clipped runs no longer lower it; forty do.
        let window = Entry::window(Some(&relapsed), LIMITS, 2);
        assert_eq!(window, 40);
        let clean = runs(40, 16384);
        let lower = |sample: &[Brief]| decide(LIMITS, 2, window, sample, first(0)).step;
        assert_eq!(lower(&clean[..20]), Step::Hold);
        assert_eq!(lower(&clean), Step::Lower);
        let advice = advise(LIMITS, Some(&relapsed), &clean[..25], first(0), 40);
        assert!(
            advice.reason.ends_with(
                "0 of the last 20 clipped runs were reopened in full · \
                 25 of the 40 runs needed to lower, none reopened in full"
            ),
            "{}",
            advice.reason
        );
        // Lowering from it again keeps the count, and the next return doubles it.
        let again = Entry::step(Some(&relapsed), LIMITS, 2, 1, 50);
        assert_eq!((again.lowered_from, again.relapses), (Some(2), 1));
        let twice = Entry::step(Some(&again), LIMITS, 1, 2, 60);
        assert_eq!(Entry::window(Some(&twice), LIMITS, 2), 80);
        let many = Entry {
            relapses: 9,
            ..twice.clone()
        };
        assert_eq!(Entry::window(Some(&many), LIMITS, 2), 160);
        // The raise that relapses says so.
        let entry = Entry {
            reason: "lowered".into(),
            ..again.clone()
        };
        let advice = advise(LIMITS, Some(&entry), &runs(5, 8192), first(5), 70);
        assert_eq!(advice.bytes, 16384);
        assert_eq!(
            advice.reason,
            "raised from 8.0 KB: 5 of the last 5 clipped runs were reopened in full · \
             raised back here twice, so lowering it again waits for 80 runs"
        );
        // Lowering further, or raising past it, starts over.
        let further = Entry::step(Some(&again), LIMITS, 1, 0, 80);
        assert_eq!((further.lowered_from, further.relapses), (Some(1), 0));
        let past = Entry::step(Some(&twice), LIMITS, 2, 3, 90);
        assert_eq!((past.lowered_from, past.relapses), (None, 0));
    }

    #[test]
    fn runners_keep_the_default_budget() {
        for (family, label) in [
            ("npm", "npm run"),
            ("other", "pnpm run"),
            ("cargo", "cargo run"),
            ("go", "go run"),
            ("other", "sh"),
            ("other", "bash"),
            ("other", "python3"),
            ("other", "node"),
            ("other", "make"),
            ("cargo", "cargo"),
            ("git", "git"),
            ("other", "kubectl"),
        ] {
            assert_eq!(key(family, label), None, "{label}");
        }
        for (family, label, expected) in [
            ("cargo test", "cargo test", "cargo test"),
            ("git", "git log", "git log"),
            ("other", "make test", "make test"),
            ("pytest", "python3", "pytest"),
            ("mypy", "mypy", "mypy"),
            ("kubectl get pods", "kubectl get", "kubectl get pods"),
            ("rg", "rg", "rg"),
            ("other", "gen.sh", "gen.sh"),
            // A linter whose subcommand happens to be `run`.
            (
                "golangci-lint run",
                "golangci-lint run",
                "golangci-lint run",
            ),
        ] {
            assert_eq!(key(family, label).as_deref(), Some(expected), "{label}");
        }
    }

    #[test]
    fn the_cache_reads_each_record_once_and_keeps_only_runs_a_budget_can_limit() {
        let temporary = tempfile::tempdir().unwrap();
        let runs = temporary.path();
        let record = |id: &str, size: usize, done: u64| {
            let directory = runs.join(id);
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("stdout"), "x".repeat(size)).unwrap();
            fs::write(directory.join("stderr"), "").unwrap();
            let record = serde_json::json!({
                "id": id, "status": 0, "raw_bytes": size, "preview_bytes": size.min(2000),
                "completed_ms": done, "family": "other", "label": "gen.sh",
            });
            fs::write(directory.join("record.json"), record.to_string()).unwrap();
        };
        record("run-small", 100, 1);
        record("run-large", 9_000, 2);
        fs::create_dir(runs.join("run-unfinished")).unwrap();
        let (index, changed) = refresh(runs, Index::default()).unwrap();
        assert!(changed);
        assert_eq!(index.seen.lines().count(), 2);
        assert_eq!(index.runs["gen.sh"].len(), 1);
        assert_eq!(index.runs["gen.sh"][0].id, "run-large");
        // Nothing new: no record is read again and nothing needs saving.
        let (index, changed) = refresh(runs, index).unwrap();
        assert!(!changed);
        record("run-later", 9_000, 3);
        let (index, changed) = refresh(runs, index).unwrap();
        assert!(changed);
        let ids: Vec<_> = index.runs["gen.sh"].iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["run-later", "run-large"]);
        // A deleted run makes the caller rebuild the cache.
        fs::remove_dir_all(runs.join("run-small")).unwrap();
        assert!(refresh(runs, index).is_none());
    }

    #[test]
    fn well_compacted_views_and_stderr_tests_count_accurately() {
        let temporary = tempfile::tempdir().unwrap();
        let runs = temporary.path();
        let write_run = |id: &str,
                         family: &str,
                         label: &str,
                         stdout_len: usize,
                         stderr_len: usize,
                         preview_bytes: usize,
                         replaced: Option<&str>,
                         budget_bytes: Option<usize>,
                         explicit_budget: bool,
                         summary: serde_json::Value| {
            let dir = runs.join(id);
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("stdout"), "o".repeat(stdout_len)).unwrap();
            fs::write(dir.join("stderr"), "e".repeat(stderr_len)).unwrap();
            let mut rec = serde_json::json!({
                "id": id,
                "status": 0,
                "raw_bytes": stdout_len + stderr_len,
                "preview_bytes": preview_bytes,
                "completed_ms": 1_000_000,
                "family": family,
                "label": label,
                "summary": summary,
            });
            if let Some(r) = replaced {
                rec["replaced"] = serde_json::json!(r);
            }
            if let Some(b) = budget_bytes {
                rec["budget_bytes"] = serde_json::json!(b);
            }
            if explicit_budget {
                rec["explicit_budget"] = serde_json::json!(true);
            }
            let bytes = rec.to_string().into_bytes();
            brief(id, &dir, &bytes).expect("expected brief")
        };

        // 1. Compact logview (replaced="both", small preview vs large stdout+stderr)
        let (_, log_compact) = write_run(
            "run-log1",
            "docker logs",
            "docker logs",
            40_000,
            8_000,
            550,
            Some("both"),
            Some(4096),
            false,
            serde_json::json!({}),
        );
        assert_eq!(log_compact.replaced, Replaced::Both);
        assert!(log_compact.made_at(4096));
        assert!(!log_compact.made_at(8192));
        assert_eq!(log_compact.limited(4096, 4096), [false, false]);

        // 2. Budget-filling logview counts as clipped and raises when reopened
        let (_, log_filled) = write_run(
            "run-log2",
            "docker logs",
            "docker logs",
            40_000,
            8_000,
            2_800,
            Some("both"),
            Some(4096),
            false,
            serde_json::json!({}),
        );
        assert!(log_filled.made_at(4096));
        assert_eq!(log_filled.limited(4096, 4096), [true, true]);
        let log_sample: Vec<_> = (0..6)
            .map(|i| Brief {
                id: format!("run-{i}"),
                completed_ms: 1_000_000 - i as u64,
                ..log_filled.clone()
            })
            .collect();
        assert_eq!(
            decide(LIMITS, 0, WINDOW, &log_sample, first(2)).step,
            Step::Raise
        );

        // 3. Well-compacted gitview, extra_inventory_view, json_view (replaced="stdout")
        for (id, family, label, shown) in [
            ("run-git", "git", "git log", 700),
            ("run-inv", "other", "kubectl get", 600),
            ("run-json", "other", "gh api", 800),
        ] {
            let (_, b) = write_run(
                id,
                family,
                label,
                35_000,
                0,
                shown,
                Some("stdout"),
                Some(4096),
                false,
                serde_json::json!({}),
            );
            assert_eq!(b.replaced, Replaced::Stdout, "{id}");
            assert!(b.made_at(4096), "{id} should not be dropped");
            assert_eq!(b.limited(4096, 4096), [false, false], "{id}");
        }

        // 4. Clean build / lint line (replaced="both", tiny preview vs large stderr)
        let (_, clean_build) = write_run(
            "run-clean",
            "cargo clippy",
            "cargo clippy",
            0,
            18_000,
            72,
            Some("both"),
            Some(4096),
            false,
            serde_json::json!({}),
        );
        assert_eq!(clean_build.replaced, Replaced::Both);
        assert!(clean_build.made_at(4096));
        assert_eq!(clean_build.limited(4096, 4096), [false, false]);

        // 5. Stderr-heavy jest/vitest/bun test run where summary covers stderr (replaced="both")
        let (_, jest_compact) = write_run(
            "run-jest1",
            "jest",
            "jest",
            0,
            30_000,
            480,
            Some("both"),
            Some(4096),
            false,
            serde_json::json!({"kind": "tests", "outcomes": ["10 passed"]}),
        );
        assert_eq!(jest_compact.replaced, Replaced::Both);
        assert!(jest_compact.made_at(4096));
        assert_eq!(jest_compact.limited(4096, 4096), [false, false]);
        // Reopening compact jest summaries counts as uncounted, NOT clipped.
        let jest_sample: Vec<_> = (0..6)
            .map(|i| Brief {
                id: format!("run-{i}"),
                completed_ms: 1_000_000 - i as u64,
                ..jest_compact.clone()
            })
            .collect();
        let d = decide(LIMITS, 0, WINDOW, &jest_sample, |_: &Brief| [false, true]);
        assert_eq!((d.step, d.clipped, d.uncounted), (Step::Hold, 0, 6));

        // Budget-filling stderr-heavy jest summary counts stderr as limited.
        let (_, jest_filled) = write_run(
            "run-jest2",
            "jest",
            "jest",
            0,
            30_000,
            2_400,
            Some("both"),
            Some(4096),
            false,
            serde_json::json!({"kind": "tests", "outcomes": ["10 failed"]}),
        );
        assert!(jest_filled.made_at(4096));
        assert_eq!(jest_filled.limited(4096, 4096), [false, true]);

        // 6. Newer record with budget_bytes and NO replaced keeps Replaced::Nothing
        //    even if summary is populated (e.g., plain preview was smaller than summary).
        let (_, unreplaced) = write_run(
            "run-plain-new",
            "cargo test",
            "cargo test",
            2_000,
            0,
            2_000,
            None,
            Some(4096),
            false,
            serde_json::json!({"kind": "tests", "outcomes": ["3 passed"]}),
        );
        assert_eq!(unreplaced.replaced, Replaced::Nothing);

        // 7. Legacy record without budget_bytes or replaced still uses summary heuristic.
        let (_, legacy) = write_run(
            "run-legacy",
            "cargo test",
            "cargo test",
            20_000,
            0,
            500,
            None,
            None,
            false,
            serde_json::json!({"kind": "tests", "outcomes": ["3 passed"]}),
        );
        assert_eq!(legacy.replaced, Replaced::Stdout);
        assert!(legacy.made_at(4096));
    }

    #[test]
    fn explicit_budget_runs_near_adaptive_budget_are_excluded() {
        // Explicit --max-bytes runs anywhere around [0.5x, 2x] of 4096 (including
        // an exact match at 4096!) must never count toward raises or lowerings.
        for explicit_bytes in [2048u64, 3000, 3500, 4096, 5500, 8192] {
            let run = Brief {
                id: "run-0".into(),
                completed_ms: 1_000_000,
                stdout: 40_000,
                stderr: 0,
                shown: explicit_bytes / 2,
                replaced: Replaced::Nothing,
                budget: Some(explicit_bytes),
                explicit: true,
            };
            assert!(
                !run.made_at(4096),
                "explicit --max-bytes {explicit_bytes} must not count at 4096"
            );
            let sample: Vec<_> = (0..20)
                .map(|i| Brief {
                    id: format!("run-{i}"),
                    completed_ms: 1_000_000 - i as u64,
                    ..run.clone()
                })
                .collect();
            let clean = decide(LIMITS, 0, WINDOW, &sample, first(0));
            assert_eq!((clean.step, clean.clipped, clean.over), (Step::Hold, 0, 0));
            let reopened = decide(LIMITS, 0, WINDOW, &sample, first(20));
            assert_eq!(
                (reopened.step, reopened.clipped, reopened.reopened),
                (Step::Hold, 0, 0)
            );
        }
        assert_eq!(INDEX_VERSION, 3);
    }

    #[test]
    fn corrupt_or_empty_state_and_seen_lines_are_skipped_without_panicking() {
        // 1. Empty, whitespace, invalid JSON, and partially corrupt `budgets.json`
        //    skip bad/empty entries cleanly while keeping valid families.
        assert!(parse_state(b"").is_empty());
        assert!(parse_state(b"   \n\t  ").is_empty());
        assert!(parse_state(b"{corrupt").is_empty());
        assert!(parse_state(b"[1, 2, 3]").is_empty());

        let mixed_state = br#"{
            "": {"level": 2, "since_ms": 100},
            "   ": {"level": 1, "since_ms": 200},
            "broken": "not-an-entry",
            "null-entry": null,
            "cargo test": {"level": 1, "since_ms": 500, "reason": "raised"}
        }"#;
        let parsed = parse_state(mixed_state);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed["cargo test"].level, 1);
        assert_eq!(parsed["cargo test"].since_ms, 500);

        // 2. Corrupt, empty, duplicate, and unterminated lines in `Index::seen`
        //    are skipped and normalized without breaking deleted-run detection.
        let temporary = tempfile::tempdir().unwrap();
        let runs = temporary.path();
        let dir = runs.join("run-ok");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("stdout"), "x".repeat(8_000)).unwrap();
        fs::write(dir.join("stderr"), "").unwrap();
        fs::write(
            dir.join("record.json"),
            serde_json::json!({
                "id": "run-ok", "status": 0, "raw_bytes": 8_000, "preview_bytes": 2_000,
                "completed_ms": 10, "family": "other", "label": "gen.sh"
            })
            .to_string(),
        )
        .unwrap();

        let dirty = Index {
            version: INDEX_VERSION,
            seen: "\n  \ncorrupt-line\nrun-\n  run-ok  \nrun-ok".into(),
            runs: BTreeMap::from([("".into(), Vec::new())]),
        };
        let (cleaned, changed) = refresh(runs, dirty).expect("valid seen run still exists");
        assert!(changed);
        assert_eq!(cleaned.seen, "run-ok\n");
        assert!(!cleaned.runs.contains_key(""));

        // 3. Extreme byte counts and levels do not overflow or panic.
        let extreme = Brief {
            id: "run-0".into(),
            completed_ms: u64::MAX,
            stdout: u64::MAX,
            stderr: u64::MAX,
            shown: u64::MAX,
            replaced: Replaced::Both,
            budget: None,
            explicit: false,
        };
        assert_eq!(extreme.limited(4096, 4096), [true, true]);
        assert!(!extreme.made_at(4096));
        assert_eq!(
            decide(
                LIMITS,
                i32::MAX,
                WINDOW,
                std::slice::from_ref(&extreme),
                first(1)
            )
            .step,
            Step::Hold
        );
        assert_eq!(
            decide(LIMITS, i32::MIN, WINDOW, &[extreme], first(0)).step,
            Step::Hold
        );
    }
}
