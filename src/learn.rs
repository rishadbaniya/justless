//! `justless learn`: turn command mistakes that agents corrected or keep repeating into
//! short project rules.
//!
//! Evidence comes from Claude Code transcripts, Codex rollouts and the run archive, all
//! local. Only
//! a closed table of signatures counts, every rule is checked against the current
//! tree, `PATH` or `$SHELL`, and rules are built from validated slots, never copied
//! from commands. Without `--apply` nothing is written.
mod block;
pub(crate) mod events;
mod patterns;
mod privacy;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use clap::{Arg, ArgMatches, Command};
use serde_json::{Value, json};

use crate::history::{human_bytes, now_ms, utc_date};
use crate::workspace::{Workspace, lock_state};
use events::Pointer;
use patterns::{Candidate, Check, Env, Scope};

const DAY_MS: u64 = 86_400_000;

/// Whether the run archive should leave out a command line because it may carry a
/// credential or an address, or runs a network tool. `learn` drops such commands
/// anyway, so withholding them loses no evidence.
pub(crate) fn withhold_command(argv: &[String]) -> bool {
    let raw = shlex::try_join(argv.iter().map(String::as_str)).unwrap_or_else(|_| argv.join(" "));
    privacy::carries_credential(&raw, &[])
}

pub fn command() -> Command<'static> {
    Command::new("learn")
        .about("Propose project rules from command mistakes agents corrected or keep repeating")
        .arg(
            Arg::new("days")
                .long("days")
                .value_name("N")
                .default_value("30")
                .help("Days of runs and transcripts to read"),
        )
        .arg(
            Arg::new("min-sessions")
                .long("min-sessions")
                .value_name("N")
                .default_value("2")
                .help("Sessions a pattern needs before it is proposed"),
        )
        .arg(
            Arg::new("limit")
                .long("limit")
                .value_name("N")
                .default_value("8")
                .help("Maximum rules to show and keep"),
        )
        .arg(Arg::new("json").long("json").help("Print a JSON report"))
        .arg(
            Arg::new("apply")
                .long("apply")
                .help("Write the rules into the project instructions"),
        )
        .arg(
            Arg::new("target")
                .long("target")
                .value_name("NAME")
                .takes_value(true)
                .multiple_occurrences(true)
                .possible_values(["agents", "claude", "gemini"])
                .help("Instructions file to update (repeatable; default: those that exist)"),
        )
        .arg(
            Arg::new("include-machine")
                .long("include-machine")
                .help("Also write rules about this machine into files Git shares"),
        )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reason {
    BelowMinSessions,
    Contradicted,
    Stale,
    UnsafeText,
    BlockFull,
    MachineScope,
}

impl Reason {
    /// Reasons counted in `held_counts`; contradicted and stale rules are counted in
    /// `skipped`.
    const COUNTED: [Self; 4] = [
        Self::BelowMinSessions,
        Self::UnsafeText,
        Self::BlockFull,
        Self::MachineScope,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::BelowMinSessions => "below_min_sessions",
            Self::Contradicted => "contradicted",
            Self::Stale => "stale",
            Self::UnsafeText => "unsafe_text",
            Self::BlockFull => "block_full",
            Self::MachineScope => "machine_scope",
        }
    }
}

struct Rule<'a> {
    candidate: &'a Candidate,
    check: Check,
    text: Option<String>,
    scope: Scope,
    score: f64,
    held: Option<Reason>,
}

