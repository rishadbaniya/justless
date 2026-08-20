mod agents;
mod capture;
mod context;
mod discover;
mod filters;
mod history;
mod impact;
mod index;
mod inventory;
mod lint;
mod listing;
mod map;
mod mcp;
mod observations;
mod pack;
mod polyglot;
mod rust_syntax;
mod search;
mod source;
mod summary;
mod workspace;

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, ensure};
use clap::{Arg, ArgGroup, ArgMatches, Command};

use crate::capture::Stream;
use crate::index::Index;
use crate::workspace::Workspace;

fn number(name: &'static str, value: &'static str, help: &'static str) -> Arg<'static> {
    Arg::new(name)
        .long(name)
        .value_name("N")
        .default_value(value)
        .help(help)
}

fn budget() -> Arg<'static> {
    Arg::new("budget")
        .long("budget")
        .value_name("N")
        .help("Context token ceiling (default 2400, conservatively budgeted by UTF-8 byte)")
}

fn id() -> Arg<'static> {
    Arg::new("run-id").required(true).value_name("RUN_ID")
}

fn streams() -> Arg<'static> {
    Arg::new("stream")
        .long("stream")
        .takes_value(true)
        .possible_values(["both", "stdout", "stderr"])
        .default_value("both")
}

fn cli() -> Command<'static> {
    Command::new("justless")
        .about("Compact command output and retrieve source-backed context")
        .version(env!("CARGO_PKG_VERSION"))
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(
            Arg::new("root")
                .long("root")
                .value_name("DIR")
                .global(true)
                .allow_invalid_utf8(true)
                .help("Override repository root"),
        )
        .subcommand(
            Command::new("run")
                .about("Run a noninteractive command and retain its full output")
                .arg(
                    Arg::new("session")
                        .long("session")
                        .takes_value(true)
                        .help("Scope repeat comparisons to this session"),
                )
                .arg(
                    Arg::new("fresh")
                        .long("fresh")
                        .help("Show a fresh preview without comparing a previous run"),
                )
                .arg(
                    Arg::new("max-bytes")
                        .long("max-bytes")
                        .value_name("N")
                        .help("Maximum preview bytes per stream (default 4096)"),
                )
                .arg(
                    Arg::new("raw")
                        .long("raw")
                        .help("Run with inherited streams and skip capture"),
                )
                .arg(
                    Arg::new("hook")
                        .long("hook")
                        .hide(true)
                        .help("Routed by an agent hook: print a footer only when it adds information"),
                )
                .arg(
                    Arg::new("command")
                        .required(true)
                        .multiple_values(true)
                        .allow_invalid_utf8(true)
                        .last(true)
                        .value_name("COMMAND [ARGS]"),
                ),
        )
        .subcommand(
            Command::new("log")
                .about("Replay original output or a specific line range")
                .arg(id())
                .arg(streams())
                .arg(number("from", "1", "First output line, inclusive"))
                .arg(
                    Arg::new("to")
                        .long("to")
                        .value_name("N")
                        .help("Last output line, inclusive"),
                ),
        )
        .subcommand(
            Command::new("search")
                .about("Search a captured run")
                .arg(id())
                .arg(Arg::new("text").required(true).value_name("TEXT"))
                .arg(streams())
                .arg(number("limit", "20", "Maximum matching lines")),
        )
        .subcommand(
            Command::new("pack")
                .about("Gather source, relationships and run evidence for a task")
                .arg(Arg::new("task").required(true))
                .arg(budget()),
        )
        .subcommand(
            Command::new("history")
                .about("List captured runs or search all their logs")
                .arg(Arg::new("text"))
                .arg(Arg::new("oldest").long("oldest"))
                .arg(number("limit", "12", "Maximum runs or matching lines"))
                .arg(budget()),
        )
        .subcommand(
            Command::new("resume")
                .about("Show recent runs, inspected source and stale evidence")
                .arg(budget()),
        )
        .subcommand(
            Command::new("stats")
                .about("Show output savings by day and command, recoveries and hook coverage")
                .arg(number("days", "7", "Days in the timeline"))
                .arg(Arg::new("json").long("json").help("Print a JSON report")),
        )
        .subcommand(
            Command::new("discover")
                .about("Find shell commands agents ran here without Justless, from Claude Code transcripts")
                .arg(number("days", "30", "Days of transcripts to scan"))
                .arg(Arg::new("json").long("json").help("Print a JSON report")),
        )
        .subcommand(Command::new("index").about("Refresh the incremental repository index"))
        .subcommand(
            Command::new("query")
                .about("Find a symbol, its references, callers, or tests")
                .arg(Arg::new("symbol").required(true))
                .arg(
                    Arg::new("view")
                        .long("view")
                        .takes_value(true)
                        .default_value("all")
                        .possible_values([
                            "definition",
                            "references",
                            "callers",
                            "tests",
                            "dependencies",
                            "all",
                        ]),
                )
                .arg(budget()),
        )
        .subcommand(
            Command::new("find")
                .about("Find exact text across source, docs, and configuration")
                .arg(Arg::new("text").required(true).value_name("TEXT"))
                .arg(
                    Arg::new("ignore-case")
                        .long("ignore-case")
                        .short('i')
                        .help("Ignore ASCII case"),
                )
                .arg(number("limit", "20", "Maximum matching lines"))
                .arg(budget()),
        )
        .subcommand(
            Command::new("path")
                .about("Trace a path between indexed symbols")
                .arg(Arg::new("from").required(true).value_name("FROM"))
                .arg(Arg::new("to").required(true).value_name("TO"))
                .arg(budget()),
        )
        .subcommand(
            Command::new("context")
                .about("Connect a run, changed lines, or a location to source")
                .arg(Arg::new("run").long("run").value_name("RUN_ID"))
                .arg(
                    Arg::new("changes")
                        .long("changes")
                        .help("Use current Git changes"),
                )
                .arg(
                    Arg::new("staged")
                        .long("staged")
                        .requires("changes")
                        .help("Use staged changes"),
                )
                .arg(Arg::new("at").long("at").value_name("PATH:LINE[-LINE]"))
                .arg(Arg::new("symbol").long("symbol").value_name("SYMBOL"))
                .group(
                    ArgGroup::new("input")
                        .required(true)
                        .args(&["run", "changes", "at", "symbol"]),
                )
                .arg(budget()),
        )
        .subcommand(
            Command::new("code")
                .about("Print an exact workspace source range")
                .arg(Arg::new("file").required(true).value_name("FILE"))
                .arg(number("from", "1", "First source line, inclusive"))
                .arg(
                    Arg::new("to")
                        .long("to")
                        .value_name("N")
                        .required(true)
                        .help("Last source line, inclusive"),
                )
                .arg(budget()),
        )
        .subcommand(
            Command::new("init")
                .visible_alias("init-codex")
                .about("Install project instructions and optional agent hooks")
                .arg(
                    Arg::new("agent")
                        .long("agent")
                        .takes_value(true)
                        .multiple_occurrences(true)
                        .possible_values([
                            "codex", "claude", "agy", "opencode", "cursor", "gemini", "copilot",
                        ])
                        .default_value("codex")
                        .help("Agent to configure (repeat to select several)"),
                )
                .arg(
                    Arg::new("hooks")
                        .long("hooks")
                        .help("Install project-local hooks"),
                ),
        )
        .subcommand(map::command())
        .subcommand(impact::command())
        .subcommand(mcp::command())
        .subcommands(source::commands())
        .subcommands(observations::commands())
        .subcommands(["hook-codex", "hook-claude", "hook-agy"].map(|name| {
            Command::new(name)
                .hide(true)
                .arg(Arg::new("mode").required(true).possible_values([
                    "session-start",
                    "before-command",
                    "after-read",
                    "after-search",
                    "after-compact",
                    "before-compact",
                ]))
        }))
        // No value list here: an unknown mode must still reach the no-op handler.
        .subcommands(["hook-cursor", "hook-gemini", "hook-copilot"].map(|name| {
            Command::new(name).hide(true).arg(Arg::new("mode"))
        }))
}

