//! `justless doctor` checks the binary, each agent's hooks and instructions, MCP
//! entries and `.justless/` against what this version of Justless writes, and prints
//! a fix for each problem. It only reads: it never opens the index, takes a lock or
//! creates `.justless/`.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::agents::{self, AGENTS, Agent};
use crate::claude_permissions;
use crate::history::{human_bytes, now_ms};
use crate::mcp::Client;
use crate::workspace::{PROJECT_MARKERS, Settings, Workspace};

pub fn command() -> Command<'static> {
    Command::new("doctor")
        .about("Check this project's Justless setup and print a fix for each problem")
        .arg(Arg::new("json").long("json").help("Print a JSON report"))
}

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The Git commit this binary was built from, or empty outside Git. See `build.rs`.
pub const REVISION: &str = env!("JUSTLESS_REVISION");
/// Run records read for evidence that hooks route commands, newest first.
const SCANNED_RUNS: usize = 500;
/// Run directories measured for the archive size.
const MEASURED_RUNS: usize = 20_000;
/// Directories in `.justless/` that Justless creates files in.
const STATE_DIRECTORIES: &[&str] = &["", "runs", "observations", "baselines"];
/// The lock files `lock_state` opens in `.justless/`.
const LOCK_FILES: &[&str] = &[
    "activity.lock",
    "capture.lock",
    "edit.lock",
    "index.lock",
    "setup.lock",
];
/// Executable suffixes tried for a program name on PATH.
const SUFFIXES: &[&str] = if cfg!(windows) {
    &[".exe", ".cmd", ".bat"]
} else {
    &[""]
};

pub fn run(workspace: &Workspace, matches: &ArgMatches, explicit_root: bool) -> Result<i32> {
    let here = std::env::current_dir()
        .and_then(|directory| directory.canonicalize())
        .ok();
    let doctor = Doctor {
        workspace,
        explicit_root,
        at_root: here.as_deref() == Some(workspace.root.as_path()),
    };
    let report = doctor.report(here.as_deref());
    if matches.is_present("json") {
        println!("{}", serde_json::to_string_pretty(&doctor.json(&report))?);
    } else {
        print!("{}", doctor.text(&report));
    }
    Ok(i32::from(report.count(Status::Fail) > 0))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Ok,
    Info,
    Warn,
    Fail,
}

impl Status {
    fn symbol(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Info => "·",
            Self::Warn => "!",
            Self::Fail => "✗",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

enum Fix {
    /// `justless init` for these agents, with or without `--hooks`, which doctor
    /// has planned as `init` plans it and found to run.
    Init {
        agents: Vec<&'static str>,
        hooks: bool,
    },
    /// A shell command to run from the directory doctor ran in.
    Command(String),
    /// An edit to make by hand.
    Manual(String),
}

struct Check {
    status: Status,
    message: String,
    fix: Option<Fix>,
}

impl Check {
    fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            fix: None,
        }
    }

    fn fix(mut self, fix: Fix) -> Self {
        self.fix = Some(fix);
        self
    }
}

struct Section {
    id: String,
    title: String,
    checks: Vec<Check>,
}

impl Section {
    fn new(id: &str, title: &str) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            checks: Vec::new(),
        }
    }

    fn worst(&self) -> Option<Status> {
        self.checks.iter().map(|check| check.status).max()
    }
}

struct Report {
    /// Whether the directory is a project that doctor checked.
    project: bool,
    /// The project root, or the directory checked outside a project.
    place: PathBuf,
    sections: Vec<Section>,
    agents: Vec<Value>,
    facts: Option<Facts>,
}

impl Report {
    fn checks(&self) -> impl Iterator<Item = &Check> {
        self.sections.iter().flat_map(|section| &section.checks)
    }

    fn count(&self, status: Status) -> usize {
        self.checks().filter(|check| check.status == status).count()
    }
}

/// How doctor names and finds each agent.
struct Profile {
    name: &'static str,
    agent: Agent,
    label: &'static str,
    /// Commands `install.sh` detects the client by.
    programs: &'static [&'static str],
    /// What a user may still have to do before the client runs project hooks.
    trust: &'static str,
}

fn profiles() -> Vec<Profile> {
    AGENTS
        .iter()
        .map(|&(name, agent)| {
            let (label, programs, trust): (_, &'static [&'static str], _) = match name {
                "claude" => ("Claude Code", &["claude"], ""),
                "codex" => (
                    "Codex",
                    &["codex"],
                    " In Codex, review and trust the project hooks with /hooks first.",
                ),
                // The Cursor CLI's `agent` command is too generic to detect.
                "cursor" => (
                    "Cursor",
                    &["cursor", "cursor-agent"],
                    " Cursor runs project hooks only in a trusted workspace.",
                ),
                "gemini" => (
                    "Gemini CLI",
                    &["gemini"],
                    " Gemini CLI asks before it runs a new or changed project hook.",
                ),
                "copilot" => ("GitHub Copilot CLI", &["copilot"], ""),
                "opencode" => ("OpenCode", &["opencode"], ""),
                _ => ("Antigravity CLI", &["agy"], ""),
            };
            Profile {
                name,
                agent,
                label,
                programs,
                trust,
            }
        })
        .collect()
}

impl Profile {
    /// A version requirement that finding the CLI on PATH does not show. The
    /// installer runs `opencode --version`; doctor runs no agent CLI.
    fn requirement(&self) -> &'static str {
        if self.agent == Agent::OpenCode {
            " (the plugin needs OpenCode v1)"
        } else {
            ""
        }
    }

    /// The client's CLI line, such as `Claude Code: /usr/local/bin/claude`.
    fn found(&self, cli: &Path) -> String {
        format!("{}: {}{}", self.label, cli.display(), self.requirement())
    }
}

/// What doctor found in one agent's hook file.
struct Hooks {
    /// The file has Justless entries, or a problem doctor reports.
    hooked: bool,
    /// Its handlers call justless by a path, which `init` does not recognize.
    by_path: bool,
    checks: Vec<Check>,
}

impl Hooks {
    fn none() -> Self {
        Self {
            hooked: false,
            by_path: false,
            checks: Vec::new(),
        }
    }

    fn problem(check: Check) -> Self {
        Self {
            hooked: true,
            by_path: false,
            checks: vec![check],
        }
    }
}

/// One agent's hooks and instructions.
struct AgentAudit<'p> {
    profile: &'p Profile,
    cli: Option<PathBuf>,
    /// The hook file has Justless entries, or a problem doctor reports.
    hooked: bool,
    /// The agent's own instructions file has a Justless section.
    instructed: bool,
    checks: Vec<Check>,
}

struct Doctor<'a> {
    workspace: &'a Workspace,
    explicit_root: bool,
    /// The shell that runs a fix command is at the project root.
    at_root: bool,
}