fn number(matches: &ArgMatches, name: &str, minimum: u64, maximum: u64) -> Result<u64> {
    let value = matches.value_of_t::<u64>(name)?;
    ensure!(
        (minimum..=maximum).contains(&value),
        if maximum == u64::MAX {
            format!("--{name} expects a number of at least {minimum}")
        } else {
            format!("--{name} expects a number from {minimum} to {maximum}")
        }
    );
    Ok(value)
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<()> {
    let days = number(matches, "days", 1, u64::MAX)?;
    let min_sessions = number(matches, "min-sessions", 1, u64::MAX)?;
    let limit = number(matches, "limit", 1, 32)? as usize;
    let as_json = matches.is_present("json");
    let apply = matches.is_present("apply");
    let include_machine = matches.is_present("include-machine");
    let explicit: Vec<String> = matches
        .values_of("target")
        .map(|values| values.map(str::to_owned).collect())
        .unwrap_or_default();

    let now = now_ms();
    let since = now.saturating_sub(days.saturating_mul(DAY_MS));
    let mut transcript_stats = events::TranscriptStats::default();
    let mut transcripts = events::transcripts(workspace, since, &mut transcript_stats);
    let mut codex_stats = events::TranscriptStats::default();
    events::rollouts(workspace, since, &mut codex_stats, &mut transcripts);
    let mut agy_stats = events::TranscriptStats::default();
    events::agy_sessions(workspace, since, &mut agy_stats, &mut transcripts);
    let sessions: BTreeSet<String> = transcript_stats
        .sessions
        .iter()
        .chain(&codex_stats.sessions)
        .chain(&agy_stats.sessions)
        .cloned()
        .collect();
    let mut run_stats = events::RunStats::default();
    let runs = events::runs(
        workspace,
        since,
        &sessions,
        &transcripts.linked,
        &mut run_stats,
    );
    let mut all = transcripts.events;
    all.extend(runs);
    let analysis = patterns::analyze(&workspace.root, &all);
    let env = Env::current();

    let mut rules: Vec<Rule> = analysis
        .candidates
        .iter()
        .map(|candidate| {
            let check = patterns::check(&workspace.root, &env, candidate.class, &candidate.slots);
            let text = patterns::render(candidate.class, &candidate.slots, &check);
            let qualified = candidate.sessions.len() as u64 >= min_sessions
                || patterns::single_correction(candidate) && matches!(check, Check::Verified(_));
            let held = if candidate.contradicted {
                Some(Reason::Contradicted)
            } else if matches!(check, Check::Stale(_)) {
                Some(Reason::Stale)
            } else if text.is_none() {
                Some(Reason::UnsafeText)
            } else if !qualified {
                Some(Reason::BelowMinSessions)
            } else {
                None
            };
            Rule {
                candidate,
                scope: patterns::scope(candidate.class, &candidate.slots),
                score: patterns::score(candidate, now / DAY_MS),
                check,
                text,
                held,
            }
        })
        .collect();
    rules.sort_by(|a, b| {
        let (x, y) = (a.candidate, b.candidate);
        b.score
            .total_cmp(&a.score)
            .then(y.threads.len().cmp(&x.threads.len()))
            .then(y.failures.cmp(&x.failures))
            .then((y.last_ms / DAY_MS).cmp(&(x.last_ms / DAY_MS)))
            .then(x.id.cmp(&y.id))
    });

    // Learned state is read without creating .justless; only --apply writes it. A
    // preview notes a state file it cannot read and carries on without it.
    let mut notes = Vec::new();
    let mut state = match block::load(workspace) {
        Ok(state) => state,
        Err(error) if !apply => {
            notes.push(format!("{error:#}; `--apply` stops on it."));
            block::State {
                version: 1,
                ..block::State::default()
            }
        }
        Err(error) => return Err(error),
    };
    // Written rules keep the text they were written with.
    for rule in &mut rules {
        if let Some(applied) = state.rules.get(&rule.candidate.id)
            && rule.text.is_some()
        {
            rule.text = Some(applied.text.clone());
        }
    }
    let mut skipped = BTreeMap::from([
        ("typos", analysis.typos),
        ("credentials", analysis.credentials + run_stats.withheld),
        ("contradicted", count(&rules, Reason::Contradicted)),
        ("stale", count(&rules, Reason::Stale)),
        ("nested_checkout", analysis.nested_checkout),
        ("dismissed", 0),
        ("over_limit", 0),
    ]);
    let ready: Vec<&Rule> = rules.iter().filter(|rule| rule.held.is_none()).collect();
    let transcripts_contributed = ready.iter().any(|rule| rule.candidate.transcript);
    let targets = block::targets(workspace, &explicit, transcripts_contributed, apply)?;
    notes.extend(targets.notes.iter().cloned());

    let _guard = if apply {
        workspace.prepare_state()?;
        let guard = lock_state(&workspace.state.join("setup.lock"))?;
        // Reload under the lock in case another learn or init ran meanwhile.
        state = block::load(workspace)?;
        Some(guard)
    } else {
        None
    };
    let mut eligible: Vec<&Rule> = Vec::new();
    for rule in ready {
        if state.dismissed.contains(&rule.candidate.id) {
            *skipped.get_mut("dismissed").expect("key") += 1;
        } else {
            eligible.push(rule);
        }
    }
    let original = state.clone();
    let (removals, kept) = removals(&mut state, &workspace.root, &env, &rules, now);
    // Every eligible rule is offered in rank order: rules already in the block stay
    // whatever --limit says, and new ones fill the room that is left.
    let proposals: Vec<block::Proposal> = eligible
        .iter()
        .map(|rule| {
            let candidate = rule.candidate;
            let text = rule.text.clone().expect("eligible text");
            block::Proposal {
                id: candidate.id.clone(),
                text: text.clone(),
                applied: block::Applied {
                    class: candidate.class.name().to_owned(),
                    scope: rule.scope.name().to_owned(),
                    text,
                    slots: candidate.slots.clone(),
                    sessions: candidate.sessions.len() as u64,
                    failures: candidate.failures,
                    last_seen_ms: candidate.last_ms,
                    files: BTreeSet::new(),
                    stale_since_ms: None,
                },
                machine: rule.scope == Scope::Machine,
            }
        })
        .collect();
    // A preview plans the same merge in memory, so it shows what --apply would write.
    let outcome = block::plan(
        &workspace.root,
        &mut state,
        &block::Plan {
            targets: &targets,
            proposals: &proposals,
            removals: &removals,
            limit,
            include_machine,
            strict: apply,
            now,
        },
        &mut notes,
    )?;
    if apply {
        block::write(workspace, &mut state, &original, &outcome)?;
    }

    let within = |id: &str, pick: fn(&block::Change) -> &Vec<String>| {
        outcome
            .changes
            .iter()
            .any(|change| pick(change).iter().any(|other| other == id))
    };
    let mut proposed: Vec<&Rule> = Vec::new();
    let mut full: Vec<(&Rule, Reason)> = Vec::new();
    let mut machine: Vec<(&Rule, Reason)> = Vec::new();
    for rule in eligible {
        let id = rule.candidate.id.as_str();
        let counter = if outcome.changes.is_empty() {
            // No file can take rules: show the best ones up to --limit.
            if proposed.len() < limit {
                proposed.push(rule);
                continue;
            }
            "over_limit"
        } else if within(id, |change| &change.present) {
            proposed.push(rule);
            continue;
        } else if within(id, |change| &change.dismissed) {
            "dismissed"
        } else if within(id, |change| &change.full) {
            full.push((rule, Reason::BlockFull));
            continue;
        } else if within(id, |change| &change.machine) && !within(id, |change| &change.over) {
            machine.push((rule, Reason::MachineScope));
            continue;
        } else {
            "over_limit"
        };
        *skipped.get_mut(counter).expect("key") += 1;
    }

    // Rules the block had no room for come first, so --limit never hides them.
    let mut held: Vec<(&Rule, Reason)> = full;
    held.extend(machine);
    held.extend(rules.iter().filter_map(|rule| Some((rule, rule.held?))));
    // Count every held rule before the list is cut to --limit.
    let held_counts: BTreeMap<&'static str, u64> = Reason::COUNTED
        .into_iter()
        .map(|reason| {
            let count = held.iter().filter(|(_, held)| *held == reason).count();
            (reason.name(), count as u64)
        })
        .collect();
    held.truncate(limit);
    if run_stats.without_command > 0 {
        notes.insert(
            0,
            format!(
                "{} archived run(s) predate command capture and were skipped.",
                run_stats.without_command
            ),
        );
    }
    if !kept.is_empty() {
        let details: BTreeSet<&str> = kept.iter().map(String::as_str).collect();
        notes.push(format!(
            "Kept {} written rule(s) whose check fails here ({}); `--apply` removes them if the check still fails a day later.",
            kept.len(),
            details.into_iter().collect::<Vec<_>>().join("; ")
        ));
    }
    if held_counts[Reason::MachineScope.name()] > 0 {
        let shared: Vec<&str> = targets.shared.iter().map(String::as_str).collect();
        notes.push(format!(
            "Rules about this machine stay out of {}, which Git shares, so teammates and CI are not told them; `--include-machine` writes them, or copy them to a personal file such as ~/.claude/CLAUDE.md.",
            shared.join(" and ")
        ));
    }
    if !proposed.is_empty()
        && let Some(shadow) = &targets.shadow
    {
        notes.push(format!(
            "Claude Code reads {} here, not AGENTS.md; add a CLAUDE.md with the line @AGENTS.md so it reads these rules.",
            shadow.display()
        ));
    }

    let report = Report {
        days,
        min_sessions,
        transcripts: &transcript_stats,
        codex: &codex_stats,
        agy: &agy_stats,
        runs: &run_stats,
        proposed: &proposed,
        held: &held,
        held_counts: &held_counts,
        skipped: &skipped,
        notes: &notes,
        targets: &targets,
        outcome: &outcome,
        applied: apply,
        justless: &workspace.justless(),
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report.json())?);
    } else {
        print!("{}", report.text());
    }
    Ok(())
}

