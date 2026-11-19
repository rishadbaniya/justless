//! `justless init --remove`: take Justless back out of a project.
//!
//! Setup records each file it writes in `.justless/setup.json`: a digest of the
//! bytes it wrote and the file as it was without Justless, or that it did not
//! exist. It also records the directories it creates and the clients each `init`
//! set up. A file that still holds exactly what Justless wrote is restored from that
//! record byte for byte. Any other file, such as one edited since, one set up by an
//! older version or one in a fresh clone, is edited in place: only the instruction
//! section, Justless hook handlers, a status line that runs `justless` and the
//! `justless` MCP server are deleted, and every other byte stays.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item};

use crate::agents::{self, Agent, END, START};
use crate::json_edit::{self, Step};
use crate::mcp::Client;
use crate::workspace::{Workspace, lock_state, write_json, write_text};

/// Every client `init` sets up, by the name `--agent` takes.
const AGENTS: [&str; 7] = [
    "codex", "claude", "agy", "opencode", "cursor", "gemini", "copilot",
];
const RECORD: &str = "setup.json";
const IGNORE_LINE: &str = "/.justless/";
const PLUGIN_HEADER: &str = "// Managed by Justless.";
const FIX: &str = "fix it or delete its justless entries by hand, then run this again";

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<()> {
    // `--agent` defaults to codex for setup. Removal without it covers every client.
    let selected = if matches.occurrences_of("agent") == 0 {
        None
    } else {
        Some(
            matches
                .values_of("agent")
                .into_iter()
                .flatten()
                .map(Agent::parse)
                .collect::<Result<Vec<_>>>()?,
        )
    };
    let purge = matches.is_present("purge");
    ensure!(
        !(purge && selected.is_some()),
        "--purge deletes state that every client shares; run it without --agent"
    );
    // Below the home directory, root discovery can stop at a `~/.justless/` that
    // any command run there creates. The client folders there hold user settings
    // that apply to every project, so only an explicit root reaches them.
    if !matches.is_present("root")
        && let Some(reason) = user_level(&workspace.root)
    {
        bail!(
            "{reason}. Run `justless init --remove` inside a project folder, or pass --root {} to remove Justless from those settings",
            workspace.root.display()
        );
    }
    remove(
        workspace,
        selected.as_deref(),
        matches.is_present("dry-run"),
        purge,
    )
}

/// Why `root` holds the user's own client settings, if it does.
fn user_level(root: &Path) -> Option<String> {
    let directory = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .and_then(|value| PathBuf::from(value).canonicalize().ok())
    };
    if ["HOME", "USERPROFILE"]
        .into_iter()
        .any(|name| directory(name).as_deref() == Some(root))
    {
        return Some(format!(
            "{} is your home directory, where .claude, .codex, .cursor and .gemini hold your user settings",
            root.display()
        ));
    }
    for (name, folder) in [("CLAUDE_CONFIG_DIR", ".claude"), ("CODEX_HOME", ".codex")] {
        if let Some(config) = directory(name)
            && root
                .join(folder)
                .canonicalize()
                .is_ok_and(|dir| dir == config)
        {
            return Some(format!(
                "{folder} in {} is your {name}, which holds your user settings",
                root.display()
            ));
        }
    }
    None
}

fn remove(
    workspace: &Workspace,
    selected: Option<&[Agent]>,
    dry_run: bool,
    purge: bool,
) -> Result<()> {
    // Removal never creates the state directory. It locks out a concurrent setup
    // only when the directory is already there.
    let state = fs::symlink_metadata(&workspace.state).is_ok_and(|meta| meta.is_dir());
    let mut guard = if state && !dry_run {
        Some(lock_state(&workspace.state.join("setup.lock"))?)
    } else {
        None
    };
    let mut record = if state {
        load(workspace)
    } else {
        Record::default()
    };
    let chosen = selected.map_or_else(all_agents, <[Agent]>::to_vec);
    // Every file is read and checked before any is changed.
    let plan = plan(workspace, &chosen, purge, &record).context("nothing was removed")?;
    let mut kept = Vec::new();
    if selected.is_none() && !purge {
        if state {
            kept.push(".justless/");
        }
        let ignore = workspace.root.join(".gitignore");
        if regular_file(&workspace.root, &ignore)?
            && fs::read_to_string(&ignore)
                .is_ok_and(|text| text.lines().any(|line| line.trim() == IGNORE_LINE))
        {
            kept.push("the /.justless/ line in .gitignore");
        }
    }

    for filename in &plan.skipped {
        eprintln!(
            "Skipped {filename}, a link to a file outside the project. Remove the Justless section from its target yourself."
        );
    }
    let root = workspace.root.display();
    if dry_run {
        if plan.is_empty() {
            println!("Nothing to remove in {root}.");
        } else {
            println!("Dry run, nothing changed. Removing Justless from {root} would change:");
        }
        for line in plan.lines(true) {
            println!("  {line}");
        }
        plan.notes(&kept);
        return Ok(());
    }

    let applied = apply(workspace, &plan, &mut guard);
    if state && !applied.state && forget(&mut record, &plan, &applied, &chosen) {
        // A stale entry never matches the edited file, so this is best effort.
        let _ = save(workspace, &record);
    }
    drop(guard);
    let lines = plan.applied(&applied);
    if let Some(error) = applied.error {
        if !lines.is_empty() {
            println!("Removed part of Justless from {root} before an error:");
            for line in &lines {
                println!("  {line}");
            }
        }
        bail!("{error:#}. Fix it and run this command again to finish");
    }
    if plan.is_empty() {
        println!("Nothing to remove in {root}.");
    } else {
        println!("Removed Justless from {root}:");
    }
    for line in &lines {
        println!("  {line}");
    }
    plan.notes(&kept);
    if plan.changes.iter().any(|change| change.config) {
        println!("Start a new agent session so the clients stop loading Justless.");
    }
    Ok(())
}

/// What `apply` got done before it finished or stopped.
#[derive(Default)]
struct Applied {
    /// How many of the planned file changes were made, in order.
    changes: usize,
    directories: Vec<PathBuf>,
    state: bool,
    error: Option<anyhow::Error>,
}

