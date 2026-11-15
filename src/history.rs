use std::collections::BTreeMap;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::capture::{self, Run, Stream};
use crate::context::Report;
use crate::workspace::{Workspace, lock_state, write_json};

pub fn now_ms() -> u64 {
    millis(SystemTime::now())
}

/// Programs whose second word names a subcommand rather than an argument.
const SUBCOMMANDS: &[&str] = &[
    "bazel",
    "brew",
    "bun",
    "bundle",
    "cabal",
    "cargo",
    "composer",
    "conda",
    "dart",
    "deno",
    "docker",
    "dotnet",
    "flutter",
    "gh",
    "git",
    "go",
    "golangci-lint",
    "gradle",
    "gradlew",
    "hatch",
    "helm",
    "justless",
    "kubectl",
    "make",
    "mix",
    "mvn",
    "mvnw",
    "nix",
    "npm",
    "pdm",
    "pip",
    "pip3",
    "pixi",
    "pnpm",
    "podman",
    "poetry",
    "rustup",
    "rye",
    "ruff",
    "stack",
    "swift",
    "terraform",
    "tofu",
    "uv",
    "vitest",
    "yarn",
    "zig",
];

/// A command label without arguments, such as `cargo test`, `make` or `mysql`.
/// Only programs known to take subcommands keep their second word, so host,
/// database and file names never appear in a label.
pub fn label(words: &[&str]) -> String {
    let words = crate::filters::unwrap_tool(words);
    let Some((program, rest)) = words.split_first() else {
        return "other".into();
    };
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if program.is_empty()
        || program.starts_with(['-', '.'])
        || !program
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
    {
        return "other".into();
    }
    match rest.first() {
        Some(sub)
            if SUBCOMMANDS.contains(&program)
                && sub.starts_with(|c: char| c.is_ascii_lowercase())
                && sub.chars().all(|c| c.is_ascii_lowercase() || c == '-') =>
        {
            format!("{program} {sub}")
        }
        _ => program.to_owned(),
    }
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn age(ms: u64) -> String {
    let seconds = now_ms().saturating_sub(ms) / 1000;
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn skipped(id: &str, error: &anyhow::Error) {
    eprintln!("justless: skipped run {id}: {error:#}");
}

/// Recorded runs ordered by completion time, newest first unless `oldest`.
/// A record that cannot be read is skipped with a warning naming its run, so
/// the others stay readable. The iterator never yields an error.
pub fn records(
    workspace: &Workspace,
    oldest: bool,
) -> Result<impl Iterator<Item = Result<Run>> + '_> {
    /// The ordering field, read without decoding the whole record.
    #[derive(Deserialize)]
    struct Stamp {
        #[serde(default)]
        completed_ms: u64,
    }
    let path = workspace.state.join("runs");
    let mut entries = Vec::new();
    if path.exists() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let directory = entry.path();
            let record = directory.join("record.json");
            let stamp = match fs::read(&record) {
                Ok(bytes) => serde_json::from_slice::<Stamp>(&bytes).context("decode run record"),
                // A run that Justless could not finish has streams but may lack a
                // record. It sorts by the last write to its output.
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    if let Ok(modified) =
                        fs::metadata(directory.join("stdout")).and_then(|meta| meta.modified())
                    {
                        entries.push((millis(modified), id));
                    }
                    continue;
                }
                Err(e) => Err(e).context("read run record"),
            };
            // Records stored before completion times existed sort by modification time.
            let completed = match stamp {
                Ok(stamp) if stamp.completed_ms > 0 => stamp.completed_ms,
                Ok(_) => fs::metadata(&record)
                    .and_then(|meta| meta.modified())
                    .map_or(0, millis),
                Err(error) => {
                    skipped(&id, &error);
                    continue;
                }
            };
            entries.push((completed, id));
        }
    }
    entries.sort_unstable();
    if !oldest {
        entries.reverse();
    }
    Ok(entries
        .into_iter()
        .filter_map(|(completed, id)| match capture::load(workspace, &id) {
            Ok((_, mut run)) => {
                if run.completed_ms == 0 {
                    run.completed_ms = completed;
                }
                if run.family.is_empty() {
                    run.family = "legacy".into();
                }
                Some(Ok(run))
            }
            Err(error) => {
                skipped(&id, &error);
                None
            }
        }))
}

/// A later request that printed part of a run's output again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retrieval {
    /// `log` replaying whole streams.
    Full,
    /// `log` with a line range.
    Range,
    /// `search` within the run.
    Search,
    /// `context --run`.
    Context,
    /// Lines of the run that `history TEXT`, `lookup --logs` or `pack` printed.
    Excerpt,
    /// A replay with `--unredacted`, counted on top of its full, range or search
    /// request, which carries the bytes.
    Unredacted,
}

/// How often a run's output was retrieved after its preview, in `recalls.json`
/// beside the run. Records written before bytes were counted hold only the counts.
#[derive(Default, Serialize, Deserialize)]
struct Recalls {
    #[serde(default)]
    full: u64,
    #[serde(default)]
    range: u64,
    #[serde(default)]
    search: u64,
    #[serde(default)]
    context: u64,
    /// Replays with `--unredacted`, counted on top of the full, range or search call.
    #[serde(default)]
    unredacted: u64,
    #[serde(default)]
    excerpts: u64,
    /// The bytes printed by the retrievals that `measured` counts.
    #[serde(default)]
    bytes: u64,
    /// Retrievals whose bytes are in `bytes`. The rest of each count was made
    /// before bytes were counted, or by a caller that failed to record them.
    #[serde(default)]
    measured: Measured,
}

