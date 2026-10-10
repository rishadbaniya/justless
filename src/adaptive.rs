//! Adaptive preview budgets. When `run` has no `--max-bytes`, each command
//! family's budget follows how often agents reopened its clipped logs in full.
//!
//! The advisor is deterministic and local. It reads the project's run records,
//! their recall counts and `.justless/budgets.json`, which holds each family's
//! current step and when it last changed. Only runs completed since that change
//! count, so every step is judged by the runs it produced and the budget moves
//! one step at a time. `.justless/budget-runs.json` caches the few fields it
//! needs from each finished run, so a `run` reads only records it has not seen.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::history::{human_bytes, now_ms};
use crate::workspace::{Settings, Workspace, write_json};

/// Clipped runs considered at the current budget, newest first.
const WINDOW: usize = 20;
/// Clipped runs needed at the current budget before it can be raised. Lowering
/// needs a full window: a smaller preview can hide evidence, while a larger one
/// costs only bytes.
const MIN_SAMPLE: usize = 5;
const STATE: &str = "budgets.json";

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

/// One recorded run, newest first, as evidence for its family's budget.
#[derive(Clone, Copy, Debug)]
struct Evidence {
    raw: u64,
    shown: u64,
    unchanged: bool,
    reopened: bool,
}

/// A run is clipped when its preview was smaller than its output and used at
/// least a third of the budget, so the budget rather than a compact summary
/// limited what the agent saw. A clipped plain stream shows its first and last
/// lines in about half of the budget; a test summary that fits is far smaller,
/// and a larger budget would not change it.
fn clipped(raw: u64, shown: u64, budget: usize) -> bool {
    raw > shown && shown.saturating_mul(3) >= budget as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Hold,
    Raise,
    Lower,
}

#[derive(Debug, PartialEq, Eq)]
struct Decision {
    step: Step,
    /// Clipped runs considered, at most `WINDOW`.
    clipped: usize,
    /// How many of them were reopened in full.
    reopened: usize,
}

/// Decides the next step from the runs made at the current level, newest first.
/// Raise when at least a quarter of five or more clipped runs were reopened in
/// full. Lower after a full window of clipped runs that nobody reopened in full.
/// Targeted lookups count toward neither: they mean the preview's line markers
/// led to what was needed.
fn decide(limits: Limits, level: i32, runs: &[Evidence]) -> Decision {
    let budget = limits.budget(level);
    let sample: Vec<_> = runs
        .iter()
        .filter(|run| !run.unchanged && clipped(run.raw, run.shown, budget))
        .take(WINDOW)
        .collect();
    let clipped = sample.len();
    let reopened = sample.iter().filter(|run| run.reopened).count();
    let step = if wants_raise(clipped, reopened) && limits.budget(level + 1) > budget {
        Step::Raise
    } else if wants_lower(clipped, reopened) && limits.budget(level - 1) < budget {
        Step::Lower
    } else {
        Step::Hold
    };
    Decision {
        step,
        clipped,
        reopened,
    }
}

fn wants_raise(clipped: usize, reopened: usize) -> bool {
    clipped >= MIN_SAMPLE && reopened * 4 >= clipped
}

fn wants_lower(clipped: usize, reopened: usize) -> bool {
    clipped >= WINDOW && reopened == 0
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

/// The budget key for a run: its family, or its command label when that names
/// the command more precisely, as `git log` does within `git` and `make test`
/// within `other`. Runs that only the catch-all `other` or `legacy` describe
/// mix unrelated commands, so they keep the default.
fn key(family: &str, label: &str) -> Option<String> {
    let specific = family == "other"
        || label
            .strip_prefix(family)
            .is_some_and(|rest| rest.starts_with(' '));
    let key = if specific { label } else { family };
    (!matches!(key, "" | "other" | "legacy")).then(|| key.to_owned())
}

fn command_key(args: &[OsString]) -> Option<String> {
    let words: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
    key(&family(args), &crate::history::label(&words))
}

/// A family's saved step.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Entry {
    level: i32,
    /// When the step last changed. Runs completed before it ran at another budget.
    since_ms: u64,
    /// Why it changed, as `stats` shows it.
    #[serde(default)]
    reason: String,
}

type State = BTreeMap<String, Entry>;