/// Make the planned changes in order, stopping at the first file that fails.
fn apply(workspace: &Workspace, plan: &Plan, guard: &mut Option<fs::File>) -> Applied {
    let mut applied = Applied::default();
    for change in &plan.changes {
        let result = match &change.outcome {
            Outcome::Write(text) => write_text(&change.path, text),
            Outcome::Delete => fs::remove_file(&change.path).map_err(Into::into),
        };
        if let Err(error) = result {
            applied.error = Some(error.context(format!("remove Justless from {}", change.key)));
            break;
        }
        applied.changes += 1;
    }
    // Directories go even after a file failed, since one that still holds that
    // file is not empty and stays.
    for directory in &plan.directories {
        match fs::remove_dir(directory) {
            Ok(()) => applied.directories.push(directory.clone()),
            Err(error) if applied.error.is_none() => {
                let name = key(&plan.root, directory).unwrap_or_default();
                applied.error = Some(anyhow!(error).context(format!("remove {name}/")));
            }
            Err(_) => {}
        }
    }
    if applied.error.is_none()
        && let Some(purge) = plan.state
    {
        // Release the setup lock, which lives in the directory.
        drop(guard.take());
        let result = match purge {
            Purge::Directory => fs::remove_dir_all(&workspace.state),
            Purge::Link => fs::remove_file(&workspace.state),
        };
        match result {
            Ok(()) => applied.state = true,
            Err(error) => applied.error = Some(anyhow!(error).context("delete .justless")),
        }
    }
    applied
}

/// Drop what was removed from the setup record, and the chosen clients from the
/// files they read. Returns whether the record changed.
fn forget(record: &mut Record, plan: &Plan, applied: &Applied, chosen: &[Agent]) -> bool {
    let mut changed = false;
    for change in &plan.changes[..applied.changes] {
        changed |= record.files.remove(&change.key).is_some();
        if change.instructions {
            changed |= record.readers.remove(&change.key).is_some();
        }
    }
    for directory in &applied.directories {
        if let Ok(name) = key(&plan.root, directory) {
            changed |= record.directories.remove(&name);
        }
    }
    for readers in record.readers.values_mut() {
        for &agent in chosen {
            changed |= readers.remove(name(agent));
        }
    }
    record.readers.retain(|_, readers| !readers.is_empty());
    changed
}

// ---------------------------------------------------------------------------
// Setup record

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Record {
    files: BTreeMap<String, Written>,
    /// Directories setup created, relative to the root.
    directories: BTreeSet<String>,
    /// The clients `init` set up, by the instructions file each reads.
    readers: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Written {
    /// SHA-256 of the bytes Justless last wrote.
    digest: String,
    /// The file without Justless, or `None` when Justless created it.
    before: Option<String>,
}

impl Written {
    fn restore(&self) -> Outcome {
        match &self.before {
            Some(text) => Outcome::Write(text.clone()),
            None => Outcome::Delete,
        }
    }
}

fn load(workspace: &Workspace) -> Record {
    fs::read(workspace.state.join(RECORD))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(workspace: &Workspace, record: &Record) -> Result<()> {
    write_json(&workspace.state.join(RECORD), record)
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Note that setup is about to replace `path` with `contents`, and the directories
/// it is about to create for it, so call this before creating them. Setup calls
/// this with the setup lock held. A missing record only costs exactness when
/// Justless is removed, so a failure here never stops setup.
pub fn remember(workspace: &Workspace, path: &Path, contents: &str) {
    let _ = try_remember(workspace, path, contents);
}

fn try_remember(workspace: &Workspace, path: &Path, contents: &str) -> Result<()> {
    let root = &workspace.root;
    let file = key(root, path)?;
    let current = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut record = load(workspace);
    for directory in path.ancestors().skip(1) {
        if directory == root.as_path() || !directory.starts_with(root) {
            break;
        }
        match fs::symlink_metadata(directory) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                record.directories.insert(key(root, directory)?);
            }
            // The ancestors of a directory that exists exist too.
            _ => break,
        }
    }
    let before = match (&current, record.files.get(&file)) {
        (None, _) => None,
        // Justless wrote the current bytes, so the earlier original still applies.
        (Some(text), Some(entry)) if entry.digest == digest(text) => entry.before.clone(),
        // Otherwise the original is the current file without any Justless setup
        // already in it, which is the file itself in the usual case.
        (Some(text), _) => match strip(&file, kind_of(&file), text) {
            Ok(Scan { found: None, .. }) => Some(text.clone()),
            Ok(Scan {
                found: Some(found), ..
            }) => match found.outcome {
                Outcome::Write(text) => Some(text),
                Outcome::Delete => None,
            },
            Err(_) => {
                record.files.remove(&file);
                return save(workspace, &record);
            }
        },
    };
    record.files.insert(
        file,
        Written {
            digest: digest(contents),
            before,
        },
    );
    save(workspace, &record)
}

/// Note the instructions file each client `init` set up reads, so removing another
/// client that shares the file keeps the section while this one is still set up.
/// Claude Code reads the `AGENTS.md` that `CLAUDE.md` imports.
pub fn remember_readers(workspace: &Workspace, clients: &[Agent]) {
    let root = &workspace.root;
    let mut record = load(workspace);
    let mut changed = false;
    let imported = agents::imported_instructions(root, false).map(|(_, target)| target);
    for &agent in clients {
        let path = match &imported {
            Some(target) if agent == Agent::Claude => target.clone(),
            _ => match agents::instructions_file(root, &root.join(agent.instructions())) {
                Ok(Some(path)) => path,
                _ => continue,
            },
        };
        if let Ok(file) = key(root, &path) {
            changed |= record
                .readers
                .entry(file)
                .or_default()
                .insert(name(agent).to_owned());
        }
    }
    if changed {
        let _ = save(workspace, &record);
    }
}

/// A path relative to the root with `/` separators, as the record and output use.
fn key(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside the project", path.display()))?;
    let parts = relative
        .components()
        .map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .with_context(|| format!("unexpected path {}", path.display()))?;
    Ok(parts.join("/"))
}

// ---------------------------------------------------------------------------
// What Justless writes where

#[derive(Clone, Copy, Default)]
struct JsonRules {
    /// A top-level `hooks` object of events.
    hooks: bool,
    /// Antigravity's top-level `justless` hook group.
    group: bool,
    /// Claude Code's `statusLine`.
    status_line: bool,
    /// The object that lists MCP servers.
    servers: Option<&'static str>,
    /// Cursor and Copilot hook files get `"version": 1` when setup creates them.
    versioned: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    Instructions,
    Json(JsonRules),
    Toml,
    Plugin,
    Ignore,
}

fn all_agents() -> Vec<Agent> {
    AGENTS
        .iter()
        .map(|name| Agent::parse(name).expect("known agent"))
        .collect()
}

fn name(agent: Agent) -> &'static str {
    AGENTS
        .into_iter()
        .find(|name| Agent::parse(name).is_ok_and(|parsed| parsed == agent))
        .expect("known agent")
}