/// Written rules to remove now, and the details of machine checks that fail here but
/// have not failed for a day. A `PATH` or `$SHELL` check can fail in one shell, such
/// as an activated virtual environment, and hold in the next, so one failure is not
/// enough to rewrite the instructions.
fn removals(
    state: &mut block::State,
    root: &std::path::Path,
    env: &Env,
    rules: &[Rule],
    now: u64,
) -> (BTreeMap<String, &'static str>, Vec<String>) {
    let mut removals = BTreeMap::new();
    let mut kept = Vec::new();
    for (id, written) in &mut state.rules {
        let Some(class) = patterns::Class::parse(&written.class) else {
            continue;
        };
        if rules
            .iter()
            .any(|rule| rule.candidate.id == *id && rule.candidate.contradicted)
        {
            removals.insert(id.clone(), "the command later succeeded");
            continue;
        }
        match patterns::check(root, env, class, &written.slots) {
            Check::Stale(detail) if patterns::scope(class, &written.slots) == Scope::Machine => {
                let since = *written.stale_since_ms.get_or_insert(now);
                if now.saturating_sub(since) >= DAY_MS {
                    removals.insert(id.clone(), "its check no longer holds");
                } else {
                    kept.push(detail);
                }
            }
            Check::Stale(_) => {
                removals.insert(id.clone(), "its check no longer holds");
            }
            _ => written.stale_since_ms = None,
        }
    }
    (removals, kept)
}