impl Doctor<'_> {
    fn report(&self, here: Option<&Path>) -> Report {
        let root = &self.workspace.root;
        let profiles = profiles();
        let clis: Vec<Option<PathBuf>> = profiles
            .iter()
            .map(|profile| {
                profile
                    .programs
                    .iter()
                    .find_map(|program| on_path(program).into_iter().next())
            })
            .collect();
        let mut sections = vec![self.binary()];
        let home = std::env::home_dir().and_then(|home| home.canonicalize().ok());
        let at_home = !self.explicit_root
            && (home.as_deref() == Some(root.as_path()) || root.parent().is_none());
        if at_home {
            return self.outside(sections, &profiles, &clis, true);
        }
        let mut audits: Vec<AgentAudit> = profiles
            .iter()
            .zip(&clis)
            .map(|(profile, cli)| self.audit_agent(profile, cli.clone()))
            .collect();
        // `AGENTS.md` serves several clients. Count it as Codex setup, the `init`
        // default, only when no other client that reads it has hooks.
        let shared = audits
            .iter()
            .any(|audit| audit.hooked && audit.profile.agent.instructions_name() == "AGENTS.md");
        for audit in &mut audits {
            if audit.profile.agent == Agent::Codex && shared && !audit.hooked {
                audit.instructed = false;
            }
        }
        let mcp = self.audit_mcp();
        let set_up = audits.iter().any(|audit| audit.hooked || audit.instructed) || !mcp.is_empty();
        // The files `Workspace` finds a project root by, so a project gets the same
        // report from its root as from its subdirectories.
        let marked = [".git", ".justless", ".justless.toml"]
            .iter()
            .any(|name| fs::symlink_metadata(root.join(name)).is_ok())
            || PROJECT_MARKERS.iter().any(|name| root.join(name).is_file());
        if !self.explicit_root && !set_up && !marked && here == Some(root.as_path()) {
            return self.outside(sections, &profiles, &clis, false);
        }
        let facts = Facts::collect(&self.workspace.state);
        let mut others = Section::new("agents", if set_up { "Other agents" } else { "Agents" });
        let mut missing = Vec::new();
        let mut agents_json = Vec::new();
        // The agents the setup command names, without those whose files `init`
        // cannot read: one of them would stop the whole run.
        let candidates = if set_up {
            Vec::new()
        } else {
            match detected(&profiles, &clis) {
                detected if detected.is_empty() => vec!["claude"],
                detected => detected,
            }
        };
        let ready: Vec<&'static str> = candidates
            .iter()
            .copied()
            .filter(|name| self.setup_error(&[name], true).is_none())
            .collect();
        for audit in audits {
            let profile = audit.profile;
            let set = audit.hooked || audit.instructed;
            let mut status = None;
            if set {
                let mut section = Section::new(profile.name, profile.label);
                section.checks.push(match &audit.cli {
                    Some(cli) => Check::new(
                        Status::Ok,
                        format!("CLI: {}{}", cli.display(), profile.requirement()),
                    ),
                    None => Check::new(
                        Status::Warn,
                        format!(
                            "CLI not found on PATH (looked for {}). That is fine if you start {} another way, such as from its editor.",
                            profile.programs.join(", "),
                            profile.label
                        ),
                    ),
                });
                if !audit.hooked {
                    section.checks.push(
                        Check::new(
                            Status::Warn,
                            format!(
                                "Hooks: none in {}, so {} gets the instructions but its commands are not routed",
                                profile.agent.hook_file().display(),
                                profile.label
                            ),
                        )
                        .fix(self.init_fix(profile.name, true)),
                    );
                }
                section.checks.extend(audit.checks);
                // Claude Code's permission rules decide what its command hook routes.
                let rules = (profile.agent == Agent::Claude).then(|| {
                    let root = &self.workspace.root;
                    claude_permissions::load(&claude_permissions::Sources::locate(root, root))
                });
                if audit.hooked {
                    section
                        .checks
                        .extend(routing_note(profile, &facts, rules.as_ref()));
                }
                if let Some(rules) = &rules {
                    section.checks.extend(self.claude_permission_checks(rules));
                }
                status = section.worst();
                sections.push(section);
            } else if let Some(cli) = &audit.cli {
                others.checks.push(if set_up {
                    Check::new(
                        Status::Warn,
                        format!(
                            "{} is on PATH ({}) but not set up here{}",
                            profile.label,
                            cli.display(),
                            profile.requirement()
                        ),
                    )
                    .fix(self.init_fix(profile.name, true))
                } else if candidates.contains(&profile.name) && !ready.contains(&profile.name) {
                    Check::new(
                        Status::Warn,
                        format!(
                            "{}, but setup cannot write its files here, so the setup command leaves it out",
                            profile.found(cli)
                        ),
                    )
                    .fix(self.init_fix(profile.name, true))
                } else {
                    Check::new(Status::Ok, profile.found(cli))
                });
            } else {
                missing.push(profile.label);
            }
            agents_json.push(json!({
                "agent": profile.name,
                "label": profile.label,
                "cli": audit.cli.as_ref().map(|cli| cli.display().to_string()),
                "set_up": set,
                "hooks": audit.hooked,
                "hook_file": profile.agent.hook_file().display().to_string(),
                "instructions_file": profile.agent.instructions_name(),
                "status": status.map(Status::name),
            }));
        }
        if !missing.is_empty() {
            others.checks.push(Check::new(
                Status::Info,
                format!("Not found on PATH: {}", missing.join(", ")),
            ));
        }
        if !set_up {
            let mut message = String::from(
                "Justless is not set up in this project: no agent hooks, instructions or MCP entries",
            );
            if detected(&profiles, &clis).is_empty() {
                message.push_str(
                    ". Replace claude in the fix with your agent: codex, cursor, gemini, copilot, opencode or agy",
                );
            }
            let fix = match candidates.as_slice() {
                _ if !ready.is_empty() => Fix::Init {
                    agents: ready.clone(),
                    hooks: true,
                },
                [only] => self.init_fix(only, true),
                _ => Fix::Manual(format!(
                    "Fix what stops setup for each agent above, then run `{}`",
                    self.init(&candidates, true)
                )),
            };
            others
                .checks
                .push(Check::new(Status::Fail, message).fix(fix));
        }
        sections.push(others);
        sections.push(machine_section(Some(self.workspace)));
        if !mcp.is_empty() {
            let mut section = Section::new("mcp", "MCP servers");
            section.checks = mcp;
            sections.push(section);
        }
        let mut state = Section::new("state", "State (.justless/)");
        state.checks = self.state_checks(&facts);
        sections.push(state);
        Report {
            project: true,
            place: root.clone(),
            sections,
            agents: agents_json,
            facts: Some(facts),
        }
    }

    /// The report outside a project: the binary, the agents on PATH and how to set up.
    fn outside(
        &self,
        mut sections: Vec<Section>,
        profiles: &[Profile],
        clis: &[Option<PathBuf>],
        at_home: bool,
    ) -> Report {
        let place = if at_home {
            self.workspace.root.clone()
        } else {
            self.workspace.cwd.clone()
        };
        let mut agents = Section::new("agents", "Agents on PATH");
        let mut missing = Vec::new();
        let mut agents_json = Vec::new();
        for (profile, cli) in profiles.iter().zip(clis) {
            match cli {
                Some(cli) => agents
                    .checks
                    .push(Check::new(Status::Ok, profile.found(cli))),
                None => missing.push(profile.label),
            }
            agents_json.push(json!({
                "agent": profile.name,
                "label": profile.label,
                "cli": cli.as_ref().map(|cli| cli.display().to_string()),
                "set_up": false,
            }));
        }
        if !missing.is_empty() {
            agents.checks.push(Check::new(
                Status::Info,
                format!("Not found on PATH: {}", missing.join(", ")),
            ));
        }
        sections.push(agents);
        let detected = detected(profiles, clis);
        let names = if detected.is_empty() {
            vec!["claude"]
        } else {
            detected
        };
        let mut project = Section::new("project", "Project");
        project.checks.push(Check::new(
            Status::Info,
            if !at_home {
                format!(
                    "{} has no Justless setup, and neither it nor a parent directory has .git, .justless/ or a project file such as Cargo.toml or package.json.",
                    place.display()
                )
            } else if place.parent().is_none() {
                format!(
                    "{} is the filesystem root, not a project. Justless sets up hooks per project.",
                    place.display()
                )
            } else {
                format!(
                    "{} is your home directory. Justless sets up hooks per project, and only `justless init --global` adds user-level hooks, which leave the home directory alone.",
                    place.display()
                )
            },
        ));
        project.checks.push(Check::new(
            Status::Info,
            "Inside a project, doctor also checks each agent's hooks and instructions against what `justless init` writes, MCP entries and the .justless/ archive.",
        ));
        if at_home {
            project.checks.push(Check::new(
                Status::Info,
                format!(
                    "To set up a project, run this in its folder: {}",
                    self.init(&names, true)
                ),
            ));
        } else {
            // Name only the agents whose files `init` can read here.
            let mut ready = Vec::new();
            for name in names {
                match self.setup_error(&[name], true) {
                    None => ready.push(name),
                    Some(error) => project.checks.push(Check::new(
                        Status::Info,
                        format!("Setup for {name} cannot run in this folder: {error}"),
                    )),
                }
            }
            if !ready.is_empty() {
                project.checks.push(Check::new(
                    Status::Info,
                    format!(
                        "If this folder is your project, set it up with: {}",
                        self.init(&ready, true)
                    ),
                ));
            }
        }
        sections.push(project);
        sections.push(machine_section(None));
        Report {
            project: false,
            place,
            sections,
            agents: agents_json,
            facts: None,
        }
    }

    /// `justless` with this run's `--root`, so a fix applies to the same project.
    fn justless(&self) -> String {
        if self.explicit_root {
            format!("justless --root {}", quote(&self.workspace.root))
        } else {
            "justless".into()
        }
    }

    fn init(&self, agents: &[&str], hooks: bool) -> String {
        let mut command = format!("{} init", self.justless());
        if hooks {
            command.push_str(" --hooks");
        }
        for agent in agents {
            command.push_str(" --agent ");
            command.push_str(agent);
        }
        command
    }

    fn fix_text(&self, fix: &Fix) -> String {
        match fix {
            Fix::Init { agents, hooks } => self.init(agents, *hooks),
            Fix::Command(text) | Fix::Manual(text) => text.clone(),
        }
    }

    /// Why `init` for `names` would stop before writing anything, with project
    /// paths shown relative to the root. Plans the run as `init` does.
    fn setup_error(&self, names: &[&str], hooks: bool) -> Option<String> {
        let agents: Vec<Agent> = names
            .iter()
            .filter_map(|name| Agent::parse(name).ok())
            .collect();
        let error = agents::setup_error(self.workspace, &agents, hooks)?;
        Some(self.relative(&format!("{error:#}")))
    }

    fn relative(&self, text: &str) -> String {
        text.replace(&format!("{}/", self.workspace.root.display()), "")
    }

    /// The `init` fix for one agent. When setup would stop on one of the
    /// project's files, an edit by hand that names it and then runs `init`.
    fn init_fix(&self, name: &'static str, hooks: bool) -> Fix {
        let Some(error) = self.setup_error(&[name], hooks) else {
            return Fix::Init {
                agents: vec![name],
                hooks,
            };
        };
        let init = self.init(&[name], hooks);
        // Name the hook file only when it, and not an instructions file, stops setup.
        let blocker = Agent::parse(name)
            .ok()
            .filter(|_| hooks && self.setup_error(&[name], false).is_none())
            .and_then(|agent| self.hook_blocker(agent));
        Fix::Manual(match blocker {
            Some(blocker) => format!("{blocker}, then run `{init}`"),
            None => format!("Setup stops with \"{error}\". Fix that, then run `{init}`"),
        })
    }

    /// What stops `init --hooks` from writing `agent`'s hook file, as the edit
    /// that clears it, when doctor can tell.
    fn hook_blocker(&self, agent: Agent) -> Option<String> {
        let root = &self.workspace.root;
        let relative = agent.hook_file();
        // Setup writes hooks only into real directories, not through links.
        let mut directory = PathBuf::new();
        for part in relative.parent()?.components() {
            directory.push(part);
            let metadata = fs::symlink_metadata(root.join(&directory)).ok()?;
            if metadata.file_type().is_symlink() {
                return Some(format!(
                    "Replace the {} link with a directory holding the same files (setup does not write through links)",
                    directory.display()
                ));
            }
            if !metadata.is_dir() {
                return Some(format!(
                    "Move the file {} aside, since setup keeps hooks in a directory there",
                    directory.display()
                ));
            }
        }
        let file = relative.display();
        let metadata = fs::symlink_metadata(root.join(&relative)).ok()?;
        if !metadata.is_file() {
            return Some(format!("Make {file} a regular file rather than a link"));
        }
        let text = fs::read_to_string(root.join(&relative)).ok()?;
        if agent == Agent::OpenCode || serde_json::from_str::<Value>(&text).is_ok() {
            return None;
        }
        let commented = text.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("//") || line.starts_with("/*")
        });
        Some(if commented {
            format!("Remove the comments from {file}, which setup reads as plain JSON")
        } else {
            format!(
                "Fix the JSON in {file}, which setup reads as plain JSON without comments or trailing commas"
            )
        })
    }

    /// A project path as one shell word for a fix command.
    fn shell_path(&self, relative: &str) -> String {
        if self.at_root {
            relative.to_owned()
        } else {
            quote(&self.workspace.root.join(relative))
        }
    }

    fn binary(&self) -> Section {
        let mut section = Section::new("binary", "Binary");
        let exe = std::env::current_exe().ok();
        let canonical = exe.as_ref().and_then(|exe| exe.canonicalize().ok());
        section.checks.push(Check::new(
            Status::Ok,
            match &exe {
                Some(exe) => format!("justless {VERSION} at {}", exe.display()),
                None => format!("justless {VERSION}"),
            },
        ));
        if cfg!(windows) {
            section.checks.push(Check::new(
                Status::Warn,
                "On Windows the command hooks pass every command through unchanged. Run commands as `justless run -- COMMAND`, or use the MCP tools.",
            ));
        }
        // Putting the directory this binary was started from first on PATH makes
        // `justless` resolve to it, whichever other copies exist.
        let directory = exe
            .as_deref()
            .and_then(Path::parent)
            .map(quote)
            .unwrap_or_else(|| "DIR".into());
        // Kept free of comments, which interactive zsh would pass to `export`.
        let use_this = format!("export PATH={directory}:\"$PATH\"");
        let found = on_path("justless");
        let same = found
            .first()
            .is_some_and(|first| same_file(first, canonical.as_deref()));
        // What the copy hooks run reports, unless it is this binary.
        let reported = found
            .first()
            .filter(|_| !same)
            .map(|first| version_of(first));
        match found.first() {
            None => section.checks.push(
                Check::new(
                    Status::Fail,
                    "`justless` is not on PATH. Hooks and MCP entries call it by name, so they cannot start. Add the fix line to your shell profile too, so new agent sessions see it.",
                )
                .fix(Fix::Command(use_this)),
            ),
            Some(_) if same => section
                .checks
                .push(Check::new(Status::Ok, "`justless` on PATH is this binary")),
            Some(first) => section.checks.push(match reported.clone().flatten() {
                Some((version, _)) if version == VERSION => Check::new(
                    Status::Warn,
                    format!(
                        "`justless` on PATH is {}, another copy of {VERSION}. Hooks run that copy. To use this one, put its directory first on PATH here and in your shell profile.",
                        first.display()
                    ),
                )
                .fix(Fix::Command(use_this)),
                Some((version, _)) => Check::new(
                    Status::Fail,
                    format!(
                        "`justless` on PATH is {} at version {version}, but this binary is {VERSION}. Hooks run {version}, and doctor checks them against what {VERSION} writes. Put this binary's directory first on PATH here and in your shell profile, or install {VERSION} over that copy.",
                        first.display()
                    ),
                )
                .fix(Fix::Command(use_this)),
                None => Check::new(
                    Status::Fail,
                    format!(
                        "`justless` on PATH ({}) did not report its version",
                        first.display()
                    ),
                )
                .fix(Fix::Command(use_this)),
            }),
        }
        if found.len() > 1 {
            // Listed only: a shadowed copy may be another install's, such as one
            // that `cargo install` or a package manager owns.
            let others: Vec<String> = found[1..]
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            section.checks.push(Check::new(
                Status::Warn,
                format!(
                    "{} copies of justless are on PATH. Hooks run the first, {}, and the others are shadowed: {}. Remove a copy you no longer use with the tool that installed it, such as `cargo uninstall justless`.",
                    found.len(),
                    found[0].display(),
                    others.join(", ")
                ),
            ));
        }
        if let Some(first) = found.first() {
            let revision = if same {
                Some(REVISION.to_owned()).filter(|revision| !revision.is_empty())
            } else {
                reported.flatten().and_then(|(_, revision)| revision)
            };
            section
                .checks
                .extend(self.checkout_build(first, revision.as_deref()));
        }
        section
    }

    /// In a Justless checkout, whether the `justless` that hooks run was built
    /// from its HEAD: by the revision the copy reports, or else by whether the
    /// file is older than the last commit. One `git log` reads HEAD.
    fn checkout_build(&self, copy: &Path, revision: Option<&str>) -> Option<Check> {
        let root = &self.workspace.root;
        let manifest: toml_edit::DocumentMut = fs::read_to_string(root.join("Cargo.toml"))
            .ok()?
            .parse()
            .ok()?;
        if manifest.get("package")?.get("name")?.as_str()? != "justless" {
            return None;
        }
        let output = Process::new("git")
            .args(["log", "-1", "--format=%H %ct"])
            .current_dir(root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        let line = String::from_utf8(output.stdout).ok()?;
        let (head, time) = line.trim().split_once(' ')?;
        let committed = time.parse::<u64>().ok()?.saturating_mul(1000);
        let short = |id: &str| id.get(..12).unwrap_or(id).to_owned();
        let problem = match revision {
            Some(revision) if revision == head => {
                return Some(Check::new(
                    Status::Ok,
                    format!(
                        "`justless` on PATH was built from this checkout's HEAD commit, {}. Uncommitted changes are not compared.",
                        short(head)
                    ),
                ));
            }
            Some(revision) => format!(
                "`justless` on PATH ({}) was built from {}, but this checkout's HEAD is {}.",
                copy.display(),
                short(revision),
                short(head)
            ),
            None => {
                let modified = millis(fs::metadata(copy).ok()?.modified().ok()?);
                if modified >= committed {
                    return None;
                }
                format!(
                    "`justless` on PATH ({}) reports no build revision and was built {}, before this checkout's last commit {}.",
                    copy.display(),
                    ago(modified),
                    ago(committed)
                )
            }
        };
        let message = format!(
            "{problem} Hooks run that copy, so changes since then do not run in agent sessions."
        );
        let install = format!(
            "cargo install --path {} --locked --force",
            if self.at_root {
                ".".into()
            } else {
                quote(root)
            }
        );
        // `cargo install` replaces the copy only where Cargo installs binaries.
        let cargo_bin = std::env::var_os("CARGO_INSTALL_ROOT")
            .or_else(|| std::env::var_os("CARGO_HOME"))
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))
            .map(|base| base.join("bin"));
        let installs_there = copy
            .parent()
            .zip(cargo_bin)
            .is_some_and(|(directory, bin)| {
                same_file(directory, bin.canonicalize().ok().as_deref())
            });
        Some(if installs_there {
            Check::new(Status::Warn, message).fix(Fix::Command(install))
        } else {
            Check::new(Status::Warn, message).fix(Fix::Manual(format!(
                "reinstall {} from this checkout with the tool that installed it, or put Cargo's bin directory first on PATH and run `{install}`",
                copy.display()
            )))
        })
    }

    fn audit_agent<'p>(&self, profile: &'p Profile, cli: Option<PathBuf>) -> AgentAudit<'p> {
        let hooks = self.audit_hooks(profile);
        // Offer `init --hooks` for the instructions only where it runs and adds no
        // second copy of hooks that call justless by path.
        let rerun =
            hooks.hooked && !hooks.by_path && self.setup_error(&[profile.name], true).is_none();
        let (block, instructions) = self.audit_instructions(profile, rerun);
        // Without hooks, a section in a file only this client reads still shows setup.
        let instructed = !hooks.hooked
            && block
            && matches!(profile.agent, Agent::Claude | Agent::Gemini | Agent::Codex);
        let mut checks = hooks.checks;
        if hooks.hooked || instructed {
            checks.extend(instructions);
        }
        AgentAudit {
            profile,
            cli,
            hooked: hooks.hooked,
            instructed,
            checks,
        }
    }

    /// Claude Code settings files the command hook cannot use, which stop it from
    /// rewriting any command, and allow rules, such as `Bash(justless run *)` saved
    /// by "Yes, and don't ask again" on a routed command, that let the agent run any
    /// command as `justless run -- COMMAND` without asking. The command hook does
    /// not need those: it approves a routed command when the rules allow the
    /// command itself.
    fn claude_permission_checks(&self, rules: &claude_permissions::Rules) -> Vec<Check> {
        let unusable = rules.broken().iter().map(|broken| {
            let file = self.relative(&broken.path.display().to_string());
            Check::new(
                Status::Warn,
                format!(
                    "Permissions: cannot use Claude Code settings {file} ({}), so the command hook leaves every command as written, in every permission mode",
                    broken.problem
                ),
            )
            .fix(Fix::Manual(if broken.path.is_dir() {
                format!("Make {file} readable by this user")
            } else if broken.path.is_relative() {
                // No variable locates the user settings file.
                "Set HOME, or CLAUDE_CONFIG_DIR, in the environment Claude Code and its hooks run in".into()
            } else {
                format!(
                    "Fix {file} so that it reads as a JSON object whose `permissions` lists hold strings, or remove it; `claude doctor` also names what Claude Code rejects"
                )
            }))
        });
        let broad = rules.broad_justless_rules().into_iter().map(|(file, rule)| {
            let file = self.relative(&file.display().to_string());
            Check::new(
                Status::Warn,
                format!(
                    "Permissions: {rule} in {file} lets the agent run any command without asking, as `justless run -- COMMAND`"
                ),
            )
            .fix(Fix::Manual(format!(
                "Remove {rule} from {file}, and allow the commands themselves instead, such as Bash(cargo test *); the command hook approves a routed command those rules allow"
            )))
        });
        unusable.chain(broad).collect()
    }

    /// The agent's hook file: whether it holds Justless entries, and its checks.
    fn audit_hooks(&self, profile: &Profile) -> Hooks {
        let relative = profile.agent.hook_file();
        let file = relative.display().to_string();
        let text = match fs::read_to_string(self.workspace.root.join(&relative)) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Hooks::none(),
            Err(error) => {
                return Hooks::problem(
                    Check::new(Status::Fail, format!("Hooks: cannot read {file}: {error}")).fix(
                        Fix::Manual(format!(
                            "Make {file} a readable file, then run `{}`",
                            self.init(&[profile.name], true)
                        )),
                    ),
                );
            }
        };
        let pending = agents::pending_hooks(self.workspace, profile.agent);
        if profile.agent == Agent::OpenCode {
            return Hooks::problem(match pending {
                Ok(None) => Check::new(Status::Ok, format!("Plugin: {file} is current")),
                Ok(Some(_)) => Check::new(
                    Status::Fail,
                    format!("Plugin: {file} is from another version of Justless"),
                )
                .fix(self.init_fix(profile.name, true)),
                Err(_) => {
                    let path = self.shell_path(&file);
                    Check::new(
                        Status::Fail,
                        format!("Plugin: {file} is not managed by Justless, so setup will not replace it"),
                    )
                    .fix(Fix::Command(format!(
                        "mv {path} {path}.bak && {}",
                        self.init(&[profile.name], true)
                    )))
                }
            });
        }
        let tag = format!("justless hook-{}", profile.name);
        let config: Value = match serde_json::from_str(&text) {
            Ok(config) => config,
            Err(error) if text.contains(&tag) => {
                // Setup would add a second copy of a hook that calls justless by path.
                let then = if text.contains(&format!("/{tag}")) {
                    String::new()
                } else {
                    format!(", then run `{}`", self.init(&[profile.name], true))
                };
                return Hooks::problem(
                    Check::new(
                        Status::Fail,
                        format!("Hooks: {file} is not valid JSON ({error})"),
                    )
                    .fix(Fix::Manual(format!(
                        "Fix the JSON in {file}, which setup reads as plain JSON without comments or trailing commas{then}"
                    ))),
                );
            }
            Err(_) => return Hooks::none(),
        };
        let expected = agents::fresh_hooks(profile.agent)
            .ok()
            .and_then(|fresh| serde_json::from_str::<Value>(&fresh).ok())
            .and_then(|fresh| expected_hooks(&fresh));
        let Some((group, expected)) = expected else {
            return Hooks::problem(
                Check::new(
                    Status::Fail,
                    format!("Hooks: cannot work out the hooks setup writes to {file}"),
                )
                .fix(Fix::Manual(
                    "Reinstall justless, since this build cannot produce its own hook file, and report a bug if a fresh install does the same".into(),
                )),
            );
        };
        let disk = audit_config(profile.agent, &config, &group, &expected);
        if let Some(shape) = &disk.shape {
            if !text.contains(&tag) {
                return Hooks::none();
            }
            return Hooks::problem(
                Check::new(Status::Fail, format!("Hooks: in {file}, {shape}")).fix(Fix::Manual(
                    format!(
                        "Fix the structure of {file}, then run `{}`",
                        self.init(&[profile.name], true)
                    ),
                )),
            );
        }
        if disk.found == 0 && disk.unexpected.is_empty() {
            return Hooks::none();
        }
        let by_path = !disk.paths.is_empty();
        let mut problems = Vec::new();
        let mut notes = Vec::new();
        // Setup adds missing entries and leaves existing ones as they are, so audit
        // what it would write: a problem left there needs a fix by hand. Setup does
        // not see handlers that call justless by path, so it would copy them.
        let merged = match &pending {
            Ok(Some(contents)) if !by_path => serde_json::from_str::<Value>(contents)
                .ok()
                .map(|merged| audit_config(profile.agent, &merged, &group, &expected)),
            _ => None,
        };
        let after = merged.as_ref().unwrap_or(&disk);
        if !disk.missing.is_empty() {
            let modes = list(disk.missing.iter().map(|&index| expected[index].mode()));
            let hooks = plural_word(disk.missing.len(), "hook");
            let lacks = format!("Hooks: {file} lacks the {modes} {hooks}");
            problems.push(match &pending {
                _ if by_path => Check::new(Status::Fail, lacks)
                    .fix(Fix::Manual(add_by_hand(&file, &group, &expected, &disk))),
                Ok(Some(_)) if after.missing.is_empty() => {
                    Check::new(Status::Fail, lacks).fix(self.init_fix(profile.name, true))
                }
                Err(error) => Check::new(
                    Status::Fail,
                    format!(
                        "{lacks}, and setup cannot add {}: {}",
                        if disk.missing.len() == 1 {
                            "it"
                        } else {
                            "them"
                        },
                        self.relative(&format!("{error:#}"))
                    ),
                )
                .fix(self.init_fix(profile.name, true)),
                _ => Check::new(
                    Status::Fail,
                    format!("{lacks}, which setup does not add over the entries it finds"),
                )
                .fix(Fix::Manual(format!(
                    "Remove the Justless entries from {file}, then run `{}`",
                    self.init(&[profile.name], true)
                ))),
            });
        } else if !by_path {
            match &pending {
                Ok(Some(_)) => problems.push(
                    Check::new(Status::Fail, format!("Hooks: setup would update {file}"))
                        .fix(self.init_fix(profile.name, true)),
                ),
                // Every hook is in place, so the hooks work, and only a later
                // `init --hooks` would stop.
                Err(error) => {
                    let then = format!(
                        "so a later `{}` can update it",
                        self.init(&[profile.name], true)
                    );
                    let fix = match self.hook_blocker(profile.agent) {
                        Some(blocker) => format!("{blocker}, {then}"),
                        None => format!("Fix what stops setup, {then}"),
                    };
                    notes.push(
                        Check::new(
                            Status::Warn,
                            format!(
                                "Hooks: setup cannot update {file}: {}",
                                self.relative(&format!("{error:#}"))
                            ),
                        )
                        .fix(Fix::Manual(fix)),
                    );
                }
                Ok(None) => {}
            }
        }
        if by_path {
            notes.push(Check::new(
                Status::Info,
                format!(
                    "Hooks: {file} calls justless by its path, {}. Setup looks only for the bare `justless` command, so `init --hooks` would add a second copy of each hook. Update this file by hand.",
                    disk.paths.join(", ")
                ),
            ));
            for path in &disk.paths {
                if let Some(program) = expand(path)
                    && !executable(&program)
                {
                    problems.push(
                        Check::new(
                            Status::Fail,
                            format!("Hooks: {file} runs {path}, which is not an executable file"),
                        )
                        .fix(Fix::Manual(format!(
                            "In {file}, point the Justless hooks at an installed justless, such as {}",
                            std::env::current_exe()
                                .map_or_else(|_| "justless".into(), |exe| quote(&exe))
                        ))),
                    );
                }
            }
        }
        problems.extend(after.drift.iter().map(|drift| drift.check(&file)));
        problems.extend(disk.unexpected.iter().map(|entry| {
            Check::new(
                Status::Warn,
                format!("Hooks: {file} also runs {entry}, which setup does not write"),
            )
            .fix(Fix::Manual(format!("Remove that hook from {file}")))
        }));
        if problems.is_empty() {
            let modes: Vec<&str> = expected.iter().map(Expected::mode).collect();
            let all = match expected.len() {
                1 => "its Justless hook".to_owned(),
                2 => "both Justless hooks".to_owned(),
                count => format!("all {count} Justless hooks"),
            };
            problems.push(Check::new(
                Status::Ok,
                format!("Hooks: {file} has {all} ({})", modes.join(", ")),
            ));
        }
        problems.extend(notes);
        Hooks {
            hooked: true,
            by_path,
            checks: problems,
        }
    }

    /// Whether the agent's own instructions file has a Justless section, and the
    /// check of the section the agent loads.
    fn audit_instructions(&self, profile: &Profile, hooks: bool) -> (bool, Vec<Check>) {
        let root = &self.workspace.root;
        let name = profile.agent.instructions_name();
        let path = root.join(name);
        let init = self.init(&[profile.name], hooks);
        // Setup skips a link that leaves the project.
        let editable =
            agents::instructions_target(root, &path).map_or(true, |target| target.is_some());
        let rewrite = || {
            if editable {
                self.init_fix(profile.name, hooks)
            } else {
                Fix::Manual(format!(
                    "{name} links outside the project, so setup skips it. Replace the link with a file, or update the Justless section in its target yourself"
                ))
            }
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return (
                    false,
                    vec![
                        Check::new(Status::Fail, format!("Instructions: {name} does not exist"))
                            .fix(rewrite()),
                    ],
                );
            }
            Err(error) => {
                return (
                    false,
                    vec![
                        Check::new(
                            Status::Fail,
                            format!("Instructions: cannot read {name}: {error}"),
                        )
                        .fix(Fix::Manual(format!(
                            "Make {name} a readable file, then run `{init}`"
                        ))),
                    ],
                );
            }
        };
        let state = block_state(&text);
        // Claude Code reads the block of the `AGENTS.md` that `CLAUDE.md` imports.
        let imported =
            profile.agent == Agent::Claude && agents::imported_instructions(root, false).is_some();
        let check = match state {
            Block::Current | Block::Stale if imported => Check::new(
                Status::Fail,
                format!(
                    "Instructions: {name} has its own Justless block and imports AGENTS.md, which has one too, so Claude Code loads it twice"
                ),
            )
            .fix(rewrite()),
            Block::Current => Check::new(
                Status::Ok,
                format!("Instructions: {name} has the current Justless block"),
            ),
            Block::Stale => Check::new(
                Status::Fail,
                format!("Instructions: {name} has an older Justless block"),
            )
            .fix(rewrite()),
            Block::Broken(why) => {
                Check::new(Status::Fail, format!("Instructions: {name} has {why}")).fix(
                    Fix::Manual(format!(
                        "Remove the partial Justless section from {name}, then run `{init}`"
                    )),
                )
            }
            Block::Absent => match (profile.agent == Agent::Claude)
                .then(|| self.imported_block(name, &text))
                .flatten()
            {
                Some(check) => check,
                None => Check::new(
                    Status::Fail,
                    format!("Instructions: {name} has no Justless block"),
                )
                .fix(rewrite()),
            },
        };
        (state != Block::Absent || imported, vec![check])
    }

    /// The check for a Justless section that `CLAUDE.md` brings in with an
    /// `@path` import line, which Claude Code expands when it loads the file.
    fn imported_block(&self, name: &str, text: &str) -> Option<Check> {
        let root = &self.workspace.root;
        for line in text.lines() {
            let Some(target) = line.trim().strip_prefix('@') else {
                continue;
            };
            if target.is_empty() || target.contains(char::is_whitespace) {
                continue;
            }
            let Ok(path) = root.join(target).canonicalize() else {
                continue;
            };
            if !path.starts_with(root) {
                continue;
            }
            let Ok(imported) = fs::read_to_string(&path) else {
                continue;
            };
            // The client whose instructions file `init` writes there.
            let owner = match target {
                "AGENTS.md" => Some("codex"),
                "GEMINI.md" => Some("gemini"),
                _ => None,
            };
            return Some(match block_state(&imported) {
                Block::Absent => continue,
                Block::Current => Check::new(
                    Status::Ok,
                    format!(
                        "Instructions: {name} imports {target}, which has the current Justless block"
                    ),
                ),
                Block::Stale => Check::new(
                    Status::Fail,
                    format!(
                        "Instructions: {name} imports {target}, which has an older Justless block"
                    ),
                )
                .fix(match owner {
                    Some(owner) => self.init_fix(owner, false),
                    None => Fix::Manual(format!(
                        "Update the Justless section in {target} to the one setup writes into AGENTS.md"
                    )),
                }),
                Block::Broken(why) => Check::new(
                    Status::Fail,
                    format!("Instructions: {name} imports {target}, which has {why}"),
                )
                .fix(Fix::Manual(match owner {
                    Some(owner) => format!(
                        "Remove the partial Justless section from {target}, then run `{}`",
                        self.init(&[owner], false)
                    ),
                    None => format!(
                        "Replace the partial Justless section in {target} with the one setup writes into AGENTS.md"
                    ),
                })),
            });
        }
        None
    }

    /// Checks for each MCP client config that lists a `justless` server.
    fn audit_mcp(&self) -> Vec<Check> {
        let mut checks = Vec::new();
        for client in Client::NAMES
            .iter()
            .filter_map(|name| Client::parse(name).ok())
        {
            let file = client.file();
            let text = match fs::read_to_string(self.workspace.root.join(file)) {
                Ok(text) if text.contains("justless") => text,
                _ => continue,
            };
            let label = client.label();
            let entry = if client == Client::Codex {
                match text.parse::<toml_edit::DocumentMut>() {
                    Ok(document) => document
                        .get("mcp_servers")
                        .and_then(|servers| servers.get("justless"))
                        .map(|entry| {
                            let args = entry
                                .get("args")
                                .and_then(|args| args.as_array())
                                .map(|args| {
                                    args.iter()
                                        .filter_map(|arg| arg.as_str().map(str::to_owned))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let command = entry
                                .get("command")
                                .and_then(|command| command.as_str())
                                .unwrap_or_default();
                            (command.to_owned(), args)
                        }),
                    Err(error) => {
                        checks.push(Check::new(
                            Status::Warn,
                            format!("{label}: {file} is not valid TOML, so doctor cannot check its justless server ({error})"),
                        ));
                        continue;
                    }
                }
            } else {
                match serde_json::from_str::<Value>(&text) {
                    Ok(config) => config
                        .get(client.servers_key())
                        .and_then(|servers| servers.get("justless"))
                        .map(|entry| {
                            (
                                entry["command"].as_str().unwrap_or_default().to_owned(),
                                strings(&entry["args"]),
                            )
                        }),
                    Err(_) => {
                        checks.push(Check::new(
                            Status::Warn,
                            format!("{label}: {file} is not plain JSON, perhaps because it has comments, so doctor cannot check its justless server"),
                        ));
                        continue;
                    }
                }
            };
            let Some((command, args)) = entry else {
                continue;
            };
            let shown = std::iter::once(command.as_str())
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            let want = strings(&client.server_entry()["args"]);
            let program = Path::new(&command);
            // `--root`, the one global option, may come before the subcommand.
            let mut rest = args.as_slice();
            loop {
                match rest {
                    [flag, _, tail @ ..] if flag == "--root" => rest = tail,
                    [flag, tail @ ..] if flag.starts_with("--root=") => rest = tail,
                    _ => break,
                }
            }
            let problem = if rest.first().map(String::as_str) != Some("mcp") {
                Some((
                    "which does not start the MCP server",
                    "args",
                    Value::from(want),
                ))
            } else if program.file_stem().is_none_or(|stem| stem != "justless") {
                Some(("which is not justless", "command", json!("justless")))
            } else if program.is_absolute() && !program.is_file() {
                Some(("and that file does not exist", "command", json!("justless")))
            } else {
                None
            };
            checks.push(if let Some((why, key, value)) = problem {
                Check::new(
                    Status::Fail,
                    format!("{label}: {file} runs `{shown}`, {why}"),
                )
                .fix(Fix::Manual(format!(
                    "In {file}, set the justless server's {key} to {value}"
                )))
            } else {
                Check::new(Status::Ok, format!("{label}: {file} runs `{shown}`"))
            });
        }
        checks
    }

    fn state_checks(&self, facts: &Facts) -> Vec<Check> {
        let root = &self.workspace.root;
        let state = &self.workspace.state;
        let directory = self.shell_path(".justless");
        let mut checks = Vec::new();
        let mut exists = true;
        match fs::symlink_metadata(state) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                exists = false;
                checks.push(Check::new(
                    Status::Info,
                    ".justless/ does not exist yet. The first routed command, query or `justless init` creates it.",
                ));
            }
            Err(error) => checks.push(
                Check::new(Status::Fail, format!("Cannot inspect .justless/: {error}")).fix(
                    Fix::Manual(format!(
                        "Give your user access to {} and its .justless/ directory; `ls -ld {}` shows their owners and modes",
                        root.display(),
                        self.shell_path(".")
                    )),
                ),
            ),
            Ok(metadata) if !metadata.is_dir() => checks.push(
                Check::new(
                    Status::Fail,
                    format!(
                        ".justless is a {}, but Justless keeps its state in a directory there",
                        if metadata.file_type().is_symlink() {
                            "link"
                        } else {
                            "file"
                        }
                    ),
                )
                .fix(Fix::Command(format!("mv {directory} {directory}.bak"))),
            ),
            Ok(metadata) => {
                let before = checks.len();
                // State files are replaced by a rename, so only their directories
                // need write access. Lock files are opened for reading and writing.
                let locked = STATE_DIRECTORIES
                    .iter()
                    .map(|name| (state.join(name), true))
                    .chain(LOCK_FILES.iter().map(|name| (state.join(name), false)))
                    .find(|(path, _)| fs::symlink_metadata(path).is_ok() && !writable(path));
                if let Some((path, directory_entry)) = locked {
                    let shown = path.strip_prefix(root).unwrap_or(&path).display();
                    let fix = if cfg!(unix) && foreign(&path, root) {
                        format!("sudo chown -R \"$(id -un)\" {directory}")
                    } else {
                        format!("chmod -R u+rwX {directory}")
                    };
                    let message = if directory_entry {
                        format!("{shown} is not writable, so runs and the index cannot be saved")
                    } else {
                        format!(
                            "{shown} cannot be opened for reading and writing, so commands that take this lock fail"
                        )
                    };
                    checks.push(Check::new(Status::Fail, message).fix(Fix::Command(fix)));
                }
                if shared(&metadata) {
                    checks.push(
                        Check::new(
                            Status::Warn,
                            ".justless/ is readable by other users, and captured output can include secrets",
                        )
                        .fix(Fix::Command(format!("chmod 700 {directory}"))),
                    );
                }
                if !state.join(".gitignore").is_file() {
                    checks.push(
                        Check::new(
                            Status::Warn,
                            ".justless/ has no .gitignore of its own, so Git can see archived output",
                        )
                        .fix(Fix::Command(format!(
                            "printf '*\\n' > {directory}/.gitignore"
                        ))),
                    );
                }
                if checks.len() == before {
                    checks.push(Check::new(
                        Status::Ok,
                        if cfg!(unix) {
                            ".justless/ is writable, private and has its own .gitignore"
                        } else {
                            ".justless/ is writable and has its own .gitignore"
                        },
                    ));
                }
            }
        }
        let settings = match fs::symlink_metadata(root.join(".justless.toml")) {
            Ok(_) => Some(self.workspace.settings()),
            Err(_) => None,
        };
        if exists {
            checks.push(match (facts.index_bytes, facts.index_saved) {
                (Some(bytes), Some(saved)) => Check::new(
                    Status::Ok,
                    format!("Index: {}, saved {}", human_bytes(bytes), ago(saved)),
                ),
                (Some(bytes), None) => {
                    Check::new(Status::Ok, format!("Index: {}", human_bytes(bytes)))
                }
                _ => Check::new(
                    Status::Info,
                    "No index yet. The session-start hook or the first query builds it.",
                ),
            });
            checks.push(facts.runs_check());
            let limits = match &settings {
                Some(Ok(settings)) => retention(settings),
                _ => retention(&Settings::default()),
            };
            if facts.runs > 0 || facts.observations > 0 {
                checks.push(facts.archive_check(limits));
            }
        }
        match settings {
            Some(Ok(_)) => checks.push(Check::new(Status::Ok, ".justless.toml is valid")),
            Some(Err(error)) => checks.push(
                Check::new(
                    Status::Fail,
                    format!(".justless.toml: {error:#}. Every routed command stops on this error."),
                )
                .fix(Fix::Manual(
                    "Fix .justless.toml; the Configuration section of docs/reference.md lists its keys"
                        .into(),
                )),
            ),
            None => {}
        }
        checks
    }

    fn text(&self, report: &Report) -> String {
        let mut out = String::new();
        if report.project {
            let _ = writeln!(out, "justless doctor · {}", report.place.display());
        } else {
            let _ = writeln!(
                out,
                "justless doctor · no project at {}",
                report.place.display()
            );
        }
        for section in &report.sections {
            if section.checks.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n{}", section.title);
            for check in &section.checks {
                let _ = writeln!(out, "  {} {}", check.status.symbol(), check.message);
                if let Some(fix) = &check.fix {
                    let label = if matches!(fix, Fix::Manual(_)) {
                        "by hand"
                    } else {
                        "fix"
                    };
                    let _ = writeln!(out, "    {label}: {}", self.fix_text(fix));
                }
            }
        }
        let problems = report.count(Status::Fail);
        let warnings = report.count(Status::Warn);
        out.push('\n');
        if problems == 0 {
            out.push_str("No problems found.");
            if warnings > 0 {
                let _ = write!(out, " {} above.", plural(warnings, "warning"));
            }
            out.push('\n');
        } else {
            let _ = write!(out, "{}", plural(problems, "problem"));
            if warnings > 0 {
                let _ = write!(out, " and {}", plural(warnings, "warning"));
            }
            out.push_str(if problems == 1 {
                ". To fix the problem:\n"
            } else {
                ". To fix the problems:\n"
            });
            for (manual, fix) in self.fixes(report) {
                let _ = writeln!(out, "  {}{fix}", if manual { "by hand: " } else { "" });
            }
            let _ = writeln!(out, "Then run `{} doctor` again.", self.justless());
        }
        out
    }

    /// The fix for every problem, and whether it is an edit by hand: commands
    /// first, such as one that puts `justless` on PATH, then the `init` runs
    /// combined into one command, then the edits by hand. Each `init` fix was
    /// planned on its own, so none of them depends on an edit by hand.
    fn fixes(&self, report: &Report) -> Vec<(bool, String)> {
        let mut hooked: Vec<&'static str> = Vec::new();
        let mut plain: Vec<&'static str> = Vec::new();
        let mut commands = Vec::new();
        let mut manual = Vec::new();
        for check in report.checks().filter(|check| check.status == Status::Fail) {
            match &check.fix {
                Some(Fix::Init { agents, hooks }) => {
                    let list = if *hooks { &mut hooked } else { &mut plain };
                    for agent in agents {
                        if !list.contains(agent) {
                            list.push(agent);
                        }
                    }
                }
                Some(Fix::Command(text)) if !commands.contains(text) => {
                    commands.push(text.clone());
                }
                Some(Fix::Manual(text)) if !manual.contains(text) => manual.push(text.clone()),
                _ => {}
            }
        }
        // `init --hooks` also writes the instructions.
        plain.retain(|agent| !hooked.contains(agent));
        let mut fixes: Vec<(bool, String)> = commands.into_iter().map(|fix| (false, fix)).collect();
        for (agents, hooks) in [(hooked, true), (plain, false)] {
            if agents.is_empty() {
                continue;
            }
            // `init` plans every file before it writes one, so one agent's file
            // could stop the others in a combined run.
            if agents.len() == 1 || self.setup_error(&agents, hooks).is_none() {
                fixes.push((false, self.init(&agents, hooks)));
            } else {
                fixes.extend(
                    agents
                        .iter()
                        .map(|agent| (false, self.init(&[agent], hooks))),
                );
            }
        }
        fixes.extend(manual.into_iter().map(|fix| (true, fix)));
        fixes
    }

    fn json(&self, report: &Report) -> Value {
        let sections: Vec<Value> = report
            .sections
            .iter()
            .map(|section| {
                let checks: Vec<Value> = section
                    .checks
                    .iter()
                    .map(|check| {
                        json!({
                            "status": check.status.name(),
                            "message": check.message,
                            "fix": check.fix.as_ref().map(|fix| match fix {
                                Fix::Manual(text) => json!({"manual": text}),
                                _ => json!({"command": self.fix_text(fix)}),
                            }),
                        })
                    })
                    .collect();
                json!({"id": section.id, "title": section.title, "checks": checks})
            })
            .collect();
        let problems = report.count(Status::Fail);
        json!({
            "version": VERSION,
            "binary": std::env::current_exe().ok().map(|exe| exe.display().to_string()),
            "project": report.project,
            "root": report.project.then(|| report.place.display().to_string()),
            "directory": report.place.display().to_string(),
            "healthy": problems == 0,
            "problems": problems,
            "warnings": report.count(Status::Warn),
            "sections": sections,
            "fixes": self.fixes(report).into_iter().map(|(manual, fix)| {
                if manual { json!({"manual": fix}) } else { json!({"command": fix}) }
            }).collect::<Vec<_>>(),
            "agents": report.agents,
            "state": report.facts.as_ref().map(Facts::json),
        })
    }
}