/// The MCP client whose project file belongs to an agent. VS Code's file goes with
/// Copilot, the agent harness that reads it.
fn mcp_client(agent: Agent) -> Option<Client> {
    match agent {
        Agent::Claude => Some(Client::Claude),
        Agent::Cursor => Some(Client::Cursor),
        Agent::Gemini => Some(Client::Gemini),
        Agent::Codex => Some(Client::Codex),
        Agent::Copilot => Some(Client::VsCode),
        Agent::Agy | Agent::OpenCode => None,
    }
}

/// Every config file `init --hooks` and `mcp --install` write, with its kind and
/// the agent that owns it.
fn catalog() -> Vec<(String, Kind, Agent)> {
    let mut files = Vec::new();
    for agent in all_agents() {
        let (directory, file) = agent.config();
        let hooks = match agent {
            Agent::OpenCode => Kind::Plugin,
            _ => Kind::Json(JsonRules {
                hooks: true,
                group: agent == Agent::Agy,
                status_line: agent == Agent::Claude,
                servers: None,
                versioned: matches!(agent, Agent::Cursor | Agent::Copilot),
            }),
        };
        files.push((format!("{directory}/{file}"), hooks, agent));
        // A status line can also be a personal setting.
        if agent == Agent::Claude {
            files.push((format!("{directory}/settings.local.json"), hooks, agent));
        }
        if let Some(client) = mcp_client(agent) {
            let kind = match client {
                Client::Codex => Kind::Toml,
                _ => Kind::Json(JsonRules {
                    servers: Some(client.servers_key()),
                    ..JsonRules::default()
                }),
            };
            files.push((client.file().to_owned(), kind, agent));
        }
    }
    files
}

/// Everything Justless may have put in a file, merged across the clients that
/// share it, such as Gemini CLI's hooks and MCP server in one settings file.
fn kind_of(key: &str) -> Kind {
    let mut kind = None;
    for (file, found, _) in catalog() {
        if file != key {
            continue;
        }
        kind = Some(match (kind, found) {
            (Some(Kind::Json(a)), Kind::Json(b)) => Kind::Json(JsonRules {
                hooks: a.hooks || b.hooks,
                group: a.group || b.group,
                status_line: a.status_line || b.status_line,
                servers: a.servers.or(b.servers),
                versioned: a.versioned || b.versioned,
            }),
            _ => found,
        });
    }
    kind.unwrap_or(if key == ".gitignore" {
        Kind::Ignore
    } else {
        Kind::Instructions
    })
}

// ---------------------------------------------------------------------------
// Planning

#[derive(Default)]
struct Plan {
    changes: Vec<Change>,
    directories: Vec<PathBuf>,
    /// Shared instruction sections left in place, with the reason.
    kept: Vec<String>,
    /// Justless mentions removal leaves in place, by file and JSON pointer or
    /// TOML key, or with no location for a whole file.
    leftover: Vec<(String, String)>,
    /// Instruction files that link outside the project to a Justless section.
    skipped: Vec<&'static str>,
    /// Delete `.justless`.
    state: Option<Purge>,
    root: PathBuf,
}

#[derive(Clone, Copy)]
enum Purge {
    Directory,
    Link,
}

struct Change {
    key: String,
    path: PathBuf,
    what: Vec<String>,
    outcome: Outcome,
    /// The file is one Justless wrote whole, such as the OpenCode plugin.
    whole: bool,
    /// A client config file rather than instructions or `.gitignore`.
    config: bool,
    /// An instructions file.
    instructions: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    Write(String),
    Delete,
}

/// What a file holds of Justless's, and the file without it.
struct Found {
    what: Vec<String>,
    outcome: Outcome,
}

/// What reading one file found: what to remove, and the Justless mentions that
/// stay because setup does not write them in that form.
#[derive(Default)]
struct Scan {
    found: Option<Found>,
    leftover: Vec<String>,
}

fn verbs(dry_run: bool) -> (&'static str, &'static str) {
    if dry_run {
        ("remove", "delete")
    } else {
        ("removed", "deleted")
    }
}

impl Change {
    fn line(&self, dry_run: bool) -> String {
        let (remove, delete) = verbs(dry_run);
        let what = join(&self.what);
        match self.outcome {
            Outcome::Delete if self.whole => format!("{}: {delete} {what}", self.key),
            Outcome::Delete => format!(
                "{}: {remove} {what} and {delete} the file, which held nothing else",
                self.key
            ),
            Outcome::Write(_) => format!("{}: {remove} {what}", self.key),
        }
    }
}

impl Purge {
    fn line(self, dry_run: bool) -> String {
        let (_, delete) = verbs(dry_run);
        match self {
            Self::Directory => format!(".justless/: {delete} the archived runs, index and history"),
            Self::Link => format!(".justless: {delete} the link"),
        }
    }
}

impl Plan {
    fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.directories.is_empty() && self.state.is_none()
    }

    fn directory_line(&self, directory: &Path, dry_run: bool) -> String {
        let (remove, _) = verbs(dry_run);
        let name = key(&self.root, directory).unwrap_or_default();
        format!("{name}/: {remove} the empty directory")
    }

    fn lines(&self, dry_run: bool) -> Vec<String> {
        let mut lines: Vec<String> = self
            .changes
            .iter()
            .map(|change| change.line(dry_run))
            .collect();
        for directory in &self.directories {
            lines.push(self.directory_line(directory, dry_run));
        }
        lines.extend(self.state.map(|purge| purge.line(dry_run)));
        lines
    }

    /// The lines for what was done, which is all of the plan unless it stopped.
    fn applied(&self, applied: &Applied) -> Vec<String> {
        let mut lines: Vec<String> = self.changes[..applied.changes]
            .iter()
            .map(|change| change.line(false))
            .collect();
        for directory in &applied.directories {
            lines.push(self.directory_line(directory, false));
        }
        if applied.state {
            lines.extend(self.state.map(|purge| purge.line(false)));
        }
        lines
    }

    fn notes(&self, kept: &[&str]) {
        for note in &self.kept {
            println!("{note}");
        }
        if !self.leftover.is_empty() {
            println!(
                "Left in place, since setup does not write them in this form. Edit them by hand if they still run Justless:"
            );
            for (file, at) in &self.leftover {
                if at.is_empty() {
                    println!("  {file}");
                } else {
                    println!("  {file} {at}");
                }
            }
        }
        if !kept.is_empty() {
            println!(
                "Kept {}. Add --purge to remove {} too.",
                kept.join(" and "),
                if kept.len() == 1 { "it" } else { "them" }
            );
        }
    }
}

