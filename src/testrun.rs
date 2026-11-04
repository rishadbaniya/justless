//! `justless test`: runs the full suite, or with `--affected` the tests that
//! reach the current Git changes, through the same capture as `justless run`.
//! A subset is always labeled as one and never reported as the whole suite.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use clap::{Arg, ArgMatches, Command};
use serde_json::{Value, json};

use crate::capture;
use crate::index::Index;
use crate::runners::{self, Kind, Tally};
use crate::selection::{self, Mode, Plan, Reason, RunnerPlan, Step, StepKind};
use crate::workspace::{Workspace, write_json};

/// Bytes of the step command echoed before each run.
const ECHO: usize = 200;
/// Failing tests explained in the verdict.
const EVIDENCE: usize = 3;
/// Bytes per evidence line.
const EVIDENCE_BYTES: usize = 160;
/// Largest selection record kept with a run.
const RECORD_BYTES: usize = 256 * 1024;

pub fn command() -> Command<'static> {
    Command::new("test")
        .about("Run the project's tests, or with --affected only those that reach the current Git changes")
        .arg(
            Arg::new("affected")
                .long("affected")
                .help("Run only the tests that can reach the current Git changes, labeled as a subset"),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .visible_alias("plan")
                .help("Print the selection, the reasons and the commands without running them"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .requires("dry-run")
                .help("Print the plan as JSON"),
        )
        .arg(
            Arg::new("no-fallback")
                .long("no-fallback")
                .requires("affected")
                .help("Keep the subset when confidence is low instead of running the full suite"),
        )
        .arg(crate::base_arg().requires("affected"))
        .arg(
            Arg::new("why")
                .long("why")
                .value_name("RUN_ID")
                .conflicts_with_all(&["affected", "dry-run", "command"])
                .help("Show why a recorded run selected its tests"),
        )
        .arg(
            Arg::new("test")
                .long("test")
                .value_name("TEST")
                .requires("why")
                .help("With --why, only the tests whose name contains TEST"),
        )
        .arg(
            Arg::new("session")
                .long("session")
                .takes_value(true)
                .help("Scope repeat comparisons to this session"),
        )
        .arg(
            Arg::new("fresh")
                .long("fresh")
                .help("Show fresh previews without comparing previous runs"),
        )
        .arg(
            Arg::new("max-bytes")
                .long("max-bytes")
                .value_name("N")
                .help("Maximum preview bytes per stream (default 4096)"),
        )
        .arg(crate::budget())
        .arg(
            Arg::new("command")
                .multiple_values(true)
                .allow_invalid_utf8(true)
                .last(true)
                .value_name("RUNNER [ARGS]")
                .help("The command that runs the full suite, such as cargo test --offline"),
        )
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<i32> {
    let budget = crate::context_budget(workspace, matches)?;
    if let Some(id) = matches.value_of("why") {
        why(workspace, id, matches.value_of("test"), budget)?;
        return Ok(0);
    }
    let command = matches
        .values_of_os("command")
        .map(|values| {
            values
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_owned)
                        .context("the test command must be UTF-8")
                })
                .collect::<Result<Vec<String>>>()
        })
        .transpose()?;
    if let Some(command) = &command {
        ensure!(!command.is_empty(), "expected a command after --");
    }
    let settings = workspace.settings()?;
    let index = Index::open(workspace)?;
    let runners = runners::detect(workspace, &index, command.as_deref())?;
    let affected = matches.is_present("affected");
    let plan = if affected {
        selection::plan(
            workspace,
            &index,
            runners,
            &settings.test,
            matches.is_present("no-fallback"),
            matches.value_of("base"),
        )?
    } else {
        selection::full_suite(&runners)
    };
    if matches.is_present("dry-run") {
        if matches.is_present("json") {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print!("{}", render(&plan, affected, budget));
        }
        return Ok(0);
    }
    let max_bytes = if matches.is_present("max-bytes") {
        crate::read_num(matches, "max-bytes", 512)?
    } else {
        settings.max_output_bytes
    };
    let default_session = std::env::var("JUSTLESS_SESSION").unwrap_or_else(|_| "terminal".into());
    // The command that runs the full suite of the same runners in the same workspace.
    let mut full_suite = format!("{} test", workspace.justless());
    if let Some(command) = &command {
        full_suite.push_str(" --");
        for word in command {
            full_suite.push(' ');
            full_suite.push_str(&quote(word));
        }
    }
    let options = Options {
        justless: workspace.justless(),
        full_suite,
        max_bytes,
        session: matches
            .value_of("session")
            .unwrap_or(&default_session)
            .to_owned(),
        fresh: matches.is_present("fresh"),
        affected,
    };
    execute(workspace, &plan, &options)
}

struct Options {
    /// How a printed command starts. See [`Workspace::justless`].
    justless: String,
    /// The printed command for the full suite.
    full_suite: String,
    max_bytes: usize,
    session: String,
    fresh: bool,
    affected: bool,
}

fn plural(count: usize, noun: &str) -> String {
    crate::map::plural(count, noun)
}

/// The noun alone, plural unless the count is one.
fn units(count: usize, noun: &str) -> String {
    if count == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
}

fn codes(plan: &RunnerPlan) -> String {
    let mut codes: Vec<&str> = plan.reasons.iter().map(|reason| reason.code).collect();
    codes.dedup();
    codes.join(", ")
}