/// The archive limits `justless run` enforces, or `None` when this version keeps
/// every run until it is deleted by hand.
fn retention(settings: &Settings) -> Option<(usize, u64)> {
    Some((settings.max_runs, settings.max_archive_bytes))
}

/// The note for a configured client when no hook has routed a command yet. Runs
/// do not record which client routed them, so any hook-routed run, or a command a
/// hook passed through, counts for every client. For Claude Code, `rules` are its
/// permission rules, which can make the command hook leave commands as written.
fn routing_note(
    profile: &Profile,
    facts: &Facts,
    rules: Option<&claude_permissions::Rules>,
) -> Option<Check> {
    use claude_permissions::{Answer, Verdict, answer};
    if profile.agent == Agent::OpenCode || facts.hook_runs > 0 || facts.passed_through > 0 {
        return None;
    }
    let scope = if facts.scanned < facts.runs {
        format!(" in the newest {} runs", facts.scanned)
    } else {
        " here".into()
    };
    let label = profile.label;
    let advice = match rules.map(|rules| (rules.broken().is_empty(), rules.default_mode())) {
        Some((false, _)) => "While a Claude Code settings file cannot be used, the command hook leaves every command as written; see Permissions below.".to_owned(),
        Some((true, mode)) if answer(mode, Verdict::Allow) == Answer::Leave => format!(
            "Your settings start sessions in `{mode}` permission mode, where the command hook leaves every command as written. Start a session in another mode, then run a build or test."
        ),
        Some((true, mode)) if answer(mode, Verdict::Unknown) == Answer::Leave => format!(
            "Your settings start sessions in `{mode}` permission mode, where the command hook routes only commands that allow rules cover and leaves the rest for {label} to ask about. Allow the builds and tests you run, such as Bash(cargo test *), or answer \"Yes, and don't ask again\" when {label} asks, then run one; `justless discover` counts the commands the hook left."
        ),
        _ => format!("Start a new {label} session in this project and run a build or test."),
    };
    Some(Check::new(
        Status::Warn,
        format!(
            "{label} has not routed a command through Justless{scope} yet. {advice}{}",
            profile.trust
        ),
    ))
}