fn plan(workspace: &Workspace, chosen: &[Agent], purge: bool, record: &Record) -> Result<Plan> {
    let root = &workspace.root;
    let mut plan = Plan {
        root: root.clone(),
        ..Plan::default()
    };

    // Instruction files, grouped by the file each client reads, so a link such as
    // `CLAUDE.md -> AGENTS.md` is handled once, as setup does. Claude Code also
    // reads the `AGENTS.md` that `CLAUDE.md` imports.
    let mut readers: Vec<(PathBuf, Vec<Agent>)> = Vec::new();
    if let Some((_, target)) = agents::imported_instructions(root, false) {
        readers.push((target, vec![Agent::Claude]));
    }
    for agent in all_agents() {
        let filename = agent.instructions();
        let file = root.join(filename);
        let resolved = agents::instructions_file(root, &file);
        let resolved = if chosen.contains(&agent) {
            resolved.with_context(|| format!("inspect {filename}"))?
        } else {
            // A client cannot read a link that does not resolve, such as one to
            // itself, so it shares no file with the chosen clients.
            let Ok(resolved) = resolved else {
                continue;
            };
            resolved
        };
        match resolved {
            Some(path) => match readers.iter_mut().find(|(file, _)| *file == path) {
                Some((_, owners)) => owners.push(agent),
                None => readers.push((path, vec![agent])),
            },
            None if chosen.contains(&agent)
                && !plan.skipped.contains(&filename)
                && holds_section(&file) =>
            {
                plan.skipped.push(filename);
            }
            None => {}
        }
    }
    for (path, owners) in readers {
        if !owners.iter().any(|owner| chosen.contains(owner)) {
            continue;
        }
        let key = key(root, &path)?;
        let Some(change) = plan_file(
            workspace,
            record,
            &key,
            &path,
            Kind::Instructions,
            &mut plan.leftover,
        )?
        else {
            continue;
        };
        // Keep a shared section while another client that reads it is still set up.
        let others: Vec<&str> = owners
            .iter()
            .copied()
            .filter(|&owner| !chosen.contains(&owner) && configured(workspace, record, owner, &key))
            .map(name)
            .collect();
        if others.is_empty() {
            plan.changes.push(change);
        } else {
            plan.kept.push(format!(
                "Kept the Justless section in {key}, which {} also {}.",
                join(&others),
                if others.len() == 1 { "reads" } else { "read" }
            ));
        }
    }

    let mut seen = BTreeSet::new();
    for (key, _, agent) in catalog() {
        if !chosen.contains(&agent) || !seen.insert(key.clone()) {
            continue;
        }
        let path = root.join(&key);
        if let Some(change) = plan_file(
            workspace,
            record,
            &key,
            &path,
            kind_of(&key),
            &mut plan.leftover,
        )? {
            plan.changes.push(change);
        }
    }

    if purge {
        let path = root.join(".gitignore");
        if let Some(change) = plan_file(
            workspace,
            record,
            ".gitignore",
            &path,
            Kind::Ignore,
            &mut plan.leftover,
        )? {
            plan.changes.push(change);
        }
        plan.state = match fs::symlink_metadata(&workspace.state) {
            Ok(metadata) if metadata.is_dir() => Some(Purge::Directory),
            Ok(metadata) if metadata.file_type().is_symlink() => Some(Purge::Link),
            Ok(_) => bail!(
                ".justless is not a directory, so --purge would not delete it. Move it away, or run without --purge"
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("inspect .justless"),
        };
    }

    plan.directories = empty_directories(root, chosen, record, &plan.changes);
    Ok(plan)
}

/// The directories removal leaves empty and deletes, deepest first. A directory
/// goes when setup created it, or when every deleted file in it is one setup has
/// no record of, as in a fresh clone. A directory setup created that is already
/// empty also goes, such as one left by a removal that stopped partway.
fn empty_directories(
    root: &Path,
    chosen: &[Agent],
    record: &Record,
    changes: &[Change],
) -> Vec<PathBuf> {
    let deleted: Vec<&Path> = changes
        .iter()
        .filter(|change| change.config && change.outcome == Outcome::Delete)
        .map(|change| change.path.as_path())
        .collect();
    let mut candidates: BTreeSet<PathBuf> = deleted
        .iter()
        .flat_map(|path| path.ancestors().skip(1))
        .filter(|dir| dir.starts_with(root) && *dir != root)
        .map(Path::to_path_buf)
        .collect();
    let files = catalog();
    for directory in &record.directories {
        let prefix = format!("{directory}/");
        if files
            .iter()
            .any(|(file, _, agent)| chosen.contains(agent) && file.starts_with(&prefix))
        {
            candidates.insert(root.join(directory));
        }
    }
    let mut candidates: Vec<PathBuf> = candidates.into_iter().collect();
    candidates.sort_by_key(|dir| Reverse(dir.components().count()));
    let mut removed: Vec<PathBuf> = Vec::new();
    for directory in candidates {
        let created = key(root, &directory).is_ok_and(|name| record.directories.contains(&name));
        let unrecorded = || {
            let mut inside = deleted
                .iter()
                .filter(|path| path.starts_with(&directory))
                .peekable();
            inside.peek().is_some()
                && inside
                    .all(|path| key(root, path).is_ok_and(|name| !record.files.contains_key(&name)))
        };
        let directory_here = real(root, &directory)
            .ok()
            .flatten()
            .is_some_and(|meta| meta.is_dir());
        if !(created || unrecorded()) || !directory_here {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let empty = entries.into_iter().all(|entry| {
            entry.is_ok_and(|entry| {
                let path = entry.path();
                deleted.contains(&path.as_path()) || removed.contains(&path)
            })
        });
        if empty {
            removed.push(directory);
        }
    }
    removed
}

/// Whether an instructions file outside the project, reached through a link, may
/// hold a Justless section.
fn holds_section(path: &Path) -> bool {
    match fs::read_to_string(path) {
        Ok(text) => text.contains(START),
        Err(error) => error.kind() == ErrorKind::PermissionDenied,
    }
}

/// The change that takes Justless out of one file, or `None` when it holds nothing
/// of Justless's. Mentions of Justless it leaves are added to `leftover`.
fn plan_file(
    workspace: &Workspace,
    record: &Record,
    key: &str,
    path: &Path,
    kind: Kind,
    leftover: &mut Vec<(String, String)>,
) -> Result<Option<Change>> {
    if !regular_file(&workspace.root, path)? {
        return Ok(None);
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {key}"))?;
    let scan = strip(key, kind, &text)?;
    leftover.extend(scan.leftover.into_iter().map(|at| (key.to_owned(), at)));
    let recorded = record
        .files
        .get(key)
        .filter(|entry| entry.digest == digest(&text));
    let (what, outcome) = match (scan.found, recorded) {
        (None, None) => return Ok(None),
        (Some(found), Some(entry)) => (found.what, entry.restore()),
        (Some(found), None) => (found.what, found.outcome),
        (None, Some(entry)) => (vec!["the changes Justless made".into()], entry.restore()),
    };
    if outcome == Outcome::Write(text) {
        return Ok(None);
    }
    Ok(Some(Change {
        key: key.to_owned(),
        path: path.to_path_buf(),
        what,
        outcome,
        whole: matches!(kind, Kind::Plugin),
        config: matches!(kind, Kind::Json(_) | Kind::Toml | Kind::Plugin),
        instructions: matches!(kind, Kind::Instructions),
    }))
}

/// Whether a client is still set up to read the instructions `file`: `init`
/// recorded it, or it has Justless hooks, its plugin or an MCP server here. A
/// file that cannot be read counts, so a shared section is kept rather than lost.
fn configured(workspace: &Workspace, record: &Record, agent: Agent, file: &str) -> bool {
    record
        .readers
        .get(file)
        .is_some_and(|readers| readers.contains(name(agent)))
        || catalog()
            .into_iter()
            .filter(|(_, _, owner)| *owner == agent)
            .any(|(key, _, _)| {
                let path = workspace.root.join(&key);
                match regular_file(&workspace.root, &path) {
                    Ok(false) => false,
                    Ok(true) => fs::read_to_string(&path).map_or(true, |text| {
                        strip(&key, kind_of(&key), &text).map_or(true, |scan| scan.found.is_some())
                    }),
                    Err(_) => true,
                }
            })
}

/// The metadata of `path` when it is reached through real directories, or `None`
/// when it or a directory on the way is missing or a link. Setup never writes
/// through a link, so a linked file or directory holds nothing of Justless's.
fn real(root: &Path, path: &Path) -> Result<Option<fs::Metadata>> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let parts: Vec<Component> = relative.components().collect();
    let mut current = root.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if index + 1 == parts.len() => {
                return Ok((!metadata.file_type().is_symlink()).then_some(metadata));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", current.display()));
            }
        }
    }
    Ok(None)
}