/// One line per runner: what runs and why.
fn runner_line(plan: &RunnerPlan, affected: bool) -> String {
    let who = if plan.root.is_empty() {
        plan.runner.label().to_owned()
    } else {
        format!("{} ({})", plan.runner.label(), plan.root)
    };
    let counts = || {
        let mut text = format!(
            "{} of {} indexed {}",
            plan.selected,
            plan.indexed,
            units(plan.indexed, plan.unit)
        );
        if let Some((chosen, total)) = plan.targets {
            text.push_str(&format!(" in {chosen} of {total} test targets"));
        }
        text
    };
    let first_reason = || {
        plan.reasons
            .first()
            .map(|reason| reason.message.clone())
            .unwrap_or_default()
    };
    match plan.mode {
        Mode::Subset => format!("{who} · subset · {}", counts()),
        Mode::LowConfidence => format!(
            "{who} · low-confidence subset ({}) · {}",
            codes(plan),
            counts()
        ),
        Mode::BuildOnly if !plan.reasons.is_empty() => format!(
            "{who} · build only, low confidence ({}) · no test reaches the changed code",
            codes(plan)
        ),
        Mode::BuildOnly => format!("{who} · build only · no test reaches the changed code"),
        Mode::Nothing => format!("{who} · nothing to run · no change reaches its tests"),
        Mode::Command => format!("{who} · command as given · {}", first_reason()),
        Mode::Full if affected && !plan.reasons.is_empty() => {
            let extra = plan.reasons.len() - 1;
            format!(
                "{who} · full suite · {}{}",
                first_reason(),
                if extra > 0 {
                    format!(" (+{} more)", extra)
                } else {
                    String::new()
                }
            )
        }
        Mode::Full => match plan.reasons.first() {
            Some(reason) => format!("{who} · full suite · {}", reason.message),
            None => format!("{who} · full suite"),
        },
    }
}

/// The step command as echoed before it runs: test names collapse to a count.
fn echo(step: &Step) -> String {
    let names: BTreeSet<&str> = step.names.values().flatten().map(String::as_str).collect();
    let mut words = Vec::new();
    let mut collapsed = 0;
    for word in &step.argv {
        if names.contains(word.as_str()) || (step.kind == StepKind::Filtered && word.contains("::"))
        {
            collapsed += 1;
            continue;
        }
        if collapsed > 0 {
            words.push(format!("<{}>", plural(collapsed, "test name")));
            collapsed = 0;
        }
        words.push(quote(word));
    }
    if collapsed > 0 {
        words.push(format!("<{}>", plural(collapsed, "test name")));
    }
    let text = words.join(" ");
    if text.len() > ECHO {
        format!("{}…", &text[..text.floor_char_boundary(ECHO)])
    } else {
        text
    }
}

fn quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_./:=@%+,-".contains(&byte))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn header(plan: &Plan, affected: bool) -> String {
    if !affected {
        return "Tests · full suite".into();
    }
    let symbols = plan
        .seeds
        .iter()
        .filter(|note| !note.detail.starts_with("removed from"))
        .count();
    format!(
        "Affected tests · working tree vs {} · {} · {}",
        plan.base,
        plural(plan.changed_files(), "changed file"),
        plural(symbols, "starting symbol")
    )
}