#[derive(Default, Serialize, Deserialize)]
struct Measured {
    #[serde(default)]
    full: u64,
    #[serde(default)]
    range: u64,
    #[serde(default)]
    search: u64,
    #[serde(default)]
    context: u64,
    #[serde(default)]
    excerpts: u64,
}

impl Recalls {
    fn count(&mut self, retrieval: Retrieval) {
        match retrieval {
            Retrieval::Full => self.full += 1,
            Retrieval::Range => self.range += 1,
            Retrieval::Search => self.search += 1,
            Retrieval::Context => self.context += 1,
            Retrieval::Excerpt => self.excerpts += 1,
            Retrieval::Unredacted => self.unredacted += 1,
        }
    }

    fn measure(&mut self, retrieval: Retrieval, bytes: u64) {
        let measured = &mut self.measured;
        match retrieval {
            Retrieval::Full => measured.full += 1,
            Retrieval::Range => measured.range += 1,
            Retrieval::Search => measured.search += 1,
            Retrieval::Context => measured.context += 1,
            Retrieval::Excerpt => measured.excerpts += 1,
            Retrieval::Unredacted => return,
        }
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn replayed(&self, directory: &Path) -> Replayed {
        let measured = &self.measured;
        let full = self.full.saturating_sub(measured.full);
        // A whole replay prints both archived streams, so their size bounds it.
        let archive = || {
            ["stdout", "stderr"]
                .iter()
                .filter_map(|stream| fs::metadata(directory.join(stream)).ok())
                .map(|meta| meta.len())
                .sum::<u64>()
        };
        Replayed {
            measured: self.bytes,
            estimated: if full > 0 {
                full.saturating_mul(archive())
            } else {
                0
            },
            estimated_replays: full,
            unmeasured: self.range.saturating_sub(measured.range)
                + self.search.saturating_sub(measured.search)
                + self.context.saturating_sub(measured.context)
                + self.excerpts.saturating_sub(measured.excerpts),
        }
    }
}

/// What later retrievals of a run printed after its preview.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Replayed {
    /// Bytes measured as they were printed.
    pub measured: u64,
    /// Whole replays recorded without their size, each estimated as the size of
    /// the run's archived streams.
    pub estimated: u64,
    pub estimated_replays: u64,
    /// Range, search, context and excerpt requests recorded without their size.
    /// Nothing bounds them, so they are counted but not subtracted.
    pub unmeasured: u64,
}

impl Replayed {
    /// Every byte subtracted from the run's savings: measured and estimated.
    pub fn bytes(&self) -> u64 {
        self.measured.saturating_add(self.estimated)
    }

    fn add(&mut self, other: Replayed) {
        self.measured += other.measured;
        self.estimated += other.estimated;
        self.estimated_replays += other.estimated_replays;
        self.unmeasured += other.unmeasured;
    }
}

/// What later retrievals of the run in `directory` printed. A damaged
/// `recalls.json` counts as no retrieval, with a warning unless `quiet`.
pub fn replayed(directory: &Path, quiet: bool) -> Replayed {
    let path = directory.join("recalls.json");
    let recalls: Recalls = if quiet {
        get(&path).unwrap_or_default()
    } else {
        get_or_warn(&path)
    };
    recalls.replayed(directory)
}

/// Savings net of later retrievals: raw output minus what the agent received at
/// first and every byte it retrieved afterwards. Negative when the retrievals
/// cost more than the preview saved.
pub fn net_bytes(raw: u64, shown: u64, replayed: u64) -> i64 {
    let signed = |bytes: u64| i64::try_from(bytes).unwrap_or(i64::MAX);
    signed(raw)
        .saturating_sub(signed(shown))
        .saturating_sub(signed(replayed))
}

#[derive(Default, Serialize, Deserialize)]
struct Activity {
    bypasses: BTreeMap<String, u64>,
    reads: Vec<Read>,
    repeat_reads: u64,
    #[serde(default)]
    last_pack: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct Read {
    file: String,
    digest: String,
    from: usize,
    to: usize,
    /// The file's outline rather than its lines, with `from` and `to` 0.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    outline: bool,
}

fn get<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

/// Reads state for a report, warning about a damaged file instead of failing.
fn get_or_warn<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    get(path).unwrap_or_else(|error| {
        eprintln!("justless: ignored unreadable {}: {error:#}", path.display());
        T::default()
    })
}

fn update<T: serde::de::DeserializeOwned + Serialize + Default>(
    path: &Path,
    f: impl FnOnce(&mut T),
) -> Result<()> {
    let _guard = lock_state(&path.with_extension("lock"))?;
    let mut value = get::<T>(path)?;
    f(&mut value);
    write_json(path, &value)
}

/// Records a retrieval of the run in `directory` and the bytes it printed.
/// `bytes` is `None` when the caller that shows the result records its size
/// with [`measured`], as the MCP server does for a `log` page. Until then the
/// retrieval counts as unmeasured, and a whole replay is estimated.
pub fn recall(directory: &Path, retrieval: Retrieval, bytes: Option<u64>) {
    let _ = update(&directory.join("recalls.json"), |r: &mut Recalls| {
        r.count(retrieval);
        if let Some(bytes) = bytes {
            r.measure(retrieval, bytes);
        }
    });
}