/// Whether `path` is a regular file reached through real directories.
fn regular_file(root: &Path, path: &Path) -> Result<bool> {
    Ok(real(root, path)?.is_some_and(|metadata| metadata.is_file()))
}

// ---------------------------------------------------------------------------
// Finding Justless in one file

/// Whether text mentions Justless in any letter case.
fn mentions(text: &str) -> bool {
    text.to_ascii_lowercase().contains("justless")
}

fn strip(key: &str, kind: Kind, text: &str) -> Result<Scan> {
    let rest = |rest: String| {
        if rest.is_empty() {
            Outcome::Delete
        } else {
            Outcome::Write(rest)
        }
    };
    let found = |found: Option<Found>| Scan {
        found,
        leftover: Vec::new(),
    };
    Ok(match kind {
        Kind::Instructions => found(strip_section(key, text)?.map(|left| Found {
            what: vec!["the Justless section".into()],
            outcome: rest(left),
        })),
        Kind::Ignore => found(strip_ignore(text).map(|left| Found {
            what: vec![format!("the {IGNORE_LINE} line")],
            outcome: rest(left),
        })),
        Kind::Plugin if text.starts_with(PLUGIN_HEADER) => found(Some(Found {
            what: vec!["the Justless plugin".into()],
            outcome: Outcome::Delete,
        })),
        // A plugin Justless did not write, such as one the user adapted.
        Kind::Plugin => Scan {
            found: None,
            leftover: if mentions(text) {
                vec![String::new()]
            } else {
                Vec::new()
            },
        },
        // A config file that never mentions Justless holds nothing to remove, even
        // in a form such as JSON with comments that removal cannot edit.
        Kind::Json(_) | Kind::Toml if !mentions(text) => Scan::default(),
        Kind::Toml => strip_toml(key, text)?,
        Kind::Json(rules) => strip_json(key, rules, text)?,
    })
}

/// The text without its Justless section, checked the way setup checks it.
pub(crate) fn strip_section(key: &str, text: &str) -> Result<Option<String>> {
    let Some(start) = text.find(START) else {
        ensure!(
            !text.contains(END),
            "{key} has an incomplete Justless section"
        );
        return Ok(None);
    };
    let (prefix, rest) = (&text[..start], &text[start + START.len()..]);
    let (inside, suffix) = rest
        .split_once(END)
        .with_context(|| format!("{key} has an incomplete Justless section"))?;
    ensure!(
        !prefix.contains(END) && !suffix.contains(END) && !rest.contains(START),
        "duplicate Justless markers in {key}"
    );
    let end = start + START.len() + inside.len() + END.len();
    Ok(Some(cut(text, start, end)))
}

/// The text without its `/.justless/` lines, or `None` when it has none.
fn strip_ignore(text: &str) -> Option<String> {
    let mut text = text.to_owned();
    let mut found = false;
    loop {
        let mut offset = 0;
        let mut line = None;
        for raw in text.split_inclusive('\n') {
            let body = raw.trim_end_matches(['\n', '\r']);
            if body.trim() == IGNORE_LINE {
                line = Some(offset..offset + body.len());
                break;
            }
            offset += raw.len();
        }
        let Some(line) = line else {
            break;
        };
        text = cut(&text, line.start, line.end);
        found = true;
    }
    found.then_some(text)
}

/// Remove `start..end`, a block that begins a line, with the line break after it.
/// At the end of the file it also takes the blank line `init` adds before an
/// appended block. Between blank lines it takes one of them.
fn cut(text: &str, start: usize, end: usize) -> String {
    let line_break = |text: &str| -> usize {
        if text.starts_with("\r\n") {
            2
        } else {
            usize::from(text.starts_with('\n'))
        }
    };
    let prefix = &text[..start];
    let mut suffix = &text[end..];
    suffix = &suffix[line_break(suffix)..];
    if suffix.is_empty() {
        let prefix = ["\r\n", "\n"]
            .into_iter()
            .find_map(|eol| prefix.strip_suffix(eol).filter(|left| left.ends_with(eol)))
            .unwrap_or(prefix);
        return prefix.to_owned();
    }
    if prefix.is_empty() || prefix.ends_with("\n\n") || prefix.ends_with("\n\r\n") {
        suffix = &suffix[line_break(suffix)..];
    }
    format!("{prefix}{suffix}")
}