/// The `--agent` names of the clients found on PATH, in setup order. OpenCode is
/// left out: its plugin needs v1, which only running `opencode` would show.
fn detected(profiles: &[Profile], clis: &[Option<PathBuf>]) -> Vec<&'static str> {
    profiles
        .iter()
        .zip(clis)
        .filter(|(profile, cli)| cli.is_some() && profile.requirement().is_empty())
        .map(|(profile, _)| profile.name)
        .collect()
}

// ---------------------------------------------------------------------------
// Hook files

/// A hook entry that setup writes.
struct Expected {
    event: String,
    /// The handler command, without a trailing `|| true`.
    command: String,
    /// The entry is the handler, rather than a group of handlers.
    flat: bool,
    entry: Map<String, Value>,
    handler: Map<String, Value>,
}

impl Expected {
    fn mode(&self) -> &str {
        self.command.rsplit(' ').next().unwrap_or_default()
    }
}

/// The command part of a handler before any `||`.
fn head(command: &str) -> &str {
    command.split("||").next().unwrap_or_default().trim()
}

/// The hooks in a fresh hook file, under its one object-valued key.
fn expected_hooks(fresh: &Value) -> Option<(String, Vec<Expected>)> {
    let (group, events) = fresh
        .as_object()?
        .iter()
        .find(|(_, value)| value.is_object())?;
    let mut expected = Vec::new();
    for (event, entries) in events.as_object()? {
        for entry in entries.as_array()? {
            let entry = entry.as_object()?;
            match entry.get("hooks").and_then(Value::as_array) {
                Some(handlers) => {
                    for handler in handlers {
                        let handler = handler.as_object()?;
                        expected.push(Expected {
                            event: event.clone(),
                            command: head(handler.get("command")?.as_str()?).to_owned(),
                            flat: false,
                            entry: entry.clone(),
                            handler: handler.clone(),
                        });
                    }
                }
                None => {
                    let command = entry.get("command").or_else(|| entry.get("bash"))?;
                    expected.push(Expected {
                        event: event.clone(),
                        command: head(command.as_str()?).to_owned(),
                        flat: true,
                        entry: entry.clone(),
                        handler: entry.clone(),
                    });
                }
            }
        }
    }
    Some((group.clone(), expected))
}