/// Records the bytes of a retrieval that [`recall`] counted without them.
pub fn measured(directory: &Path, retrieval: Retrieval, bytes: u64) {
    let _ = update(&directory.join("recalls.json"), |r: &mut Recalls| {
        r.measure(retrieval, bytes);
    });
}

/// Records the bytes that excerpts of several runs printed, such as the
/// matching lines of `history TEXT`, as one retrieval of each run.
pub fn excerpts(workspace: &Workspace, printed: &BTreeMap<String, u64>) {
    for (id, &bytes) in printed {
        if let Ok((directory, _)) = capture::load(workspace, id) {
            recall(&directory, Retrieval::Excerpt, Some(bytes));
        }
    }
}

/// The archived tool results that `justless recall` printed, one JSON line each
/// with its time and size. They came from the agent's client, not from a run.
const ARCHIVE_RECALLS: &str = "archive-recalls.jsonl";

#[derive(Serialize, Deserialize)]
struct ArchiveRecall {
    ms: u64,
    bytes: u64,
}

/// Records that `justless recall` printed `bytes` of an archived tool result.
pub fn recalled_archive(workspace: &Workspace, bytes: u64) {
    if workspace.prepare_state().is_err() {
        return;
    }
    let Ok(mut line) = serde_json::to_vec(&ArchiveRecall {
        ms: now_ms(),
        bytes,
    }) else {
        return;
    };
    line.push(b'\n');
    crate::statusline::append(&workspace.state.join(ARCHIVE_RECALLS), &line);
}

/// How many archived tool results `justless recall` printed since `since_ms`,
/// and their bytes.
pub fn archive_recalls(state: &Path, since_ms: u64) -> (u64, u64) {
    let Ok(text) = fs::read_to_string(state.join(ARCHIVE_RECALLS)) else {
        return (0, 0);
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<ArchiveRecall>(line).ok())
        .filter(|recall| recall.ms >= since_ms)
        .fold((0, 0), |(count, bytes), recall| {
            (count + 1, bytes + recall.bytes)
        })
}

/// Records that the agent was shown lines `from..=to` of the workspace file
/// `file`, whose bytes have `digest` ([`capture::source_digest`]), for `resume`,
/// the compaction handoff and their `[source changed]` notices. Each command
/// that prints source lines calls it once per range it printed. The same range
/// of unchanged source shown again counts as a repeated read in `stats`.
pub fn saw_source(workspace: &Workspace, file: &str, from: usize, to: usize, digest: String) {
    saw(workspace, file, from, to, digest, false);
}

/// Records that the agent was shown `file`'s outline: its revision, rows and
/// line ranges, but not its lines. A file keeps one outline entry whatever the
/// page, and it never counts as a repeated read.
pub fn saw_outline(workspace: &Workspace, file: &str, digest: String) {
    saw(workspace, file, 0, 0, digest, true);
}

fn saw(workspace: &Workspace, file: &str, from: usize, to: usize, digest: String, outline: bool) {
    if workspace.prepare_state().is_err() {
        return;
    }
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| {
            if let Some(i) = a.reads.iter().position(|r| {
                r.file == file && r.from == from && r.to == to && r.outline == outline
            }) {
                let old = a.reads.remove(i);
                if old.digest == digest && !outline {
                    a.repeat_reads += 1;
                }
            }
            a.reads.insert(
                0,
                Read {
                    file: file.into(),
                    digest,
                    from,
                    to,
                    outline,
                },
            );
            a.reads.truncate(32);
        },
    );
}

pub fn saw_pack(workspace: &Workspace, files: BTreeMap<String, String>) {
    if !files.is_empty() {
        let _ = update(
            &workspace.state.join("activity.json"),
            |a: &mut Activity| a.last_pack = files,
        );
    }
}

pub fn bypass(workspace: &Workspace, command: &str) {
    if !workspace.state.is_dir() {
        return;
    }
    let Some(word) = command.split_whitespace().next() else {
        return;
    };
    let family = match word {
        "justless" => return,
        "git" | "cargo" | "pytest" | "go" | "npm" | "rg" | "python" | "python3" | "cat" | "ls"
        | "sed" | "grep" | "find" | "pnpm" | "yarn" | "make" => word,
        _ => "other",
    };
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| *a.bypasses.entry(family.to_owned()).or_default() += 1,
    );
}

/// Byte totals for a group of runs. `preview_bytes` is what the agent received
/// at first, footers included. Later retrievals are kept apart from it.
#[derive(Default, Serialize)]
struct Totals {
    raw_bytes: u64,
    preview_bytes: u64,
    /// Bytes that later retrievals printed, as measured.
    replayed_bytes: u64,
    /// Whole replays recorded without their size, estimated from the archive.
    estimated_replay_bytes: u64,
    /// Raw output minus the preview and every measured or estimated retrieval.
    net_saved_bytes: i64,
}

impl Totals {
    fn add(&mut self, raw: u64, shown: u64, replayed: Replayed) {
        self.raw_bytes += raw;
        self.preview_bytes += shown;
        self.replayed_bytes += replayed.measured;
        self.estimated_replay_bytes += replayed.estimated;
        self.net_saved_bytes =
            self.net_saved_bytes
                .saturating_add(net_bytes(raw, shown, replayed.bytes()));
    }

    /// Savings at first preview, before any retrieval.
    fn initial(&self) -> u64 {
        self.raw_bytes.saturating_sub(self.preview_bytes)
    }
}