fn read_state(workspace: &Workspace) -> State {
    fs::read(workspace.state.join(STATE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The fields of a run record the advisor needs.
struct Recorded {
    directory: PathBuf,
    completed_ms: u64,
    raw_bytes: u64,
    preview_bytes: u64,
    unchanged: bool,
}

/// What the advisor keeps of a finished run: its budget key, completion time,
/// output and preview sizes, and whether it was reported unchanged. `None` marks
/// a run outside every budget or with a damaged record.
type Brief = Option<(String, u64, u64, u64, bool)>;

/// Briefs of finished runs by run ID, cached in `.justless/budget-runs.json`.
/// Records never change once written, so each one is read once, and a `run`
/// lists the directory instead of reading every record. A cache written by
/// another version is rebuilt.
#[derive(Default, Serialize, Deserialize)]
struct Index {
    version: u32,
    runs: BTreeMap<String, Brief>,
}

const INDEX: &str = "budget-runs.json";
const INDEX_VERSION: u32 = 1;

fn brief(record: &[u8]) -> Brief {
    #[derive(Deserialize)]
    struct Fields {
        #[serde(default)]
        family: String,
        #[serde(default)]
        label: String,
        #[serde(default)]
        raw_bytes: u64,
        #[serde(default)]
        preview_bytes: u64,
        #[serde(default)]
        completed_ms: u64,
        #[serde(default)]
        unchanged: bool,
    }
    let run: Fields = serde_json::from_slice(record).ok()?;
    Some((
        key(&run.family, &run.label)?,
        run.completed_ms,
        run.raw_bytes,
        run.preview_bytes,
        run.unchanged,
    ))
}

/// Recorded runs by budget key, newest first. An unfinished run is skipped until
/// its record exists, and a damaged record silently: this runs before every
/// command, and a warning there would reach the agent with the command's output.
/// `save` refreshes the cached index.
fn recorded(
    workspace: &Workspace,
    only: Option<&str>,
    save: bool,
) -> BTreeMap<String, Vec<Recorded>> {
    let mut runs: BTreeMap<String, Vec<Recorded>> = BTreeMap::new();
    let directory = workspace.state.join("runs");
    let Ok(entries) = fs::read_dir(&directory) else {
        return runs;
    };
    let path = workspace.state.join(INDEX);
    let mut cached = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Index>(&bytes).ok())
        .filter(|index| index.version == INDEX_VERSION)
        .unwrap_or_default()
        .runs;
    let mut index = Index {
        version: INDEX_VERSION,
        runs: BTreeMap::new(),
    };
    let mut read = false;
    for entry in entries.flatten() {
        let Ok(id) = entry.file_name().into_string() else {
            continue;
        };
        if !id.starts_with("run-") {
            continue;
        }
        let brief = match cached.remove(&id) {
            Some(brief) => brief,
            None => match fs::read(entry.path().join("record.json")) {
                Ok(record) => {
                    read = true;
                    brief(&record)
                }
                Err(_) => continue,
            },
        };
        index.runs.insert(id, brief);
    }
    // Cached runs left over were deleted.
    if save && (read || !cached.is_empty()) {
        let _ = write_json(&path, &index);
    }
    for (id, brief) in index.runs {
        let Some((key, completed_ms, raw_bytes, preview_bytes, unchanged)) = brief else {
            continue;
        };
        if only.is_some_and(|only| only != key) {
            continue;
        }
        runs.entry(key).or_default().push(Recorded {
            directory: directory.join(id),
            completed_ms,
            raw_bytes,
            preview_bytes,
            unchanged,
        });
    }
    for list in runs.values_mut() {
        list.sort_by(|a, b| b.completed_ms.cmp(&a.completed_ms));
    }
    runs
}

/// Whether the agent replayed a whole stream of the run, as `stats` counts it.
fn reopened(directory: &Path) -> bool {
    #[derive(Deserialize)]
    struct Recalls {
        #[serde(default)]
        full: u64,
    }
    fs::read(directory.join("recalls.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Recalls>(&bytes).ok())
        .is_some_and(|recalls| recalls.full > 0)
}

/// The clipped runs made since `since_ms` at `budget`, newest first. Recall
/// counts are read only for them.
fn evidence(runs: &[Recorded], since_ms: u64, budget: usize) -> Vec<Evidence> {
    runs.iter()
        .take_while(|run| run.completed_ms > since_ms)
        .filter(|run| !run.unchanged && clipped(run.raw_bytes, run.preview_bytes, budget))
        .take(WINDOW)
        .map(|run| Evidence {
            raw: run.raw_bytes,
            shown: run.preview_bytes,
            unchanged: run.unchanged,
            reopened: reopened(&run.directory),
        })
        .collect()
}

/// The budget the next run of a family gets, and why.
#[derive(Debug)]
struct Advice {
    level: i32,
    bytes: usize,
    /// The level differs from the saved one; the next run saves it.
    changed: bool,
    reason: String,
    clipped: usize,
    reopened: usize,
}

fn advise(limits: Limits, entry: Option<&Entry>, runs: &[Recorded]) -> Advice {
    let saved = entry.map_or(0, |entry| limits.normalize(entry.level));
    let since = entry.map_or(0, |entry| entry.since_ms);
    let budget = limits.budget(saved);
    let decision = decide(limits, saved, &evidence(runs, since, budget));
    let (clipped, reopened) = (decision.clipped, decision.reopened);
    let (level, reason) = match decision.step {
        Step::Raise => (
            saved + 1,
            format!(
                "raised from {}: {}",
                human_bytes(budget as u64),
                reopened_text(clipped, reopened)
            ),
        ),
        Step::Lower => (
            saved - 1,
            format!(
                "lowered from {}: none of the last {clipped} clipped runs was reopened in full",
                human_bytes(budget as u64)
            ),
        ),
        Step::Hold => {
            let needed = if clipped < MIN_SAMPLE {
                format!(", {MIN_SAMPLE} needed")
            } else {
                String::new()
            };
            let limit = if wants_raise(clipped, reopened) {
                " · at the cap"
            } else if wants_lower(clipped, reopened) {
                " · at the floor"
            } else {
                ""
            };
            let reason = match entry {
                // After a change, the runs made since show how the step is doing.
                Some(entry) if !entry.reason.is_empty() => format!(
                    "{} · since then {reopened} of {clipped} reopened{needed}{limit}",
                    entry.reason
                ),
                _ if clipped < MIN_SAMPLE => format!(
                    "default · {reopened} of {clipped} clipped run{} reopened in full{needed}",
                    if clipped == 1 { "" } else { "s" }
                ),
                _ => format!("default · {}{limit}", reopened_text(clipped, reopened)),
            };
            (saved, reason)
        }
    };
    Advice {
        level,
        bytes: limits.budget(level),
        changed: level != saved,
        reason,
        clipped,
        reopened,
    }
}

fn reopened_text(clipped: usize, reopened: usize) -> String {
    format!(
        "{reopened} of the last {clipped} clipped runs {} reopened in full",
        if reopened == 1 { "was" } else { "were" }
    )
}

/// The preview budget for a `run` without `--max-bytes`. A settings error stops
/// the run as before. Any other failure falls back to the configured default.
pub fn preview_budget(workspace: &Workspace, args: &[OsString]) -> Result<usize> {
    let settings = workspace.settings()?;
    let limits = Limits::new(&settings);
    if !settings.adaptive_previews {
        return Ok(limits.default);
    }
    let Some(key) = command_key(args) else {
        return Ok(limits.default);
    };
    let runs = recorded(workspace, Some(&key), true)
        .remove(&key)
        .unwrap_or_default();
    let mut state = read_state(workspace);
    let advice = advise(limits, state.get(&key), &runs);
    if advice.changed {
        // Written without a lock: a concurrent change to another family can be
        // lost, and the next run of that family makes the same decision again.
        state.insert(
            key,
            Entry {
                level: advice.level,
                since_ms: now_ms(),
                reason: advice.reason,
            },
        );
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
    /// The clipped runs the reason is based on, and how many were reopened in full.
    clipped_runs: usize,
    reopened_runs: usize,
    /// With no clipped runs: shortened runs reopened in full although their
    /// preview was a summary well under the budget, so the budget holds.
    summaries_reopened: usize,
}

/// Among the newest shortened runs since `since_ms` that were not clipped at
/// `budget`, how many there were and how many were reopened in full. Their
/// previews were summaries, such as a failing `cargo test` reported in a few
/// hundred bytes, so a larger budget would show the same.
fn summaries(runs: &[Recorded], since_ms: u64, budget: usize) -> (usize, usize) {
    let sample: Vec<_> = runs
        .iter()
        .take_while(|run| run.completed_ms > since_ms)
        .filter(|run| {
            !run.unchanged
                && run.raw_bytes > run.preview_bytes
                && !clipped(run.raw_bytes, run.preview_bytes, budget)
        })
        .take(WINDOW)
        .collect();
    let replayed = sample.iter().filter(|run| reopened(&run.directory)).count();
    (sample.len(), replayed)
}

/// What `stats` reports: the budget each family's next run gets, without saving.
fn report(workspace: &Workspace) -> Result<Report> {
    let settings = workspace.settings()?;
    let limits = Limits::new(&settings);
    let mut families = Vec::new();
    if settings.adaptive_previews {
        let state = read_state(workspace);
        let mut runs = recorded(workspace, None, false);
        for key in state.keys() {
            runs.entry(key.clone()).or_default();
        }
        for (key, runs) in runs {
            let entry = state.get(&key);
            let advice = advise(limits, entry, &runs);
            let mut reason = advice.reason;
            let mut summaries_reopened = 0;
            if advice.clipped == 0 && !advice.changed {
                let since = entry.map_or(0, |entry| entry.since_ms);
                let (shortened, reopened) = summaries(&runs, since, advice.bytes);
                if reopened > 0 {
                    summaries_reopened = reopened;
                    let base = match entry {
                        Some(entry) if !entry.reason.is_empty() => {
                            format!("{} · since then", entry.reason)
                        }
                        _ => "default ·".into(),
                    };
                    reason = format!(
                        "{base} {reopened} of the last {shortened} shortened runs {} reopened in full, \
                         but their previews were summaries under a third of the budget, \
                         which a larger budget would not change",
                        if reopened == 1 { "was" } else { "were" }
                    );
                }
            }
            families.push(Row {
                family: key,
                budget_bytes: advice.bytes,
                adjustment: match advice.bytes.cmp(&limits.default) {
                    std::cmp::Ordering::Greater => "raised",
                    std::cmp::Ordering::Less => "lowered",
                    std::cmp::Ordering::Equal => "default",
                },
                reason,
                clipped_runs: advice.clipped,
                reopened_runs: advice.reopened,
                summaries_reopened,
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

    /// `count` plain runs clipped at `budget`, which show about half of it, the
    /// first `reopened` of them reopened in full.
    fn runs(count: usize, reopened: usize, budget: usize) -> Vec<Evidence> {
        (0..count)
            .map(|i| Evidence {
                raw: 40_000,
                shown: budget as u64 / 2 - 30,
                unchanged: false,
                reopened: i < reopened,
            })
            .collect()
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
        let raise = decide(LIMITS, 0, &runs(9, 4, 4096));
        assert_eq!(
            raise,
            Decision {
                step: Step::Raise,
                clipped: 9,
                reopened: 4
            }
        );
        assert_eq!(decide(LIMITS, 0, &runs(9, 2, 4096)).step, Step::Hold);
        assert_eq!(decide(LIMITS, 0, &runs(8, 2, 4096)).step, Step::Raise);
        // Four reopened runs are not a sample yet.
        assert_eq!(decide(LIMITS, 0, &runs(4, 4, 4096)).step, Step::Hold);
        // At the cap the budget holds.
        assert_eq!(decide(LIMITS, 2, &runs(9, 9, 16384)).step, Step::Hold);
    }

    #[test]
    fn lowers_only_after_a_full_window_without_reopens() {
        assert_eq!(decide(LIMITS, 0, &runs(19, 0, 4096)).step, Step::Hold);
        assert_eq!(decide(LIMITS, 0, &runs(20, 0, 4096)).step, Step::Lower);
        assert_eq!(decide(LIMITS, 0, &runs(30, 1, 4096)).step, Step::Hold);
        assert_eq!(decide(LIMITS, -1, &runs(20, 0, 2048)).step, Step::Hold);
    }

    #[test]
    fn previews_that_fit_or_stayed_small_are_not_evidence() {
        let mut sample = runs(8, 0, 4096);
        for run in &mut sample {
            // Shown whole, or summarized well under the budget.
            run.reopened = true;
            run.shown = 600;
        }
        sample[0].raw = 600;
        sample.extend(runs(2, 2, 4096).into_iter().map(|run| Evidence {
            unchanged: true,
            ..run
        }));
        let decision = decide(LIMITS, 0, &sample);
        assert_eq!((decision.step, decision.clipped), (Step::Hold, 0));
    }

    #[test]
    fn a_raise_that_worked_holds_until_a_full_clean_window() {
        // At 8 KB nobody reopens the larger previews: the raise holds through 19
        // clean runs and is tested one step down only after a full window.
        assert_eq!(decide(LIMITS, 1, &runs(19, 0, 8192)).step, Step::Hold);
        assert_eq!(decide(LIMITS, 1, &runs(20, 0, 8192)).step, Step::Lower);
    }
}
