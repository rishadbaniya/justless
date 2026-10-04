//! `justless learn`: turn command mistakes that agents corrected or keep repeating into
//! short project rules.
//!
//! Evidence comes from Claude Code transcripts and the run archive, both local. Only
//! a closed table of signatures counts, every rule is checked against the current
//! tree, `PATH` or `$SHELL`, and rules are built from validated slots, never copied
//! from commands. Without `--apply` nothing is written.
mod block;
mod events;
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reason {
    BelowMinSessions,
    Contradicted,
    Stale,
    UnsafeText,
    BlockFull,
}

impl Reason {
    fn name(self) -> &'static str {
        match self {
            Self::BelowMinSessions => "below_min_sessions",
            Self::Contradicted => "contradicted",
            Self::Stale => "stale",
            Self::UnsafeText => "unsafe_text",
            Self::BlockFull => "block_full",
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
    let explicit: Vec<String> = matches
        .values_of("target")
        .map(|values| values.map(str::to_owned).collect())
        .unwrap_or_default();

    let now = now_ms();
    let since = now.saturating_sub(days.saturating_mul(DAY_MS));
    let mut transcript_stats = events::TranscriptStats::default();
    let transcripts = events::transcripts(workspace, since, &mut transcript_stats);
    let mut run_stats = events::RunStats::default();
    let runs = events::runs(
        workspace,
        since,
        &transcript_stats.sessions,
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

    // Learned state is read without creating .justless; only --apply writes it.
    let mut state = block::load(workspace)?;
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
        ("credentials", analysis.credentials),
        ("contradicted", count(&rules, Reason::Contradicted)),
        ("stale", count(&rules, Reason::Stale)),
        ("nested_checkout", analysis.nested_checkout),
        ("dismissed", 0),
        ("over_limit", 0),
    ]);
    let mut proposed: Vec<&Rule> = Vec::new();
    for rule in rules.iter().filter(|rule| rule.held.is_none()) {
        if state.dismissed.contains(&rule.candidate.id) {
            *skipped.get_mut("dismissed").expect("key") += 1;
        } else if proposed.len() < limit {
            proposed.push(rule);
        } else {
            *skipped.get_mut("over_limit").expect("key") += 1;
        }
    }
    let transcripts_contributed = proposed.iter().any(|rule| rule.candidate.transcript);
    let targets = block::targets(&workspace.root, &explicit, transcripts_contributed)?;
    let mut notes = targets.notes.clone();