/// A handler command with a path-qualified first word naming `justless`, such as
/// `/opt/bin/justless hook-claude before-command`, written with the bare command
/// as setup writes it, and that path.
fn unqualified(command: &str) -> (Cow<'_, str>, Option<String>) {
    let text = command.trim_start();
    let (word, rest) = match text.chars().next() {
        Some(quote @ ('\'' | '"')) => match text[1..].find(quote) {
            Some(end) => (&text[1..=end], &text[end + 2..]),
            None => return (command.into(), None),
        },
        _ => {
            let end = text.find(char::is_whitespace).unwrap_or(text.len());
            (&text[..end], &text[end..])
        }
    };
    let named = Path::new(word)
        .file_name()
        .is_some_and(|name| name == "justless" || name == "justless.exe");
    if word.contains('/') && named {
        (format!("justless{rest}").into(), Some(word.to_owned()))
    } else {
        (command.into(), None)
    }
}

#[derive(Default)]
struct Audit {
    /// Expected hooks present in the file.
    found: usize,
    /// Indexes of expected hooks the file lacks.
    missing: Vec<usize>,
    drift: Vec<Drift>,
    /// Justless handlers that setup does not write, such as another client's.
    unexpected: Vec<String>,
    /// Why the file's structure cannot hold hooks.
    shape: Option<String>,
    /// The paths that present handlers call justless by, instead of `justless`.
    paths: Vec<String>,
}

