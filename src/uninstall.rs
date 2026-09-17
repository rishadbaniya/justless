//! `justless init --remove`: take Justless back out of a project.
//!
//! Setup records each file it writes in `.justless/setup.json`: a digest of the
//! bytes it wrote and the file as it was without Justless, or that it did not
//! exist. A file that still holds exactly what Justless wrote is restored from that
//! record byte for byte. Any other file, such as one edited since, one set up by an
//! older version or one in a fresh clone, is edited in place: only the instruction
//! section, Justless hook handlers, a status line that runs `justless` and the
//! `justless` MCP server are deleted, and every other byte stays.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
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
    remove(
        workspace,
        selected.as_deref(),
        matches.is_present("dry-run"),
        purge,
    )
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
    let guard = if state && !dry_run {
        Some(lock_state(&workspace.state.join("setup.lock"))?)
    } else {
        None
    };
    let mut record = if state {
        load(workspace)
    } else {
        Record::default()
    };
    // Every file is read and checked before any is changed.
    let plan = plan(workspace, selected, purge, &record).context("nothing was removed")?;
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
    if !dry_run && !plan.is_empty() {
        apply(workspace, &plan, &mut record, guard, state)?;
    }

    for filename in &plan.skipped {
        eprintln!(
            "Skipped {filename}, a link to a file outside the project. Remove the Justless section from its target yourself."
        );
    }
    let root = workspace.root.display();
    if plan.is_empty() {
        println!("Nothing to remove in {root}.");
    } else if dry_run {
        println!("Dry run, nothing changed. Removing Justless from {root} would change:");
    } else {
        println!("Removed Justless from {root}:");
    }
    for line in plan.lines(dry_run) {
        println!("  {line}");
    }
    for note in &plan.kept {
        println!("{note}");
    }
    if !kept.is_empty() {
        println!(
            "Kept {}. Add --purge to remove them too.",
            kept.join(" and ")
        );
    }
    if !dry_run && plan.changes.iter().any(|change| change.config) {
        println!("Start a new agent session so the clients stop loading Justless.");
    }
    Ok(())
}