#[derive(Default, Serialize)]
struct FamilyStats {
    family: String,
    runs: u64,
    #[serde(flatten)]
    totals: Totals,
    reopened_runs: u64,
    full_reads: u64,
    targeted_lookups: u64,
    /// Lines of these runs that `history TEXT`, `lookup --logs` or `pack` printed.
    excerpts: u64,
    unchanged_runs: u64,
    /// Runs whose printed preview and footer were larger than the raw output.
    grown_runs: u64,
}

#[derive(Default, Serialize)]
struct DayStats {
    date: String,
    runs: u64,
    #[serde(flatten)]
    totals: Totals,
}

/// Agents and providers tokenize differently. Like the benchmarks, the report
/// estimates tokens as UTF-8 bytes divided by four and labels the figure as such.
pub fn estimated_tokens(bytes: u64) -> u64 {
    bytes.div_ceil(4)
}

/// [`estimated_tokens`] for net savings, which can be negative.
fn estimated_tokens_signed(bytes: i64) -> i64 {
    let tokens = i64::try_from(estimated_tokens(bytes.unsigned_abs())).unwrap_or(i64::MAX);
    if bytes < 0 { -tokens } else { tokens }
}

/// [`human_bytes`] for net savings, which can be negative.
fn signed_bytes(bytes: i64) -> String {
    let size = human_bytes(bytes.unsigned_abs());
    if bytes < 0 { format!("-{size}") } else { size }
}

