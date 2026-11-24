//! `justless init --global`: opt-in user-level hooks for Claude Code, Codex and Antigravity CLI.
//!
//! Setup merges the same hooks `init --hooks` writes into the user's own hook file,
//! `settings.json` in `CLAUDE_CONFIG_DIR` or `~/.claude`, `hooks.json` in
//! `CODEX_HOME` or `~/.codex`, and `hooks.json` in `~/.gemini/config`, with every
//! other key kept. Each handler runs `justless hook-CLIENT MODE --global`, which
//! finds the project per call from the hook input's `cwd` and answers with the
//! client's no-op when there is none to serve: outside a marked project, in the
//! home directory, in a project whose `.justless.toml` sets `global_hooks = false`
//! or fails to load, and in a project whose own settings already run Justless's
//! hook for that event, which takes precedence so no command is handled twice.
//!
//! Setup records each file it writes, with its digest and the file as it was, in
//! the private `global-setup.json` beside the project registry. `init --remove
//! --global` restores a file that still holds what setup wrote byte for byte, and
//! otherwise deletes only the handlers that pass `--global`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::agents::{self, Agent};
use crate::workspace::{Workspace, private_dir, write_json, write_text};

/// The clients with user-level hooks.
pub const CLIENTS: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Agy];
/// What each global handler passes after its mode.
const SUFFIX: &str = " --global";
const RECORD: &str = "global-setup.json";

fn variable(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home() -> Option<PathBuf> {
    variable(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
}

fn name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        _ => "agy",
    }
}

fn label(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude Code",
        Agent::Codex => "Codex",
        _ => "Antigravity CLI",
    }
}

/// A client's user-level config directory: `CLAUDE_CONFIG_DIR` or `~/.claude`,
/// `CODEX_HOME` or `~/.codex`, and `~/.gemini/config` for Antigravity CLI
/// (overridden in tests by `JUSTLESS_TEST_AGY_CONFIG_DIR`).
pub fn config_dir(agent: Agent) -> Option<PathBuf> {
    let (key, folder) = match agent {
        Agent::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
        Agent::Codex => ("CODEX_HOME", ".codex"),
        Agent::Agy => ("JUSTLESS_TEST_AGY_CONFIG_DIR", ".gemini/config"),
        _ => return None,
    };
    variable(key).or_else(|| home().map(|home| home.join(folder)))
}

/// The user-level file `init --global` writes for a client.
pub fn hook_file(agent: Agent) -> Option<PathBuf> {
    let file = match agent {
        Agent::Claude => "settings.json",
        _ => "hooks.json",
    };
    config_dir(agent).map(|directory| directory.join(file))
}

// ---------------------------------------------------------------------------
// Setup and removal