    let mut held: Vec<(&Rule, Reason)> = rules
        .iter()
        .filter_map(|rule| Some((rule, rule.held?)))
        .collect();
    let mut applied = None;
    if apply {
        workspace.prepare_state()?;
        let _guard = lock_state(&workspace.state.join("setup.lock"))?;
        // Reload under the lock in case another learn or init ran meanwhile.
        state = block::load(workspace)?;
        let removals: BTreeMap<String, &'static str> = state
            .rules
            .iter()
            .filter_map(|(id, written)| {
                let class = patterns::Class::parse(&written.class)?;
                if let Check::Stale(_) =
                    patterns::check(&workspace.root, &env, class, &written.slots)
                {
                    Some((id.clone(), "its check no longer holds"))
                } else if rules
                    .iter()
                    .any(|rule| rule.candidate.id == *id && rule.candidate.contradicted)
                {
                    Some((id.clone(), "the command later succeeded"))
                } else {
                    None
                }
            })
            .collect();
        let proposals: Vec<block::Proposal> = proposed
            .iter()
            .map(|rule| {
                let candidate = rule.candidate;
                let text = rule.text.clone().expect("proposed text");
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
                    },
                }
            })
            .collect();
        let outcome = block::apply(
            workspace,
            &mut state,
            &targets.files,
            &proposals,
            &removals,
            limit,
        )?;
        let mut full: Vec<(&Rule, Reason)> = Vec::new();
        for change in &outcome.changes {
            for id in &change.full {
                if let Some(rule) = rules.iter().find(|rule| rule.candidate.id == *id)
                    && !full.iter().any(|(seen, _)| seen.candidate.id == *id)
                {
                    full.push((rule, Reason::BlockFull));
                }
            }
        }
        // Rules the block had no room for come first, so --limit never hides them.
        held.splice(0..0, full);
        applied = Some((outcome, removals));
    }
    // Count every held rule before the list is cut to --limit.
    let held_counts: BTreeMap<&'static str, u64> = [
        Reason::BelowMinSessions,
        Reason::UnsafeText,
        Reason::BlockFull,
    ]
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

    let report = Report {
        days,
        min_sessions,
        transcripts: &transcript_stats,
        runs: &run_stats,
        proposed: &proposed,
        held: &held,
        held_counts: &held_counts,
        skipped: &skipped,
        notes: &notes,
        targets: &targets,
        applied: applied.as_ref().map(|(outcome, _)| outcome),
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report.json())?);
    } else {
        print!("{}", report.text());
    }
    Ok(())
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
    runs: &'a events::RunStats,
    proposed: &'a [&'a Rule<'a>],
    held: &'a [(&'a Rule<'a>, Reason)],
    held_counts: &'a BTreeMap<&'static str, u64>,
    skipped: &'a BTreeMap<&'static str, u64>,
    notes: &'a [String],
    targets: &'a block::Targets,
    applied: Option<&'a block::Outcome>,
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
        let apply = match self.applied {
            Some(outcome) => json!({
                "applied": true,
                "targets": self.targets.files,
                "changed": outcome.changes.iter().filter(|c| c.content.is_some()).map(|c| &c.file).collect::<Vec<_>>(),
                "skipped": self.skipped_targets(),
                "added": unique(outcome.changes.iter().flat_map(|c| &c.added)),
                "removed": unique(outcome.changes.iter().flat_map(|c| c.removed.iter().map(|(id, _)| id))),
            }),
            None => json!({
                "applied": false,
                "targets": self.targets.files,
                "changed": [],
                "skipped": self.skipped_targets(),
                "added": [],
                "removed": [],
            }),
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
        if self.proposed.is_empty() {
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
                "{} rule(s) proposed, most frequent first:\n\n",
                self.proposed.len()
            ));
            for (index, rule) in self.proposed.iter().enumerate() {
                out.push_str(&format!(
                    "{}. {}\n   {}\n",
                    index + 1,
                    rule.text.as_deref().unwrap_or_default(),
                    stats_line(rule)
                ));
                if let Some(Pointer::Run { id, stream }) = rule
                    .candidate
                    .evidence()
                    .into_iter()
                    .find(|pointer| matches!(pointer, Pointer::Run { .. }))
                {
                    out.push_str(&format!(
                        "   evidence: justless log {id} --stream {stream}\n"
                    ));
                }
            }
        }
        let held = self.held_line();
        if !held.is_empty() || !self.notes.is_empty() {
            out.push('\n');
        }
        if !held.is_empty() {
            out.push_str(&format!("Held back: {held}\n"));
        }
        for note in self.notes {
            out.push_str(note);
            out.push('\n');
        }
        let files = self.targets.files.join(" and ");
        match self.applied {
            None if !self.proposed.is_empty() && !files.is_empty() => out.push_str(&format!(
                "Preview only. `justless learn --apply` writes these rules to {files}.\n"
            )),
            None => {}
            Some(outcome) => out.push_str(&self.apply_text(outcome)),
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
            (held(Reason::BlockFull), "not written: the block is full"),
        ] {
            if value > 0 {
                parts.push(format!("{value} {label}"));
            }
        }
        parts.join(" · ")
    }

    fn apply_text(&self, outcome: &block::Outcome) -> String {
        let mut out = String::new();
        for (file, reason) in &self.targets.skipped {
            let reason = match reason.as_str() {
                "symbolic link" => "it is a symbolic link".to_owned(),
                "not a regular file" => "it is not a regular file".to_owned(),
                other => format!("it {other}"),
            };
            out.push_str(&format!("Skipped {file}: {reason}.\n"));
        }
        let mut wrote = false;
        for change in &outcome.changes {
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