/// The `--dry-run` text, bounded by the budget.
fn render(plan: &Plan, affected: bool, budget: usize) -> String {
    let mut lines = vec![header(plan, affected)];
    lines.extend(plan.note.clone());
    let mut details = Vec::new();
    for note in &plan.changes {
        details.push(format!("Changed {} · {}", note.subject, note.detail));
    }
    for note in &plan.ignored {
        details.push(format!("{} · {}", note.subject, note.detail));
    }
    for note in &plan.seeds {
        details.push(format!("Start {} · {}", note.subject, note.detail));
    }
    let mut runner_blocks = Vec::new();
    for runner in &plan.runners {
        let mut block = vec![runner_line(runner, affected)];
        for reason in &runner.reasons {
            block.push(format!("  {}: {}", reason.code, reason.message));
        }
        for step in &runner.steps {
            block.push(format!(
                "  Run ({}): {}",
                step_label(step.kind),
                step.argv
                    .iter()
                    .map(|word| quote(word))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        let mut tail = Vec::new();
        for test in &runner.tests {
            tail.push(format!(
                "  {} · {} · {} · {}",
                test.id, test.target, test.confidence, test.why
            ));
        }
        for note in &runner.skipped {
            tail.push(format!("  {} · {}", note.subject, note.detail));
        }
        runner_blocks.push((block, tail));
    }
    // Runner lines and steps first, then changes, then per-test evidence.
    let mut used: usize = lines.iter().map(|line| line.len() + 1).sum();
    let mut omitted = 0;
    let mut push = |lines: &mut Vec<String>, line: String, used: &mut usize| {
        if *used + line.len() < budget {
            *used += line.len() + 1;
            lines.push(line);
        } else {
            omitted += 1;
        }
    };
    let mut head = Vec::new();
    for (block, _) in &runner_blocks {
        for line in block {
            push(&mut head, line.clone(), &mut used);
        }
    }
    let mut middle = Vec::new();
    for line in details {
        push(&mut middle, line, &mut used);
    }
    lines.extend(middle);
    lines.extend(head);
    for (_, tail) in &runner_blocks {
        for line in tail {
            push(&mut lines, line.clone(), &mut used);
        }
    }
    if omitted > 0 {
        lines.push(format!(
            "… {} not shown; raise --budget or add --json",
            plural(omitted, "line")
        ));
    }
    if affected {
        lines.push("Nothing ran (dry run).".into());
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn step_label(kind: StepKind) -> &'static str {
    match kind {
        StepKind::Package => "whole packages",
        StepKind::Filtered => "selected tests",
        StepKind::Unfiltered => "whole targets",
        StepKind::Doctests => "doctests",
        StepKind::Build => "build only",
        StepKind::Full => "full suite",
    }
}

fn run_ids(workspace: &Workspace) -> BTreeSet<String> {
    fs::read_dir(workspace.state.join("runs"))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("run-"))
        .collect()
}

/// What one step left behind.
struct Outcome {
    status: i32,
    run: Option<String>,
    tally: Tally,
    elapsed_ms: u64,
    shown: u64,
    raw: u64,
    /// Why the runner itself could not start, which is no test result.
    unavailable: Option<String>,
}

/// Why a step's runner could not start: the program is missing, or Python
/// lacks pytest.
fn unavailable(kind: Kind, program: &str, status: i32, stderr: &str) -> Option<String> {
    if status == 127 {
        return Some(format!("{program} could not be started"));
    }
    (kind == Kind::Pytest && status != 0 && stderr.contains("No module named pytest"))
        .then(|| "No module named pytest".to_owned())
}

fn step_workspace(workspace: &Workspace, cwd: &str) -> Workspace {
    Workspace {
        root: workspace.root.clone(),
        cwd: selection::runner_dir(workspace, cwd),
        state: workspace.state.clone(),
        origin: workspace.origin.clone(),
    }
}

/// Runs one step through capture and reads back what it recorded. `mode` is
/// what the step runs as, which a fallback after the plan can change.
fn run_step(
    workspace: &Workspace,
    plan: &RunnerPlan,
    mode: Mode,
    argv: &[String],
    echoed: &str,
    options: &Options,
) -> Result<Outcome> {
    println!("Run: {echoed}");
    std::io::stdout().flush()?;
    let before = run_ids(workspace);
    let started = Instant::now();
    let args: Vec<OsString> = argv.iter().map(OsString::from).collect();
    let status = match capture::execute(
        &step_workspace(workspace, &plan.cwd),
        &args,
        options.max_bytes,
        false,
        &options.session,
        options.fresh,
        false,
        false,
    ) {
        Ok(status) => status,
        // A runner that cannot start fails its step like a missing command in a
        // shell; steps that already ran keep their results.
        Err(error) => {
            eprintln!("justless: {error:#}");
            127
        }
    };
    std::io::stdout().flush()?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let program = argv.first().map_or("", String::as_str);
    let mut outcome = Outcome {
        status,
        run: None,
        tally: Tally::default(),
        elapsed_ms,
        shown: 0,
        raw: 0,
        unavailable: unavailable(plan.runner, program, status, ""),
    };
    // The run this step recorded: the newest directory that did not exist before.
    let mut fresh: Vec<(u64, String)> = run_ids(workspace)
        .difference(&before)
        .filter_map(|id| {
            let (_, run) = capture::load(workspace, id).ok()?;
            Some((run.completed_ms, id.clone()))
        })
        .collect();
    fresh.sort();
    if let Some((_, id)) = fresh.pop()
        && let Ok((directory, mut run)) = capture::load(workspace, &id)
    {
        let stdout = fs::read(directory.join("stdout")).unwrap_or_default();
        let stderr = fs::read(directory.join("stderr")).unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr);
        outcome.tally = runners::tally(plan.runner, &String::from_utf8_lossy(&stdout), &stderr);
        outcome.unavailable = unavailable(plan.runner, program, status, &stderr);
        outcome.shown = (run.preview_bytes + run.footer_bytes) as u64;
        outcome.raw = run.raw_bytes;
        // Label the record so history, resume and stats show it as a subset.
        let family = match (mode, plan.runner) {
            (_, Kind::Command) => None,
            (Mode::Subset, kind) => Some(format!("{} (affected subset)", kind.family())),
            (Mode::LowConfidence, kind) => Some(format!(
                "{} (affected low-confidence subset)",
                kind.family()
            )),
            (Mode::BuildOnly, kind) => Some(format!("{} (affected build only)", kind.family())),
            (Mode::Full | Mode::Command | Mode::Nothing, kind) => Some(kind.family().to_owned()),
        };
        if let Some(family) = family {
            run.label = plan.runner.family().to_owned();
            run.family = family;
            if let Err(error) = write_json(&directory.join("record.json"), &run) {
                eprintln!("justless: could not label run {id}: {error:#}");
            }
        }
        outcome.run = Some(id);
    }
    Ok(outcome)
}

/// Names a filtered step selected that no result line reported. Quiet output
/// names no tests, so there a target's summary must count at least as many.
fn missing_names(step: &Step, tally: &Tally) -> BTreeMap<String, Vec<String>> {
    let mut missing = BTreeMap::new();
    let anywhere: BTreeSet<&String> = tally.names.values().flatten().collect();
    // `cargo test -q` also drops the `Running` lines that say which target a
    // summary belongs to; then only the total can be checked.
    let selected: usize = step.names.values().map(Vec::len).sum();
    let total_reported = tally.names.is_empty()
        && tally
            .counts
            .get("*")
            .is_some_and(|&count| count >= selected);
    for (target, names) in &step.names {
        let reported = tally.names.get(target);
        if reported.is_none_or(BTreeSet::is_empty)
            && (total_reported
                || tally
                    .counts
                    .get(target)
                    .is_some_and(|&count| count >= names.len()))
        {
            continue;
        }
        let reported = reported.filter(|list| !list.is_empty());
        let absent: Vec<String> = names
            .iter()
            .filter(|name| {
                // A substring filter can report a longer name, such as a generated case.
                let seen = |list: &BTreeSet<String>| {
                    list.iter()
                        .any(|ran| ran == *name || ran.starts_with(&format!("{name}::")))
                };
                match reported {
                    Some(list) => !seen(list),
                    None if tally.names.contains_key("*") => {
                        !anywhere.iter().any(|ran| *ran == *name)
                    }
                    None => true,
                }
            })
            .cloned()
            .collect();
        if !absent.is_empty() {
            missing.insert(target.clone(), absent);
        }
    }
    missing
}

struct RunnerResult {
    status: i32,
    ran: usize,
    failed: Vec<String>,
    known: bool,
    runs: Vec<String>,
    elapsed_ms: u64,
    shown: u64,
    raw: u64,
    notes: Vec<String>,
    doctests: usize,
    cached: usize,
    packages: usize,
    steps: Vec<Value>,
    /// What the runner ran as, once a fallback after the plan has changed it.
    mode: Mode,
    /// Fallback reasons found while running, such as runner-mismatch.
    reasons: Vec<Reason>,
    /// Why the runner could not start.
    unavailable: Option<String>,
}

impl RunnerResult {
    fn new(mode: Mode) -> Self {
        Self {
            status: 0,
            ran: 0,
            failed: Vec::new(),
            known: false,
            runs: Vec::new(),
            elapsed_ms: 0,
            shown: 0,
            raw: 0,
            notes: Vec::new(),
            doctests: 0,
            cached: 0,
            packages: 0,
            steps: Vec::new(),
            mode,
            reasons: Vec::new(),
            unavailable: None,
        }
    }

    /// Adds a step's results. A step that only showed the selection was wrong,
    /// and that a wider step replaces, does not set the status.
    fn record(&mut self, outcome: Outcome, kind: &str, argv: &[String], counted: bool) {
        if counted && self.status == 0 && outcome.status != 0 {
            self.status = outcome.status;
        }
        self.ran += outcome.tally.ran;
        self.failed.extend(outcome.tally.failed.iter().cloned());
        self.known |= outcome.tally.known;
        self.elapsed_ms += outcome.elapsed_ms;
        self.shown += outcome.shown;
        self.raw += outcome.raw;
        self.doctests += outcome.tally.doctests;
        self.cached += outcome.tally.cached;
        self.packages += outcome.tally.packages;
        if self.unavailable.is_none() {
            self.unavailable = outcome.unavailable;
        }
        self.steps.push(json!({
            "kind": kind,
            "argv": argv,
            "run": outcome.run,
            "status": outcome.status,
            "counted": counted,
            "ran": outcome.tally.ran,
            "failed": outcome.tally.failed,
            "elapsed_ms": outcome.elapsed_ms,
        }));
        if let Some(id) = outcome.run {
            self.runs.push(id);
        }
    }

    fn note(&mut self, note: String) {
        println!("{note}");
        self.notes.push(note);
    }
}

/// The command that reruns a filtered step's targets or packages whole for
/// the names it selected that did not report.
fn widening(runner: &RunnerPlan, step: &Step, outcome: &Outcome) -> Option<(usize, Vec<String>)> {
    if step.kind != StepKind::Filtered
        || outcome.status != 0
        || !outcome.tally.known
        || !matches!(runner.runner, Kind::Cargo | Kind::Go)
    {
        return None;
    }
    let missing = missing_names(step, &outcome.tally);
    let count: usize = missing.values().map(Vec::len).sum();
    if count == 0 {
        return None;
    }
    if runner.runner == Kind::Go {
        let packages: Vec<String> = missing
            .keys()
            .filter_map(|key| step.widen.get(key))
            .flatten()
            .cloned()
            .collect();
        return Some((count, selection::go_argv(&runner.base, &packages)));
    }
    let mut flags = Vec::new();
    let mut packages = BTreeSet::new();
    let mut target_flags = Vec::new();
    for key in missing.keys() {
        if let Some(list) = step.widen.get(key) {
            let mut words = list.iter();
            while let Some(word) = words.next() {
                if word == "-p" {
                    if let Some(name) = words.next() {
                        packages.insert(name.clone());
                    }
                } else if word.starts_with("--") {
                    let mut flag = vec![word.clone()];
                    if word != "--lib"
                        && let Some(value) = words.next()
                    {
                        flag.push(value.clone());
                    }
                    if !target_flags.contains(&flag) {
                        target_flags.push(flag);
                    }
                }
            }
        }
    }
    for package in packages {
        flags.push("-p".into());
        flags.push(package);
    }
    flags.extend(target_flags.into_iter().flatten());
    Some((count, selection::cargo_argv(&runner.base, &flags, &[])))
}

/// Filters of a passing doctest step that matched no doctest it reported.
/// rustdoc can name an item otherwise than the index does, as for one a macro
/// generates; such a step reruns its crates' doctests whole.
fn doc_misses(step: &Step, outcome: &Outcome) -> Vec<String> {
    if step.kind != StepKind::Doctests
        || step.filters.is_empty()
        || outcome.status != 0
        || !outcome.tally.known
    {
        return Vec::new();
    }
    let doc = |key: &String| key.starts_with("doc:") || key == "*";
    let reported: Vec<&String> = outcome
        .tally
        .names
        .iter()
        .filter(|(key, _)| doc(key))
        .flat_map(|(_, names)| names)
        .collect();
    if reported.is_empty() {
        // Quiet output names no test, and filters may match the same one,
        // so only a step that ran no doctest at all is known to have missed.
        let counted: usize = outcome
            .tally
            .counts
            .iter()
            .filter(|(key, _)| doc(key))
            .map(|(_, count)| count)
            .sum();
        return if counted > 0 {
            Vec::new()
        } else {
            step.filters.clone()
        };
    }
    step.filters
        .iter()
        .filter(|filter| !reported.iter().any(|name| name.contains(filter.as_str())))
        .cloned()
        .collect()
}

fn echoed(argv: &[String]) -> String {
    argv.iter()
        .map(|word| quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn execute(workspace: &Workspace, plan: &Plan, options: &Options) -> Result<i32> {
    println!("{}", header(plan, options.affected));
    if let Some(note) = &plan.note {
        println!("{note}");
    }
    let mut status = 0;
    let mut verdicts = Vec::new();
    let mut first_run = None;
    let mut modes = Vec::new();
    for runner in &plan.runners {
        println!("{}", runner_line(runner, options.affected));
        let mut result = RunnerResult::new(runner.mode);
        for step in &runner.steps {
            let outcome = run_step(
                workspace,
                runner,
                result.mode,
                &step.argv,
                &echo(step),
                options,
            )?;
            let kind = serde_json::to_value(step.kind)?
                .as_str()
                .unwrap_or_default()
                .to_owned();
            // Selected names that never reported rerun with their whole target or
            // package. Only a passing step is checked: after a failure cargo stops
            // at the failing binary, and the step has already failed the subset.
            let widen = widening(runner, step, &outcome);
            let missed = doc_misses(step, &outcome);
            // pytest: unknown node IDs (exit 4) widen to their files, and an empty
            // collection (exit 5) means the selection did not match what pytest
            // collects, so the full suite runs. Either way the wider run's status
            // replaces the rejected selection's.
            let pytest = runner.runner == Kind::Pytest && step.kind == StepKind::Filtered;
            let selected = &step.argv[runner.base.len().min(step.argv.len())..];
            let rejected =
                pytest && outcome.status == 4 && selected.iter().any(|id| id.contains("::"));
            let mut collected_none = pytest && outcome.status == 5;
            let not_found = outcome.tally.not_found.len();
            result.record(outcome, &kind, &step.argv, !(rejected || collected_none));
            if let Some((count, argv)) = widen {
                result.note(format!(
                    "widened: {} reported no result under their test names; running their {} in full",
                    plural(count, "selected test"),
                    if runner.runner == Kind::Go {
                        "packages"
                    } else {
                        "targets"
                    }
                ));
                let outcome = run_step(
                    workspace,
                    runner,
                    result.mode,
                    &argv,
                    &echoed(&argv),
                    options,
                )?;
                result.record(outcome, "widened", &argv, true);
            }
            if !missed.is_empty()
                && let Some(flags) = step.widen.get("--doc")
            {
                let argv = selection::cargo_argv(&runner.base, flags, &[]);
                let filters: Vec<String> = missed.iter().map(|filter| quote(filter)).collect();
                result.note(format!(
                    "widened: no doctest matched {}; running the crates' doctests in full",
                    filters.join(", ")
                ));
                let outcome = run_step(
                    workspace,
                    runner,
                    result.mode,
                    &argv,
                    &echoed(&argv),
                    options,
                )?;
                result.record(outcome, "widened", &argv, true);
            }
            if rejected {
                let mut argv = runner.base.clone();
                let files: BTreeSet<String> = selected
                    .iter()
                    .map(|id| id.split("::").next().unwrap_or(id).to_owned())
                    .collect();
                argv.extend(files);
                result.note(if not_found == 0 {
                    "widened: pytest rejected the selection (exit 4); running the selected files"
                        .to_owned()
                } else {
                    format!(
                        "widened: pytest did not find {}; running their files",
                        plural(not_found, "node ID")
                    )
                });
                let outcome = run_step(
                    workspace,
                    runner,
                    result.mode,
                    &argv,
                    &echoed(&argv),
                    options,
                )?;
                collected_none = outcome.status == 5;
                result.record(outcome, "widened", &argv, !collected_none);
            }
            if collected_none {
                result.note(
                    "runner-mismatch: pytest collected none of the selected tests; running the full suite"
                        .to_owned(),
                );
                result.mode = Mode::Full;
                result.reasons.push(Reason {
                    code: "runner-mismatch",
                    message: "pytest collected none of the selected tests".into(),
                });
                let argv = runner.base.clone();
                let outcome = run_step(
                    workspace,
                    runner,
                    Mode::Full,
                    &argv,
                    &echoed(&argv),
                    options,
                )?;
                result.record(outcome, "full", &argv, true);
            }
        }
        if status == 0 && result.status != 0 {
            status = result.status;
        }
        if first_run.is_none() {
            first_run = result.runs.first().cloned();
        }
        // Archive the plan and verdict with every run this runner made.
        let verdict = verdict(runner, &result, options.affected);
        let archived = archive(workspace, plan, runner, &result, &verdict);
        for id in &result.runs {
            if let Ok((directory, _)) = capture::load(workspace, id)
                && let Err(error) = write_json(&directory.join("selection.json"), &archived)
            {
                eprintln!("justless: could not record the selection for {id}: {error:#}");
            }
        }
        verdicts.push(verdict);
        modes.push(result.mode);
    }
    let mut text = String::new();
    for verdict in &verdicts {
        text.push_str(verdict);
        text.push('\n');
    }
    let subset = modes
        .iter()
        .any(|mode| matches!(mode, Mode::Subset | Mode::LowConfidence | Mode::BuildOnly));
    if subset {
        text.push_str(&format!("Full suite: {}", options.full_suite));
        if let Some(id) = &first_run {
            text.push_str(&format!(" · Why: {} test --why {id}", options.justless));
        }
        text.push('\n');
    } else if modes.iter().all(|mode| *mode == Mode::Nothing) {
        text.push_str(&format!("Full suite: {}\n", options.full_suite));
    }
    eprint!("{text}");
    Ok(status)
}

fn seconds(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

/// Fallback codes, from the plan and from the run.
fn all_codes(plan: &RunnerPlan, result: &RunnerResult) -> String {
    let mut codes: Vec<&str> = plan
        .reasons
        .iter()
        .chain(&result.reasons)
        .map(|reason| reason.code)
        .collect();
    codes.dedup();
    codes.join(", ")
}

/// The closing lines for one runner. A subset says so; it never claims the suite passed.
fn verdict(plan: &RunnerPlan, result: &RunnerResult, affected: bool) -> String {
    let who = plan.runner.label();
    let time = seconds(result.elapsed_ms);
    let status = result.status;
    let failed = result.failed.len();
    // What ran: tests, or for Go without names only the packages.
    let ran = if result.ran == 0 && result.packages > 0 {
        format!("tests in {}", plural(result.packages, "package"))
    } else {
        plural(result.ran, "test")
    };
    let mut lines = Vec::new();
    if let Some(why) = &result.unavailable {
        lines.push(format!(
            "Runner not available ({who}): {why}; exit {status}. Its tests did not run; this is not a test result ({time})."
        ));
        for note in &result.notes {
            lines.push(format!("  {note}"));
        }
        return lines.join("\n");
    }
    match result.mode {
        Mode::Nothing => {
            lines.push(format!("Nothing ran for {who}; this is not a test result."));
        }
        Mode::BuildOnly => {
            let built: Vec<&str> = plan
                .steps
                .iter()
                .flat_map(|step| step.built.iter().map(String::as_str))
                .collect();
            let built = if built.is_empty() {
                "the changed targets".to_owned()
            } else {
                built.join(", ")
            };
            lines.push(if status == 0 {
                format!("Build only ({who}): {built} compiled ({time}). No tests ran; this is not a test result.")
            } else {
                format!("Build only ({who}) failed with exit {status} building {built} ({time}). No tests ran.")
            });
        }
        Mode::Subset | Mode::LowConfidence => {
            let label = if result.mode == Mode::Subset {
                "Subset".to_owned()
            } else {
                format!("Low-confidence subset ({})", all_codes(plan, result))
            };
            let not_run = plan.indexed.saturating_sub(plan.selected);
            let scope = format!(
                "{} of {} indexed {} selected, {not_run} not run",
                plan.selected,
                plan.indexed,
                units(plan.indexed, plan.unit),
            );
            if !result.known {
                lines.push(format!(
                    "{label} ({who}): exit {status}; Justless could not count the tests that ran ({scope}); the full suite was not run ({time})."
                ));
            } else if status == 0 && failed == 0 {
                lines.push(format!(
                    "{label} passed ({who}): ran {ran} reachable from your changes ({scope}); the full suite was not run ({time})."
                ));
            } else if failed == 0 {
                lines.push(format!(
                    "{label} failed ({who}): exit {status} with no failing test reported, such as a build error; ran {ran} ({scope}); the full suite was not run ({time})."
                ));
            } else {
                lines.push(format!(
                    "{label} failed ({who}): {failed} of {ran} that ran failed, exit {status} ({scope}); the full suite was not run ({time})."
                ));
                let selected = |name: &str| {
                    // Doctests report as `src/tax.rs - tax::vat (line 3)`.
                    let doctest = name
                        .split_once(" - ")
                        .and_then(|(_, item)| item.rsplit_once(" (line "))
                        .map(|(item, _)| format!("{item} (doctest)"));
                    plan.tests.iter().find(|test| {
                        test.id == name
                            || doctest.as_deref() == Some(test.id.as_str())
                            || name.ends_with(&format!("::{}", test.id))
                            || test.id.ends_with(&format!("::{name}"))
                            // Go names a suite method `TestCartSuite/TestCountTwo`.
                            || (plan.runner == Kind::Go
                                && name.ends_with(&format!("/{}", test.id)))
                    })
                };
                // Failures with the most direct evidence first; runners report in
                // parallel order, so sort for a stable verdict.
                let rank = |name: &str| match selected(name).map(|test| test.confidence) {
                    Some("exact" | "changed-test") => 0,
                    Some("possible" | "mention") => 1,
                    Some(_) => 2,
                    None => 3,
                };
                let mut failing: Vec<&String> = result.failed.iter().collect();
                failing.sort_by(|a, b| (rank(a), *a).cmp(&(rank(b), *b)));
                for name in failing.into_iter().take(EVIDENCE) {
                    let evidence = selected(name)
                        .map(|test| {
                            // The first link says how it was chosen; --why has the chain.
                            let first = test.why.split(" · ").next().unwrap_or(&test.why);
                            format!("  {name} · {} · {first}", test.confidence)
                        })
                        .unwrap_or_else(|| format!("  {name}"));
                    let end = evidence.floor_char_boundary(evidence.len().min(EVIDENCE_BYTES));
                    lines.push(evidence[..end].to_owned());
                }
                if failed > EVIDENCE {
                    lines.push(format!("  … {} more in the run logs", failed - EVIDENCE));
                }
            }
        }
        Mode::Full | Mode::Command => {
            let label = match result.mode {
                Mode::Command => "Command as given".to_owned(),
                _ if affected && (!plan.reasons.is_empty() || !result.reasons.is_empty()) => {
                    format!(
                        "Full suite (fallback from --affected: {})",
                        all_codes(plan, result)
                    )
                }
                _ => "Full suite".to_owned(),
            };
            let counted = if plan.runner == Kind::Go && result.packages > 0 {
                format!(
                    "{} reported, {} replayed from Go's test cache, {failed} failing tests",
                    plural(result.packages, "package"),
                    result.cached
                )
            } else if result.known {
                format!("{ran} ran, {failed} failed")
            } else {
                "Justless could not count the tests that ran".to_owned()
            };
            lines.push(format!(
                "{label} ({who}): exit {status}, {counted} ({time})."
            ));
        }
    }
    for note in &result.notes {
        lines.push(format!("  {note}"));
    }
    lines.join("\n")
}

fn archive(
    workspace: &Workspace,
    plan: &Plan,
    runner: &RunnerPlan,
    result: &RunnerResult,
    verdict: &str,
) -> Value {
    let digests: BTreeMap<&str, String> = plan
        .changes
        .iter()
        .filter_map(|note| {
            let path = workspace.source_path(&note.subject).ok()?;
            Some((
                note.subject.as_str(),
                capture::source_digest(&fs::read(path).ok()?),
            ))
        })
        .collect();
    let mut value = json!({
        "version": 1,
        "base": plan.base,
        "changes": plan.changes,
        "seeds": plan.seeds,
        "ignored": plan.ignored,
        "runner": runner,
        "digests": digests,
        "steps": result.steps,
        "verdict": {
            "status": result.status,
            "ran": result.ran,
            "failed": result.failed,
            "doctests": result.doctests,
            "elapsed_ms": result.elapsed_ms,
            "shown_bytes": result.shown,
            "raw_bytes": result.raw,
            "text": verdict,
        },
    });
    // A fallback found while running is what the runner ran as.
    if result.mode != runner.mode {
        value["runner"]["mode"] = json!(result.mode);
    }
    if let Some(reasons) = value["runner"]["reasons"].as_array_mut() {
        reasons.extend(result.reasons.iter().map(|reason| json!(reason)));
    }
    // Keep the record bounded: drop per-test evidence past the cap, counting it.
    let size = serde_json::to_string(&value).map_or(0, |text| text.len());
    if size > RECORD_BYTES
        && let Some(tests) = value["runner"]["tests"].as_array_mut()
    {
        let total = tests.len();
        let keep = total * RECORD_BYTES / size.max(1) / 2;
        tests.truncate(keep);
        value["runner"]["tests_omitted"] = json!(total - keep);
    }
    value
}

/// `--why RUN_ID`: the archived selection, with stale source marked.
fn why(workspace: &Workspace, id: &str, filter: Option<&str>, budget: usize) -> Result<()> {
    let (directory, run) = capture::load(workspace, id)?;
    let path = directory.join("selection.json");
    if !path.is_file() {
        bail!("{id} was not run by justless test; it has no selection record");
    }
    let record: Value =
        serde_json::from_slice(&fs::read(&path)?).context("read selection record")?;
    let runner = &record["runner"];
    let mut lines = Vec::new();
    lines.push(format!(
        "{id} · {} · exit {} · {} · {}",
        run.family,
        run.status,
        runner["mode"].as_str().unwrap_or_default(),
        record["base"].as_str().unwrap_or_default()
    ));
    if let Some(text) = record["verdict"]["text"].as_str() {
        lines.extend(text.lines().map(str::to_owned));
    }
    for reason in runner["reasons"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  {}: {}",
            reason["code"].as_str().unwrap_or_default(),
            reason["message"].as_str().unwrap_or_default()
        ));
    }
    let digests = record["digests"].as_object();
    for change in record["changes"].as_array().into_iter().flatten() {
        let file = change["subject"].as_str().unwrap_or_default();
        let stale = digests
            .and_then(|map| map.get(file))
            .and_then(Value::as_str)
            .is_some_and(|old| {
                workspace
                    .source_path(file)
                    .ok()
                    .and_then(|path| fs::read(path).ok())
                    .is_none_or(|bytes| capture::source_digest(&bytes) != old)
            });
        lines.push(format!(
            "Changed {file} · {}{}",
            change["detail"].as_str().unwrap_or_default(),
            if stale { " [source changed]" } else { "" }
        ));
    }
    for seed in record["seeds"].as_array().into_iter().flatten() {
        lines.push(format!(
            "Start {} · {}",
            seed["subject"].as_str().unwrap_or_default(),
            seed["detail"].as_str().unwrap_or_default()
        ));
    }
    for step in record["steps"].as_array().into_iter().flatten() {
        let argv: Vec<String> = step["argv"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(quote)
            .collect();
        lines.push(format!(
            "Step {} · {} · exit {} · {} ran · {}",
            step["kind"].as_str().unwrap_or_default(),
            step["run"].as_str().unwrap_or("not recorded"),
            step["status"],
            step["ran"],
            argv.join(" ")
        ));
    }
    for test in runner["tests"].as_array().into_iter().flatten() {
        let name = test["id"].as_str().unwrap_or_default();
        if filter.is_some_and(|filter| !name.contains(filter)) {
            continue;
        }
        lines.push(format!(
            "  {name} · {} · {} · {}",
            test["target"].as_str().unwrap_or_default(),
            test["confidence"].as_str().unwrap_or_default(),
            test["why"].as_str().unwrap_or_default()
        ));
    }
    for note in runner["skipped"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  {} · {}",
            note["subject"].as_str().unwrap_or_default(),
            note["detail"].as_str().unwrap_or_default()
        ));
    }
    let verdict = &record["verdict"];
    if let (Some(shown), Some(raw)) = (
        verdict["shown_bytes"].as_u64(),
        verdict["raw_bytes"].as_u64(),
    ) {
        lines.push(format!(
            "Cost: {} · {shown} of {raw} output bytes shown",
            seconds(verdict["elapsed_ms"].as_u64().unwrap_or(0))
        ));
    }
    let mut used = 0;
    let mut shown = 0;
    for line in &lines {
        if used + line.len() + 1 > budget {
            break;
        }
        used += line.len() + 1;
        shown += 1;
        println!("{line}");
    }
    if shown < lines.len() {
        println!(
            "… {} not shown; raise --budget or narrow with --test",
            plural(lines.len() - shown, "line")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered(names: &[(&str, &[&str])], argv: &[&str]) -> Step {
        Step {
            kind: StepKind::Filtered,
            argv: argv.iter().map(|word| (*word).to_owned()).collect(),
            names: names
                .iter()
                .map(|(target, list)| {
                    (
                        (*target).to_owned(),
                        list.iter().map(|name| (*name).to_owned()).collect(),
                    )
                })
                .collect(),
            widen: BTreeMap::new(),
            built: Vec::new(),
            filters: Vec::new(),
        }
    }

    #[test]
    fn names_a_passing_step_never_reported_are_missing_from_their_target() {
        let step = filtered(
            &[
                ("ledger_core", &["tax::tests::a", "tax::tests::gone"]),
                ("totals", &["case"]),
            ],
            &[],
        );
        let stdout = "\nrunning 1 test\ntest tax::tests::a ... ok\n\ntest result: ok.\n\nrunning 1 test\ntest case::one ... ok\n\ntest result: ok.\n";
        let stderr = "     Running unittests src/lib.rs (target/debug/deps/ledger_core-1)\n     Running tests/totals.rs (target/debug/deps/totals-2)\n";
        let missing = missing_names(&step, &runners::tally(Kind::Cargo, stdout, stderr));
        assert_eq!(
            missing,
            BTreeMap::from([(
                "ledger_core".to_owned(),
                vec!["tax::tests::gone".to_owned()]
            )])
        );
    }

    #[test]
    fn quiet_output_and_go_packages_are_checked_by_what_they_report() {
        // `cargo test -q`: no names and no `Running` lines, only summaries.
        let step = filtered(&[("reports", &["stats::tests::a", "stats::tests::b"])], &[]);
        let quiet = |passed: usize| {
            runners::tally(
                Kind::Cargo,
                &format!(
                    "\nrunning {passed} tests\n..\ntest result: ok. {passed} passed; 0 failed; 0 ignored; 0 measured; 8 filtered out\n"
                ),
                "",
            )
        };
        assert!(missing_names(&step, &quiet(2)).is_empty());
        assert_eq!(missing_names(&step, &quiet(1)).len(), 1);
        // Go: a package where `-run` matched nothing names no test.
        let step = filtered(
            &[("./price", &["TestVat"]), ("./cart", &["TestCount"])],
            &[],
        );
        let tally = runners::tally(
            Kind::Go,
            "=== RUN   TestVat\n--- PASS: TestVat (0.00s)\nPASS\nok  \texample.com/shop/price\t0.01s\nok  \texample.com/shop/cart\t0.01s [no tests to run]\n",
            "",
        );
        assert_eq!(
            missing_names(&step, &tally),
            BTreeMap::from([("./cart".to_owned(), vec!["TestCount".to_owned()])])
        );
    }

    #[test]
    fn echoed_commands_count_test_names_instead_of_listing_them() {
        let step = filtered(
            &[("api", &["tests::a", "tests::b"])],
            &[
                "cargo", "test", "-p", "api", "--lib", "--", "tests::a", "tests::b", "--exact",
            ],
        );
        assert_eq!(
            echo(&step),
            "cargo test -p api --lib -- <2 test names> --exact"
        );
    }

    #[test]
    fn a_doctest_filter_that_matched_nothing_is_a_miss() {
        let step = Step {
            kind: StepKind::Doctests,
            argv: Vec::new(),
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
            built: Vec::new(),
            filters: vec!["::total (line ".to_owned(), "price".to_owned()],
        };
        let outcome = |stdout: &str| Outcome {
            status: 0,
            run: None,
            tally: runners::tally(Kind::Cargo, stdout, "   Doc-tests probe\n"),
            elapsed_ms: 0,
            shown: 0,
            raw: 0,
            unavailable: None,
        };
        let both = "\nrunning 2 tests\ntest src/lib.rs - Shop<T>::total (line 4) ... ok\ntest src/lib.rs - price (line 9) ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out\n";
        assert!(doc_misses(&step, &outcome(both)).is_empty());
        let one = "\nrunning 1 test\ntest src/lib.rs - price (line 9) ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out\n";
        assert_eq!(doc_misses(&step, &outcome(one)), ["::total (line "]);
        // Quiet output only counts, and two filters can match one doctest:
        // only a step that ran none is a miss.
        let quiet = "\nrunning 1 test\n.\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out\n";
        assert!(doc_misses(&step, &outcome(quiet)).is_empty());
        let none = "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 5 filtered out\n";
        assert_eq!(doc_misses(&step, &outcome(none)).len(), 2);
    }
}