/// Whether a program path names the Justless binary, with `/` or `\` separators.
fn is_justless(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    name == "justless" || name.eq_ignore_ascii_case("justless.exe")
}

/// A shell `NAME=value` assignment, as in `JUSTLESS_LOCK_TIMEOUT=1 justless …`.
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
    })
}

/// The words after the program of a shell command whose program is `justless`,
/// after any leading assignments or `env`, or `None`. Words are split as the shell
/// splits them, so a quoted path may hold spaces, and as plain whitespace for a
/// Windows path whose backslashes the shell would read as escapes.
fn justless_args(command: Option<&str>) -> Option<Vec<String>> {
    let command = command?;
    let plain = command
        .split_whitespace()
        .map(|word| word.trim_matches(['"', '\'']).to_owned())
        .collect();
    [shlex::split(command), Some(plain)]
        .into_iter()
        .flatten()
        .find_map(|words| {
            let mut words = words
                .into_iter()
                .skip_while(|word| word == "env" || assignment(word));
            let program = words.next()?;
            is_justless(&program).then(|| words.collect())
        })
}

/// The subcommand in Justless arguments: the first word that is neither an option
/// nor the value of the global `--root`.
fn subcommand(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--root" {
            args.next();
        } else if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

/// A hook handler that runs `justless hook-CLIENT`, in the `command` field most
/// clients use or Copilot's `bash`.
fn is_hook(handler: &Value) -> bool {
    ["command", "bash"].into_iter().any(|field| {
        justless_args(handler.get(field).and_then(Value::as_str)).is_some_and(|args| {
            subcommand(&args)
                .and_then(|name| name.strip_prefix("hook-"))
                .is_some_and(|client| AGENTS.contains(&client))
        })
    })
}

/// An MCP server entry that runs `justless mcp`. Clients start `command` directly,
/// so a path with spaces needs no quotes there.
fn is_server(command: Option<&str>, args: &[&str]) -> bool {
    let Some(command) = command else {
        return false;
    };
    let mut words = if is_justless(command.trim()) {
        Vec::new()
    } else {
        match justless_args(Some(command)) {
            Some(words) => words,
            None => return false,
        }
    };
    words.extend(args.iter().map(|arg| (*arg).to_owned()));
    subcommand(&words) == Some("mcp")
}

/// A JSON pointer, as in `/hooks/PreToolUse/0/command`.
fn pointer(path: &[Step]) -> String {
    path.iter()
        .map(|step| match step {
            Step::Key(name) => format!("/{}", name.replace('~', "~0").replace('/', "~1")),
            Step::Index(index) => format!("/{index}"),
        })
        .collect()
}

/// A path into the original document as it reads once `deleted` are gone: each
/// array index drops by the deleted elements before it.
fn shifted(path: &[Step], deleted: &[Vec<Step>]) -> Vec<Step> {
    let mut shifted = path.to_vec();
    for (depth, step) in path.iter().enumerate() {
        let Step::Index(index) = step else {
            continue;
        };
        let before = deleted
            .iter()
            .filter(|gone| {
                gone.len() == depth + 1
                    && gone[..depth] == path[..depth]
                    && matches!(gone[depth], Step::Index(other) if other < *index)
            })
            .count();
        shifted[depth] = Step::Index(index - before);
    }
    shifted
}

/// Add the pointer of each string under `value` that mentions Justless and is not
/// deleted, or of a key that does when nothing under it does, as the file reads
/// after the deletions.
fn mentions_at(value: &Value, at: Vec<Step>, deleted: &[Vec<Step>], found: &mut Vec<String>) {
    if deleted.iter().any(|path| at.starts_with(path)) {
        return;
    }
    match value {
        Value::String(text) if mentions(text) => found.push(pointer(&shifted(&at, deleted))),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let mut next = at.clone();
                next.push(Step::Index(index));
                mentions_at(item, next, deleted, found);
            }
        }
        Value::Object(members) => {
            for (name, member) in members {
                let mut next = at.clone();
                next.push(Step::Key(name.clone()));
                let before = found.len();
                mentions_at(member, next.clone(), deleted, found);
                if found.len() == before
                    && mentions(name)
                    && !deleted.iter().any(|path| next.starts_with(path))
                {
                    found.push(pointer(&shifted(&next, deleted)));
                }
            }
        }
        _ => {}
    }
}

