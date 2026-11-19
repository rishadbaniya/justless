mod adaptive;
mod agents;
mod batch;
mod capture;
mod claude_permissions;
mod context;
mod discover;
mod doctests;
mod doctor;
mod exec;
mod filters;
mod find;
mod focus;
mod history;
mod impact;
mod index;
mod inventory;
mod json_edit;
mod learn;
mod lint;
mod listing;
mod lookup;
mod map;
mod markdown;
mod mcp;
mod merge;
mod observations;
mod pack;
mod polyglot;
mod postroute;
mod proxy;
mod redact;
mod runners;
mod rust_syntax;
mod search;
mod selection;
mod shell_form;
mod shellread;
mod source;
mod statusline;
mod summary;
mod testrun;
mod uninstall;
mod workspace;
mod wrappers;

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

/// `--base REF`: compare with the merge base of a ref and `HEAD`, as for a
/// branch review, instead of with `HEAD` alone.
fn base_arg() -> Arg<'static> {
    Arg::new("base").long("base").value_name("REF").help(
        "Compare with the merge base of REF and HEAD: the branch's commits and the working tree",
    )
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
                .multiple_occurrences(true)
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
                        .help("Maximum preview bytes per stream (default 4096, adapted per command family; see `stats`)"),
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
                    Arg::new("merge")
                        .long("merge")
                        .hide(true)
                        .help("Archive stderr with stdout in the order written, for a command run with 2>&1"),
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
                )
                .arg(redact::unredacted()),
        )
        .subcommand(
            Command::new("search")
                .about("Search a captured run")
                .arg(id())
                .arg(Arg::new("text").required(true).value_name("TEXT"))
                .arg(streams())
                .arg(number("limit", "20", "Maximum matching lines"))
                .arg(redact::unredacted()),
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
                .arg(Arg::new("json").long("json").help("Print a JSON report"))
                .arg(
                    Arg::new("plan")
                        .long("plan")
                        .help("Rank unrouted command families by output and mark what compacts now"),
                ),
        )
        .subcommand(learn::command())
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
        .subcommand(find::command())
        .subcommand(lookup::command())
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
                .arg(base_arg().requires("changes").conflicts_with("staged"))
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
                )
                .arg(
                    Arg::new("remove")
                        .long("remove")
                        .conflicts_with("hooks")
                        .help("Remove what setup and mcp --install added, for every agent unless --agent selects some"),
                )
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .requires("remove")
                        .help("With --remove, print each change without making it"),
                )
                .arg(
                    Arg::new("purge")
                        .long("purge")
                        .requires("remove")
                        .help("With --remove, also delete .justless/ and its .gitignore line"),
                )
                .arg(
                    Arg::new("statusline")
                        .long("statusline")
                        .conflicts_with("remove")
                        .help("Show this session's savings in Claude Code's status line"),
                ),
        )
        .subcommand(exec::command())
        .subcommand(map::command())
        .subcommand(statusline::command())
        .subcommand(impact::command())
        .subcommand(testrun::command())
        .subcommand(mcp::command())
        .subcommand(doctor::command())
        .subcommands(source::commands())
        .subcommands(observations::commands())
        .subcommands(proxy::commands())
        .subcommand(
            Command::new("drain")
                .hide(true)
                .about("Store a background process's late output in a run archive")
                .arg(Arg::new("room").required(true)),
        )
        .subcommands(["hook-codex", "hook-claude", "hook-agy"].map(|name| {
            Command::new(name)
                .hide(true)
                .arg(Arg::new("mode").required(true).possible_values([
                    "session-start",
                    "before-command",
                    "after-read",
                    "after-search",
                    "after-bash",
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

/// The `--root` values given before the subcommand. Clap copies a global
/// argument given after the subcommand over the one given before it, so
/// `justless --root A query X --root B` would otherwise run in B without a word.
/// The scan stops at the subcommand, so arguments for a command `run` or `wrap`
/// starts are never read as Justless's own.
fn leading_roots(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut roots = Vec::new();
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--root" {
            match args.next() {
                Some(value) => roots.push(value),
                None => break,
            }
        } else if let Some(value) = arg.to_str().and_then(|text| text.strip_prefix("--root=")) {
            roots.push(OsString::from(value));
        } else {
            break;
        }
    }
    roots
}

/// `--root` may be repeated when every copy names the same directory, as when an
/// agent adds one to a printed command that already carries it.
fn check_root(matches: &ArgMatches) -> Result<()> {
    let leading = leading_roots(std::env::args_os());
    let mut roots = leading.iter().map(Path::new).chain(
        matches
            .values_of_os("root")
            .into_iter()
            .flatten()
            .map(Path::new),
    );
    let Some(first) = roots.next() else {
        return Ok(());
    };
    for other in roots {
        let same = match (first.canonicalize(), other.canonicalize()) {
            (Ok(first), Ok(other)) => first == other,
            _ => first == other,
        };
        ensure!(
            same,
            "--root names two different directories: {} and {}",
            first.display(),
            other.display()
        );
    }
    Ok(())
}

fn dispatch(matches: &ArgMatches) -> Result<i32> {
    let root = matches.value_of_os("root").map(Path::new);
    // A drainer started by `run` stores a background process's late output. It
    // works on its standard streams alone, wherever it starts.
    if let Some(("drain", sub)) = matches.subcommand() {
        return capture::drain(sub.value_of_t("room")?);
    }
    // Cursor, Gemini CLI and Copilot block a tool call when its hook fails, so these
    // handlers answer every error, including a missing workspace, with a no-op.
    if let Some((name, sub)) = matches.subcommand()
        && let Some(agent) = agents::client(name)
    {
        agents::run_client_hook(root, agent, sub.value_of("mode").unwrap_or_default());
        return Ok(0);
    }
    // Claude Code blanks its status line when the command fails, so this one finds
    // its projects from the session JSON and always prints a line.
    if let Some(("statusline", _)) = matches.subcommand() {
        statusline::run(root);
        return Ok(0);
    }
    check_root(matches)?;
    // Claude Code starts MCP servers with CLAUDE_PROJECT_DIR set to its launch
    // directory. Without an explicit root the server searches upward from there, as
    // hooks and commands do from theirs. `--install` writes to the current project.
    let project = std::env::var_os("CLAUDE_PROJECT_DIR")
        .filter(
            |_| matches!(matches.subcommand(), Some(("mcp", sub)) if !sub.is_present("install")),
        )
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_dir());
    let workspace = match (root, project) {
        (None, Some(start)) => Workspace::discover_from(&start)?,
        _ => Workspace::discover(root)?,
    };
    match matches.subcommand().expect("subcommand is required") {
        (name @ ("read" | "expand" | "edit"), sub) => {
            source::run(&workspace, name, sub, matches.is_present("root"))?
        }
        (name @ ("mask" | "recall"), sub) => observations::run(&workspace, name, sub)?,
        ("mcp", sub) => mcp::run(&workspace, sub)?,
        ("doctor", sub) => return doctor::run(&workspace, sub, matches.is_present("root")),
        ("exec", sub) => return exec::run(&workspace, sub),
        ("map", sub) => map::run(&workspace, sub)?,
        ("affected", sub) => impact::run(&workspace, sub)?,
        ("lookup", sub) => lookup::run(&workspace, sub)?,
        (name @ ("proxy" | "wrap"), sub) => return proxy::run(&workspace, name, sub),
        ("test", sub) => return testrun::run(&workspace, sub),
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
                adaptive::preview_budget(&workspace, &command)?
            };
            let default_session =
                std::env::var("JUSTLESS_SESSION").unwrap_or_else(|_| "terminal".into());
            // A hook-routed run in a nested project lists that project for its
            // session's status line.
            if sub.is_present("hook")
                && let Some(session) = sub.value_of("session")
            {
                statusline::register(&workspace.root, session);
            }
            return capture::execute(
                &workspace,
                &command,
                max_bytes,
                raw,
                sub.value_of("session").unwrap_or(&default_session),
                sub.is_present("fresh"),
                sub.is_present("hook"),
                sub.is_present("merge"),
            );
        }
        ("log", sub) => {
            let (directory, _) = capture::load(&workspace, sub.value_of("run-id").unwrap())?;
            if sub.is_present("unredacted") {
                redact::bypass();
                history::recall(&directory, history::Retrieval::Unredacted, None);
            }
            let to = if sub.is_present("to") {
                read_num(sub, "to", 1)?
            } else {
                usize::MAX
            };
            let printed = capture::log(
                &directory,
                Stream::select(sub.value_of("stream").unwrap()),
                read_num(sub, "from", 1)?,
                to,
            )?;
            let full = to == usize::MAX && sub.value_of("from") == Some("1");
            // The MCP server shows a long replay one page at a time and records
            // the size of the page itself.
            if std::env::var_os(mcp::PAGED_REPLAY).is_none() {
                let retrieval = if full {
                    history::Retrieval::Full
                } else {
                    history::Retrieval::Range
                };
                history::recall(&directory, retrieval, Some(printed));
            }
            if full {
                adaptive::note_replay(&directory, sub.value_of("stream").unwrap());
            }
        }
        ("search", sub) => {
            let (directory, run) = capture::load(&workspace, sub.value_of("run-id").unwrap())?;
            if sub.is_present("unredacted") {
                redact::bypass();
                history::recall(&directory, history::Retrieval::Unredacted, None);
            }
            let printed = capture::search(
                &directory,
                Stream::select(sub.value_of("stream").unwrap()),
                sub.value_of("text").unwrap(),
                read_num(sub, "limit", 1)?,
                run.may_have_failed(),
            )?;
            history::recall(&directory, history::Retrieval::Search, Some(printed));
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
            sub.is_present("plan"),
        )?,
        ("learn", sub) => learn::run(&workspace, sub)?,
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
            for (file, reason) in &index.refresh.skipped {
                eprintln!("{file}: not indexed, {reason}");
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
        ("find", sub) => find::run(&workspace, sub, matches.is_present("root"))?,
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
                let since = context::Since::resolve(
                    &workspace,
                    sub.is_present("staged"),
                    sub.value_of("base"),
                )?;
                context::changed(&workspace, &index, &since, limit)?;
            }
        }
        ("code", sub) => context::code(
            &workspace,
            sub.value_of("file").unwrap(),
            read_num(sub, "from", 1)?,
            read_num(sub, "to", 1)?,
            context_budget(&workspace, sub)?,
        )?,
        ("init", sub) if sub.is_present("remove") => uninstall::run(&workspace, sub)?,
        ("init", sub) => {
            let selected = sub
                .values_of("agent")
                .unwrap()
                .map(agents::Agent::parse)
                .collect::<Result<Vec<_>>>()?;
            let hooks = sub.is_present("hooks");
            let statusline = sub.is_present("statusline");
            let before = statusline::project_settings(&workspace);
            // Check the status line's files too before any agent setup is written.
            if statusline {
                statusline::check(&workspace)?;
            }
            // `--statusline` alone changes only Claude Code's settings, not the
            // default agent's instructions.
            if !statusline || hooks || sub.occurrences_of("agent") > 0 {
                agents::init(&workspace, &selected, hooks)?;
            }
            if statusline {
                statusline::install(&workspace)?;
            } else if hooks && selected.contains(&agents::Agent::Claude) {
                statusline::hint(&workspace, before);
            }
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