/// A field of a present hook that differs from what setup writes.
struct Drift {
    mode: String,
    command: String,
    key: String,
    have: Option<Value>,
    want: Option<Value>,
    severe: bool,
}

impl Drift {
    fn check(&self, file: &str) -> Check {
        let have = match &self.have {
            Some(value) => format!("has {} {value}", self.key),
            None => format!("has no {}", self.key),
        };
        let want = self
            .want
            .as_ref()
            .map_or_else(|| "none".to_owned(), Value::to_string);
        let fix = match &self.want {
            Some(value) => format!(
                "In {file}, set \"{}\": {value} on the hook that runs `{}`",
                self.key, self.command
            ),
            None => format!(
                "In {file}, remove \"{}\" from the hook that runs `{}`",
                self.key, self.command
            ),
        };
        let still = if self.key == "matcher" && !self.severe {
            ". Both select the events the hook handles"
        } else {
            ""
        };
        Check::new(
            if self.severe {
                Status::Fail
            } else {
                Status::Warn
            },
            format!(
                "Hooks: the {} hook in {file} {have}, and setup writes {want}{still}",
                self.mode
            ),
        )
        .fix(Fix::Manual(fix))
    }
}

fn audit_config(agent: Agent, config: &Value, group: &str, expected: &[Expected]) -> Audit {
    let mut audit = Audit::default();
    let Some(object) = config.as_object() else {
        audit.shape = Some("the file must contain a JSON object".into());
        return audit;
    };
    let empty = Map::new();
    let hooks = match object.get(group) {
        None => &empty,
        Some(Value::Object(hooks)) => hooks,
        Some(_) => {
            audit.shape = Some(format!("\"{group}\" must contain an object"));
            return audit;
        }
    };
    for (index, hook) in expected.iter().enumerate() {
        let hit = hooks
            .get(&hook.event)
            .and_then(Value::as_array)
            .and_then(|entries| {
                entries.iter().find_map(|entry| {
                    handler_for(agent, entry, hook).map(|handler| (entry, handler))
                })
            });
        match hit {
            Some((entry, handler)) => {
                audit.found += 1;
                for text in ["command", "bash"]
                    .iter()
                    .filter_map(|key| handler.get(key)?.as_str())
                {
                    if let (_, Some(path)) = unqualified(text)
                        && !audit.paths.contains(&path)
                    {
                        audit.paths.push(path);
                    }
                }
                compare(hook, entry, handler, &mut audit.drift);
            }
            None => audit.missing.push(index),
        }
    }
    for (event, entries) in hooks {
        for entry in entries.as_array().into_iter().flatten() {
            for command in commands(entry) {
                let bare = unqualified(command).0;
                let short = head(&bare);
                if short.starts_with("justless hook-")
                    && !expected
                        .iter()
                        .any(|hook| hook.event == *event && hook.command == short)
                {
                    audit.unexpected.push(format!("`{command}` under {event}"));
                }
            }
        }
    }
    audit
}

/// The handler commands of a hook entry, flat or grouped.
fn commands(entry: &Value) -> Vec<&str> {
    let mut found: Vec<&str> = ["command", "bash"]
        .iter()
        .filter_map(|key| entry.get(key)?.as_str())
        .collect();
    for handler in entry
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(command) = handler.get("command").and_then(Value::as_str) {
            found.push(command);
        }
    }
    found
}

/// The handler in `entry` that setup would count as `hook`, matched the way setup
/// matches it, so doctor and setup agree on what is missing. A handler that calls
/// justless by path also counts, although setup would not see it.
fn handler_for<'v>(agent: Agent, entry: &'v Value, hook: &Expected) -> Option<&'v Value> {
    let loose = agent.loose_hook_commands();
    let runs = |value: &Value| {
        value.as_str().is_some_and(|text| {
            let text = unqualified(text).0;
            if loose {
                head(&text) == hook.command
            } else {
                text == hook.command
            }
        })
    };
    if (hook.flat || loose) && (runs(&entry["command"]) || runs(&entry["bash"])) {
        return Some(entry);
    }
    if hook.flat && !loose {
        return None;
    }
    entry
        .get("hooks")?
        .as_array()?
        .iter()
        .find(|handler| runs(&handler["command"]))
}

fn compare(hook: &Expected, entry: &Value, handler: &Value, drift: &mut Vec<Drift>) {
    let mut differ = |key: &str, have: Option<&Value>, want: Option<&Value>, severe: bool| {
        drift.push(Drift {
            mode: hook.mode().to_owned(),
            command: hook.command.clone(),
            key: key.to_owned(),
            have: have.cloned(),
            want: want.cloned(),
            severe,
        });
    };
    // The matcher decides which tool calls reach the hook. One that still selects
    // every call setup's matcher selects is a warning: the handler checks the tool.
    let (have, want) = (entry.get("matcher"), hook.entry.get("matcher"));
    if have != want {
        differ("matcher", have, want, !selects(have, want));
    }
    for (key, want) in &hook.handler {
        if key == "matcher" {
            continue;
        }
        let have = handler.get(key);
        let severe = match key.as_str() {
            "type" => true,
            // A command that lost its `|| true` can fail a client's tool call. A
            // path to justless in place of the bare command is reported elsewhere.
            "command" | "bash" => {
                let text = have.and_then(Value::as_str);
                if text.is_some_and(|text| unqualified(text).0 == want.as_str().unwrap_or("")) {
                    continue;
                }
                want.as_str().is_some_and(|text| text.contains("||"))
                    && !text.is_some_and(|text| text.contains("||"))
            }
            _ => false,
        };
        if have != Some(want) {
            differ(key, have, Some(want), severe);
        }
    }
}

/// Whether a hook matcher selects every name that the matcher setup writes
/// selects. A missing, empty or `*` matcher selects everything. Only a matcher
/// that setup writes as an anchored pattern, such as `^Bash$` or
/// `^(Grep|Glob)$`, is read as one, so for a client such as Cursor, which may
/// compare matchers as plain names, any other matcher counts as a change.
fn selects(have: Option<&Value>, want: Option<&Value>) -> bool {
    let have = match have {
        None => return true,
        Some(Value::String(have)) if have.is_empty() || have == "*" => return true,
        Some(Value::String(have)) => have,
        Some(_) => return false,
    };
    let Some(names) = want
        .and_then(Value::as_str)
        .and_then(|want| want.strip_prefix('^')?.strip_suffix('$'))
    else {
        return false;
    };
    let names = names
        .strip_prefix('(')
        .and_then(|names| names.strip_suffix(')'))
        .unwrap_or(names);
    // Claude Code compares a matcher of letters, digits, `_` and `|` as a list of
    // names, and tests any other as a regular expression. Either reading counts.
    let listed = have
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|');
    let pattern = regex::Regex::new(have).ok();
    names.split('|').all(|name| {
        (listed && have.split('|').any(|item| item == name))
            || pattern
                .as_ref()
                .is_some_and(|pattern| pattern.is_match(name))
    })
}