fn strip_json(key: &str, rules: JsonRules, text: &str) -> Result<Scan> {
    let config: Value = serde_json::from_str(text)
        .map_err(|error| anyhow!("{key} is not valid JSON; {FIX}: {error}"))?;
    let root = config
        .as_object()
        .with_context(|| format!("{key} must contain a JSON object"))?;
    let mut paths: Vec<Vec<Step>> = Vec::new();
    let mut removed = BTreeSet::new();
    let (mut hooks, mut status_line, mut server) = (0, false, false);
    for (name, value) in root {
        let member = || vec![Step::Key(name.clone())];
        let group = (rules.hooks && name == "hooks") || (rules.group && name == "justless");
        if group && let Some(events) = value.as_object() {
            let start = paths.len();
            let (found, emptied) = prune_group(events, name, &mut paths);
            hooks += found;
            if emptied {
                paths.truncate(start);
                paths.push(member());
                removed.insert(name.as_str());
            }
        } else if rules.status_line
            && name == "statusLine"
            && justless_args(value.get("command").and_then(Value::as_str)).is_some()
        {
            status_line = true;
            paths.push(member());
            removed.insert(name.as_str());
        } else if rules.servers == Some(name.as_str())
            && let Some(servers) = value.as_object()
            && let Some(entry) = servers.get("justless")
        {
            let args: Vec<&str> = entry
                .get("args")
                .and_then(Value::as_array)
                .map(|args| args.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !is_server(entry.get("command").and_then(Value::as_str), &args) {
                continue;
            }
            server = true;
            if servers.len() == 1 {
                paths.push(member());
                removed.insert(name.as_str());
            } else {
                paths.push(vec![Step::Key(name.clone()), Step::Key("justless".into())]);
            }
        }
    }
    // Hooks, the status line and servers in a form setup does not write stay, so
    // name any that still mention Justless.
    let mut leftover = Vec::new();
    for (name, value) in root {
        let scanned = (rules.hooks && name == "hooks")
            || (rules.group && name == "justless")
            || (rules.status_line && name == "statusLine")
            || rules.servers == Some(name.as_str());
        if scanned {
            mentions_at(value, vec![Step::Key(name.clone())], &paths, &mut leftover);
        }
    }
    if paths.is_empty() {
        return Ok(Scan {
            found: None,
            leftover,
        });
    }
    let mut what = Vec::new();
    if hooks > 0 {
        what.push(format!("{hooks} hook{}", if hooks == 1 { "" } else { "s" }));
    }
    if status_line {
        what.push("the status line".into());
    }
    if server {
        what.push("the justless MCP server".into());
    }
    let left: Vec<&str> = root
        .keys()
        .map(String::as_str)
        .filter(|name| !removed.contains(name))
        .collect();
    // Setup creates a Cursor or Copilot hook file with `"version": 1`.
    let outcome =
        if left.is_empty() || rules.versioned && left == ["version"] && root["version"] == 1 {
            Outcome::Delete
        } else {
            let edited = json_edit::delete(text, &paths).with_context(|| format!("edit {key}"))?;
            let mut expected = config.clone();
            json_edit::delete_value(&mut expected, &paths);
            ensure!(
                serde_json::from_str::<Value>(&edited).ok() == Some(expected),
                "could not edit {key} without changing its other settings"
            );
            Outcome::Write(edited)
        };
    Ok(Scan {
        found: Some(Found { what, outcome }),
        leftover,
    })
}

/// Add the paths that delete Justless handlers from one group of hook events, such
/// as `hooks`. A handler shares an entry with other handlers in some clients, so
/// only the handler goes unless the entry holds nothing else. Returns how many
/// handlers were found and whether every event of the group was emptied.
fn prune_group(
    events: &Map<String, Value>,
    group: &str,
    paths: &mut Vec<Vec<Step>>,
) -> (usize, bool) {
    let mut found = 0;
    let mut emptied = 0;
    for (event, entries) in events {
        let Some(entries) = entries.as_array() else {
            continue;
        };
        let prefix = vec![Step::Key(group.into()), Step::Key(event.clone())];
        let start = paths.len();
        let mut dropped = 0;
        for (index, entry) in entries.iter().enumerate() {
            let mut at = prefix.clone();
            at.push(Step::Index(index));
            if is_hook(entry) {
                found += 1;
                dropped += 1;
                paths.push(at);
                continue;
            }
            let Some(handlers) = entry.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            let ours: Vec<usize> = (0..handlers.len())
                .filter(|&index| is_hook(&handlers[index]))
                .collect();
            found += ours.len();
            if !ours.is_empty() && ours.len() == handlers.len() {
                dropped += 1;
                paths.push(at);
            } else {
                for index in ours {
                    let mut handler = at.clone();
                    handler.extend([Step::Key("hooks".into()), Step::Index(index)]);
                    paths.push(handler);
                }
            }
        }
        if dropped > 0 && dropped == entries.len() {
            paths.truncate(start);
            paths.push(prefix);
            emptied += 1;
        }
    }
    (found, emptied > 0 && emptied == events.len())
}

fn strip_toml(key: &str, text: &str) -> Result<Scan> {
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| anyhow!("{key} is not valid TOML; {FIX}: {error}"))?;
    let table = document.as_table_mut();
    let Some(servers) = table
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(Scan::default());
    };
    let ours = servers
        .get("justless")
        .and_then(Item::as_table_like)
        .is_some_and(|entry| {
            let args: Vec<&str> = entry
                .get("args")
                .and_then(Item::as_array)
                .map(|args| args.iter().filter_map(|arg| arg.as_str()).collect())
                .unwrap_or_default();
            is_server(entry.get("command").and_then(Item::as_str), &args)
        });
    let leftover: Vec<String> = servers
        .iter()
        .filter(|(name, item)| {
            !(ours && *name == "justless") && (mentions(name) || mentions(&item.to_string()))
        })
        .map(|(name, _)| format!("mcp_servers.{name}"))
        .collect();
    if !ours {
        return Ok(Scan {
            found: None,
            leftover,
        });
    }
    servers.remove("justless");
    // Setup adds an implicit `mcp_servers` table, one with no header of its own.
    if servers.is_empty()
        && table
            .get("mcp_servers")
            .and_then(Item::as_table)
            .is_some_and(|servers| servers.is_implicit())
    {
        table.remove("mcp_servers");
    }
    let edited = document.to_string();
    Ok(Scan {
        found: Some(Found {
            what: vec!["the justless MCP server".into()],
            outcome: if edited.trim().is_empty() {
                Outcome::Delete
            } else {
                Outcome::Write(edited)
            },
        }),
        leftover,
    })
}