fn count(rules: &[Rule], reason: Reason) -> u64 {
    rules
        .iter()
        .filter(|rule| rule.held == Some(reason))
        .count() as u64
}

struct Report<'a> {
    days: u64,
    min_sessions: u64,
    transcripts: &'a events::TranscriptStats,
    codex: &'a events::TranscriptStats,
    agy: &'a events::TranscriptStats,
    runs: &'a events::RunStats,
    proposed: &'a [&'a Rule<'a>],
    held: &'a [(&'a Rule<'a>, Reason)],
    held_counts: &'a BTreeMap<&'static str, u64>,
    skipped: &'a BTreeMap<&'static str, u64>,
    notes: &'a [String],
    targets: &'a block::Targets,
    /// The merge `--apply` made, or a preview planned.
    outcome: &'a block::Outcome,
    applied: bool,
    /// How a printed command starts. See [`Workspace::justless`].
    justless: &'a str,
}

impl Report<'_> {
    fn json(&self) -> Value {
        let t = self.transcripts;
        let r = self.runs;
        let rules: Vec<Value> = self
            .proposed
            .iter()
            .map(|rule| {
                let candidate = rule.candidate;
                let (state, detail) = match &rule.check {
                    Check::Verified(detail) => ("verified", Value::from(detail.as_str())),
                    Check::Unverified => ("unverified", Value::Null),
                    Check::Stale(detail) => ("stale", Value::from(detail.as_str())),
                };
                let example = patterns::example(candidate.class, &candidate.slots).map(
                    |(wrong, right, signal)| json!({"wrong": wrong, "right": right, "signal": signal}),
                );
                json!({
                    "id": candidate.id,
                    "class": candidate.class.name(),
                    "scope": rule.scope.name(),
                    "text": rule.text,
                    "failures": candidate.failures,
                    "corrections": candidate.corrections,
                    "sessions": candidate.sessions.len(),
                    "threads": candidate.threads.len(),
                    "failure_bytes": candidate.failure_bytes,
                    "first_seen": utc_date(candidate.first_ms),
                    "last_seen": utc_date(candidate.last_ms),
                    "score": rule.score,
                    "check": {"state": state, "detail": detail},
                    "example": example,
                    "evidence": candidate.evidence().into_iter().map(|pointer| match pointer {
                        Pointer::Run { id, stream } => json!({"source": "run", "id": id, "stream": stream}),
                        Pointer::Transcript { file, line } => json!({"source": "transcript", "file": file, "line": line}),
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        let held: Vec<Value> = self
            .held
            .iter()
            .map(|(rule, reason)| {
                let candidate = rule.candidate;
                json!({
                    "id": candidate.id,
                    "class": candidate.class.name(),
                    "scope": rule.scope.name(),
                    "text": rule.text,
                    "sessions": candidate.sessions.len(),
                    "threads": candidate.threads.len(),
                    "failures": candidate.failures,
                    "reason": reason.name(),
                })
            })
            .collect();
        let skipped: serde_json::Map<String, Value> = [
            "typos",
            "credentials",
            "contradicted",
            "stale",
            "nested_checkout",
            "dismissed",
            "over_limit",
        ]
        .iter()
        .map(|key| ((*key).to_owned(), Value::from(self.skipped[key])))
        .collect();
        let outcome = self.outcome;
        let apply = if self.applied {
            json!({
                "applied": true,
                "targets": self.targets.files,
                "changed": outcome.changes.iter().filter(|c| c.content.is_some()).map(|c| &c.file).collect::<Vec<_>>(),
                "skipped": self.skipped_targets(),
                "added": unique(outcome.changes.iter().flat_map(|c| &c.added)),
                "removed": unique(outcome.changes.iter().flat_map(|c| c.removed.iter().map(|(id, _)| id))),
            })
        } else {
            json!({
                "applied": false,
                "targets": self.targets.files,
                "changed": [],
                "skipped": self.skipped_targets(),
                "added": [],
                "removed": [],
            })
        };
        json!({
            "days": self.days,
            "min_sessions": self.min_sessions,
            "sources": {
                "transcripts": {
                    "available": t.available,
                    "directory": t.directory.as_ref().map(|dir| dir.to_string_lossy()),
                    "files": t.files,
                    "sessions": t.sessions.len(),
                    "threads": t.threads.len(),
                    "results": t.results,
                    "not_run": t.not_run,
                    "status_unknown": t.status_unknown,
                },
                "codex_rollouts": {
                    "available": self.codex.available,
                    "directory": self.codex.directory.as_ref().map(|dir| dir.to_string_lossy()),
                    "files": self.codex.files,
                    "sessions": self.codex.sessions.len(),
                    "threads": self.codex.threads.len(),
                    "results": self.codex.results,
                    "not_run": self.codex.not_run,
                },
                "agy_sessions": {
                    "available": self.agy.available,
                    "directory": self.agy.directory.as_ref().map(|dir| dir.to_string_lossy()),
                    "files": self.agy.files,
                    "sessions": self.agy.sessions.len(),
                    "threads": self.agy.threads.len(),
                    "results": self.agy.results,
                    "not_run": self.agy.not_run,
                },
                "runs": {
                    "available": r.available,
                    "scanned": r.scanned,
                    "without_command": r.without_command,
                    "unreadable": r.unreadable,
                    "linked": r.linked,
                    "covered_by_transcripts": r.covered_by_transcripts,
                    "capped": r.capped,
                },
            },
            "rules": rules,
            "held": held,
            "skipped": skipped,
            "notes": self.notes,
            "apply": apply,
        })
    }

    fn skipped_targets(&self) -> Vec<Value> {
        self.targets
            .skipped
            .iter()
            .map(|(file, reason)| json!({"file": file, "reason": reason}))
            .collect()
    }

    fn text(&self) -> String {
        let mut out = String::new();
        let t = self.transcripts;
        let (sessions, results, runs) = (
            thousands(t.sessions.len() as u64),
            thousands(t.results),
            thousands(self.runs.scanned),
        );
        let listed = |reason: Reason| -> Vec<&str> {
            self.held
                .iter()
                .filter(|(_, held)| *held == reason)
                .filter_map(|(rule, _)| rule.text.as_deref())
                .collect()
        };
        let (full, machine) = (listed(Reason::BlockFull), listed(Reason::MachineScope));
        if self.proposed.is_empty() && full.is_empty() && machine.is_empty() {
            out.push_str(&format!(
                "No repeated command mistakes in the last {} day(s): {sessions} Claude Code session(s), {results} shell results, {runs} archived run(s).\n",
                self.days
            ));
        } else {
            out.push_str(&format!(
                "Read {sessions} Claude Code session(s) ({} agent thread(s), {results} shell results) and {runs} archived run(s) from the last {} day(s).\n",
                thousands(t.threads.len() as u64),
                self.days
            ));
        }
        if self.codex.results > 0 {
            out.push_str(&format!(
                "Also read {} Codex session(s) ({} shell results).\n",
                thousands(self.codex.sessions.len() as u64),
                thousands(self.codex.results)
            ));
        }
        if self.agy.results > 0 {
            out.push_str(&format!(
                "Also read {} Antigravity CLI session(s) ({} shell results).\n",
                thousands(self.agy.sessions.len() as u64),
                thousands(self.agy.results)
            ));
        }
        if !t.available {
            match &t.directory {
                Some(dir) => out.push_str(&format!("Claude Code transcripts: none under {}.\n", dir.display())),
                None => out.push_str("Claude Code transcripts: no configuration directory (set HOME or CLAUDE_CONFIG_DIR).\n"),
            }
        }
        if self.runs.unreadable > 0 {
            out.push_str(&format!(
                "{} archived run(s) could not be read.\n",
                self.runs.unreadable
            ));
        }
        if self.runs.capped {
            out.push_str("Stopped after 5,000 archived runs; use --days to read fewer.\n");
        }
        if !self.proposed.is_empty() {
            out.push_str(&format!(
                "{} rule(s) {}, most frequent first:\n\n",
                self.proposed.len(),
                if self.applied {
                    "in the learned block"
                } else {
                    "proposed"
                }
            ));
            for (index, rule) in self.proposed.iter().enumerate() {
                out.push_str(&format!(
                    "{}. {}\n   {}\n",
                    index + 1,
                    rule.text.as_deref().unwrap_or_default(),
                    stats_line(rule)
                ));
                let evidence = rule.candidate.evidence();
                // A run replays its output; a transcript line holds the call and result.
                match evidence
                    .iter()
                    .find(|pointer| matches!(pointer, Pointer::Run { .. }))
                    .or(evidence.first())
                {
                    Some(Pointer::Run { id, stream }) => out.push_str(&format!(
                        "   evidence: {} log {id} --stream {stream}\n",
                        self.justless
                    )),
                    Some(Pointer::Transcript { file, line }) => {
                        let path = t.directory.as_ref().map_or_else(
                            || file.clone(),
                            |dir| dir.join(file).display().to_string(),
                        );
                        out.push_str(&format!("   evidence: {path}:{line}\n"));
                    }
                    None => {}
                }
            }
        }
        if !full.is_empty() {
            out.push_str("\nNot written, the block is full:\n");
            for text in &full {
                out.push_str(&format!("- {text}\n"));
            }
            out.push_str("Delete a rule's line between the markers to dismiss it and make room.\n");
        }
        if !machine.is_empty() {
            out.push_str("\nAbout this machine, kept out of files Git shares:\n");
            for text in &machine {
                out.push_str(&format!("- {text}\n"));
            }
        }
        let held = self.held_line();
        let skipped = self.applied || !self.proposed.is_empty();
        if !held.is_empty() || !self.notes.is_empty() || skipped && !self.targets.skipped.is_empty()
        {
            out.push('\n');
        }
        if !held.is_empty() {
            out.push_str(&format!("Held back: {held}\n"));
        }
        if skipped {
            for (file, reason) in &self.targets.skipped {
                let reason = match reason.as_str() {
                    "symbolic link" => "it is a symbolic link".to_owned(),
                    "not a regular file" => "it is not a regular file".to_owned(),
                    other => format!("it {other}"),
                };
                out.push_str(&format!("Skipped {file}: {reason}.\n"));
            }
        }
        for note in self.notes {
            out.push_str(note);
            out.push('\n');
        }
        if self.applied {
            out.push_str(&self.apply_text());
        } else if !self.proposed.is_empty() {
            let changed: Vec<&str> = self
                .outcome
                .changes
                .iter()
                .filter(|change| change.content.is_some())
                .map(|change| change.file.as_str())
                .collect();
            if !changed.is_empty() {
                out.push_str(&format!(
                    "Preview only. `{} learn --apply` writes these rules to {}.\n",
                    self.justless,
                    changed.join(" and ")
                ));
            } else if !self.outcome.changes.is_empty() {
                out.push_str(&format!(
                    "Preview only. {} already up to date.\n",
                    self.targets.files.join(" and ")
                ));
            }
        }
        out
    }

    fn held_line(&self) -> String {
        let held = |reason: Reason| self.held_counts[reason.name()];
        let below = held(Reason::BelowMinSessions);
        let mut parts = Vec::new();
        if below > 0 {
            parts.push(if self.min_sessions > 1 {
                format!("{below} below --min-sessions (--min-sessions 1 lists them)")
            } else {
                format!("{below} below --min-sessions")
            });
        }
        let s = |key: &str| self.skipped[key];
        for (value, label) in [
            (s("typos"), "typo(s)"),
            (s("credentials"), "dropped for a possible credential"),
            (s("contradicted"), "contradicted by a later success"),
            (s("stale"), "no longer true by its check"),
            (s("nested_checkout"), "split across nested checkouts"),
            (
                held(Reason::UnsafeText),
                "with text that is not safe to write",
            ),
            (s("dismissed"), "dismissed"),
            (s("over_limit"), "beyond --limit"),
        ] {
            if value > 0 {
                parts.push(format!("{value} {label}"));
            }
        }
        parts.join(" · ")
    }

    fn apply_text(&self) -> String {
        let mut out = String::new();
        let mut wrote = false;
        for change in &self.outcome.changes {
            if change.content.is_some() {
                let mut parts = Vec::new();
                if !change.added.is_empty() {
                    parts.push(format!("{} added", change.added.len()));
                }
                match change.removed.as_slice() {
                    [] => {}
                    [(_, reason)] => parts.push(format!("1 removed: {reason}")),
                    removed => parts.push(format!("{} removed", removed.len())),
                }
                if !change.dismissed.is_empty() {
                    parts.push(format!("{} dismissed", change.dismissed.len()));
                }
                let detail = if parts.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", parts.join(", "))
                };
                out.push_str(&format!(
                    "Updated {}: {} learned rule(s){detail}.\n",
                    change.file, change.lines
                ));
                wrote = true;
            } else if !change.full.is_empty() {
                out.push_str(&format!(
                    "{} unchanged: {} learned rule(s); {} rule(s) did not fit.\n",
                    change.file,
                    change.lines,
                    change.full.len()
                ));
                wrote = true;
            } else if change.lines > 0 {
                out.push_str(&format!(
                    "{} is up to date ({} learned rule(s)).\n",
                    change.file, change.lines
                ));
                wrote = true;
            }
        }
        if !wrote {
            out.push_str("No learned rules to write.\n");
        }
        out
    }
}

fn stats_line(rule: &Rule) -> String {
    let candidate = rule.candidate;
    let mut counts = format!("{} failure(s)", candidate.failures);
    if candidate.corrections > 0 {
        counts.push_str(&format!(", {} correction(s)", candidate.corrections));
    }
    counts.push_str(&format!(" in {} session(s)", candidate.sessions.len()));
    if candidate.threads.len() > candidate.sessions.len() {
        counts.push_str(&format!(", {} thread(s)", candidate.threads.len()));
    }
    let check = match &rule.check {
        Check::Verified(detail) => format!("checked: {detail}"),
        _ => "not checked".to_owned(),
    };
    format!(
        "{} · {} · {counts} · {} of failed output · last {} · {check}",
        candidate.class.label(),
        rule.scope.name(),
        human_bytes(candidate.failure_bytes),
        utc_date(candidate.last_ms)
    )
}

fn unique<'a>(ids: impl Iterator<Item = &'a String>) -> Vec<&'a String> {
    let mut seen = BTreeSet::new();
    ids.filter(|id| seen.insert(*id)).collect()
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}
