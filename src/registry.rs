//! The machine-wide project registry: `$XDG_STATE_HOME/justless/projects.json`,
//! or `~/.local/state/justless/projects.json`.
//!
//! Each project keeps its own data in its `.justless/`, so deleting that directory
//! still deletes its archived output. The registry only lists where those
//! directories are: each project root, its repository (the resolved
//! `git rev-parse --git-common-dir`, shared by a repository's worktrees) and when
//! Justless last used it. It never holds output or command lines. A project is
//! added or refreshed when its `.justless/` is used, at most once an hour, so the
//! file is safe to delete and is rebuilt as projects are used. It lets `stats --all`
//! total every project, `log`, `search` and `context --run` find a run made in
//! another project, and `doctor` list the projects with hooks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::capture::Run;
use crate::history::{Summary, human_bytes, now_ms};
use crate::workspace::{Workspace, private_dir, write_json};

const FILE: &str = "projects.json";
/// How long a use counts as recent enough that the entry is not rewritten.
const REFRESH_MS: u64 = 60 * 60 * 1000;
/// The most projects kept, most recently used first.
const MAX_PROJECTS: usize = 1000;
/// Internal, for tests: the folders that count as temporary, separated like `PATH`,
/// instead of the system's temporary folder, `/tmp` and `/var/tmp`.
pub const TEMP_OVERRIDE: &str = "JUSTLESS_TEST_TEMP_DIRS";

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    projects: Vec<Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub root: PathBuf,
    /// The repository's common Git directory, the same for each of its worktrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<PathBuf>,
    /// When Justless last used the project, in Unix milliseconds.
    #[serde(default)]
    pub used_ms: u64,
}

/// The registry's directory, or `None` without a state or home directory.
pub fn home() -> Option<PathBuf> {
    let variable = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    variable("XDG_STATE_HOME")
        .filter(|path| path.is_absolute())
        .or_else(|| variable(home).map(|home| home.join(".local").join("state")))
        .map(|state| state.join("justless"))
}