fn signed_percent(part: i64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

fn rounded(percent: f64) -> f64 {
    (percent * 10.0).round() / 10.0
}

pub fn human_bytes(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// The UTC calendar date for a Unix timestamp in milliseconds.
pub fn utc_date(ms: u64) -> String {
    // Civil-from-days conversion for the proleptic Gregorian calendar.
    let z = (ms / 86_400_000) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn stats(workspace: &Workspace, days: u64, as_json: bool) -> Result<()> {
    let since = now_ms().saturating_sub(days.saturating_mul(86_400_000));
    let mut count = 0u64;
    let mut totals = Totals::default();
    let mut replayed = Replayed::default();
    let (mut secrets, mut unredacted) = (0u64, 0u64);
    let mut families: BTreeMap<String, FamilyStats> = BTreeMap::new();
    let mut timeline: BTreeMap<String, DayStats> = BTreeMap::new();
    for run in records(workspace, false)? {
        let run = run?;
        // Count everything the agent received, including the footer.
        let shown = (run.preview_bytes + run.footer_bytes) as u64;
        count += 1;
        secrets += run.redaction.as_ref().map_or(0, |found| found.count);
        let directory = workspace.state.join("runs").join(&run.id);
        let c: Recalls = get_or_warn(&directory.join("recalls.json"));
        let later = c.replayed(&directory);
        totals.add(run.raw_bytes, shown, later);
        replayed.add(later);
        unredacted += c.unredacted;
        let f = families.entry(run.family.clone()).or_default();
        f.family = run.family;
        f.runs += 1;
        f.totals.add(run.raw_bytes, shown, later);
        f.reopened_runs += u64::from(c.full > 0);
        f.full_reads += c.full;
        f.targeted_lookups += c.range + c.search + c.context;
        f.excerpts += c.excerpts;
        f.unchanged_runs += u64::from(run.unchanged);
        f.grown_runs += u64::from(shown > run.raw_bytes);
        // Retrievals are not timed, so they count on the day their run completed.
        if run.completed_ms >= since && run.completed_ms > 0 {
            let date = utc_date(run.completed_ms);
            let d = timeline.entry(date.clone()).or_default();
            d.date = date;
            d.runs += 1;
            d.totals.add(run.raw_bytes, shown, later);
        }
    }
    let activity: Activity = get_or_warn(&workspace.state.join("activity.json"));
    let proxy = crate::proxy::report(workspace, since)?;
    let (raw, preview, saved, net) = (
        totals.raw_bytes,
        totals.preview_bytes,
        totals.initial(),
        totals.net_saved_bytes,
    );
    let (recalls, recalled_bytes) = archive_recalls(&workspace.state, since);
    let mut commands: Vec<_> = families.into_values().collect();
    commands.sort_by(|a, b| {
        b.totals
            .net_saved_bytes
            .cmp(&a.totals.net_saved_bytes)
            .then(a.family.cmp(&b.family))
    });
    if as_json {
        // The keys from before retrievals were measured keep their meaning:
        // `saved_*` and `estimated_tokens_saved` are savings at first preview.
        let report = serde_json::json!({
            "runs": count,
            "raw_bytes": raw,
            "preview_bytes": preview,
            "saved_bytes": saved,
            "saved_percent": rounded(percent(saved, raw)),
            "estimated_tokens_saved": estimated_tokens(saved),
            "replayed_bytes": replayed.measured,
            "estimated_replay_bytes": replayed.estimated,
            "estimated_replays": replayed.estimated_replays,
            "unmeasured_retrievals": replayed.unmeasured,
            "net_saved_bytes": net,
            "net_saved_percent": rounded(signed_percent(net, raw)),
            "estimated_tokens_net_saved": estimated_tokens_signed(net),
            "token_estimate": "UTF-8 bytes / 4",
            "days": timeline.into_values().collect::<Vec<_>>(),
            "commands": commands,
            "repeated_source_reads": activity.repeat_reads,
            "redacted_secrets": secrets,
            "unredacted_replays": unredacted,
            "hook_pass_through": activity.bypasses,
            "proxy": proxy.as_ref().map(|p| p.json()),
            "archive_recalls": {"recalls": recalls, "bytes": recalled_bytes},
            "preview_budgets": crate::adaptive::stats_json(workspace),
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if count == 0 {
        let justless = workspace.justless();
        println!(
            "No captured runs yet. Run a command with `{justless} run -- COMMAND` or install hooks with `{justless} init --hooks`."
        );
    } else {
        println!(
            "Kept {} of {} command output out of context at first preview ({:.1}%) across {count} run(s)",
            human_bytes(saved),
            human_bytes(raw),
            percent(saved, raw)
        );
        let net_saved = format!(
            "net saved: {} ({:.1}%)",
            signed_bytes(net),
            signed_percent(net, raw)
        );
        if replayed.bytes() == 0 && replayed.unmeasured == 0 {
            println!("No run output was retrieved again, so {net_saved}");
        } else {
            let estimate = if replayed.estimated_replays > 0 {
                format!(
                    " and about {} estimated for {} whole replay(s) recorded without their size",
                    human_bytes(replayed.estimated),
                    replayed.estimated_replays
                )
            } else {
                String::new()
            };
            println!(
                "Later replays and lookups printed {} of run output again{estimate}, so {net_saved}",
                human_bytes(replayed.measured)
            );
        }
        if replayed.unmeasured > 0 {
            println!(
                "{} earlier range, search or context request(s) were recorded without their size and are not subtracted",
                replayed.unmeasured
            );
        }
        println!(
            "≈{} tokens saved at first preview, ≈{} net (estimated as bytes ÷ 4)",
            estimated_tokens(saved),
            estimated_tokens_signed(net)
        );
        if secrets > 0 {
            println!("Redacted {secrets} secret(s) in captured output");
        }
        if !timeline.is_empty() {
            println!("\nLast {days} day(s), UTC, with retrievals on the day their run completed");
            let peak = timeline
                .values()
                .map(|d| d.totals.net_saved_bytes)
                .max()
                .unwrap_or(0)
                .max(1) as u64;
            for d in timeline.values() {
                let t = &d.totals;
                let bar = t.net_saved_bytes.max(0) as u64;
                let width = (bar * 20).div_ceil(peak) as usize;
                let row = format!(
                    "  {}  {:>4} run(s)  {:>9} → {:<9} {:>5.1}%  net {:>6.1}%  {}",
                    d.date,
                    d.runs,
                    human_bytes(t.raw_bytes),
                    human_bytes(t.preview_bytes),
                    percent(t.initial(), t.raw_bytes),
                    signed_percent(t.net_saved_bytes, t.raw_bytes),
                    "█".repeat(width)
                );
                println!("{}", row.trim_end());
            }
        }
        println!("\nBy command, most net savings first");
        let wide = commands
            .iter()
            .map(|f| f.family.chars().count())
            .max()
            .unwrap_or(0)
            .min(28);
        for f in &commands {
            let t = &f.totals;
            let mut notes = Vec::new();
            if f.reopened_runs > 0 {
                notes.push(format!("{}/{} reopened in full", f.reopened_runs, f.runs));
            }
            if f.targeted_lookups > 0 {
                notes.push(format!("{} targeted lookup(s)", f.targeted_lookups));
            }
            if f.excerpts > 0 {
                notes.push(format!("{} log excerpt(s)", f.excerpts));
            }
            let later = t.replayed_bytes + t.estimated_replay_bytes;
            if later > 0 {
                notes.push(format!(
                    "{}{} retrieved later",
                    if t.estimated_replay_bytes > 0 {
                        "about "
                    } else {
                        ""
                    },
                    human_bytes(later)
                ));
            }
            if f.unchanged_runs > 0 {
                notes.push(format!("{} unchanged", f.unchanged_runs));
            }
            if f.grown_runs > 0 {
                notes.push(format!("{} grew", f.grown_runs));
            }
            let family = crate::summary::short(&f.family, 28);
            println!(
                "  {family:<wide$}  {:>4} run(s)  {:>9} → {:<9} {:>5.1}%  net {:>6.1}%{}",
                f.runs,
                human_bytes(t.raw_bytes),
                human_bytes(t.preview_bytes),
                percent(t.initial(), t.raw_bytes),
                signed_percent(t.net_saved_bytes, t.raw_bytes),
                if notes.is_empty() {
                    String::new()
                } else {
                    format!("  · {}", notes.join(" · "))
                }
            );
        }
        crate::adaptive::print_stats(workspace);
    }
    let reopened: u64 = commands.iter().map(|f| f.reopened_runs).sum();
    let targeted: u64 = commands.iter().map(|f| f.targeted_lookups).sum();
    let excerpts: u64 = commands.iter().map(|f| f.excerpts).sum();
    if count > 0 || activity.repeat_reads > 0 {
        println!(
            "\nRecovery: {reopened} run(s) reopened in full · {targeted} targeted lookup(s){} · {} repeated source-range read(s){}",
            if excerpts > 0 {
                format!(" · {excerpts} log excerpt(s)")
            } else {
                String::new()
            },
            activity.repeat_reads,
            if unredacted > 0 {
                format!(" · {unredacted} unredacted replay(s)")
            } else {
                String::new()
            }
        );
    }
    if !activity.bypasses.is_empty() {
        let passed: Vec<_> = activity
            .bypasses
            .iter()
            .map(|(family, n)| format!("{family} ×{n}"))
            .collect();
        println!(
            "Hooks passed through: {}. Run `{} discover` to see what agents ran without Justless.",
            passed.join(", "),
            workspace.justless()
        );
    }
    if let Some(proxy) = &proxy {
        proxy.print(days, &workspace.justless());
    }
    if recalls > 0 {
        println!(
            "\nArchived tool results printed by `justless recall`, last {days} day(s): {recalls}, {} back in context. Request-boundary and mask savings do not subtract them.",
            human_bytes(recalled_bytes)
        );
    }
    Ok(())
}

pub fn history(
    workspace: &Workspace,
    needle: Option<&str>,
    oldest: bool,
    limit: usize,
    cap: usize,
) -> Result<()> {
    let runs = records(workspace, oldest)?;
    if let Some(needle) = needle {
        return search_runs(workspace, runs, needle, limit, cap);
    }
    let mut report = Report::new(cap);
    let justless = workspace.justless();
    let mut count = 0;
    for run in runs {
        let run = run?;
        report.line(format!(
            "{} · {} · {} · {} (epoch ms {})",
            run.id,
            command(&run),
            outcome(&run, &justless),
            age(run.completed_ms),
            run.completed_ms
        ));
        count += 1;
        if count >= limit || report.clipped {
            break;
        }
    }
    if count == 0 {
        report.line("No matching captured runs.");
    }
    report.finish()
}

/// Lists the lines that contain `needle` in `runs`, in their order, up to
/// `limit` and within `cap` bytes. Every run is searched to the end, so a last
/// line can count the matches left out. Within a run that the limit cuts,
/// each stream keeps a place and a failed run's failure lines come first, as
/// in `search`. A stream whose matches all lie past the cut keeps a place too.
fn search_runs(
    workspace: &Workspace,
    runs: impl Iterator<Item = Result<Run>>,
    needle: &str,
    limit: usize,
    cap: usize,
) -> Result<()> {
    let mut found = Vec::new();
    let mut earlier = 0;
    for run in runs {
        let run = run?;
        let directory = workspace.state.join("runs").join(&run.id);
        // A run past the limit keeps only the first line of each kind, which
        // a stream left out may need.
        let keep = limit.saturating_sub(earlier).max(1);
        match capture::Found::scan(
            &directory,
            Stream::select("both"),
            needle,
            run.may_have_failed(),
            keep,
        ) {
            Ok(matches) if matches.total() > 0 => {
                earlier += matches.total();
                found.push((run, matches));
            }
            Ok(_) => {}
            Err(error) => skipped(&run.id, &error),
        }
    }
    let mut all = capture::Tally::default();
    for (_, matches) in &found {
        all.add(&matches.tally());
    }
    let total = all.total();
    if total == 0 {
        let mut report = Report::new(cap);
        report.line("No matching captured runs.");
        return report.finish();
    }
    // The lines for `want` places. Each run gets the places the runs before it
    // left. When that leaves out every match of a stream, such as an older
    // failed run's stderr under a newer run's stdout, the first run with one
    // shows it in the place of the last line listed.
    let lines = |want: usize| {
        let mut places = Vec::with_capacity(found.len());
        let mut left = want;
        for (_, matches) in &found {
            let take = left.min(matches.total());
            places.push(take);
            left -= take;
        }
        let mut pinned = Vec::new();
        if want < total && want >= 2 {
            for stream in [Stream::Stdout, Stream::Stderr] {
                let listed = found.iter().zip(&places).any(|((_, matches), &take)| {
                    matches.choose(take).iter().any(|line| line.0 == stream)
                });
                let first = found
                    .iter()
                    .enumerate()
                    .find_map(|(index, (_, matches))| Some((index, matches.first(stream)?)));
                let last = places.iter().rposition(|&take| take > 0);
                if let (false, Some(first), Some(last)) = (listed, first, last) {
                    places[last] -= 1;
                    pinned.push(first);
                }
            }
        }
        let mut lines = Vec::new();
        for (index, ((run, matches), &take)) in found.iter().zip(&places).enumerate() {
            let mut chosen = matches.choose(take);
            for (_, line) in pinned.iter().filter(|(pinned, _)| *pinned == index) {
                if !chosen.contains(line) {
                    chosen.push(*line);
                }
            }
            for (stream, n, excerpt) in chosen {
                let line = format!(
                    "[{}] {} {} {}:{n}: {excerpt}",
                    age(run.completed_ms),
                    run.id,
                    run.family,
                    stream.name()
                );
                lines.push((index, stream, line));
            }
        }
        lines
    };
    // The notice in its fullest form first. When that leaves no room for a
    // match, as under a small --budget, the run count and the hint are dropped,
    // and then the counts per stream.
    let notice = |form: usize, tally: &capture::Tally, runs: usize, budget: bool| {
        let shown = tally.shown();
        if form == 2 {
            return format!("[{shown} of {total} matches shown]\n");
        }
        if form == 1 {
            return format!("[{shown} of {total} matches shown ({tally})]\n");
        }
        let hint = match (total > limit, budget) {
            (true, true) => format!("--limit {total} and a larger --budget show all"),
            (true, false) => format!("--limit {total} shows all"),
            _ => "a larger --budget shows all".to_owned(),
        };
        format!(
            "[{shown} of {total} matches shown from {runs} of {} matching runs ({tally}); {hint}]\n",
            found.len()
        )
    };
    // The output with one form of the notice, and the matches it shows.
    let attempt = |form: usize| {
        // Room for the notice, set aside once matches are left out. Every
        // number in it is at most the one in this form.
        let widest = notice(form, &all.every_shown(), found.len(), true).len();
        let mut want = limit.min(total);
        let mut room = if want < total { widest } else { 0 };
        loop {
            let mut report = Report::new(cap.saturating_sub(room));
            let mut tally = all;
            let mut listed = Vec::new();
            let mut shown = 0;
            let mut printed = BTreeMap::new();
            for (index, stream, line) in lines(want) {
                let before = report.left();
                report.line(line);
                let bytes = before - report.left();
                if bytes > 0 {
                    *printed.entry(found[index].0.id.clone()).or_insert(0u64) += bytes as u64;
                }
                if report.clipped {
                    break;
                }
                tally.show(stream);
                if listed.last() != Some(&index) {
                    listed.push(index);
                }
                shown += 1;
            }
            // When the budget leaves out lines the limit allowed, the lines
            // that fit are chosen again, so that cut also keeps each stream's
            // place and puts a failure first.
            if report.clipped && room == 0 {
                room = widest;
                continue;
            }
            if report.clipped && shown > 0 && shown < want {
                want = shown;
                continue;
            }
            let budget = shown < limit.min(total);
            // Lines chosen again to fit still mark the budget as reached.
            report.clipped |= budget;
            let mut output = report.into_string();
            if tally.shown() < total {
                output.push_str(&notice(form, &tally, listed.len(), budget));
            }
            return (output, tally, printed);
        }
    };
    // The fullest notice that leaves room for matches of as many streams as
    // any form does, so a larger budget never shows fewer streams. With no
    // room for any match, the shortest.
    let covered = |tally: &capture::Tally| {
        [Stream::Stdout, Stream::Stderr]
            .into_iter()
            .filter(|&stream| tally.of(stream).0 > 0)
            .count()
    };
    let mut attempts = (0..3).map(attempt).collect::<Vec<_>>();
    let most = attempts
        .iter()
        .map(|(_, tally, _)| covered(tally))
        .max()
        .unwrap_or_default();
    let (output, _, printed) = match attempts
        .iter()
        .position(|(_, tally, _)| most > 0 && covered(tally) == most)
    {
        Some(form) => attempts.swap_remove(form),
        None => attempts.pop().unwrap_or_default(),
    };
    io::stdout().lock().write_all(output.as_bytes())?;
    excerpts(workspace, &printed);
    Ok(())
}

/// How lists name a run: its command label, or the output family when that is
/// more specific, as for `kubectl get pods`, or adds detail, as for `pytest`.
fn command(run: &Run) -> String {
    let (label, family) = (run.label.as_str(), run.family.as_str());
    if label.is_empty() || family.starts_with(label) {
        family.to_owned()
    } else if matches!(family, "other" | "legacy") || label.starts_with(family) {
        label.to_owned()
    } else {
        format!("{label} ({family})")
    }
}

pub fn resume(workspace: &Workspace, cap: usize) -> Result<()> {
    print!("{}", resume_text(workspace, cap)?);
    Ok(())
}

/// Recent runs, inspected source and the last pack, with stale source marked.
/// Returns `None` inside the text when the project has no recorded activity.
pub fn resume_text(workspace: &Workspace, cap: usize) -> Result<String> {
    let mut report = Report::new(cap);
    let activity: Activity = get_or_warn(&workspace.state.join("activity.json"));
    let justless = workspace.justless();
    let mut has_runs = false;
    for run in records(workspace, false)?.take(5) {
        let run = run?;
        has_runs = true;
        report.line(format!(
            "{} · {} · {}{}{}",
            run.id,
            command(&run),
            outcome(&run, &justless),
            if run.unchanged {
                " · unchanged output"
            } else {
                ""
            },
            run.summary
                .headline()
                .map(|h| match crate::redact::for_workspace(workspace) {
                    Some(redactor) => redactor.text(&h).into_owned(),
                    None => h,
                })
                .map(|h| format!(" · {}", crate::summary::short(&h, 160)))
                .unwrap_or_default()
        ));
        for d in run.diagnostics.iter().take(2) {
            let stale = run
                .source_digests
                .get(&d.location.file)
                .is_some_and(|old| !current(workspace, &d.location.file, old));
            report.line(format!(
                "  {}:{} -> {}{}",
                d.stream.name(),
                d.log_line,
                d.location,
                if stale { " [source changed]" } else { "" }
            ));
        }
    }
    // Source ranges and the last pack share the space the runs leave. The pack
    // keeps up to half of it, so neither list crowds out the other. A line that
    // does not fit whole is left out, and files are hashed only while their
    // lines can still fit.
    let room = report.left();
    let mut pack = Vec::new();
    let mut pack_bytes = 0;
    for (file, digest) in &activity.last_pack {
        if pack_bytes >= room {
            break;
        }
        let line = format!(
            "Last pack: {file}{}",
            if current(workspace, file, digest) {
                ""
            } else {
                " [source changed]"
            }
        );
        pack_bytes += line.len() + 1;
        pack.push(line);
    }
    let reserve = pack_bytes.min(room / 2);
    let mut dropped = pack.len() < activity.last_pack.len();
    // Activity keeps the 32 most recent ranges and outlines, newest first.
    for read in activity.reads.iter().take(32) {
        if report.clipped {
            break;
        }
        let stale = if current(workspace, &read.file, &read.digest) {
            ""
        } else {
            " [source changed]"
        };
        let line = if read.outline {
            format!("Outline {}{stale}", read.file)
        } else {
            format!("Read {}:{}-{}{stale}", read.file, read.from, read.to)
        };
        if line.len() + 1 + reserve > report.left() {
            dropped = true;
            break;
        }
        report.line(line);
    }
    for line in pack {
        if line.len() + 1 > report.left() {
            dropped = true;
            break;
        }
        report.line(line);
    }
    report.clipped |= dropped;
    if !has_runs && activity.reads.is_empty() && activity.last_pack.is_empty() {
        report.line("No recorded project activity yet.");
    }
    Ok(report.into_string())
}

/// `justless` starts the command that replays an unfinished run.
fn outcome(run: &Run, justless: &str) -> String {
    if run.unfinished {
        format!("unfinished, output so far in {justless} log")
    } else {
        // A run the Bash post-command hook recorded from a pipe or a list holds the
        // whole line's status, and says so.
        format!("exit {}", crate::postroute::exit_status(run))
    }
}

fn current(workspace: &Workspace, file: &str, old: &str) -> bool {
    workspace
        .source_path(file)
        .ok()
        .and_then(|p| fs::read(p).ok())
        .is_some_and(|b| capture::source_digest(&b) == old)
}

/// How long a run directory without a record or a lock can still be starting.
/// Justless locks a new run before it writes the run's first record.
const STARTING_MS: u64 = 60 * 1000;
/// How long an unfinished run without a lock file, recorded where locks are
/// unavailable or by an earlier Justless, counts as still in progress.
const UNLOCKED_MS: u64 = 24 * 60 * 60 * 1000;

/// Whether another Justless process is still recording this run. That process
/// holds the run's lock for as long as it lives, so a free lock means it was killed
/// and the run is pruned like any other. A finished run has no lock file.
fn in_flight(directory: &std::path::Path, age_ms: u64) -> bool {
    #[derive(Deserialize)]
    struct State {
        #[serde(default)]
        unfinished: bool,
    }
    let record = directory.join("record.json");
    match capture::recording(directory) {
        // The first record is written once the lock is held, so a free lock without
        // a record can belong to a run that is just starting.
        Some(recording) => recording || age_ms < STARTING_MS && !record.exists(),
        // Only a recent run can still be in progress, so older records stay unread.
        None => {
            age_ms < UNLOCKED_MS
                && match fs::read(&record) {
                    Ok(bytes) => {
                        serde_json::from_slice::<State>(&bytes).is_ok_and(|state| state.unfinished)
                    }
                    Err(_) => age_ms < STARTING_MS,
                }
        }
    }
}

struct Stored {
    id: String,
    modified: u64,
    referenced: bool,
    files: Vec<(u64, u64, u64)>,
}

/// Delete the oldest runs beyond `max_runs` or `max_bytes`. The current run and runs
/// that another live Justless process is still recording are always kept, and runs
/// that a rerun compares against go last. The caller holds the capture lock.
/// Returns the number of deleted runs.
pub fn prune(workspace: &Workspace, current: &str, max_runs: usize, max_bytes: u64) -> usize {
    let runs = workspace.state.join("runs");
    let Ok(entries) = fs::read_dir(&runs) else {
        return 0;
    };
    let baselines: BTreeMap<String, Vec<std::path::PathBuf>> =
        fs::read_dir(workspace.state.join("baselines"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let id = fs::read_to_string(entry.path()).ok()?;
                Some((id.trim().to_owned(), entry.path()))
            })
            .fold(BTreeMap::new(), |mut map, (id, path)| {
                map.entry(id).or_default().push(path);
                map
            });
    let now = now_ms();
    let mut stored = Vec::new();
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        if !id.starts_with("run-") || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let directory = entry.path();
        let modified = fs::metadata(directory.join("record.json"))
            .or_else(|_| fs::metadata(&directory))
            .and_then(|meta| meta.modified())
            .map_or(now, millis);
        if id != current && in_flight(&directory, now.saturating_sub(modified)) {
            continue;
        }
        let files = fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|file| file.metadata().ok())
            .map(|meta| file_identity(&meta))
            .collect();
        stored.push(Stored {
            referenced: baselines.contains_key(&id),
            id,
            modified,
            files,
        });
    }
    // Keep the current run, then runs that reruns compare against, then the newest.
    stored.sort_by_key(|run| {
        (
            run.id != current,
            !run.referenced,
            std::cmp::Reverse(run.modified),
        )
    });
    // Files that kept runs hold. A file shared through hard links stays on disk while
    // any kept run links it, so it counts once, against the first run kept with it.
    // A deleted run holds nothing, so an older run that shares its file pays for it.
    let mut held = std::collections::BTreeSet::new();
    let (mut count, mut bytes, mut deleted) = (0, 0, 0);
    for run in stored {
        let added: Vec<_> = run
            .files
            .iter()
            .filter(|(device, inode, _)| *inode == 0 || !held.contains(&(*device, *inode)))
            .collect();
        let size: u64 = added.iter().map(|(_, _, size)| size).sum();
        let keep = run.id == current || (count < max_runs && bytes + size <= max_bytes);
        if !keep && fs::remove_dir_all(runs.join(&run.id)).is_ok() {
            deleted += 1;
            for baseline in baselines.get(&run.id).into_iter().flatten() {
                let _ = fs::remove_file(baseline);
            }
            continue;
        }
        count += 1;
        bytes += size;
        held.extend(added.iter().map(|(device, inode, _)| (*device, *inode)));
    }
    deleted
}

fn file_identity(meta: &fs::Metadata) -> (u64, u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino(), meta.len())
    }
    #[cfg(not(unix))]
    {
        (0, 0, meta.len())
    }
}
