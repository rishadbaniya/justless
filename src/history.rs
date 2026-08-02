use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::capture::{self, Run, Stream};
use crate::context::Report;
use crate::workspace::{Workspace, lock_state, write_json};

pub fn now_ms() -> u64 {
    millis(SystemTime::now())
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

pub fn records(
    workspace: &Workspace,
    oldest: bool,
) -> Result<impl Iterator<Item = Result<Run>> + '_> {
    let path = workspace.state.join("runs");
    let mut entries = Vec::new();
    if path.exists() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            match fs::metadata(entry.path().join("record.json")) {
                Ok(meta) => entries.push((meta.modified()?, entry.file_name())),
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
    entries.sort_unstable();
    if !oldest {
        entries.reverse();
    }
    Ok(entries.into_iter().map(|(date, id)| {
        let (_, mut run) = capture::load(workspace, &id.to_string_lossy())?;
        if run.completed_ms == 0 {
            run.completed_ms = millis(date);
        }
        if run.family.is_empty() {
            run.family = "legacy".into();
        }
        Ok(run)
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

#[derive(Default)]
struct FamilyStats {
    runs: u64,
    reopened: u64,
    full: u64,
    targeted: u64,
    unchanged: u64,
    preview_bytes: usize,
}

pub fn stats(workspace: &Workspace) -> Result<()> {
    let (mut count, mut raw, mut preview) = (0u64, 0u64, 0usize);
    let mut families: BTreeMap<String, FamilyStats> = BTreeMap::new();
    for run in records(workspace, false)? {
        let run = run?;
        count += 1;
        raw += run.raw_bytes;
        preview += run.preview_bytes;
        let c: Recalls = get(&workspace
            .state
            .join("runs")
            .join(&run.id)
            .join("recalls.json"))?;
        let f = families.entry(run.family).or_default();
        f.runs += 1;
        f.reopened += u64::from(c.full > 0);
        f.full += c.full;
        f.targeted += c.range + c.search + c.context;
        f.unchanged += u64::from(run.unchanged);
        f.preview_bytes += run.preview_bytes;
    }
    println!("{count} runs · {raw} raw bytes · {preview} preview bytes");
    for (
        family,
        FamilyStats {
            runs,
            reopened,
            full,
            targeted,
            unchanged,
            preview_bytes,
        },
    ) in families
    {
        println!(
            "{family}: {reopened}/{runs} runs reopened in full · {full} full reads · {targeted} targeted lookups · {unchanged} unchanged runs · {preview_bytes} preview bytes"
        );
    }
    let activity: Activity = get(&workspace.state.join("activity.json"))?;
    println!(
        "{} repeated source-range reads (recent 32 ranges)",
        activity.repeat_reads
    );
    for (family, n) in activity.bypasses {
        println!("Hook passed through: {family} × {n}");
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
                run.family,
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

pub fn resume(workspace: &Workspace, cap: usize) -> Result<()> {
    let mut report = Report::new(cap);
    let activity: Activity = get(&workspace.state.join("activity.json"))?;
    let mut has_runs = false;
    for run in records(workspace, false)?.take(5) {
        let run = run?;
        has_runs = true;
        report.line(format!(
            "{} · {} · exit {}{}",
            run.id,
            run.family,
            run.status,
            if run.unchanged {
                " · unchanged output"
            } else {
                ""
            }
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
    for read in activity.reads.iter().take(10) {
        report.line(format!(
            "Read {}:{}-{}{}",
            read.file,
            read.from,
            read.to,
            if current(workspace, &read.file, &read.digest) {
                ""
            } else {
                " [source changed]"
            }
        ));
    }
    for (file, digest) in &activity.last_pack {
        report.line(format!(
            "Last pack: {file}{}",
            if current(workspace, file, digest) {
                ""
            } else {
                " [source changed]"
            }
        ));
        if report.clipped {
            break;
        }
    }
    if !has_runs && activity.reads.is_empty() && activity.last_pack.is_empty() {
        report.line("No recorded project activity yet.");
    }
    report.finish()
}

fn current(workspace: &Workspace, file: &str, old: &str) -> bool {
    workspace
        .source_path(file)
        .ok()
        .and_then(|p| fs::read(p).ok())
        .is_some_and(|b| capture::source_digest(&b) == old)
}