fn load(directory: &Path) -> Registry {
    fs::read(directory.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The registered projects, most recently used first.
pub fn projects() -> Vec<Entry> {
    let mut projects = home().map(|home| load(&home).projects).unwrap_or_default();
    projects.sort_by_key(|entry| std::cmp::Reverse(entry.used_ms));
    projects
}

/// Note that Justless used `workspace`. Rewrites the file only when the project is
/// new or its last use is over an hour old, never waits for another process, and
/// ignores every error: the registry is a convenience.
pub fn touch(workspace: &Workspace) {
    if crate::rewrite::previewing() {
        return;
    }
    let Some(directory) = home() else {
        return;
    };
    let now = now_ms();
    let recent = |registry: &Registry| {
        registry.projects.iter().any(|entry| {
            entry.root == workspace.root && now.saturating_sub(entry.used_ms) < REFRESH_MS
        })
    };
    if recent(&load(&directory)) {
        return;
    }
    let _ = update(&directory, |registry| {
        if recent(registry) {
            return false;
        }
        let repository = repository(&workspace.root);
        registry
            .projects
            .retain(|entry| entry.root != workspace.root);
        registry.projects.push(Entry {
            root: workspace.root.clone(),
            repository,
            used_ms: now,
        });
        // Drop projects whose state is gone, and the oldest past the limit.
        registry
            .projects
            .retain(|entry| entry.root.join(".justless").is_dir());
        registry
            .projects
            .sort_by_key(|entry| std::cmp::Reverse(entry.used_ms));
        registry.projects.truncate(MAX_PROJECTS);
        true
    });
}

/// Drop `root` from the registry, as `init --remove --purge` does.
pub fn forget(root: &Path) {
    if let Some(directory) = home() {
        let _ = update(&directory, |registry| {
            let before = registry.projects.len();
            registry.projects.retain(|entry| entry.root != root);
            registry.projects.len() != before
        });
    }
}

/// Apply `change` under the registry's lock, skipping when another process holds it.
fn update(directory: &Path, change: impl FnOnce(&mut Registry) -> bool) -> Result<()> {
    if let Some(parent) = directory.parent() {
        fs::create_dir_all(parent)?;
    }
    private_dir(directory)?;
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("projects.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let mut registry = load(directory);
    if change(&mut registry) {
        // The temporary file `write_json` renames into place is private.
        write_json(&directory.join(FILE), &registry)?;
    }
    Ok(())
}

/// The resolved common Git directory of the repository at `root`, if it is one.
fn repository(root: &Path) -> Option<PathBuf> {
    fs::symlink_metadata(root.join(".git")).ok()?;
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let path = text.trim_end_matches(['\n', '\r']);
    (output.status.success() && !path.is_empty())
        .then(|| root.join(path).canonicalize().ok())
        .flatten()
}

/// The folders whose projects `stats --all` leaves out unless asked.
fn temporary_folders() -> Vec<PathBuf> {
    let folders = match std::env::var_os(TEMP_OVERRIDE) {
        Some(list) => std::env::split_paths(&list).collect(),
        None => vec![
            std::env::temp_dir(),
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
        ],
    };
    folders
        .into_iter()
        .filter_map(|folder| folder.canonicalize().ok())
        .collect()
}

/// Find run `id` in `workspace`, or else in another registered project, which is
/// returned with it. Run IDs are unique, so the project that holds it owns it.
pub fn locate(workspace: &Workspace, id: &str) -> Result<(Option<Workspace>, PathBuf, Run)> {
    let local = match crate::capture::load(workspace, id) {
        Ok((directory, run)) => return Ok((None, directory, run)),
        Err(error) => error,
    };
    if !local.to_string().starts_with("unknown run ID") {
        return Err(local);
    }
    for entry in projects() {
        if entry.root == workspace.root || !entry.root.join(".justless/runs").join(id).is_dir() {
            continue;
        }
        let Ok(owner) = Workspace::discover(Some(&entry.root)) else {
            continue;
        };
        if let Ok((directory, run)) = crate::capture::load(&owner, id) {
            return Ok((Some(owner), directory, run));
        }
    }
    Err(local)
}

/// `stats --all`: one total across the registered projects, by repository, of
/// the runs that finished in the last `days` days.
pub fn stats(matches: &ArgMatches, workspace: &Workspace, days: u64) -> Result<()> {
    let since = now_ms().saturating_sub(days.saturating_mul(86_400_000));
    let include_temp = matches.is_present("include-temp");
    let temporary = temporary_folders();
    let mut groups: Vec<(PathBuf, Vec<PathBuf>, Summary)> = Vec::new();
    let mut skipped = 0;
    let mut entries = projects();
    // The current project counts even before the registry lists it.
    if workspace.state.join("runs").is_dir()
        && !entries.iter().any(|entry| entry.root == workspace.root)
    {
        entries.push(Entry {
            root: workspace.root.clone(),
            repository: repository(&workspace.root),
            used_ms: now_ms(),
        });
    }
    for entry in entries {
        if !entry.root.join(".justless/runs").is_dir() {
            continue;
        }
        if !include_temp
            && temporary
                .iter()
                .any(|folder| entry.root.starts_with(folder))
        {
            skipped += 1;
            continue;
        }
        let Ok(project) = Workspace::discover(Some(&entry.root)) else {
            continue;
        };
        let summary = crate::history::summary(&project, since);
        if summary.runs == 0 {
            continue;
        }
        let key = entry
            .repository
            .clone()
            .unwrap_or_else(|| entry.root.clone());
        match groups.iter_mut().find(|(group, _, _)| *group == key) {
            Some((_, roots, total)) => {
                roots.push(entry.root);
                total.add(&summary);
            }
            None => groups.push((key, vec![entry.root], summary)),
        }
    }
    let mut total = Summary::default();
    for (_, _, summary) in &groups {
        total.add(summary);
    }
    groups.sort_by(|a, b| {
        b.2.net_saved_bytes
            .cmp(&a.2.net_saved_bytes)
            .then_with(|| a.0.cmp(&b.0))
    });
    let label = |key: &Path, roots: &[PathBuf]| -> PathBuf {
        if key.file_name().is_some_and(|name| name == ".git")
            && let Some(parent) = key.parent()
        {
            return parent.to_path_buf();
        }
        roots
            .iter()
            .min()
            .cloned()
            .unwrap_or_else(|| key.to_path_buf())
    };
    if matches.is_present("json") {
        let repositories: Vec<_> = groups
            .iter()
            .map(|(key, roots, summary)| {
                json!({
                    "repository": label(key, roots),
                    "roots": roots,
                    "runs": summary.runs,
                    "raw_bytes": summary.raw_bytes,
                    "preview_bytes": summary.preview_bytes,
                    "saved_bytes": summary.saved_bytes(),
                    "replayed_bytes": summary.replayed_bytes,
                    "net_saved_bytes": summary.net_saved_bytes,
                })
            })
            .collect();
        let report = json!({
            "days": days,
            "projects": groups.iter().map(|(_, roots, _)| roots.len()).sum::<usize>(),
            "runs": total.runs,
            "raw_bytes": total.raw_bytes,
            "preview_bytes": total.preview_bytes,
            "saved_bytes": total.saved_bytes(),
            "replayed_bytes": total.replayed_bytes,
            "net_saved_bytes": total.net_saved_bytes,
            "estimated_tokens_saved": total.estimate.saved_tokens(),
            "estimated_tokens_net_saved": total.estimate.net_tokens(),
            "token_estimate": crate::tokens::method(),
            "estimate_by_content": total.estimate.json(),
            "repositories": repositories,
            "temporary_projects_left_out": skipped,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if groups.is_empty() {
        println!("No captured runs in any registered project in the last {days} day(s).");
    } else {
        let projects: usize = groups.iter().map(|(_, roots, _)| roots.len()).sum();
        println!(
            "In the last {days} day(s), kept {} of {} command output out of context at first preview ({:.1}%) across {} run(s) in {projects} project(s), net saved {} (≈{} tokens at first preview, ≈{} net, estimated at {})",
            human_bytes(total.saved_bytes()),
            human_bytes(total.raw_bytes),
            percent(total.saved_bytes(), total.raw_bytes),
            total.runs,
            crate::history::signed_bytes(total.net_saved_bytes),
            total.estimate.saved_tokens(),
            total.estimate.net_tokens(),
            total.estimate.basis(),
        );
        println!("\nBy repository, most net savings first");
        for (key, roots, summary) in &groups {
            let worktrees = if roots.len() > 1 {
                format!(" ({} checkouts)", roots.len())
            } else {
                String::new()
            };
            println!(
                "  {}{worktrees}  {} run(s)  {} → {}  net {}",
                label(key, roots).display(),
                summary.runs,
                human_bytes(summary.raw_bytes),
                human_bytes(summary.preview_bytes),
                crate::history::signed_bytes(summary.net_saved_bytes),
            );
        }
    }
    if skipped > 0 {
        println!(
            "Left out {skipped} project(s) in temporary folders. Add --include-temp to count them."
        );
    }
    Ok(())
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// The registered projects whose own client settings run Justless hooks, with
/// the files that do, for `doctor`. Reads only.
pub fn hooked_projects() -> Vec<(PathBuf, Vec<String>)> {
    let mut found = Vec::new();
    for entry in projects() {
        let files: Vec<String> = crate::agents::AGENTS
            .iter()
            .map(|(_, agent)| agent.hook_file())
            .filter(|file| {
                fs::read_to_string(entry.root.join(file)).is_ok_and(|text| {
                    text.contains("justless hook-") || text.starts_with("// Managed by Justless.")
                })
            })
            .map(|file| file.display().to_string())
            .collect();
        if !files.is_empty() {
            found.push((entry.root, files));
        }
    }
    found
}