/// The edit that adds missing hooks to a file whose others call justless by
/// path, since `init --hooks` would also add a second copy of those.
fn add_by_hand(file: &str, group: &str, expected: &[Expected], disk: &Audit) -> String {
    let program = disk.paths.first().map_or_else(
        || "justless".to_owned(),
        |path| shlex::try_quote(path).map_or_else(|_| path.clone(), |quoted| quoted.into_owned()),
    );
    let entries: Vec<String> = disk
        .missing
        .iter()
        .map(|&index| {
            let hook = &expected[index];
            let mut entry = Value::Object(hook.entry.clone());
            qualify(&mut entry, &program);
            format!("under \"{group}\".\"{}\" add {entry}", hook.event)
        })
        .collect();
    format!(
        "In {file}, {}. Setup looks only for the bare `justless` command, so `init --hooks` would add a second copy of the others",
        entries.join("; ")
    )
}

/// Points the handler commands in a hook entry at `program`.
fn qualify(value: &mut Value, program: &str) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if let ("command" | "bash", Value::String(text)) = (key.as_str(), &mut *item)
                    && let Some(rest) = text.strip_prefix("justless ")
                {
                    *text = format!("{program} {rest}");
                } else {
                    qualify(item, program);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| qualify(item, program)),
        _ => {}
    }
}

/// The file a hook's path to justless names, when doctor can tell without a shell.
fn expand(path: &str) -> Option<PathBuf> {
    if Path::new(path).is_absolute() {
        return Some(PathBuf::from(path));
    }
    let home = std::env::home_dir()?;
    ["~/", "$HOME/", "${HOME}/"]
        .iter()
        .find_map(|prefix| path.strip_prefix(prefix))
        .map(|rest| home.join(rest))
}

// ---------------------------------------------------------------------------
// Instructions

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Current,
    Stale,
    Absent,
    Broken(&'static str),
}

/// The Justless section of an instructions file, judged as setup judges it.
fn block_state(text: &str) -> Block {
    let (start, end, template) = agents::instruction_block();
    match (text.split_once(start), text.split_once(end)) {
        (None, None) => Block::Absent,
        (Some((prefix, rest)), Some(_)) => {
            let Some((body, suffix)) = rest.split_once(end) else {
                return Block::Broken("a Justless end marker before its start marker");
            };
            if prefix.contains(end) || suffix.contains(end) || rest.contains(start) {
                Block::Broken("duplicate Justless markers")
            } else if format!("{start}{body}{end}") == template {
                Block::Current
            } else {
                Block::Stale
            }
        }
        _ => Block::Broken("an incomplete Justless section, with only one of its markers"),
    }
}

// ---------------------------------------------------------------------------
// State

#[derive(Default)]
struct Facts {
    runs: usize,
    /// Run records read, newest first, up to `SCANNED_RUNS`.
    scanned: usize,
    hook_runs: usize,
    last_run: Option<u64>,
    last_hook_run: Option<(u64, String)>,
    run_bytes: u64,
    /// Runs whose files were measured, up to `MEASURED_RUNS`.
    measured: usize,
    observations: usize,
    observation_bytes: u64,
    last_observation: Option<u64>,
    index_bytes: Option<u64>,
    index_saved: Option<u64>,
    /// Commands the hooks saw and passed through unchanged.
    passed_through: u64,
}

#[derive(Deserialize)]
struct RunFacts {
    #[serde(default)]
    completed_ms: u64,
    #[serde(default)]
    hook: bool,
    #[serde(default)]
    label: String,
    #[serde(default)]
    family: String,
}

#[derive(Default, Deserialize)]
struct Activity {
    #[serde(default)]
    bypasses: BTreeMap<String, u64>,
}

impl Facts {
    fn collect(state: &Path) -> Self {
        let mut facts = Self::default();
        let mut runs: Vec<(u64, PathBuf)> = fs::read_dir(state.join("runs"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry.file_name().to_string_lossy().starts_with("run-")
                    && entry.file_type().is_ok_and(|kind| kind.is_dir())
            })
            .map(|entry| {
                let directory = entry.path();
                let modified = fs::metadata(directory.join("record.json"))
                    .or_else(|_| entry.metadata())
                    .and_then(|metadata| metadata.modified())
                    .map_or(0, millis);
                (modified, directory)
            })
            .collect();
        runs.sort_unstable_by(|a, b| b.cmp(a));
        facts.runs = runs.len();
        let mut seen = HashSet::new();
        for (position, (modified, directory)) in runs.iter().enumerate() {
            if position < SCANNED_RUNS {
                facts.scanned += 1;
                let record = fs::read(directory.join("record.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<RunFacts>(&bytes).ok());
                if let Some(record) = record {
                    let when = if record.completed_ms > 0 {
                        record.completed_ms
                    } else {
                        *modified
                    };
                    facts.last_run = facts.last_run.max(Some(when));
                    if record.hook {
                        facts.hook_runs += 1;
                        if facts
                            .last_hook_run
                            .as_ref()
                            .is_none_or(|(last, _)| when > *last)
                        {
                            let label = if record.label.is_empty() {
                                record.family
                            } else {
                                record.label
                            };
                            facts.last_hook_run = Some((when, label));
                        }
                    }
                }
            }
            if position < MEASURED_RUNS {
                facts.measured += 1;
                for file in fs::read_dir(directory).into_iter().flatten().flatten() {
                    if let Ok(metadata) = file.metadata()
                        && metadata.is_file()
                        && seen.insert(identity(&metadata, &file.path()))
                    {
                        facts.run_bytes += metadata.len();
                    }
                }
            }
        }
        for file in fs::read_dir(state.join("observations"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Ok(metadata) = file.metadata()
                && metadata.is_file()
                && file.path().extension().is_some_and(|ext| ext == "json")
            {
                facts.observations += 1;
                facts.observation_bytes += metadata.len();
                let modified = metadata.modified().map_or(0, millis);
                facts.last_observation = facts.last_observation.max(Some(modified));
            }
        }
        if let Ok(metadata) = fs::metadata(state.join("index.json")) {
            facts.index_bytes = Some(metadata.len());
            facts.index_saved = metadata.modified().ok().map(millis);
        }
        facts.passed_through = fs::read(state.join("activity.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Activity>(&bytes).ok())
            .map_or(0, |activity| activity.bypasses.values().sum());
        facts
    }

    fn runs_check(&self) -> Check {
        if self.runs == 0 {
            let mut message = String::from("No captured runs yet.");
            if self.passed_through > 0 {
                let _ = write!(
                    message,
                    " Hooks passed {} through unchanged.",
                    plural(self.passed_through as usize, "command")
                );
            }
            return Check::new(Status::Info, message);
        }
        let mut message = format!("Runs: {} captured", self.runs);
        if self.scanned < self.runs {
            let _ = write!(
                message,
                "; {} of the newest {} routed by hooks",
                self.hook_runs, self.scanned
            );
        } else {
            let _ = write!(message, ", {} routed by hooks", self.hook_runs);
        }
        match &self.last_hook_run {
            Some((when, label)) => {
                let _ = write!(message, ", the last {} ({label})", ago(*when));
            }
            None => {
                if let Some(when) = self.last_run {
                    let _ = write!(message, "; last run {}", ago(when));
                }
            }
        }
        if self.passed_through > 0 {
            let _ = write!(
                message,
                ". Hooks passed {} through unchanged",
                plural(self.passed_through as usize, "other command")
            );
        }
        Check::new(Status::Ok, message)
    }

    fn archive_check(&self, limits: Option<(usize, u64)>) -> Check {
        let at_least = if self.measured < self.runs {
            "at least "
        } else {
            ""
        };
        let observations = if self.observations > 0 {
            format!(
                ", and {} in {}",
                human_bytes(self.observation_bytes),
                plural(self.observations, "retired tool result")
            )
        } else {
            String::new()
        };
        match limits {
            None => Check::new(
                Status::Info,
                format!(
                    "Archive: {at_least}{} of run output{observations}. Justless deletes none of it; remove old run directories under .justless/runs/ to reclaim space.",
                    human_bytes(self.run_bytes)
                ),
            ),
            Some((max_runs, max_bytes)) => {
                let over = self.runs > max_runs || self.run_bytes > max_bytes;
                Check::new(
                    if over { Status::Warn } else { Status::Ok },
                    format!(
                        "Archive: {} of max_runs {max_runs}, {at_least}{} of max_archive_bytes {}{observations}{}",
                        plural(self.runs, "run"),
                        human_bytes(self.run_bytes),
                        human_bytes(max_bytes),
                        if over {
                            ". The next run removes the oldest runs over the limit."
                        } else {
                            ""
                        }
                    ),
                )
            }
        }
    }

    fn json(&self) -> Value {
        json!({
            "runs": self.runs,
            "runs_scanned": self.scanned,
            "hook_routed_runs": self.hook_runs,
            "last_run_ms": self.last_run,
            "last_hook_routed_run_ms": self.last_hook_run.as_ref().map(|(when, _)| when),
            "last_hook_routed_label": self.last_hook_run.as_ref().map(|(_, label)| label),
            "hook_pass_through": self.passed_through,
            "run_bytes": self.run_bytes,
            "runs_measured": self.measured,
            "observations": self.observations,
            "observation_bytes": self.observation_bytes,
            "last_observation_ms": self.last_observation,
            "index_bytes": self.index_bytes,
            "index_saved_ms": self.index_saved,
        })
    }
}

/// A key that is the same for every hard link to one file.
#[cfg(unix)]
fn identity(metadata: &fs::Metadata, _: &Path) -> (u64, u64, PathBuf) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino(), PathBuf::new())
}

#[cfg(not(unix))]
fn identity(_: &fs::Metadata, path: &Path) -> (u64, u64, PathBuf) {
    (0, 0, path.to_path_buf())
}

/// Whether this user can create files in a directory, or open a file for
/// reading and writing.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn writable(path: &Path) -> bool {
    use rustix::fs::{Access, access};
    let mode = if path.is_dir() {
        Access::WRITE_OK | Access::EXEC_OK
    } else {
        Access::READ_OK | Access::WRITE_OK
    };
    access(path, mode).is_ok()
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn writable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| !metadata.permissions().readonly())
}

/// Whether a directory grants any access to other users.
#[cfg(unix)]
fn shared(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o077 != 0
}

#[cfg(not(unix))]
fn shared(_: &fs::Metadata) -> bool {
    false
}

/// Whether `path` belongs to another user than the project root does.
#[cfg(unix)]
fn foreign(path: &Path, root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(path), fs::metadata(root)) {
        (Ok(path), Ok(root)) => path.uid() != root.uid(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn foreign(_: &Path, _: &Path) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Programs on PATH

/// Each executable named `program` on PATH, in PATH order, one per distinct file.
fn on_path(program: &str) -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for directory in std::env::split_paths(&path).filter(|directory| directory.is_absolute()) {
        for suffix in SUFFIXES {
            let candidate = directory.join(format!("{program}{suffix}"));
            if executable(&candidate) {
                let canonical = candidate
                    .canonicalize()
                    .unwrap_or_else(|_| candidate.clone());
                if seen.insert(canonical) {
                    found.push(candidate);
                }
                break;
            }
        }
    }
    found
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn same_file(path: &Path, canonical: Option<&Path>) -> bool {
    canonical.is_some_and(|canonical| path.canonicalize().is_ok_and(|path| path == canonical))
}

/// The version another `justless` reports, and the build revision a current one
/// adds on the next line, waiting at most two seconds. Lines are read on a helper
/// thread, because a process it starts can keep the pipe open after it exits.
fn version_of(binary: &Path) -> Option<(String, Option<String>)> {
    let mut child = Process::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout.take(4096));
        for _ in 0..2 {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || sender.send(line).is_err() {
                break;
            }
        }
    });
    let line = receiver.recv_timeout(Duration::from_secs(2));
    // Both lines arrive in one write, so an older copy only costs this wait when
    // a child keeps the pipe open.
    let next = line
        .is_ok()
        .then(|| receiver.recv_timeout(Duration::from_millis(200)).ok())
        .flatten();
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
        let _ = child.wait();
    }
    let line = line.ok()?;
    let version = line.trim().strip_prefix("justless ")?.trim();
    let revision = next
        .as_deref()
        .and_then(|next| next.trim().strip_prefix("revision "))
        .map(str::trim)
        .filter(|id| matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned);
    (!version.is_empty()).then(|| (version.to_owned(), revision))
}