fn read_num(matches: &ArgMatches, name: &str, minimum: usize) -> Result<usize> {
    let value = matches.value_of_t::<usize>(name)?;
    ensure!(
        value >= minimum,
        "--{name} expects a number of at least {minimum}"
    );
    Ok(value)
}

fn context_budget(workspace: &Workspace, matches: &ArgMatches) -> Result<usize> {
    if matches.is_present("budget") {
        read_num(matches, "budget", 128)
    } else {
        Ok(workspace.settings()?.context_budget)
    }
}

fn dispatch(matches: &ArgMatches) -> Result<i32> {
    // Claude Code starts MCP servers with CLAUDE_PROJECT_DIR set to the project, so
    // the server follows it when no explicit root is given.
    let project = std::env::var_os("CLAUDE_PROJECT_DIR")
        .filter(|_| matches!(matches.subcommand_name(), Some("mcp")))
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_dir());
    let root = matches
        .value_of_os("root")
        .map(Path::new)
        .or(project.as_deref());
    // Cursor, Gemini CLI and Copilot block a tool call when its hook fails, so these
    // handlers answer every error, including a missing workspace, with a no-op.
    if let Some((name, sub)) = matches.subcommand()
        && let Some(agent) = agents::client(name)
    {
        agents::run_client_hook(root, agent, sub.value_of("mode").unwrap_or_default());
        return Ok(0);
    }
    let workspace = Workspace::discover(root)?;
    match matches.subcommand().expect("subcommand is required") {
        (name @ ("read" | "expand" | "edit"), sub) => {
            source::run(&workspace, name, sub, matches.is_present("root"))?
        }
        (name @ ("mask" | "recall"), sub) => observations::run(&workspace, name, sub)?,
        ("mcp", sub) => mcp::run(&workspace, sub)?,
        ("map", sub) => map::run(&workspace, sub)?,
        ("affected", sub) => impact::run(&workspace, sub)?,
        ("run", sub) => {
            let command: Vec<OsString> = sub
                .values_of_os("command")
                .unwrap()
                .map(OsString::from)
                .collect();
            let raw = sub.is_present("raw");
            let max_bytes = if raw {
                4096
            } else if sub.is_present("max-bytes") {
                read_num(sub, "max-bytes", 512)?
            } else {
                workspace.settings()?.max_output_bytes
            };
            let default_session =
                std::env::var("JUSTLESS_SESSION").unwrap_or_else(|_| "terminal".into());
            return capture::execute(
                &workspace,
                &command,
                max_bytes,
                raw,
                sub.value_of("session").unwrap_or(&default_session),
                sub.is_present("fresh"),
                sub.is_present("hook"),
            );
        }
        ("log", sub) => {
            let (directory, _) = capture::load(&workspace, sub.value_of("run-id").unwrap())?;
            let to = if sub.is_present("to") {
                read_num(sub, "to", 1)?
            } else {
                usize::MAX
            };
            capture::log(
                &directory,
                Stream::select(sub.value_of("stream").unwrap()),
                read_num(sub, "from", 1)?,
                to,
            )?;
            history::recall(
                &directory,
                if to == usize::MAX && sub.value_of("from") == Some("1") {
                    "full"
                } else {
                    "range"
                },
            );
        }
        ("search", sub) => {
            let (directory, _) = capture::load(&workspace, sub.value_of("run-id").unwrap())?;
            capture::search(
                &directory,
                Stream::select(sub.value_of("stream").unwrap()),
                sub.value_of("text").unwrap(),
                read_num(sub, "limit", 1)?,
            )?;
            history::recall(&directory, "search");
        }
        ("pack", sub) => pack::pack(
            &workspace,
            &Index::open(&workspace)?,
            sub.value_of("task").unwrap(),
            context_budget(&workspace, sub)?,
        )?,
        ("history", sub) => history::history(
            &workspace,
            sub.value_of("text"),
            sub.is_present("oldest"),
            read_num(sub, "limit", 1)?,
            context_budget(&workspace, sub)?,
        )?,
        ("resume", sub) => history::resume(&workspace, context_budget(&workspace, sub)?)?,
        ("discover", sub) => discover::discover(
            &workspace,
            read_num(sub, "days", 1)? as u64,
            sub.is_present("json"),
        )?,
        ("stats", sub) => history::stats(
            &workspace,
            read_num(sub, "days", 1)? as u64,
            sub.is_present("json"),
        )?,
        ("index", _) => {
            let index = Index::open(&workspace)?;
            let refresh = &index.refresh;
            println!(
                "{} text files · {} symbols · {} relationships · {} parsed · {} reused · {} removed",
                index.files.len(),
                index.definitions.len(),
                index.edges.len(),
                refresh.parsed,
                refresh.reused,
                refresh.removed
            );
            for (file, error) in index.errors {
                eprintln!("{file}: {error}");
            }
        }
        ("query", sub) => {
            let index = Index::open(&workspace)?;
            context::query(
                &index,
                sub.value_of("symbol").unwrap(),
                sub.value_of("view").unwrap(),
                context_budget(&workspace, sub)?,
            )?;
        }
        ("find", sub) => context::find(
            &workspace,
            &Index::open(&workspace)?,
            sub.value_of("text").unwrap(),
            sub.is_present("ignore-case"),
            read_num(sub, "limit", 1)?,
            context_budget(&workspace, sub)?,
        )?,
        ("path", sub) => context::path(
            &Index::open(&workspace)?,
            sub.value_of("from").unwrap(),
            sub.value_of("to").unwrap(),
            context_budget(&workspace, sub)?,
        )?,
        ("context", sub) => {
            let limit = context_budget(&workspace, sub)?;
            let index = Index::open(&workspace)?;
            if let Some(id) = sub.value_of("run") {
                context::from_run(&workspace, &index, id, limit)?;
            } else if let Some(location) = sub.value_of("at") {
                context::at(&workspace, &index, location, limit)?;
            } else if let Some(name) = sub.value_of("symbol") {
                context::symbol(&workspace, &index, name, limit)?;
            } else {
                context::changed(&workspace, &index, sub.is_present("staged"), limit)?;
            }
        }
        ("code", sub) => context::code(
            &workspace,
            sub.value_of("file").unwrap(),
            read_num(sub, "from", 1)?,
            read_num(sub, "to", 1)?,
            context_budget(&workspace, sub)?,
        )?,
        ("init", sub) => {
            let selected = sub
                .values_of("agent")
                .unwrap()
                .map(agents::Agent::parse)
                .collect::<Result<Vec<_>>>()?;
            agents::init(&workspace, &selected, sub.is_present("hooks"))?;
        }
        (name, sub) if name.starts_with("hook-") => {
            agents::run_hook(
                &workspace,
                agents::Agent::parse(&name[5..])?,
                sub.value_of("mode").unwrap(),
            )?;
        }
        _ => unreachable!("known subcommand"),
    }
    Ok(0)
}

/// Parsers recurse once per level of source nesting, so a deep but valid file
/// needs more stack than the default main thread has.
const STACK_BYTES: usize = 512 << 20;

fn main() -> ExitCode {
    let worker = std::thread::Builder::new()
        .name("main".into())
        .stack_size(STACK_BYTES)
        .spawn(run);
    match worker {
        Ok(handle) => handle
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
        Err(_) => run(),
    }
}

fn run() -> ExitCode {
    match dispatch(&cli().get_matches()) {
        Ok(status) => ExitCode::from(u8::try_from(status).unwrap_or(1)),
        Err(error) => {
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == ErrorKind::BrokenPipe)
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("justless: {error:#}");
            ExitCode::from(2)
        }
    }
}
