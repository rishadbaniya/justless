use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::ErrorKind;
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

fn age(ms: u64) -> String {
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
            let record = entry.path().join("record.json");
            let stamp = match fs::read(&record) {
                Ok(bytes) => serde_json::from_slice::<Stamp>(&bytes).context("decode run record"),
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
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

#[derive(Default, Serialize, Deserialize)]
struct Recalls {
    full: u64,
    range: u64,
    search: u64,
    context: u64,
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
}

fn get<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match File::open(path) {
        Ok(file) => Ok(serde_json::from_reader(file)?),
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

pub fn recall(directory: &Path, operation: &str) {
    let _ = update(
        &directory.join("recalls.json"),
        |r: &mut Recalls| match operation {
            "full" => r.full += 1,
            "range" => r.range += 1,
            "search" => r.search += 1,
            _ => r.context += 1,
        },
    );
}

pub fn saw_source(workspace: &Workspace, file: &str, from: usize, to: usize, digest: String) {
    if workspace.prepare_state().is_err() {
        return;
    }
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| {
            if let Some(i) = a
                .reads
                .iter()
                .position(|r| r.file == file && r.from == from && r.to == to)
            {
                let old = a.reads.remove(i);
                if old.digest == digest {
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

#[derive(Default, Serialize)]
struct FamilyStats {
    family: String,
    runs: u64,
    raw_bytes: u64,
    preview_bytes: u64,
    reopened_runs: u64,
    full_reads: u64,
    targeted_lookups: u64,
    unchanged_runs: u64,
    /// Runs whose printed preview and footer were larger than the raw output.
    grown_runs: u64,
}

#[derive(Default, Serialize)]
struct DayStats {
    date: String,
    runs: u64,
    raw_bytes: u64,
    preview_bytes: u64,
}

/// Agents and providers tokenize differently. Like the benchmarks, the report
/// estimates tokens as UTF-8 bytes divided by four and labels the figure as such.
pub fn estimated_tokens(bytes: u64) -> u64 {
    bytes.div_ceil(4)
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
    let (mut count, mut raw, mut preview) = (0u64, 0u64, 0u64);
    let mut families: BTreeMap<String, FamilyStats> = BTreeMap::new();
    let mut timeline: BTreeMap<String, DayStats> = BTreeMap::new();
    for run in records(workspace, false)? {
        let run = run?;
        // Count everything the agent received, including the footer.
        let shown = (run.preview_bytes + run.footer_bytes) as u64;
        count += 1;
        raw += run.raw_bytes;
        preview += shown;
        let c: Recalls = get_or_warn(
            &workspace
                .state
                .join("runs")
                .join(&run.id)
                .join("recalls.json"),
        );
        let f = families.entry(run.family.clone()).or_default();
        f.family = run.family;
        f.runs += 1;
        f.raw_bytes += run.raw_bytes;
        f.preview_bytes += shown;
        f.reopened_runs += u64::from(c.full > 0);
        f.full_reads += c.full;
        f.targeted_lookups += c.range + c.search + c.context;
        f.unchanged_runs += u64::from(run.unchanged);
        f.grown_runs += u64::from(shown > run.raw_bytes);
        if run.completed_ms >= since && run.completed_ms > 0 {
            let date = utc_date(run.completed_ms);
            let d = timeline.entry(date.clone()).or_default();
            d.date = date;
            d.runs += 1;
            d.raw_bytes += run.raw_bytes;
            d.preview_bytes += shown;
        }
    }
    let activity: Activity = get_or_warn(&workspace.state.join("activity.json"));
    let saved = raw.saturating_sub(preview);
    let mut commands: Vec<_> = families.into_values().collect();
    commands.sort_by(|a, b| {
        let saved = |f: &FamilyStats| f.raw_bytes.saturating_sub(f.preview_bytes);
        saved(b).cmp(&saved(a)).then(a.family.cmp(&b.family))
    });
    if as_json {
        let report = serde_json::json!({
            "runs": count,
            "raw_bytes": raw,
            "preview_bytes": preview,
            "saved_bytes": saved,
            "saved_percent": (percent(saved, raw) * 10.0).round() / 10.0,
            "estimated_tokens_saved": estimated_tokens(saved),
            "token_estimate": "UTF-8 bytes / 4",
            "days": timeline.into_values().collect::<Vec<_>>(),
            "commands": commands,
            "repeated_source_reads": activity.repeat_reads,
            "hook_pass_through": activity.bypasses,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if count == 0 {
        println!(
            "No captured runs yet. Run a command with `justless run -- COMMAND` or install hooks with `justless init --hooks`."
        );
    } else {
        println!(
            "Kept {} of {} command output out of context ({:.1}%) across {count} run(s)",
            human_bytes(saved),
            human_bytes(raw),
            percent(saved, raw)
        );
        println!(
            "≈{} tokens saved (estimated as bytes ÷ 4)",
            estimated_tokens(saved)
        );
        if !timeline.is_empty() {
            println!("\nLast {days} day(s), UTC");
            let peak = timeline
                .values()
                .map(|d| d.raw_bytes.saturating_sub(d.preview_bytes))
                .max()
                .unwrap_or(0)
                .max(1);
            for d in timeline.values() {
                let day_saved = d.raw_bytes.saturating_sub(d.preview_bytes);
                let width = (day_saved * 20).div_ceil(peak) as usize;
                println!(
                    "  {}  {:>4} run(s)  {:>9} → {:<9} {:>5.1}%  {}",
                    d.date,
                    d.runs,
                    human_bytes(d.raw_bytes),
                    human_bytes(d.preview_bytes),
                    percent(day_saved, d.raw_bytes),
                    "█".repeat(width)
                );
            }
        }
        println!("\nBy command, most saved first");
        let wide = commands
            .iter()
            .map(|f| f.family.chars().count())
            .max()
            .unwrap_or(0)
            .min(28);
        for f in &commands {
            let mut notes = Vec::new();
            if f.reopened_runs > 0 {
                notes.push(format!("{}/{} reopened in full", f.reopened_runs, f.runs));
            }
            if f.targeted_lookups > 0 {
                notes.push(format!("{} targeted lookup(s)", f.targeted_lookups));
            }
            if f.unchanged_runs > 0 {
                notes.push(format!("{} unchanged", f.unchanged_runs));
            }
            if f.grown_runs > 0 {
                notes.push(format!("{} grew", f.grown_runs));
            }
            let family = crate::summary::short(&f.family, 28);
            println!(
                "  {family:<wide$}  {:>4} run(s)  {:>9} → {:<9} {:>5.1}%{}",
                f.runs,
                human_bytes(f.raw_bytes),
                human_bytes(f.preview_bytes),
                percent(f.raw_bytes.saturating_sub(f.preview_bytes), f.raw_bytes),
                if notes.is_empty() {
                    String::new()
                } else {
                    format!("  · {}", notes.join(" · "))
                }
            );
        }
    }
    let reopened: u64 = commands.iter().map(|f| f.reopened_runs).sum();
    let targeted: u64 = commands.iter().map(|f| f.targeted_lookups).sum();
    if count > 0 || activity.repeat_reads > 0 {
        println!(
            "\nRecovery: {reopened} run(s) reopened in full · {targeted} targeted lookup(s) · {} repeated source-range read(s)",
            activity.repeat_reads
        );
    }
    if !activity.bypasses.is_empty() {
        let passed: Vec<_> = activity
            .bypasses
            .iter()
            .map(|(family, n)| format!("{family} ×{n}"))
            .collect();
        println!(
            "Hooks passed through: {}. Run `justless discover` to see what agents ran without Justless.",
            passed.join(", ")
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
    let mut report = Report::new(cap);
    let mut count = 0;
    for run in runs {
        let run = run?;
        if let Some(needle) = needle {
            let directory = workspace.state.join("runs").join(&run.id);
            capture::matches(
                &directory,
                Stream::select("both"),
                needle,
                |stream, n, excerpt| {
                    report.line(format!(
                        "[{}] {} {} {}:{n}: {excerpt}",
                        age(run.completed_ms),
                        run.id,
                        run.family,
                        stream.name()
                    ));
                    count += 1;
                    count < limit && !report.clipped
                },
            )?;
            if count >= limit || report.clipped {
                break;
            }
        } else {
            report.line(format!(
                "{} · {} · exit {} · {} (epoch ms {})",
                run.id,
                command(&run),
                run.status,
                age(run.completed_ms),
                run.completed_ms
            ));
            count += 1;
            if count >= limit || report.clipped {
                break;
            }
        }
    }
    if count == 0 {
        report.line("No matching captured runs.");
    }
    report.finish()
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
    let mut has_runs = false;
    for run in records(workspace, false)?.take(5) {
        let run = run?;
        has_runs = true;
        report.line(format!(
            "{} · {} · exit {}{}{}",
            run.id,
            command(&run),
            run.status,
            if run.unchanged {
                " · unchanged output"
            } else {
                ""
            },
            run.summary
                .headline()
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
    // Activity keeps the 32 most recent ranges, newest first.
    for read in activity.reads.iter().take(32) {
        if report.clipped {
            break;
        }
        let line = format!(
            "Read {}:{}-{}{}",
            read.file,
            read.from,
            read.to,
            if current(workspace, &read.file, &read.digest) {
                ""
            } else {
                " [source changed]"
            }
        );
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

fn current(workspace: &Workspace, file: &str, old: &str) -> bool {
    workspace
        .source_path(file)
        .ok()
        .and_then(|p| fs::read(p).ok())
        .is_some_and(|b| capture::source_digest(&b) == old)
}