#[derive(Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    files: BTreeMap<PathBuf, Written>,
    /// Directories setup created for those files.
    #[serde(default)]
    directories: BTreeSet<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Written {
    /// SHA-256 of the bytes Justless last wrote.
    digest: String,
    /// The file without Justless, or `None` when Justless created it.
    before: Option<String>,
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn load() -> Record {
    crate::registry::home()
        .and_then(|home| fs::read(home.join(RECORD)).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(record: &Record) -> Result<()> {
    let home =
        crate::registry::home().context("no state or home directory for the setup record")?;
    if let Some(parent) = home.parent() {
        fs::create_dir_all(parent)?;
    }
    private_dir(&home)?;
    write_json(&home.join(RECORD), record)
}

/// Note that setup is about to replace `path` with `contents`, and the directories
/// it is about to create, as `uninstall::remember` does for project files. A lost
/// record only costs exactness on removal, so a failure never stops setup.
fn remember(agent: Agent, path: &Path, contents: &str) {
    let mut record = load();
    for directory in path.ancestors().skip(1) {
        match fs::symlink_metadata(directory) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                record.directories.insert(directory.to_path_buf());
            }
            _ => break,
        }
    }
    let current = fs::read_to_string(path).ok();
    let key = path.display().to_string();
    let recorded = record.files.get(path);
    let before = match (&current, recorded) {
        (None, _) => None,
        (Some(text), Some(entry)) if entry.digest == digest(text) => entry.before.clone(),
        (Some(text), _) => match crate::uninstall::strip_global(
            agent,
            &key,
            text,
            recorded.and_then(|entry| entry.before.as_deref()),
        ) {
            Ok(None) => Some(text.clone()),
            Ok(Some((_, left))) => left,
            Err(_) => {
                record.files.remove(path);
                let _ = save(&record);
                return;
            }
        },
    };
    record.files.insert(
        path.to_path_buf(),
        Written {
            digest: digest(contents),
            before,
        },
    );
    let _ = save(&record);
}

/// The clients `--agent` names, which must have user-level hooks. Without it,
/// setup takes those whose config directory exists and removal takes all of them.
fn chosen(matches: &ArgMatches, setup: bool) -> Result<Vec<Agent>> {
    if matches.occurrences_of("agent") > 0 {
        let mut agents = Vec::new();
        for value in matches.values_of("agent").into_iter().flatten() {
            let agent = Agent::parse(value)?;
            ensure!(
                CLIENTS.contains(&agent),
                "--global sets up Claude Code, Codex and Antigravity CLI only, not {value}"
            );
            if !agents.contains(&agent) {
                agents.push(agent);
            }
        }
        return Ok(agents);
    }
    if !setup {
        return Ok(CLIENTS.to_vec());
    }
    let found: Vec<Agent> = CLIENTS
        .into_iter()
        .filter(|&agent| config_dir(agent).is_some_and(|directory| directory.is_dir()))
        .collect();
    ensure!(
        !found.is_empty(),
        "found no Claude Code, Codex or Antigravity CLI config directory; name the client with --agent claude, --agent codex or --agent agy"
    );
    Ok(found)
}

/// `justless init --global`.
pub fn init(matches: &ArgMatches) -> Result<()> {
    let chosen = chosen(matches, true)?;
    // Every file is read and merged before any is written.
    let mut plan = Vec::new();
    for agent in chosen {
        let path = hook_file(agent).context("no home directory to find the user settings in")?;
        match agents::merge_hooks(&path, agent, SUFFIX)? {
            Some(contents) => plan.push((agent, path, contents)),
            None => println!(
                "{} already has the global {} hooks",
                path.display(),
                label(agent)
            ),
        }
    }
    for (agent, path, contents) in &plan {
        remember(*agent, path, contents);
        let parent = path.parent().context("hook file without a directory")?;
        if fs::symlink_metadata(parent).is_err() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        write_text(path, contents)?;
        println!("Updated {}", path.display());
    }
    if !plan.is_empty() {
        println!(
            "The global hooks work in every project with a .git, .justless.toml or project file, except the home directory. A project's own Justless hooks take precedence, and `global_hooks = false` in its .justless.toml keeps the global hooks out. Start a new agent session to load them."
        );
    }
    if plan.iter().any(|(agent, _, _)| *agent == Agent::Codex) {
        println!("Review and trust the new hooks with /hooks in Codex.");
    }
    Ok(())
}

/// `justless init --remove --global`.
pub fn remove(matches: &ArgMatches) -> Result<()> {
    let dry_run = matches.is_present("dry-run");
    let mut record = load();
    let mut changes: Vec<(PathBuf, String, Option<String>)> = Vec::new();
    for agent in chosen(matches, false)? {
        let Some(path) = hook_file(agent) else {
            continue;
        };
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
            Ok(metadata) if !metadata.is_file() => {
                bail!(
                    "{} is not a regular file; remove its Justless hooks by hand",
                    path.display()
                )
            }
            Ok(_) => {}
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let key = path.display().to_string();
        // Setup's record of the file as it was keeps the user's own hook arrays,
        // even after the file changed since.
        let entry = record.files.get(&path);
        let stripped = crate::uninstall::strip_global(
            agent,
            &key,
            &text,
            entry.and_then(|entry| entry.before.as_deref()),
        )?;
        let recorded = entry.filter(|entry| entry.digest == digest(&text));
        let (what, outcome) = match (stripped, recorded) {
            (None, None) => continue,
            (Some((what, _)), Some(entry)) => (what, entry.before.clone()),
            (Some((what, left)), None) => (what, left),
            (None, Some(entry)) => ("the changes Justless made".into(), entry.before.clone()),
        };
        if outcome.as_deref() == Some(text.as_str()) {
            continue;
        }
        changes.push((path, what, outcome));
    }
    let (remove, delete) = if dry_run {
        ("remove", "delete")
    } else {
        ("removed", "deleted")
    };
    let line = |path: &Path, what: &str, outcome: &Option<String>| match outcome {
        Some(_) => format!("{}: {remove} {what}", path.display()),
        None => format!(
            "{}: {remove} {what} and {delete} the file, which held nothing else",
            path.display()
        ),
    };
    if changes.is_empty() {
        println!("No global Justless hooks to remove.");
        return Ok(());
    }
    if dry_run {
        println!("Dry run, nothing changed. Removing the global hooks would change:");
        for (path, what, outcome) in &changes {
            println!("  {}", line(path, what, outcome));
        }
        return Ok(());
    }
    println!("Removed the global Justless hooks:");
    for (path, what, outcome) in &changes {
        match outcome {
            Some(text) => write_text(path, text)?,
            None => fs::remove_file(path).with_context(|| format!("delete {}", path.display()))?,
        }
        record.files.remove(path);
        println!("  {}", line(path, what, outcome));
    }
    // Directories setup created go when they are empty, deepest first.
    let mut directories: Vec<PathBuf> = record.directories.iter().cloned().collect();
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
    for directory in directories {
        if fs::read_dir(&directory).is_ok_and(|mut entries| entries.next().is_none())
            && fs::remove_dir(&directory).is_ok()
        {
            println!("  {}/: {remove} the empty directory", directory.display());
            record.directories.remove(&directory);
        } else if !directory.exists() {
            record.directories.remove(&directory);
        }
    }
    let _ = save(&record);
    println!("Start a new agent session so the clients stop loading the global hooks.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Running a global hook

/// Answer a `justless hook-CLIENT MODE --global` call: as the project hook would
/// in the project the input's `cwd` is in, or with the client's no-op.
pub fn run_hook(agent: Agent, mode: &str) {
    let input = agents::read_input();
    let workspace = std::panic::catch_unwind(|| project(agent, mode, &input))
        .ok()
        .flatten();
    let answered = workspace.is_some_and(|workspace| {
        std::panic::catch_unwind(|| agents::answer_hook(&workspace, agent, mode, &input))
            .is_ok_and(|result| result.is_ok())
    });
    if !answered && let Some(answer) = crate::rewrite::noop(agent, mode) {
        println!("{answer}");
    }
}

/// The project a global hook serves for this call, if any.
fn project(agent: Agent, mode: &str, input: &Value) -> Option<Workspace> {
    let cwd = if agent == Agent::Agy {
        agents::agy_input_cwd(mode, input)?
    } else {
        let cwd = input["cwd"].as_str()?;
        Path::new(cwd.strip_prefix("file://").unwrap_or(cwd))
    };
    if !cwd.is_absolute() {
        return None;
    }
    let cwd = cwd.canonicalize().ok()?;
    let root = crate::workspace::marked_root(&cwd)?;
    let workspace = Workspace::discover(Some(root)).ok()?;
    skip(agent, Some(mode), &cwd, &workspace)
        .is_none()
        .then_some(workspace)
}

/// Why the global hooks leave a project alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Neither the directory nor a parent has `.git`, `.justless/`,
    /// `.justless.toml` or a project file.
    Unmarked,
    /// The root is the filesystem root, the home directory or a client's config
    /// directory, whose settings are the user's own.
    Outside,
    /// Its `.justless.toml` sets `global_hooks = false`.
    OptedOut,
    /// Its `.justless.toml` fails to load, so it may opt out, and every Justless
    /// command stops on the error.
    Unreadable,
    /// Its own settings run Justless's hook for the same client, which wins.
    ProjectHooks,
}

fn outside(root: &Path) -> bool {
    let canonical = |path: Option<PathBuf>| path.and_then(|path| path.canonicalize().ok());
    root.parent().is_none()
        || canonical(home()).as_deref() == Some(root)
        || CLIENTS
            .into_iter()
            .filter_map(|client| canonical(config_dir(client)))
            .any(|directory| root.starts_with(directory))
}

/// Whether the global hooks for `agent` leave the project of a call made in
/// `cwd` alone, and why, for `mode` or any mode; `None` when they serve it.
pub fn skip(agent: Agent, mode: Option<&str>, cwd: &Path, workspace: &Workspace) -> Option<Skip> {
    if crate::workspace::marked_root(cwd).is_none() {
        return Some(Skip::Unmarked);
    }
    if outside(&workspace.root) {
        return Some(Skip::Outside);
    }
    match workspace.settings() {
        Err(_) => Some(Skip::Unreadable),
        Ok(settings) if !settings.global_hooks => Some(Skip::OptedOut),
        Ok(_) if project_hooks(agent, mode, cwd, &workspace.root) => Some(Skip::ProjectHooks),
        Ok(_) => None,
    }
}

/// Whether a project's own settings run Justless's hook for `agent`, for `mode`
/// or any mode: Claude Code's project settings in `$CLAUDE_PROJECT_DIR`, where it
/// loads them, or else in `cwd` and its parents up to `root`, and the Codex and
/// Antigravity CLI hook files in `cwd` and its parents up to `root`. The
/// user-level file never counts.
pub fn project_hooks(agent: Agent, mode: Option<&str>, cwd: &Path, root: &Path) -> bool {
    let user = hook_file(agent).and_then(|file| file.canonicalize().ok());
    let mut directories: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|directory| directory.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    let files: &[&str] = match agent {
        Agent::Claude => {
            if let Some(project) = variable("CLAUDE_PROJECT_DIR") {
                directories = vec![project];
            }
            &[".claude/settings.json", ".claude/settings.local.json"]
        }
        Agent::Codex => &[".codex/hooks.json"],
        _ => &[
            ".agents/hooks.json",
            ".agent/hooks.json",
            "_agents/hooks.json",
            "_agent/hooks.json",
        ],
    };
    directories.iter().any(|directory| {
        files.iter().any(|file| {
            let path = directory.join(file);
            if path.canonicalize().ok() == user {
                return false;
            }
            fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .is_some_and(|config| handlers(&config, agent, mode, false) > 0)
        })
    })
}

/// How many handlers in a hook config run `justless hook-AGENT`, for `mode` or any
/// mode, with `--global` or without it.
fn handlers(config: &Value, agent: Agent, mode: Option<&str>, global: bool) -> usize {
    let count_events = |events: &serde_json::Map<String, Value>| {
        events
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .flat_map(|entry| {
                let nested = entry["hooks"].as_array().into_iter().flatten();
                std::iter::once(entry).chain(nested)
            })
            .filter_map(|handler| handler["command"].as_str())
            .filter(|command| runs(command, agent, mode, global))
            .count()
    };
    if agent == Agent::Agy {
        return config
            .as_object()
            .into_iter()
            .flat_map(|groups| groups.values())
            .filter_map(Value::as_object)
            .filter(|events| events.get("enabled") != Some(&Value::Bool(false)))
            .map(count_events)
            .sum();
    }
    config["hooks"].as_object().map_or(0, count_events)
}

/// Whether a shell command runs `justless hook-AGENT MODE`, with `--global` or not.
fn runs(command: &str, agent: Agent, mode: Option<&str>, global: bool) -> bool {
    let Some(words) = shlex::split(command) else {
        return false;
    };
    let mut words = words
        .iter()
        .skip_while(|word| word.as_str() == "env" || word.contains('=') && !word.starts_with('-'));
    let is_justless = words.next().is_some_and(|program| {
        let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
        base == "justless" || base.eq_ignore_ascii_case("justless.exe")
    });
    let rest: Vec<&str> = words.map(String::as_str).collect();
    let subcommand = format!("hook-{}", name(agent));
    is_justless
        && rest.contains(&"--global") == global
        && rest.iter().enumerate().any(|(index, word)| {
            *word == subcommand
                && mode.is_none_or(|mode| {
                    rest[index + 1..]
                        .iter()
                        .find(|word| !word.starts_with('-'))
                        .is_some_and(|next| *next == mode)
                })
        })
}

// ---------------------------------------------------------------------------
// What `doctor` reports

/// The global hooks of one client, as its user-level file holds them.
pub struct Installed {
    pub agent: Agent,
    pub label: &'static str,
    pub file: PathBuf,
    /// Handlers that pass `--global`.
    pub global: usize,
    /// Justless handlers in the same file without `--global`, which run in every
    /// project as well and so handle commands twice.
    pub plain: usize,
    /// Setup would add hooks this version writes.
    pub incomplete: bool,
}

/// Each client whose user-level file has Justless hooks. Reads only.
pub fn installed() -> Vec<Installed> {
    let mut found = Vec::new();
    for agent in CLIENTS {
        let Some(file) = hook_file(agent) else {
            continue;
        };
        let Some(config) = fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            continue;
        };
        let global = handlers(&config, agent, None, true);
        let plain = handlers(&config, agent, None, false);
        if global + plain == 0 {
            continue;
        }
        let incomplete = global > 0
            && agents::merge_hooks(&file, agent, SUFFIX).is_ok_and(|merged| merged.is_some());
        found.push(Installed {
            agent,
            label: label(agent),
            file,
            global,
            plain,
            incomplete,
        });
    }
    found
}
