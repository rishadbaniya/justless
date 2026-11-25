//! The machine-wide project registry: `$XDG_STATE_HOME/justless/projects.json`,
//! or `~/.local/state/justless/projects.json`.
//!
//! Each project keeps its own data in its `.justless/`, so deleting that directory
//! still deletes its archived output. The registry only lists where those
//! directories are: each project root, its repository (the resolved common Git
//! directory, shared by a repository's worktrees) and when
//! Justless last used it. It never holds output or command lines. A project is
//! added or refreshed when its `.justless/` is used, at most once an hour, so the
//! file is safe to delete and is rebuilt as projects are used. It lets `stats --all`
//! total every project, `log`, `search` and `context --run` find a run made in
//! another project, and `doctor` list the projects with hooks.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::capture::Run;
use crate::history::{Summary, human_bytes, now_ms};
use crate::workspace::{Workspace, git_common_dir, private_dir, write_json};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// Deduplicates entries by `root` in place (keeping the newest timestamp and
/// preserving `repository` when set), ordered most recently used first.
fn normalize_entries(projects: &mut Vec<Entry>) {
    projects.sort_by(|a, b| a.root.cmp(&b.root).then_with(|| b.used_ms.cmp(&a.used_ms)));
    projects.dedup_by(|later, kept| {
        if later.root == kept.root {
            if kept.repository.is_none() {
                kept.repository = later.repository.take();
            }
            true
        } else {
            false
        }
    });
    projects.sort_by(|a, b| b.used_ms.cmp(&a.used_ms).then_with(|| a.root.cmp(&b.root)));
}

fn entry_is_recent(registry: &Registry, root: &Path, now: u64) -> bool {
    registry
        .projects
        .iter()
        .any(|entry| entry.root == root && now.saturating_sub(entry.used_ms) < REFRESH_MS)
}

/// The registered projects, most recently used first.
pub fn projects() -> Vec<Entry> {
    let mut projects = home().map(|home| load(&home).projects).unwrap_or_default();
    normalize_entries(&mut projects);
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
    let _ = touch_at(&directory, &workspace.root, now_ms());
}

/// Updates the registry in `directory` for `root` at `now` (Unix milliseconds).
/// Returns `true` when `projects.json` was written, and `false` when the write
/// was skipped (no `.justless/` state dir, entry updated within `REFRESH_MS`,
/// lock held by another process, or no effective change).
fn touch_at(directory: &Path, root: &Path, now: u64) -> bool {
    if !root.join(".justless").is_dir() {
        return false;
    }
    if entry_is_recent(&load(directory), root, now) {
        return false;
    }
    // Resolve the common Git directory before acquiring the non-blocking lock
    // (pure filesystem reads, zero subprocesses) to minimize contention across
    // parallel worktree agents.
    let repo = repository(root);
    update(directory, |registry| {
        if entry_is_recent(registry, root, now) {
            return false;
        }
        let before = registry.projects.clone();
        registry.projects.retain(|entry| entry.root != root);
        registry.projects.push(Entry {
            root: root.to_path_buf(),
            repository: repo,
            used_ms: now,
        });
        // Drop projects whose state is gone, deduplicate roots, and keep the
        // newest `MAX_PROJECTS` entries.
        registry
            .projects
            .retain(|entry| entry.root == root || entry.root.join(".justless").is_dir());
        normalize_entries(&mut registry.projects);
        registry.projects.truncate(MAX_PROJECTS);
        registry.projects != before
    })
    .unwrap_or(false)
}

/// Drop `root` from the registry, as `init --remove --purge` does.
pub fn forget(root: &Path) {
    if let Some(directory) = home()
        && directory.join(FILE).is_file()
    {
        let _ = update(&directory, |registry| {
            let before = registry.projects.len();
            registry.projects.retain(|entry| entry.root != root);
            registry.projects.len() != before
        });
    }
}

/// Apply `change` under the registry's lock, skipping when another process holds it.
/// Returns `Ok(true)` only when `projects.json` was actually written.
fn update(directory: &Path, change: impl FnOnce(&mut Registry) -> bool) -> Result<bool> {
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
        return Ok(false);
    }
    let mut registry = load(directory);
    if change(&mut registry) {
        // The temporary file `write_json` renames into place is private.
        write_json(&directory.join(FILE), &registry)?;
        return Ok(true);
    }
    Ok(false)
}