fn apply(
    workspace: &Workspace,
    plan: &Plan,
    record: &mut Record,
    guard: Option<fs::File>,
    state: bool,
) -> Result<()> {
    for change in &plan.changes {
        match &change.outcome {
            Outcome::Write(text) => write_text(&change.path, text),
            Outcome::Delete => fs::remove_file(&change.path).map_err(Into::into),
        }
        .with_context(|| format!("remove Justless from {}", change.key))?;
    }
    for directory in &plan.directories {
        fs::remove_dir(directory).with_context(|| format!("remove {}", directory.display()))?;
    }
    if plan.state {
        // Release the setup lock, which lives in the directory.
        drop(guard);
        fs::remove_dir_all(&workspace.state).context("delete .justless/")?;
    } else if state {
        let before = record.files.len();
        for change in &plan.changes {
            record.files.remove(&change.key);
        }
        if record.files.len() != before {
            // A stale entry never matches the edited file, so this is best effort.
            let _ = save(workspace, record);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Setup record

#[derive(Default, Serialize, Deserialize)]
struct Record {
    files: BTreeMap<String, Written>,
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

/// Note that setup is about to replace `path` with `contents`. Setup calls this
/// with the setup lock held. A missing record only costs exactness when Justless
/// is removed, so a failure here never stops setup.
pub fn remember(workspace: &Workspace, path: &Path, contents: &str) {
    let _ = try_remember(workspace, path, contents);
}

fn try_remember(workspace: &Workspace, path: &Path, contents: &str) -> Result<()> {
    let key = key(&workspace.root, path)?;
    let current = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut record = load(workspace);
    let before = match (&current, record.files.get(&key)) {
        (None, _) => None,
        // Justless wrote the current bytes, so the earlier original still applies.
        (Some(text), Some(entry)) if entry.digest == digest(text) => entry.before.clone(),
        // Otherwise the original is the current file without any Justless setup
        // already in it, which is the file itself in the usual case.
        (Some(text), _) => match strip(&key, kind_of(&key), text) {
            Ok(None) => Some(text.clone()),
            Ok(Some(found)) => match found.outcome {
                Outcome::Write(text) => Some(text),
                Outcome::Delete => None,
            },
            Err(_) => {
                record.files.remove(&key);
                return save(workspace, &record);
            }
        },
    };
    record.files.insert(
        key,
        Written {
            digest: digest(contents),
            before,
        },
    );
    save(workspace, &record)
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
    /// Instruction files that link outside the project.
    skipped: Vec<&'static str>,
    /// Delete `.justless/`.
    state: bool,
    root: PathBuf,
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

impl Plan {
    fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.directories.is_empty() && !self.state
    }

    fn lines(&self, dry_run: bool) -> Vec<String> {
        let (remove, delete) = if dry_run {
            ("remove", "delete")
        } else {
            ("removed", "deleted")
        };
        let mut lines = Vec::new();
        for change in &self.changes {
            let what = join(&change.what);
            lines.push(match change.outcome {
                Outcome::Delete if change.whole => format!("{}: {delete} {what}", change.key),
                Outcome::Delete => format!(
                    "{}: {remove} {what} and {delete} the file, which held nothing else",
                    change.key
                ),
                Outcome::Write(_) => format!("{}: {remove} {what}", change.key),
            });
        }
        for directory in &self.directories {
            let name = key(&self.root, directory).unwrap_or_default();
            lines.push(format!("{name}/: {remove} the empty directory"));
        }
        if self.state {
            lines.push(format!(
                ".justless/: {delete} the archived runs, index and history"
            ));
        }
        lines
    }
}

fn plan(
    workspace: &Workspace,
    selected: Option<&[Agent]>,
    purge: bool,
    record: &Record,
) -> Result<Plan> {
    let root = &workspace.root;
    let everyone = all_agents();
    let chosen = selected.map_or_else(|| everyone.clone(), <[Agent]>::to_vec);
    let mut plan = Plan {
        root: root.clone(),
        ..Plan::default()
    };

    // Instruction files, grouped by the file each client reads, so a link such as
    // `CLAUDE.md -> AGENTS.md` is handled once, as setup does.
    let mut readers: Vec<(PathBuf, Vec<Agent>)> = Vec::new();
    for &agent in &everyone {
        let filename = agent.instructions();
        match agents::instructions_file(root, &root.join(filename))? {
            Some(path) => match readers.iter_mut().find(|(file, _)| *file == path) {
                Some((_, owners)) => owners.push(agent),
                None => readers.push((path, vec![agent])),
            },
            None if chosen.contains(&agent) && !plan.skipped.contains(&filename) => {
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
        let Some(change) = plan_file(workspace, record, &key, &path, Kind::Instructions)? else {
            continue;
        };
        // Keep a shared section while another client that reads it is still set up.
        let others: Vec<&str> = owners
            .iter()
            .filter(|owner| !chosen.contains(owner) && configured(workspace, **owner))
            .map(|owner| name(*owner))
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
        if let Some(change) = plan_file(workspace, record, &key, &path, kind_of(&key))? {
            plan.changes.push(change);
        }
    }

    if purge {
        let path = root.join(".gitignore");
        if let Some(change) = plan_file(workspace, record, ".gitignore", &path, Kind::Ignore)? {
            plan.changes.push(change);
        }
        plan.state = fs::symlink_metadata(&workspace.state).is_ok();
    }

    // Remove the client directories that only held deleted files, deepest first.
    let deleted: BTreeSet<&Path> = plan
        .changes
        .iter()
        .filter(|change| change.config && change.outcome == Outcome::Delete)
        .map(|change| change.path.as_path())
        .collect();
    let mut candidates: Vec<PathBuf> = deleted
        .iter()
        .flat_map(|path| path.ancestors().skip(1))
        .filter(|dir| dir.starts_with(root) && *dir != root.as_path())
        .map(Path::to_path_buf)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    candidates.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for directory in candidates {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let empty = entries.into_iter().all(|entry| {
            entry.is_ok_and(|entry| {
                let path = entry.path();
                deleted.contains(path.as_path()) || plan.directories.contains(&path)
            })
        });
        if empty {
            plan.directories.push(directory);
        }
    }
    Ok(plan)
}

/// The change that takes Justless out of one file, or `None` when it holds nothing
/// of Justless's.
fn plan_file(
    workspace: &Workspace,
    record: &Record,
    key: &str,
    path: &Path,
    kind: Kind,
) -> Result<Option<Change>> {
    if !regular_file(&workspace.root, path)? {
        return Ok(None);
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {key}"))?;
    let found = strip(key, kind, &text)?;
    let recorded = record
        .files
        .get(key)
        .filter(|entry| entry.digest == digest(&text));
    let (what, outcome) = match (found, recorded) {
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
    }))
}

/// Whether a client still has Justless hooks, its plugin or an MCP server here. A
/// file that cannot be read counts, so a shared section is kept rather than lost.
fn configured(workspace: &Workspace, agent: Agent) -> bool {
    catalog()
        .into_iter()
        .filter(|(_, _, owner)| *owner == agent)
        .any(|(key, _, _)| {
            let path = workspace.root.join(&key);
            match regular_file(&workspace.root, &path) {
                Ok(false) => false,
                Ok(true) => fs::read_to_string(&path).map_or(true, |text| {
                    strip(&key, kind_of(&key), &text).map_or(true, |found| found.is_some())
                }),
                Err(_) => true,
            }
        })
}

/// Whether `path` is a regular file reached through real directories. Setup never
/// writes through a link, so a linked file or directory holds nothing of Justless's.
fn regular_file(root: &Path, path: &Path) -> Result<bool> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let parts: Vec<Component> = relative.components().collect();
    let mut current = root.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if index + 1 == parts.len() => return Ok(metadata.is_file()),
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", current.display()));
            }
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Finding Justless in one file

fn strip(key: &str, kind: Kind, text: &str) -> Result<Option<Found>> {
    let rest = |rest: String| {
        if rest.is_empty() {
            Outcome::Delete
        } else {
            Outcome::Write(rest)
        }
    };
    Ok(match kind {
        Kind::Instructions => strip_section(key, text)?.map(|left| Found {
            what: vec!["the Justless section".into()],
            outcome: rest(left),
        }),
        Kind::Ignore => strip_ignore(text).map(|left| Found {
            what: vec![format!("the {IGNORE_LINE} line")],
            outcome: rest(left),
        }),
        Kind::Plugin => text.starts_with(PLUGIN_HEADER).then(|| Found {
            what: vec!["the Justless plugin".into()],
            outcome: Outcome::Delete,
        }),
        Kind::Toml => strip_toml(key, text)?,
        Kind::Json(rules) => strip_json(key, rules, text)?,
    })
}

/// The text without its Justless section, checked the way setup checks it.
fn strip_section(key: &str, text: &str) -> Result<Option<String>> {
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

/// The arguments of a command whose program is `justless`, or `None`.
fn justless_args(command: Option<&str>) -> Option<Vec<&str>> {
    let mut words = command?.split_whitespace();
    let program = words.next()?.trim_matches(['"', '\'']);
    let name = Path::new(program).file_name()?.to_str()?;
    matches!(name, "justless" | "justless.exe").then(|| words.collect())
}

/// A hook handler that runs `justless hook-…`, in the `command` field most clients
/// use or Copilot's `bash`.
fn is_hook(handler: &Value) -> bool {
    ["command", "bash"].into_iter().any(|field| {
        justless_args(handler.get(field).and_then(Value::as_str))
            .is_some_and(|args| args.iter().any(|arg| arg.starts_with("hook-")))
    })
}

fn is_server(command: Option<&str>, args: &[&str]) -> bool {
    justless_args(command).is_some_and(|words| words.contains(&"mcp") || args.contains(&"mcp"))
}

fn strip_json(key: &str, rules: JsonRules, text: &str) -> Result<Option<Found>> {
    let config: Value =
        serde_json::from_str(text).with_context(|| format!("{key} is not valid JSON"))?;
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
    if paths.is_empty() {
        return Ok(None);
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
    Ok(Some(Found { what, outcome }))
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

fn strip_toml(key: &str, text: &str) -> Result<Option<Found>> {
    let mut document = text
        .parse::<DocumentMut>()
        .with_context(|| format!("{key} is not valid TOML"))?;
    let table = document.as_table_mut();
    let Some(servers) = table
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(None);
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
    if !ours {
        return Ok(None);
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
    Ok(Some(Found {
        what: vec!["the justless MCP server".into()],
        outcome: if edited.trim().is_empty() {
            Outcome::Delete
        } else {
            Outcome::Write(edited)
        },
    }))
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
        let found = strip_toml("c", &installed).unwrap().unwrap();
        assert_eq!(found.outcome, Outcome::Write(original.into()));
        assert!(strip_toml("c", original).unwrap().is_none());
    }
}