// ---------------------------------------------------------------------------
// Formatting

fn quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    shlex::try_quote(&text).map_or_else(|_| text.to_string(), |quoted| quoted.into_owned())
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn ago(ms: u64) -> String {
    let seconds = now_ms().saturating_sub(ms) / 1000;
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {}", plural_word(count, noun))
}

fn plural_word(count: usize, noun: &str) -> String {
    if count == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
}

/// `a`, `a and b`, or `a, b and c`.
fn list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let items: Vec<&str> = items.collect();
    match items.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// What Justless knows beyond this project: the global hooks and whether they
/// serve `project`, the project registry and the registered projects whose own
/// settings run Justless hooks. Reads only.
fn machine_section(project: Option<&Workspace>) -> Section {
    let mut section = Section::new("machine", "This machine");
    section.checks.extend(global_checks(project));
    let Some(home) = crate::registry::home() else {
        return section;
    };
    let file = home.join("projects.json");
    let projects = crate::registry::projects();
    section.checks.push(Check::new(
        Status::Info,
        if projects.is_empty() {
            format!(
                "Project registry: none yet at {}. Justless lists each project there as it uses it, for `stats --all` and finding runs from another project.",
                file.display()
            )
        } else {
            format!(
                "Project registry: {} at {}",
                plural(projects.len(), "project"),
                file.display()
            )
        },
    ));
    let hooked = crate::registry::hooked_projects();
    if !hooked.is_empty() {
        const SHOWN: usize = 8;
        let mut names: Vec<String> = hooked
            .iter()
            .take(SHOWN)
            .map(|(root, files)| format!("{} ({})", root.display(), files.join(", ")))
            .collect();
        if hooked.len() > SHOWN {
            names.push(format!("and {} more", hooked.len() - SHOWN));
        }
        section.checks.push(Check::new(
            Status::Info,
            format!("Projects with Justless hooks: {}", names.join("; ")),
        ));
    }
    section
}

/// The global hooks `init --global` installs: present, current, duplicated by
/// other Justless hooks in the same user file, and whether they serve `project`.
fn global_checks(project: Option<&Workspace>) -> Vec<Check> {
    let installed = crate::global::installed();
    if installed.is_empty() {
        return vec![Check::new(
            Status::Info,
            "Global hooks: none. `justless init --global` adds opt-in user-level hooks for Claude Code and Codex that work in every project.",
        )];
    }
    let mut checks = Vec::new();
    for hooks in &installed {
        let file = hooks.file.display();
        let name = if hooks.agent == Agent::Claude {
            "claude"
        } else {
            "codex"
        };
        if hooks.global > 0 {
            checks.push(Check::new(
                Status::Ok,
                format!(
                    "Global hooks for {}: {} in {file}",
                    hooks.label,
                    plural(hooks.global, "handler")
                ),
            ));
        }
        if hooks.incomplete {
            checks.push(
                Check::new(
                    Status::Warn,
                    format!(
                        "{file} lacks global {} hooks this version adds",
                        hooks.label
                    ),
                )
                .fix(Fix::Command(format!(
                    "justless init --global --agent {name}"
                ))),
            );
        }
        if hooks.plain > 0 {
            let both = if hooks.global > 0 {
                " as well as the global ones, so a command can be handled twice"
            } else {
                ""
            };
            checks.push(
                Check::new(
                    Status::Warn,
                    format!(
                        "{file} runs {} without --global{both}. Such a handler serves the directory the client starts in, not the project of each call",
                        plural(hooks.plain, "Justless hook")
                    ),
                )
                .fix(Fix::Manual(format!(
                    "Remove the `justless hook-{name}` handlers without --global from {file}, then run `justless init --global --agent {name}`"
                ))),
            );
        }
        let Some(workspace) = project.filter(|_| hooks.global > 0) else {
            continue;
        };
        let opted_out = workspace
            .settings()
            .is_ok_and(|settings| !settings.global_hooks);
        let here = &workspace.cwd;
        checks.push(Check::new(
            Status::Info,
            if opted_out {
                format!(
                    "This project sets global_hooks = false, so the global {} hooks leave it alone",
                    hooks.label
                )
            } else if crate::global::project_hooks(hooks.agent, None, here, &workspace.root) {
                format!(
                    "This project's own {} hooks take precedence, so the global ones leave it alone",
                    hooks.label
                )
            } else {
                format!("The global {} hooks serve this project", hooks.label)
            },
        ));
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fresh_hook_file_is_understood_and_current() {
        for (_, agent) in AGENTS {
            if agent == Agent::OpenCode {
                continue;
            }
            let fresh: Value = serde_json::from_str(&agents::fresh_hooks(agent).unwrap()).unwrap();
            let (group, expected) = expected_hooks(&fresh).unwrap();
            assert!(!expected.is_empty());
            let audit = audit_config(agent, &fresh, &group, &expected);
            assert!(audit.missing.is_empty() && audit.drift.is_empty());
            assert!(audit.unexpected.is_empty() && audit.shape.is_none());
            assert_eq!(audit.found, expected.len());
        }
    }

    #[test]
    fn a_drifted_matcher_and_a_lost_guard_are_problems() {
        let fresh: Value =
            serde_json::from_str(&agents::fresh_hooks(Agent::Copilot).unwrap()).unwrap();
        let (group, expected) = expected_hooks(&fresh).unwrap();
        let mut config = fresh.clone();
        let entry = &mut config["hooks"]["preToolUse"][0];
        entry["matcher"] = json!("shell");
        entry["bash"] = json!("justless hook-copilot before-command");
        entry["timeoutSec"] = json!(30);
        let audit = audit_config(Agent::Copilot, &config, &group, &expected);
        assert!(audit.missing.is_empty());
        let severe: Vec<(&str, bool)> = audit
            .drift
            .iter()
            .map(|drift| (drift.key.as_str(), drift.severe))
            .collect();
        assert_eq!(
            severe,
            [("matcher", true), ("bash", true), ("timeoutSec", false)]
        );
    }

    #[test]
    fn instruction_blocks_are_judged_like_setup() {
        let (start, end, template) = agents::instruction_block();
        assert!(matches!(block_state("# Notes\n"), Block::Absent));
        assert!(matches!(
            block_state(&format!("# Notes\n\n{template}\n")),
            Block::Current
        ));
        assert!(matches!(
            block_state(&format!("{start}\nold text\n{end}\n")),
            Block::Stale
        ));
        assert!(matches!(
            block_state(&format!("{start}\nno end\n")),
            Block::Broken(_)
        ));
        assert!(matches!(
            block_state(&format!("{template}\n{template}\n")),
            Block::Broken(_)
        ));
    }

    #[test]
    fn archive_limits_compare_runs_and_bytes() {
        let facts = Facts {
            runs: 250,
            measured: 250,
            run_bytes: 300 << 20,
            ..Facts::default()
        };
        let check = facts.archive_check(Some((200, 256 << 20)));
        assert!(check.status == Status::Warn, "{}", check.message);
        assert!(check.message.contains("250 runs of max_runs 200"));
        let check = facts.archive_check(Some((500, 1 << 30)));
        assert!(check.status == Status::Ok, "{}", check.message);
        let check = facts.archive_check(None);
        assert!(check.status == Status::Info);
        assert!(check.message.contains("Justless deletes none of it"));
    }

    /// Fails once `.justless.toml` accepts archive limits while doctor still
    /// reports that Justless keeps every run.
    #[test]
    fn retention_follows_the_archive_settings() {
        for (text, key) in [
            ("max_runs = 7", "max_runs"),
            ("max_archive_bytes = 1048576", "max_archive_bytes"),
        ] {
            if let Ok(settings) = toml_edit::de::from_str::<Settings>(text) {
                assert!(
                    retention(&settings).is_some(),
                    "Settings accepts {key}, so retention() must report the archive limits"
                );
            }
        }
    }

    #[test]
    fn a_matcher_that_still_selects_every_target_is_a_warning() {
        let want = json!("^Bash$");
        for have in ["Bash", "Bash|Read", ".*", "^Bash", "*", ""] {
            assert!(selects(Some(&json!(have)), Some(&want)), "{have}");
        }
        assert!(selects(None, Some(&want)));
        for have in ["Read", "bash", "^Shell$"] {
            assert!(!selects(Some(&json!(have)), Some(&want)), "{have}");
        }
        // Every name the setup matcher selects must still be selected.
        let want = json!("^(Grep|Glob)$");
        assert!(selects(Some(&json!("Grep|Glob")), Some(&want)));
        assert!(selects(Some(&json!("^G")), Some(&want)));
        assert!(!selects(Some(&json!("Grep")), Some(&want)));
        // A plain matcher, as Cursor and Copilot get, may be compared as a name.
        assert!(!selects(Some(&json!("^Shell$")), Some(&json!("Shell"))));
        assert!(selects(Some(&json!("*")), None));
        assert!(!selects(Some(&json!("startup")), None));
    }

    #[test]
    fn hooks_that_call_justless_by_path_are_present() {
        assert_eq!(
            unqualified("/opt/my tools/justless hook-claude after-read").1,
            None,
            "an unquoted path with a space is two words"
        );
        let (bare, path) = unqualified("'/opt/my tools/justless' hook-claude after-read");
        assert_eq!(bare, "justless hook-claude after-read");
        assert_eq!(path.as_deref(), Some("/opt/my tools/justless"));
        assert_eq!(
            unqualified("~/.cargo/bin/justless hook-copilot before-command || true").0,
            "justless hook-copilot before-command || true"
        );
        assert_eq!(unqualified("notjustless hook-claude x").1, None);

        for (_, agent) in AGENTS {
            if agent == Agent::OpenCode {
                continue;
            }
            let fresh = agents::fresh_hooks(agent).unwrap();
            let (group, expected) = expected_hooks(&serde_json::from_str(&fresh).unwrap()).unwrap();
            let config: Value = serde_json::from_str(
                &fresh.replace("\"justless hook-", "\"/opt/bin/justless hook-"),
            )
            .unwrap();
            let audit = audit_config(agent, &config, &group, &expected);
            assert_eq!(audit.found, expected.len());
            assert!(audit.missing.is_empty() && audit.unexpected.is_empty());
            assert!(audit.drift.is_empty(), "{}", audit.drift[0].key);
            assert_eq!(audit.paths, ["/opt/bin/justless"]);
        }
    }
}