fn join(items: &[impl AsRef<str>]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_owned(),
        [first @ .., last] => format!(
            "{} and {}",
            first
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "<!-- justless:begin -->\nrules\n<!-- justless:end -->";

    #[test]
    fn a_removed_section_takes_the_separator_setup_added() {
        let strip = |text: &str| strip_section("AGENTS.md", text).unwrap();
        assert_eq!(strip(&format!("{BLOCK}\n")).as_deref(), Some(""));
        assert_eq!(
            strip(&format!("# Team\n\n{BLOCK}\n")).as_deref(),
            Some("# Team\n")
        );
        assert_eq!(
            strip(&format!("# Team\n\n\n{BLOCK}\n")).as_deref(),
            Some("# Team\n\n")
        );
        assert_eq!(
            strip(&format!("# A\n\n{BLOCK}\n\n# B\n")).as_deref(),
            Some("# A\n\n# B\n")
        );
        assert_eq!(
            strip(&format!("# A\n{BLOCK}\n# B\n")).as_deref(),
            Some("# A\n# B\n")
        );
        assert_eq!(
            strip(&format!("# A\r\n\r\n{BLOCK}\r\n")).as_deref(),
            Some("# A\r\n")
        );
        assert_eq!(strip("# Only\n"), None);
        assert!(strip_section("AGENTS.md", "<!-- justless:begin -->\n").is_err());
        assert!(strip_section("AGENTS.md", "<!-- justless:end -->\n").is_err());
        assert!(strip_section("AGENTS.md", &format!("{BLOCK}\n{BLOCK}\n")).is_err());
    }

    #[test]
    fn ignore_lines_and_commands_are_matched_exactly() {
        assert_eq!(
            strip_ignore("target\n\n/.justless/\n").as_deref(),
            Some("target\n")
        );
        assert_eq!(strip_ignore("/.justless/\n").as_deref(), Some(""));
        assert_eq!(
            strip_ignore("a\n/.justless/\nb\n").as_deref(),
            Some("a\nb\n")
        );
        assert_eq!(strip_ignore(".justless/\n"), None);
        assert!(is_hook(
            &serde_json::json!({"command":"justless hook-claude before-command"})
        ));
        assert!(is_hook(
            &serde_json::json!({"bash":"/opt/bin/justless hook-copilot session-start || true"})
        ));
        assert!(!is_hook(
            &serde_json::json!({"command":"justless run -- cargo test"})
        ));
        assert!(!is_hook(
            &serde_json::json!({"command":"notjustless hook-claude x"})
        ));
        assert!(is_server(Some("justless"), &["mcp", "--root", "x"]));
        assert!(!is_server(Some("node"), &["mcp"]));
    }

    #[test]
    fn hooks_match_in_every_shell_form_and_only_hook_subcommands() {
        let hook = |command: &str| is_hook(&serde_json::json!({ "command": command }));
        for command in [
            "\"/Users/Jane Doe/.cargo/bin/justless\" hook-claude before-command",
            "JUSTLESS_LOCK_TIMEOUT=1 justless hook-claude before-command",
            "env A=1 B=2 justless hook-codex session-start",
            "\"$CLAUDE_PROJECT_DIR\"/bin/justless hook-claude after-read",
            "justless --root /work/app hook-claude before-command",
            "justless --root=/work/app hook-gemini before-command",
            "C:\\Users\\me\\.cargo\\bin\\justless.exe hook-claude before-command",
        ] {
            assert!(hook(command), "{command}");
        }
        for command in [
            "justless run -- npm run hook-setup",
            "justless hook-unknown before-command",
            "justless statusline",
            "input=$(cat); printf '%s' \"$input\" | { ~/bin/s.sh; }; printf '%s' \"$input\" | justless statusline",
            "sh -c 'justless hook-claude before-command'",
            "justless",
        ] {
            assert!(!hook(command), "{command}");
        }
        assert!(is_server(
            Some("/Users/Jane Doe/.cargo/bin/justless"),
            &["mcp"]
        ));
        assert!(is_server(Some("justless mcp"), &[]));
        assert!(is_server(Some("justless"), &["--root", "mcp", "mcp"]));
        assert!(!is_server(Some("justless"), &["run", "--", "mcp"]));
    }

    #[test]
    fn unmatched_mentions_are_named_by_pointer() {
        let text = serde_json::json!({
            "permissions": {"allow": ["Bash(justless:*)"]},
            "statusLine": {"type": "command", "command": "input=$(cat); echo \"$input\" | justless statusline"},
            "hooks": {"PreToolUse": [
                {"matcher": "^Bash$", "hooks": [{"type": "command", "command": "justless hook-claude before-command"}]},
                {"matcher": "^Bash$", "hooks": [{"type": "command", "command": "sh -c 'justless hook-claude before-command'"}]}
            ]},
            "mcpServers": {"justless-dev": {"command": "node", "args": ["server.js"]}, "a/b~": {"command": "Justless"}}
        })
        .to_string();
        let rules = JsonRules {
            hooks: true,
            status_line: true,
            servers: Some("mcpServers"),
            ..JsonRules::default()
        };
        let scan = strip_json("s", rules, &text).unwrap();
        assert_eq!(scan.found.unwrap().what, ["1 hook"]);
        assert_eq!(
            scan.leftover,
            [
                "/statusLine/command",
                "/hooks/PreToolUse/0/hooks/0/command",
                "/mcpServers/justless-dev",
                "/mcpServers/a~1b~0/command",
            ]
        );
        // A file that never mentions Justless is not parsed at all.
        let scan = strip("s", Kind::Json(rules), "{ // comment\n \"a\": 1, }").unwrap();
        assert!(scan.found.is_none() && scan.leftover.is_empty());
        let error = strip("s", Kind::Json(rules), "{ // justless\n}")
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .starts_with(&format!("s is not valid JSON; {FIX}: ")),
            "{error}"
        );
        let toml = "[mcp_servers.justless]\ncommand = \"justless\"\nargs = [\"mcp\"]\n\n[mcp_servers.dev]\ncommand = \"/opt/justless-dev/bin/server\"\n";
        let scan = strip_toml("c", toml).unwrap();
        assert!(scan.found.is_some());
        assert_eq!(scan.leftover, ["mcp_servers.dev"]);
    }

    #[test]
    fn the_catalog_matches_setup() {
        let keys: Vec<String> = catalog().into_iter().map(|(key, _, _)| key).collect();
        for expected in [
            ".claude/settings.json",
            ".codex/hooks.json",
            ".agents/hooks.json",
            ".opencode/plugins/justless.js",
            ".cursor/hooks.json",
            ".gemini/settings.json",
            ".github/hooks/justless.json",
            ".mcp.json",
            ".cursor/mcp.json",
            ".vscode/mcp.json",
            ".codex/config.toml",
        ] {
            assert!(keys.iter().any(|key| key == expected), "{expected}");
        }
        assert!(matches!(
            kind_of(".gemini/settings.json"),
            Kind::Json(JsonRules {
                hooks: true,
                servers: Some("mcpServers"),
                ..
            })
        ));
        assert!(matches!(kind_of("docs/AGENTS.md"), Kind::Instructions));
        assert_eq!(name(Agent::OpenCode), "opencode");
    }

    #[test]
    fn toml_removal_keeps_other_servers_and_formatting() {
        let original =
            "# Codex\nmodel = \"o\"   # pinned\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let mut document = original.parse::<DocumentMut>().unwrap();
        let servers = document["mcp_servers"].as_table_like_mut().unwrap();
        let mut table = toml_edit::Table::new();
        table.insert("command", toml_edit::value("justless"));
        table.insert(
            "args",
            toml_edit::value(["mcp"].into_iter().collect::<toml_edit::Array>()),
        );
        servers.insert("justless", Item::Table(table));
        let installed = document.to_string();
        let found = strip_toml("c", &installed).unwrap().found.unwrap();
        assert_eq!(found.outcome, Outcome::Write(original.into()));
        assert!(strip_toml("c", original).unwrap().found.is_none());
    }
}