/// The resolved common Git directory of the repository at `root`, if it is one.
fn repository(root: &Path) -> Option<PathBuf> {
    git_common_dir(root)
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

fn has_project_state(root: &Path) -> bool {
    let state = root.join(".justless");
    state.is_dir()
        && (state.join("runs").is_dir()
            || state.join("activity.json").is_file()
            || state.join("observations").is_dir()
            || state.join("proxy").is_dir()
            || state.join("archive-recalls.jsonl").is_file())
}

/// Aggregated proxy request-boundary clearing totals across registered projects
/// for `stats --all`.
#[derive(Default)]
struct ProxyAllSummary {
    projects: u64,
    requests: u64,
    cleared_requests: u64,
    newly_cleared_results: u64,
    newly_cleared_bytes: u64,
    newly_cleared_est_tokens: u64,
    recalled_bytes: u64,
    net_cleared_bytes: i64,
    tokens_not_sent: u64,
    archived_results: u64,
    archived_bytes: u64,
}

impl ProxyAllSummary {
    fn add_report(&mut self, report: &serde_json::Value) {
        let requests = report["requests"].as_u64().unwrap_or(0);
        if requests == 0 {
            return;
        }
        self.projects += 1;
        self.requests += requests;
        self.cleared_requests += report["cleared_requests"].as_u64().unwrap_or(0);
        self.newly_cleared_results += report["newly_cleared"]["results"].as_u64().unwrap_or(0);
        self.newly_cleared_bytes += report["newly_cleared"]["bytes"].as_u64().unwrap_or(0);
        self.newly_cleared_est_tokens +=
            report["newly_cleared"]["est_tokens"].as_u64().unwrap_or(0);
        self.recalled_bytes += report["recalled_bytes"].as_u64().unwrap_or(0);
        self.net_cleared_bytes =
            crate::history::net_bytes(self.newly_cleared_bytes, 0, self.recalled_bytes);
        self.tokens_not_sent += report["tokens_not_sent"].as_u64().unwrap_or(0);
        self.archived_results += report["archived_results"].as_u64().unwrap_or(0);
        self.archived_bytes += report["archived_bytes"].as_u64().unwrap_or(0);
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "projects": self.projects,
            "requests": self.requests,
            "cleared_requests": self.cleared_requests,
            "newly_cleared": {
                "results": self.newly_cleared_results,
                "bytes": self.newly_cleared_bytes,
                "est_tokens": self.newly_cleared_est_tokens,
            },
            "recalled_bytes": self.recalled_bytes,
            "net_cleared_bytes": self.net_cleared_bytes,
            "tokens_not_sent": self.tokens_not_sent,
            "archived_results": self.archived_results,
            "archived_bytes": self.archived_bytes,
            "token_estimate": "UTF-8 bytes / 4",
            "token_source": "API usage",
        })
    }

    fn line(&self, days: u64) -> String {
        format!(
            "Request boundary (justless proxy), last {days} day(s): {} request(s) across {} project(s) · {} cleared ({} net after {} recalled; ≈{} tokens estimated, {} API tokens not sent)",
            self.requests,
            self.projects,
            human_bytes(self.newly_cleared_bytes),
            crate::history::signed_bytes(self.net_cleared_bytes),
            human_bytes(self.recalled_bytes),
            self.newly_cleared_est_tokens,
            self.tokens_not_sent,
        )
    }
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
    if has_project_state(&workspace.root)
        && !entries.iter().any(|entry| entry.root == workspace.root)
    {
        entries.push(Entry {
            root: workspace.root.clone(),
            repository: repository(&workspace.root),
            used_ms: now_ms(),
        });
    }
    let mut outlines = crate::history::OutlinesSummary::default();
    let mut deltas = crate::history::DeltasSummary::default();
    let mut observations = crate::history::ObservationsSummary::default();
    let mut recalls = crate::history::ArchiveRecalls::default();
    let mut proxy_all = ProxyAllSummary::default();
    let mut agy_roots = Vec::new();
    for entry in entries {
        let state = entry.root.join(".justless");
        if !state.is_dir() {
            continue;
        }
        let has_runs = state.join("runs").is_dir();
        let has_activity = state.join("activity.json").is_file();
        let has_observations = state.join("observations").is_dir();
        let has_proxy = state.join("proxy").is_dir();
        let has_recalls = state.join("archive-recalls.jsonl").is_file();
        if !has_runs && !has_activity && !has_observations && !has_proxy && !has_recalls {
            continue;
        }
        if !include_temp
            && temporary
                .iter()
                .any(|folder| entry.root.starts_with(folder))
        {
            if has_runs
                || Workspace::discover(Some(&entry.root))
                    .ok()
                    .is_some_and(|project| {
                        crate::history::outlines_summary(&project).views > 0
                            || crate::history::deltas_summary(&project).views > 0
                            || crate::history::observations_summary(&project).archived_results > 0
                            || crate::proxy::report(&project, since, 0)
                                .ok()
                                .flatten()
                                .is_some_and(|r| r.json()["requests"].as_u64().unwrap_or(0) > 0)
                    })
            {
                skipped += 1;
            }
            continue;
        }
        agy_roots.push(entry.root.clone());
        let Ok(project) = Workspace::discover(Some(&entry.root)) else {
            continue;
        };
        if has_activity {
            outlines.add(&crate::history::outlines_summary(&project));
            deltas.add(&crate::history::deltas_summary(&project));
        }
        if has_observations {
            observations.add(&crate::history::observations_summary(&project));
        }
        let project_recalls = if has_recalls {
            crate::history::archive_recalls(&project.state, since)
        } else {
            crate::history::ArchiveRecalls::default()
        };
        recalls.add(&project_recalls);
        if has_proxy
            && let Ok(Some(report)) =
                crate::proxy::report(&project, since, project_recalls.proxy_bytes)
        {
            proxy_all.add_report(&report.json());
        }
        if !has_runs {
            continue;
        }
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
    let agy = crate::agy_sessions::stats_all(&agy_roots, since);
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
        let mut report = json!({
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
            "changed_since_read": deltas.json(),
            "source_outlines": outlines.json(),
            "observations": observations.json(&recalls),
            "archive_recalls": recalls.json(),
            "repositories": repositories,
            "temporary_projects_left_out": skipped,
        });
        if proxy_all.requests > 0 {
            report["proxy"] = proxy_all.json();
        }
        if let Some(agy) = &agy {
            report["agy"] = agy.json.clone();
        }
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
    let mut separated = false;
    let mut separate = |has_runs: bool| {
        if has_runs && !separated {
            println!();
            separated = true;
        }
    };
    if deltas.views > 0 {
        separate(!groups.is_empty());
        println!("{}", deltas.line());
    }
    if outlines.views > 0 {
        separate(!groups.is_empty());
        println!("{}", outlines.line());
    }
    if proxy_all.requests > 0 {
        separate(!groups.is_empty());
        println!("{}", proxy_all.line(days));
    } else if observations.archived_results > 0 {
        separate(!groups.is_empty());
        println!("{}", observations.line(&recalls));
    } else if recalls.count > 0 {
        separate(!groups.is_empty());
        println!(
            "Archived tool results printed by `justless recall`, last {days} day(s): {}, {} back in context.",
            recalls.count,
            human_bytes(recalls.bytes)
        );
    }
    if let Some(agy) = &agy {
        println!("{}", agy.line);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn proxy_all_summary_aggregates_projects_and_computes_net_cleared() {
        let mut summary = ProxyAllSummary::default();
        summary.add_report(&json!({
            "requests": 4,
            "cleared_requests": 2,
            "newly_cleared": {"results": 6, "bytes": 12_000, "est_tokens": 3_000},
            "recalled_bytes": 2_000,
            "net_cleared_bytes": 10_000,
            "tokens_not_sent": 2_800,
            "archived_results": 8,
            "archived_bytes": 20_000,
        }));
        summary.add_report(&json!({
            "requests": 0,
            "cleared_requests": 0,
            "newly_cleared": {"results": 0, "bytes": 0, "est_tokens": 0},
        }));
        summary.add_report(&json!({
            "requests": 3,
            "cleared_requests": 1,
            "newly_cleared": {"results": 4, "bytes": 8_000, "est_tokens": 2_000},
            "recalled_bytes": 1_000,
            "net_cleared_bytes": 7_000,
            "tokens_not_sent": 1_900,
            "archived_results": 5,
            "archived_bytes": 10_000,
        }));

        assert_eq!(summary.projects, 2);
        assert_eq!(summary.requests, 7);
        assert_eq!(summary.cleared_requests, 3);
        assert_eq!(summary.newly_cleared_results, 10);
        assert_eq!(summary.newly_cleared_bytes, 20_000);
        assert_eq!(summary.recalled_bytes, 3_000);
        assert_eq!(summary.net_cleared_bytes, 17_000);
        assert_eq!(summary.tokens_not_sent, 4_700);
        assert_eq!(summary.archived_results, 13);
        assert_eq!(summary.archived_bytes, 30_000);

        let report = summary.json();
        assert_eq!(report["projects"], 2);
        assert_eq!(report["net_cleared_bytes"], 17_000);
        assert_eq!(report["newly_cleared"]["bytes"], 20_000);

        let line = summary.line(7);
        assert!(line.contains("7 request(s) across 2 project(s)"), "{line}");
        assert!(line.contains("tokens estimated"), "{line}");
        assert!(line.contains("4700 API tokens not sent"), "{line}");
        assert!(!line.contains("÷ 4"), "{line}");
    }

    #[test]
    fn has_project_state_detects_deltas_observations_and_proxy_only_projects() {
        let dir = TempDir::new().unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir_all(empty.join(".justless")).unwrap();
        assert!(!has_project_state(&empty));

        let obs = dir.path().join("obs");
        fs::create_dir_all(obs.join(".justless/observations")).unwrap();
        assert!(has_project_state(&obs));

        let proxy = dir.path().join("proxy");
        fs::create_dir_all(proxy.join(".justless/proxy")).unwrap();
        assert!(has_project_state(&proxy));

        let act = dir.path().join("act");
        fs::create_dir_all(act.join(".justless")).unwrap();
        fs::write(act.join(".justless/activity.json"), "{}").unwrap();
        assert!(has_project_state(&act));
    }

    #[test]
    fn touch_skips_redundant_disk_rewrite_within_throttle_window_and_deduplicates() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let reg_dir = base.join("state/justless");

        // Primary repo + linked worktree, resolved purely via filesystem.
        let primary = base.join("repo");
        let wt_gitdir = primary.join(".git/worktrees/wt-1");
        let worktree = base.join("wt-1");
        fs::create_dir_all(&wt_gitdir).unwrap();
        fs::create_dir_all(primary.join(".justless")).unwrap();
        fs::create_dir_all(worktree.join(".justless")).unwrap();
        fs::write(wt_gitdir.join("commondir"), "../..\n").unwrap();
        fs::write(
            worktree.join(".git"),
            "gitdir: ../repo/.git/worktrees/wt-1\n",
        )
        .unwrap();

        let t0 = 1_700_000_000_000;
        assert!(touch_at(&reg_dir, &worktree, t0));

        let file = reg_dir.join(FILE);
        let bytes_before = fs::read(&file).unwrap();
        #[cfg(unix)]
        let inode_before = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&file).unwrap().ino()
        };

        let first = load(&reg_dir);
        assert_eq!(first.projects.len(), 1);
        assert_eq!(first.projects[0].root, worktree);
        assert_eq!(first.projects[0].repository, Some(primary.join(".git")));
        assert_eq!(first.projects[0].used_ms, t0);

        // Rapid back-to-back touches within the throttle window skip disk writes.
        assert!(!touch_at(&reg_dir, &worktree, t0 + 50));
        assert!(!touch_at(&reg_dir, &worktree, t0 + REFRESH_MS - 1));
        assert_eq!(fs::read(&file).unwrap(), bytes_before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&file).unwrap().ino(), inode_before);
        }

        // Once the throttle window elapses, the entry updates cleanly with no duplicate.
        let t1 = t0 + REFRESH_MS;
        assert!(touch_at(&reg_dir, &worktree, t1));
        let refreshed = load(&reg_dir);
        assert_eq!(refreshed.projects.len(), 1);
        assert_eq!(refreshed.projects[0].root, worktree);
        assert_eq!(refreshed.projects[0].repository, Some(primary.join(".git")));
        assert_eq!(refreshed.projects[0].used_ms, t1);

        // Pre-existing duplicate paths collapse to one entry keeping the newest timestamp.
        let mut dupes = vec![
            Entry {
                root: primary.clone(),
                repository: Some(primary.join(".git")),
                used_ms: t0,
            },
            Entry {
                root: primary.clone(),
                repository: None,
                used_ms: t1,
            },
            Entry {
                root: worktree.clone(),
                repository: Some(primary.join(".git")),
                used_ms: t0 + 100,
            },
        ];
        normalize_entries(&mut dupes);
        assert_eq!(dupes.len(), 2);
        assert_eq!(dupes[0].root, primary);
        assert_eq!(dupes[0].used_ms, t1);
        assert_eq!(dupes[0].repository, Some(primary.join(".git")));
        assert_eq!(dupes[1].root, worktree);
    }
}
