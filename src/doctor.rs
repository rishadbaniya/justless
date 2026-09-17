//! `justless doctor` checks the binary, each agent's hooks and instructions, MCP
//! entries and `.justless/` against what this version of Justless writes, and prints
//! a fix for each problem. It only reads: it never opens the index, takes a lock or
//! creates `.justless/`.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::agents::{self, AGENTS, Agent};
use crate::history::{human_bytes, now_ms};
use crate::mcp::Client;
use crate::workspace::{Settings, Workspace};

pub fn command() -> Command<'static> {
    Command::new("doctor")
        .about("Check this project's Justless setup and print a fix for each problem")
        .arg(Arg::new("json").long("json").help("Print a JSON report"))
}

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Run records read for evidence that hooks route commands, newest first.
const SCANNED_RUNS: usize = 500;
/// Run directories measured for the archive size.
const MEASURED_RUNS: usize = 20_000;
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
    /// `justless init` for one agent, with or without `--hooks`.
    Init { agent: &'static str, hooks: bool },
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
        let marked = [".git", ".justless", ".justless.toml"]
            .iter()
            .any(|name| fs::symlink_metadata(root.join(name)).is_ok());
        if !self.explicit_root && !set_up && !marked && here == Some(root.as_path()) {
            return self.outside(sections, &profiles, &clis, false);
        }
        let facts = Facts::collect(&self.workspace.state);
        let mut others = Section::new("agents", if set_up { "Other agents" } else { "Agents" });
        let mut missing = Vec::new();
        let mut agents_json = Vec::new();
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
                        .fix(Fix::Init {
                            agent: profile.name,
                            hooks: true,
                        }),
                    );
                }
                section.checks.extend(audit.checks);
                if audit.hooked {
                    section.checks.extend(routing_note(profile, &facts));
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
                    .fix(Fix::Init {
                        agent: profile.name,
                        hooks: true,
                    })
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
            let detected = detected(&profiles, &clis);
            let mut message = String::from(
                "Justless is not set up in this project: no agent hooks, instructions or MCP entries",
            );
            if detected.is_empty() {
                message.push_str(
                    ". Replace claude in the fix with your agent: codex, cursor, gemini, copilot, opencode or agy",
                );
            }
            let names = if detected.is_empty() {
                vec!["claude"]
            } else {
                detected
            };
            others
                .checks
                .push(Check::new(Status::Fail, message).fix(Fix::Command(self.init(&names, true))));
        }
        sections.push(others);
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
        let init = self.init(&names, true);
        let mut project = Section::new("project", "Project");
        project.checks.push(Check::new(
            Status::Info,
            if at_home {
                format!(
                    "{} is your home directory. Justless sets up hooks per project, so they never land in your global agent settings.",
                    place.display()
                )
            } else {
                format!(
                    "{} has no Justless setup, and neither it nor a parent directory has .git or .justless/.",
                    place.display()
                )
            },
        ));
        project.checks.push(Check::new(
            Status::Info,
            "Inside a project, doctor also checks each agent's hooks and instructions against what `justless init` writes, MCP entries and the .justless/ archive.",
        ));
        project.checks.push(Check::new(
            Status::Info,
            if at_home {
                format!("To set up a project, run this in its folder: {init}")
            } else {
                format!("If this folder is your project, set it up with: {init}")
            },
        ));
        sections.push(project);
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
            Fix::Init { agent, hooks } => self.init(&[agent], *hooks),
            Fix::Command(text) | Fix::Manual(text) => text.clone(),
        }
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
        match found.first() {
            None => section.checks.push(
                Check::new(
                    Status::Fail,
                    "`justless` is not on PATH. Hooks and MCP entries call it by name, so they cannot start. Add the fix line to your shell profile too, so new agent sessions see it.",
                )
                .fix(Fix::Command(use_this)),
            ),
            Some(first) if same_file(first, canonical.as_deref()) => section
                .checks
                .push(Check::new(Status::Ok, "`justless` on PATH is this binary")),
            Some(first) => section.checks.push(match version_of(first) {
                Some(version) if version == VERSION => Check::new(
                    Status::Warn,
                    format!(
                        "`justless` on PATH is {}, another copy of {VERSION}. Hooks run that copy. To use this one, put its directory first on PATH here and in your shell profile.",
                        first.display()
                    ),
                )
                .fix(Fix::Command(use_this)),
                Some(version) => Check::new(
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
            let others: Vec<String> = found[1..].iter().map(|path| quote(path)).collect();
            let mut check = Check::new(
                Status::Warn,
                format!(
                    "{} copies of justless are on PATH. Hooks run the first, {}, and the others are shadowed: {}",
                    found.len(),
                    found[0].display(),
                    others.join(", ")
                ),
            );
            if same_file(&found[0], canonical.as_deref()) {
                check = check.fix(Fix::Command(format!("rm {}", others.join(" "))));
            }
            section.checks.push(check);
        }
        section
    }

    fn audit_agent<'p>(&self, profile: &'p Profile, cli: Option<PathBuf>) -> AgentAudit<'p> {
        let (hooked, mut checks) = self.audit_hooks(profile);
        let (block, instructions) = self.audit_instructions(profile, hooked);
        // Without hooks, a section in a file only this client reads still shows setup.
        let instructed = !hooked
            && block
            && matches!(profile.agent, Agent::Claude | Agent::Gemini | Agent::Codex);
        if hooked || instructed {
            checks.extend(instructions);
        }
        AgentAudit {
            profile,
            cli,
            hooked,
            instructed,
            checks,
        }
    }

    /// Whether the agent's hook file holds Justless entries, and the checks for it.
    fn audit_hooks(&self, profile: &Profile) -> (bool, Vec<Check>) {
        let relative = profile.agent.hook_file();
        let file = relative.display().to_string();
        let init = Fix::Init {
            agent: profile.name,
            hooks: true,
        };
        let text = match fs::read_to_string(self.workspace.root.join(&relative)) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return (false, Vec::new()),
            Err(error) => {
                return (
                    true,
                    vec![
                        Check::new(Status::Fail, format!("Hooks: cannot read {file}: {error}"))
                            .fix(Fix::Manual(format!(
                                "Make {file} a readable file, then run `{}`",
                                self.fix_text(&init)
                            ))),
                    ],
                );
            }
        };
        let pending = agents::pending_hooks(self.workspace, profile.agent);
        if profile.agent == Agent::OpenCode {
            let check = match pending {
                Ok(None) => Check::new(Status::Ok, format!("Plugin: {file} is current")),
                Ok(Some(_)) => Check::new(
                    Status::Fail,
                    format!("Plugin: {file} is from another version of Justless"),
                )
                .fix(init),
                Err(_) => {
                    let path = self.shell_path(&file);
                    Check::new(
                        Status::Fail,
                        format!("Plugin: {file} is not managed by Justless, so setup will not replace it"),
                    )
                    .fix(Fix::Command(format!(
                        "mv {path} {path}.bak && {}",
                        self.fix_text(&init)
                    )))
                }
            };
            return (true, vec![check]);
        }
        let tag = format!("justless hook-{}", profile.name);
        let config: Value = match serde_json::from_str(&text) {
            Ok(config) => config,
            Err(error) if text.contains(&tag) => {
                return (
                    true,
                    vec![
                        Check::new(
                            Status::Fail,
                            format!("Hooks: {file} is not valid JSON ({error})"),
                        )
                        .fix(Fix::Manual(format!(
                            "Fix the JSON in {file}, then run `{}`",
                            self.fix_text(&init)
                        ))),
                    ],
                );
            }
            Err(_) => return (false, Vec::new()),
        };
        let expected = agents::fresh_hooks(profile.agent)
            .ok()
            .and_then(|fresh| serde_json::from_str::<Value>(&fresh).ok())
            .and_then(|fresh| expected_hooks(&fresh));
        let Some((group, expected)) = expected else {
            return (
                true,
                vec![Check::new(
                    Status::Fail,
                    format!("Hooks: cannot work out the hooks setup writes to {file}"),
                )],
            );
        };
        let disk = audit_config(profile.agent, &config, &group, &expected);
        if let Some(shape) = &disk.shape {
            if !text.contains(&tag) {
                return (false, Vec::new());
            }
            return (
                true,
                vec![
                    Check::new(Status::Fail, format!("Hooks: in {file}, {shape}")).fix(
                        Fix::Manual(format!(
                            "Fix the structure of {file}, then run `{}`",
                            self.fix_text(&init)
                        )),
                    ),
                ],
            );
        }
        if disk.found == 0 && disk.unexpected.is_empty() {
            return (false, Vec::new());
        }
        let mut checks = Vec::new();
        // Setup adds missing entries and leaves existing ones as they are, so audit
        // what it would write: a problem left there needs a fix by hand.
        let merged = match &pending {
            Ok(Some(contents)) => serde_json::from_str::<Value>(contents)
                .ok()
                .map(|merged| audit_config(profile.agent, &merged, &group, &expected)),
            _ => None,
        };
        let after = merged.as_ref().unwrap_or(&disk);
        if !disk.missing.is_empty() {
            let modes = list(disk.missing.iter().map(|&index| expected[index].mode()));
            let hooks = plural_word(disk.missing.len(), "hook");
            checks.push(match &pending {
                Ok(Some(_)) if after.missing.is_empty() => Check::new(
                    Status::Fail,
                    format!("Hooks: {file} lacks the {modes} {hooks}"),
                )
                .fix(init),
                Err(error) => Check::new(
                    Status::Fail,
                    format!(
                        "Hooks: {file} lacks the {modes} {hooks}, and setup cannot add them: {error:#}"
                    ),
                )
                .fix(Fix::Manual(format!(
                    "Fix {file} so setup can read it, then run `{}`",
                    self.fix_text(&init)
                ))),
                _ => Check::new(
                    Status::Fail,
                    format!(
                        "Hooks: {file} lacks the {modes} {hooks}, which setup does not add over the entries it finds"
                    ),
                )
                .fix(Fix::Manual(format!(
                    "Remove the Justless entries from {file}, then run `{}`",
                    self.fix_text(&init)
                ))),
            });
        } else {
            match &pending {
                Ok(Some(_)) => checks.push(
                    Check::new(Status::Fail, format!("Hooks: setup would update {file}")).fix(init),
                ),
                Err(error) => checks.push(
                    Check::new(
                        Status::Fail,
                        format!("Hooks: setup cannot update {file}: {error:#}"),
                    )
                    .fix(Fix::Manual(format!(
                        "Fix {file} so setup can read it, then run `{}`",
                        self.fix_text(&init)
                    ))),
                ),
                Ok(None) => {}
            }
        }
        checks.extend(after.drift.iter().map(|drift| drift.check(&file)));
        checks.extend(disk.unexpected.iter().map(|entry| {
            Check::new(
                Status::Warn,
                format!("Hooks: {file} also runs {entry}, which setup does not write"),
            )
            .fix(Fix::Manual(format!("Remove that hook from {file}")))
        }));
        if checks.is_empty() {
            let modes: Vec<&str> = expected.iter().map(Expected::mode).collect();
            let all = match expected.len() {
                1 => "its Justless hook".to_owned(),
                2 => "both Justless hooks".to_owned(),
                count => format!("all {count} Justless hooks"),
            };
            checks.push(Check::new(
                Status::Ok,
                format!("Hooks: {file} has {all} ({})", modes.join(", ")),
            ));
        }
        (true, checks)
    }

    /// Whether the agent's own instructions file has a Justless section, and the
    /// check of the section the agent loads.
    fn audit_instructions(&self, profile: &Profile, hooks: bool) -> (bool, Vec<Check>) {
        let root = &self.workspace.root;
        let name = profile.agent.instructions_name();
        let path = root.join(name);
        let init = Fix::Init {
            agent: profile.name,
            hooks,
        };
        // Setup skips a link that leaves the project.
        let editable =
            agents::instructions_target(root, &path).map_or(true, |target| target.is_some());
        let rewrite = |init: Fix| {
            if editable {
                init
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
                            .fix(rewrite(init)),
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
                            "Make {name} a readable file, then run `{}`",
                            self.fix_text(&init)
                        ))),
                    ],
                );
            }
        };
        let state = block_state(&text);
        let check = match state {
            Block::Current => Check::new(
                Status::Ok,
                format!("Instructions: {name} has the current Justless block"),
            ),
            Block::Stale => Check::new(
                Status::Fail,
                format!("Instructions: {name} has an older Justless block"),
            )
            .fix(rewrite(init)),
            Block::Broken(why) => {
                Check::new(Status::Fail, format!("Instructions: {name} has {why}")).fix(
                    Fix::Manual(format!(
                        "Remove the partial Justless section from {name}, then run `{}`",
                        self.fix_text(&init)
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
                .fix(rewrite(init)),
            },
        };
        (state != Block::Absent, vec![check])
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
            let refresh = match target {
                "AGENTS.md" => Fix::Command(self.init(&["codex"], false)),
                "GEMINI.md" => Fix::Command(self.init(&["gemini"], false)),
                _ => Fix::Manual(format!(
                    "Update the Justless section in {target} to the one setup writes into AGENTS.md"
                )),
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
                .fix(refresh),
                Block::Broken(why) => Check::new(
                    Status::Fail,
                    format!("Instructions: {name} imports {target}, which has {why}"),
                )
                .fix(Fix::Manual(format!(
                    "Remove the partial Justless section from {target}, then run `{}`",
                    self.fix_text(&refresh)
                ))),
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
            let problem = if args.first().map(String::as_str) != Some("mcp") {
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
            Err(error) => checks.push(Check::new(
                Status::Fail,
                format!("Cannot inspect .justless/: {error}"),
            )),
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
                let locked = [
                    "",
                    "runs",
                    "observations",
                    "baselines",
                    "index.json",
                    "activity.json",
                ]
                .iter()
                .map(|name| state.join(name))
                .filter(|path| path.exists())
                .find(|path| !writable(path));
                if let Some(path) = locked {
                    let shown = path.strip_prefix(root).unwrap_or(&path).display();
                    let fix = if cfg!(unix) && foreign(&path, root) {
                        format!("sudo chown -R \"$(id -un)\" {directory}")
                    } else {
                        format!("chmod -R u+rwX {directory}")
                    };
                    checks.push(
                        Check::new(
                            Status::Fail,
                            format!(
                                "{shown} is not writable, so runs and the index cannot be saved"
                            ),
                        )
                        .fix(Fix::Command(fix)),
                    );
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
            out.push_str("Then run `justless doctor` again.\n");
        }
        out
    }

    /// The fix for every problem, and whether it is an edit by hand, with the
    /// `init` runs combined into one command.
    fn fixes(&self, report: &Report) -> Vec<(bool, String)> {
        let mut hooked = Vec::new();
        let mut plain = Vec::new();
        let mut other = Vec::new();
        for check in report.checks().filter(|check| check.status == Status::Fail) {
            match &check.fix {
                Some(Fix::Init { agent, hooks: true }) if !hooked.contains(agent) => {
                    hooked.push(*agent);
                }
                Some(Fix::Init {
                    agent,
                    hooks: false,
                }) if !plain.contains(agent) => plain.push(*agent),
                Some(fix @ (Fix::Command(_) | Fix::Manual(_))) => {
                    let fix = (matches!(fix, Fix::Manual(_)), self.fix_text(fix));
                    if !other.contains(&fix) {
                        other.push(fix);
                    }
                }
                _ => {}
            }
        }
        // `init --hooks` also writes the instructions.
        plain.retain(|agent| !hooked.contains(agent));
        let mut fixes = Vec::new();
        if !hooked.is_empty() {
            fixes.push((false, self.init(&hooked, true)));
        }
        if !plain.is_empty() {
            fixes.push((false, self.init(&plain, false)));
        }
        fixes.extend(other);
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
fn retention(_settings: &Settings) -> Option<(usize, u64)> {
    None
}

/// The note for a configured client when no hook has routed a command yet. Runs
/// do not record which client routed them, so any hook-routed run, or a command a
/// hook passed through, counts for every client.
fn routing_note(profile: &Profile, facts: &Facts) -> Option<Check> {
    if profile.agent == Agent::OpenCode || facts.hook_runs > 0 || facts.passed_through > 0 {
        return None;
    }
    let scope = if facts.scanned < facts.runs {
        format!(" in the newest {} runs", facts.scanned)
    } else {
        " here".into()
    };
    Some(Check::new(
        Status::Warn,
        format!(
            "{} has not routed a command through Justless{scope} yet. Start a new {} session in this project and run a build or test.{}",
            profile.label, profile.label, profile.trust
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
        Check::new(
            if self.severe {
                Status::Fail
            } else {
                Status::Warn
            },
            format!(
                "Hooks: the {} hook in {file} {have}, and setup writes {want}",
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
                compare(hook, entry, handler, &mut audit.drift);
            }
            None => audit.missing.push(index),
        }
    }
    for (event, entries) in hooks {
        for entry in entries.as_array().into_iter().flatten() {
            for command in commands(entry) {
                let short = head(command);
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
/// matches it, so doctor and setup agree on what is missing.
fn handler_for<'v>(agent: Agent, entry: &'v Value, hook: &Expected) -> Option<&'v Value> {
    let loose = agent.loose_hook_commands();
    let runs = |value: &Value| {
        value.as_str().is_some_and(|text| {
            if loose {
                head(text) == hook.command
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
        if have != want {
            drift.push(Drift {
                mode: hook.mode().to_owned(),
                command: hook.command.clone(),
                key: key.to_owned(),
                have: have.cloned(),
                want: want.cloned(),
                severe,
            });
        }
    };
    // The matcher decides which tool calls reach the hook.
    differ(
        "matcher",
        entry.get("matcher"),
        hook.entry.get("matcher"),
        true,
    );
    for (key, want) in &hook.handler {
        if key == "matcher" {
            continue;
        }
        let have = handler.get(key);
        let severe = match key.as_str() {
            "type" => true,
            // A command that lost its `|| true` can fail a client's tool call.
            "command" | "bash" => {
                want.as_str().is_some_and(|text| text.contains("||"))
                    && !have
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains("||"))
            }
            _ => false,
        };
        differ(key, have, Some(want), severe);
    }
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

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn writable(path: &Path) -> bool {
    use rustix::fs::{Access, access};
    let mode = if path.is_dir() {
        Access::WRITE_OK | Access::EXEC_OK
    } else {
        Access::WRITE_OK
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

/// The version another `justless` reports, waiting at most two seconds.
fn version_of(binary: &Path) -> Option<String> {
    let mut child = Process::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.trim().strip_prefix("justless ")?.trim();
    (!version.is_empty()).then(|| version.to_owned())
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
}
