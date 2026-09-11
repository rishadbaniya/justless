//! `justless mcp`: a stdio Model Context Protocol server for the read-only
//! navigation commands, and project-level setup for MCP clients.
//!
//! Protocol: MCP revision 2026-07-28
//! <https://modelcontextprotocol.io/specification/2026-07-28>. A request that
//! declares `_meta["io.modelcontextprotocol/protocolVersion"]` is served
//! statelessly, as that revision requires. Clients that predate it open with
//! the `initialize` handshake of revisions 2024-11-05 through 2025-11-25, which
//! this dual-era server also answers
//! <https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning#backward-compatibility-with-initialization-based-versions>.
//!
//! Messages are newline-delimited JSON-RPC 2.0 on stdin and stdout, and stdout
//! carries nothing else. A client that negotiates 2025-03-26 may also send a
//! batch array on one line, which is answered with one array. Each tool call runs the equivalent CLI command in a
//! child process, so command output can never interleave with protocol
//! messages. Mutating commands (`run`, `edit`, `mask`, `init`) are not exposed,
//! so the client's own permission flow still governs every change.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as Process, Stdio};

use anyhow::{Context, Result, bail, ensure};
use clap::{Arg, ArgMatches, Command};
use serde_json::{Map, Value, json};
use toml_edit::{Array, DocumentMut, Item, Table};

use crate::workspace::{Workspace, lock_state, private_dir, write_text};

/// Revisions served statelessly through per-request `_meta`.
const MODERN_VERSIONS: &[&str] = &["2026-07-28"];
/// Revisions negotiated through `initialize`, newest first.
const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// The only revision whose clients may send JSON-RPC batches.
const BATCH_VERSION: &str = "2025-03-26";

const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
const SERVER_INFO_KEY: &str = "io.modelcontextprotocol/serverInfo";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_VERSION: i64 = -32022;

/// Tool lists never change while the server runs.
const LIST_TTL_MS: u64 = 3_600_000;

const INSTRUCTIONS: &str = "Justless indexes this repository's source, docs and configuration. \
Use these tools before reading whole files or grepping. `map` orients you in an unfamiliar \
repository. For a how or where question, start with `lookup`, which ranks the code, docs and \
config that best match it. `pack` gathers budgeted evidence for a task, with links, changes and \
recent failures. `query` and `path` follow \
symbols and the links between them. `affected` lists what depends on a symbol, file or the Git \
changes, and the tests to run. `find` searches literal text. \
`read` with outline=true lists a file's symbols so that `expand` or `code` can fetch \
only the lines you need. `log`, `search` and `history` reopen output captured by `justless run`. \
File paths are relative to the repository root.";

pub fn command() -> Command<'static> {
    Command::new("mcp")
        .about(
            "Serve read-only navigation tools over MCP stdio, or add the server to client config",
        )
        .arg(
            Arg::new("install")
                .long("install")
                .value_name("CLIENT")
                .multiple_values(true)
                .multiple_occurrences(true)
                .possible_values(Client::NAMES)
                .help("Add the server to a client's project config instead of serving"),
        )
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<()> {
    if let Some(names) = matches.values_of("install") {
        let clients = names.map(Client::parse).collect::<Result<BTreeSet<_>>>()?;
        return install(workspace, &clients);
    }
    serve(workspace)
}

// ---------------------------------------------------------------------------
// Protocol loop

struct Server {
    root: PathBuf,
    exe: PathBuf,
    initialized: bool,
    /// The revision negotiated through `initialize`.
    version: Option<&'static str>,
}

struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
}

fn serve(workspace: &Workspace) -> Result<()> {
    let mut server = Server {
        root: workspace.root.clone(),
        exe: std::env::current_exe().context("locate the justless executable")?,
        initialized: false,
        version: None,
    };
    eprintln!("justless mcp: serving {} over stdio", server.root.display());
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Some(response) = server.handle(&line) {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

fn reply(id: Value, outcome: Result<Value, RpcError>) -> Value {
    match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => {
            let mut body = json!({"code": error.code, "message": error.message});
            if let Some(data) = error.data {
                body["data"] = data;
            }
            json!({"jsonrpc": "2.0", "id": id, "error": body})
        }
    }
}

fn supported_versions() -> Vec<&'static str> {
    MODERN_VERSIONS
        .iter()
        .chain(LEGACY_VERSIONS)
        .copied()
        .collect()
}

fn server_info() -> Value {
    json!({"name": "justless", "title": "Justless", "version": env!("CARGO_PKG_VERSION")})
}

fn capabilities() -> Value {
    json!({"tools": {"listChanged": false}})
}

impl Server {
    /// Returns the response line for a request, or `None` for notifications
    /// and for responses the client sends back.
    fn handle(&mut self, line: &[u8]) -> Option<Value> {
        let message = match serde_json::from_slice::<Value>(line) {
            Ok(message) => message,
            Err(error) => {
                return Some(reply(
                    Value::Null,
                    Err(RpcError::new(PARSE_ERROR, format!("parse error: {error}"))),
                ));
            }
        };
        // Revision 2025-03-26 requires servers to accept JSON-RPC batches. The
        // other revisions send one message per line.
        match message {
            Value::Array(batch) if self.version == Some(BATCH_VERSION) => {
                if batch.is_empty() {
                    return Some(reply(
                        Value::Null,
                        Err(RpcError::new(INVALID_REQUEST, "empty batch")),
                    ));
                }
                let replies = batch
                    .into_iter()
                    .filter_map(|message| self.message(message))
                    .collect::<Vec<_>>();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            message => self.message(message),
        }
    }

    fn message(&mut self, message: Value) -> Option<Value> {
        let Value::Object(message) = message else {
            return Some(reply(
                Value::Null,
                Err(RpcError::new(
                    INVALID_REQUEST,
                    "expected one JSON-RPC message object per line",
                )),
            ));
        };
        let id = message.get("id");
        let valid_id = match id {
            Some(Value::String(_)) => true,
            Some(Value::Number(number)) => number.is_i64() || number.is_u64(),
            _ => false,
        };
        let reply_id = if valid_id {
            id.cloned().unwrap()
        } else {
            Value::Null
        };
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(reply(
                reply_id,
                Err(RpcError::new(INVALID_REQUEST, "jsonrpc must be \"2.0\"")),
            ));
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            if id.is_some() && (message.contains_key("result") || message.contains_key("error")) {
                return None;
            }
            return Some(reply(
                reply_id,
                Err(RpcError::new(INVALID_REQUEST, "expected a method")),
            ));
        };
        // Notifications (initialized, cancelled, ...) need no work: every
        // request completes before the next line is read.
        id?;
        if !valid_id {
            return Some(reply(
                Value::Null,
                Err(RpcError::new(
                    INVALID_REQUEST,
                    "id must be a string or an integer",
                )),
            ));
        }
        let empty = Map::new();
        let outcome = match message.get("params") {
            None | Some(Value::Null) => self.request(method, &empty),
            Some(Value::Object(params)) => self.request(method, params),
            Some(_) => Err(RpcError::params("params must be an object")),
        };
        Some(reply(reply_id, outcome))
    }

    fn request(&mut self, method: &str, params: &Map<String, Value>) -> Result<Value, RpcError> {
        let meta = params.get("_meta");
        let modern = match meta.and_then(|meta| meta.get(VERSION_KEY)) {
            Some(version) => {
                check_request_meta(meta.unwrap(), version)?;
                true
            }
            None => false,
        };
        if !modern {
            match method {
                "initialize" => return self.initialize(params),
                "ping" => return Ok(json!({})),
                _ if !self.initialized => {
                    return Err(RpcError::params(format!(
                        "missing _meta[\"{VERSION_KEY}\"]: declare a protocol version on each \
                         request or send initialize first (supported: {})",
                        supported_versions().join(", ")
                    )));
                }
                _ => {}
            }
        }
        let mut result = match method {
            "ping" => json!({}),
            "server/discover" if modern => json!({
                "supportedVersions": supported_versions(),
                "capabilities": capabilities(),
                "instructions": INSTRUCTIONS,
                "ttlMs": LIST_TTL_MS,
                "cacheScope": "public",
            }),
            "tools/list" => {
                let tools = TOOLS.iter().map(Tool::definition).collect::<Vec<_>>();
                if modern {
                    json!({"tools": tools, "ttlMs": LIST_TTL_MS, "cacheScope": "public"})
                } else {
                    json!({"tools": tools})
                }
            }
            "tools/call" => self.call(params)?,
            _ => {
                return Err(RpcError::new(
                    METHOD_NOT_FOUND,
                    format!("method not found: {method}"),
                ));
            }
        };
        if modern {
            result["resultType"] = json!("complete");
            result["_meta"] = json!({ SERVER_INFO_KEY: server_info() });
        }
        Ok(result)
    }

    fn initialize(&mut self, params: &Map<String, Value>) -> Result<Value, RpcError> {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::params("initialize requires protocolVersion"))?;
        let version = LEGACY_VERSIONS
            .iter()
            .find(|version| **version == requested)
            .unwrap_or(&LEGACY_VERSIONS[0]);
        self.initialized = true;
        self.version = Some(version);
        Ok(json!({
            "protocolVersion": version,
            "capabilities": capabilities(),
            "serverInfo": server_info(),
            "instructions": INSTRUCTIONS,
        }))
    }

    fn call(&self, params: &Map<String, Value>) -> Result<Value, RpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::params("tools/call requires a tool name"))?;
        let tool = TOOLS
            .iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| RpcError::params(format!("Unknown tool: {name}")))?;
        let empty = Map::new();
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(arguments)) => arguments,
            Some(_) => return Err(RpcError::params("arguments must be an object")),
        };
        Ok(match tool.argv(arguments) {
            Ok(argv) => self.execute(&argv, (tool.name == "log").then_some(arguments)),
            // Invalid arguments are tool execution errors, so the model can
            // correct them and retry.
            Err(message) => tool_result(vec![message], true),
        })
    }

    /// Runs one command. `log` passes its arguments so a long replay is paged.
    fn execute(&self, argv: &[String], log: Option<&Map<String, Value>>) -> Value {
        let output = Process::new(&self.exe)
            .arg("--root")
            .arg(&self.root)
            .args(argv)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .output();
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                return tool_result(vec![format!("could not run justless: {error}")], true);
            }
        };
        let failed = !output.status.success();
        let mut texts = Vec::new();
        if let Some(arguments) = log.filter(|_| !failed) {
            texts = log_pages(&output, arguments);
        } else {
            for stream in [&output.stdout, &output.stderr] {
                if !stream.is_empty() {
                    texts.push(String::from_utf8_lossy(stream).into_owned());
                }
            }
        }
        if texts.is_empty() {
            texts.push(if failed {
                format!("justless exited with {}", output.status)
            } else {
                "(no output)".into()
            });
        }
        tool_result(texts, failed)
    }
}

/// Bytes of replayed output in one `log` result. Clients limit tool results
/// (Claude Code to 25,000 tokens by default), so longer output is paged by line.
const LOG_PAGE: usize = 32 * 1024;

/// A successful `log` replay as one text per stream. A replay that exceeds
/// `LOG_PAGE` ends on a line boundary with the arguments for the next page.
fn log_pages(output: &std::process::Output, arguments: &Map<String, Value>) -> Vec<String> {
    let run = arguments["run_id"].as_str().unwrap_or_default();
    let from = arguments
        .get("from")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let to = arguments.get("to").and_then(Value::as_u64);
    let to_hint = to.map(|to| format!(", to={to}")).unwrap_or_default();
    let mut left = LOG_PAGE;
    let mut texts = Vec::new();
    // The child replays stdout to stdout and stderr to stderr.
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        if bytes.is_empty() {
            continue;
        }
        if bytes.len() <= left {
            left -= bytes.len();
            texts.push(String::from_utf8_lossy(bytes).into_owned());
            continue;
        }
        let shown = match bytes[..left].iter().rposition(|&b| b == b'\n') {
            Some(newline) => newline + 1,
            // Cut inside a line only when that line alone exceeds a page.
            None if left == LOG_PAGE => left,
            None => 0,
        };
        let lines = bytes[..shown].iter().filter(|&&b| b == b'\n').count() as u64;
        let mut text = String::from_utf8_lossy(&bytes[..shown]).into_owned();
        let hint = if shown == 0 {
            format!(
                "[justless: {stream} not shown, the page is full. Call log with stream=\"{stream}\", from={from}{to_hint}.]"
            )
        } else if lines == 0 {
            // One line is longer than a page.
            format!(
                "\n[justless: {stream} line {from} is longer than {LOG_PAGE} bytes and was cut. \
                 Run `justless log {run} --stream {stream} --from {from} --to {from}` in a shell \
                 for the whole line, or call log with stream=\"{stream}\", from={}{to_hint} to continue.]",
                from + 1
            )
        } else {
            let last = from + lines - 1;
            format!(
                "[justless: {stream} page ends at line {last} after {LOG_PAGE} bytes. \
                 Call log with stream=\"{stream}\", from={}{to_hint} for more.]",
                last + 1
            )
        };
        text.push_str(&hint);
        texts.push(text);
        // A later stream starts its own page.
        left = 0;
    }
    texts
}

fn check_request_meta(meta: &Value, version: &Value) -> Result<(), RpcError> {
    let requested = version
        .as_str()
        .ok_or_else(|| RpcError::params(format!("_meta[\"{VERSION_KEY}\"] must be a string")))?;
    if !supported_versions().contains(&requested) {
        return Err(RpcError {
            code: UNSUPPORTED_VERSION,
            message: "Unsupported protocol version".into(),
            data: Some(json!({"supported": supported_versions(), "requested": requested})),
        });
    }
    if !meta.get(CAPABILITIES_KEY).is_some_and(Value::is_object) {
        return Err(RpcError::params(format!(
            "missing _meta[\"{CAPABILITIES_KEY}\"]"
        )));
    }
    Ok(())
}

fn tool_result(texts: Vec<String>, is_error: bool) -> Value {
    let content = texts
        .into_iter()
        .map(|text| json!({"type": "text", "text": text}))
        .collect::<Vec<_>>();
    json!({"content": content, "isError": is_error})
}

// ---------------------------------------------------------------------------
// Tools

#[derive(Clone, Copy)]
enum Kind {
    Text,
    /// A workspace-relative file path.
    File,
    /// `PATH:LINE` or `PATH:START-END`.
    Location,
    RunId,
    RecallId,
    Count(u64, u64),
    Flag,
    Choice(&'static [&'static str]),
}

#[derive(Clone, Copy)]
struct Param {
    name: &'static str,
    /// Long CLI option; `None` passes the value as a positional argument.
    flag: Option<&'static str>,
    kind: Kind,
    required: bool,
    help: &'static str,
}

/// Validates how a tool's arguments combine.
type Check = fn(&Map<String, Value>) -> Result<(), String>;

struct Tool {
    name: &'static str,
    title: &'static str,
    command: &'static str,
    description: &'static str,
    params: &'static [Param],
    check: Option<Check>,
}

const MAX_TEXT: usize = 4096;
const MAX_LINE: u64 = 100_000_000;
const STREAMS: &[&str] = &["both", "stdout", "stderr"];

const fn positional(name: &'static str, kind: Kind, help: &'static str) -> Param {
    Param {
        name,
        flag: None,
        kind,
        required: true,
        help,
    }
}

const fn option(name: &'static str, flag: &'static str, kind: Kind, help: &'static str) -> Param {
    Param {
        name,
        flag: Some(flag),
        kind,
        required: false,
        help,
    }
}

const BUDGET: Param = option(
    "budget",
    "budget",
    Kind::Count(128, 1_000_000),
    "Max bytes (default 2400)",
);
const FILE: Param = positional("file", Kind::File, "Workspace-relative path");
const RUN_ID: Param = positional("run_id", Kind::RunId, "Run ID from `justless run`");
const STREAM: Param = option(
    "stream",
    "stream",
    Kind::Choice(STREAMS),
    "Stream (default both)",
);

const TOOLS: &[Tool] = &[
    Tool {
        name: "query",
        title: "Find a symbol",
        command: "query",
        description: "Definition, references, callers, tests or dependencies of a symbol. Use before grepping or reading files.",
        params: &[
            positional("symbol", Kind::Text, "Symbol name, optionally qualified"),
            option(
                "view",
                "view",
                Kind::Choice(&[
                    "definition",
                    "references",
                    "callers",
                    "tests",
                    "dependencies",
                    "all",
                ]),
                "Which relationships to show (default all)",
            ),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "path",
        title: "Trace a path between symbols",
        command: "path",
        description: "Shortest chain of calls, imports or queries linking two symbols, with the file and line of each link.",
        params: &[
            positional("from", Kind::Text, "Starting symbol"),
            positional("to", Kind::Text, "Target symbol"),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "find",
        title: "Search indexed text",
        command: "find",
        description: "Exact text across indexed source, docs and configuration, as file:line matches.",
        params: &[
            positional("text", Kind::Text, "Exact text to find"),
            option(
                "ignore_case",
                "ignore-case",
                Kind::Flag,
                "Ignore ASCII case",
            ),
            option(
                "limit",
                "limit",
                Kind::Count(1, 1000),
                "Max lines (default 20)",
            ),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "lookup",
        title: "Ranked search",
        command: "lookup",
        description: "Ranks the symbols, doc sections and config blocks that best match a question or keywords.",
        params: &[
            positional("question", Kind::Text, "Question or keywords"),
            option(
                "limit",
                "limit",
                Kind::Count(1, 50),
                "Max results (default 8)",
            ),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "context",
        title: "Source around a location or diff",
        command: "context",
        description: "Source around a location (`at`) or the Git diff (`changes`), with enclosing symbols and related tests.",
        params: &[
            option("at", "at", Kind::Location, "PATH:LINE or PATH:START-END"),
            option("changes", "changes", Kind::Flag, "Use current Git changes"),
            option("staged", "staged", Kind::Flag, "Staged changes only"),
            BUDGET,
        ],
        check: Some(context_input),
    },
    Tool {
        name: "code",
        title: "Read a line range",
        command: "code",
        description: "Exact numbered lines from a file. Use when you already know the range.",
        params: &[
            FILE,
            option(
                "from",
                "from",
                Kind::Count(1, MAX_LINE),
                "First line (default 1)",
            ),
            Param {
                required: true,
                ..option("to", "to", Kind::Count(1, MAX_LINE), "Last line")
            },
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "read",
        title: "Read a file or its outline",
        command: "read",
        description: "A small file verbatim, or an outline of a larger file's symbols with a revision for `expand`.",
        params: &[
            FILE,
            option(
                "outline",
                "outline",
                Kind::Flag,
                "Outline even a small file",
            ),
            option(
                "match",
                "match",
                Kind::Text,
                "Rows whose name or signature contains this",
            ),
            option(
                "limit",
                "limit",
                Kind::Count(1, 1000),
                "Rows per page (default 40)",
            ),
            option(
                "after",
                "after",
                Kind::Text,
                "Continue after this REV#ROW reference",
            ),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "expand",
        title: "Expand one symbol",
        command: "expand",
        description: "Exact source of one symbol, Markdown section, outline row (REV#ROW) or line range (L10-20).",
        params: &[
            FILE,
            positional(
                "selector",
                Kind::Text,
                "Symbol, REV#ROW or line range such as L20-35",
            ),
            option(
                "revision",
                "revision",
                Kind::Text,
                "Revision from a read; rejects a changed file",
            ),
        ],
        check: None,
    },
    Tool {
        name: "pack",
        title: "Gather evidence for a question",
        command: "pack",
        description: "Answers a codebase question in one call with the relevant definitions, links, changes and recent failures within a budget. Start here for open questions.",
        params: &[
            positional("question", Kind::Text, "Task or question in plain language"),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "map",
        title: "Repository map",
        command: "map",
        description: "Repo overview: areas, hubs, communities, cross-language links, entry points, tests.",
        params: &[BUDGET],
        check: None,
    },
    Tool {
        name: "affected",
        title: "Find dependents",
        command: "affected",
        description: "Dependents of a symbol, file or Git changes by distance, and the tests that reach it.",
        params: &[
            Param {
                required: false,
                ..positional("target", Kind::Text, "Symbol or file")
            },
            option("changes", "changes", Kind::Flag, "Use Git changes"),
            option("depth", "depth", Kind::Count(1, 32), "Hops (default 3)"),
            BUDGET,
        ],
        check: Some(affected_input),
    },
    Tool {
        name: "log",
        title: "Replay a captured run",
        command: "log",
        description: "Exact output of a run captured by `justless run`, optionally one stream and line range.",
        params: &[
            RUN_ID,
            STREAM,
            option(
                "from",
                "from",
                Kind::Count(1, MAX_LINE),
                "First line (default 1)",
            ),
            option("to", "to", Kind::Count(1, MAX_LINE), "Last line"),
        ],
        check: None,
    },
    Tool {
        name: "search",
        title: "Search a captured run",
        command: "search",
        description: "Literal text search within one captured run's output, with stream and line numbers.",
        params: &[
            RUN_ID,
            positional("text", Kind::Text, "Exact text to find"),
            STREAM,
            option(
                "limit",
                "limit",
                Kind::Count(1, 1000),
                "Max lines (default 20)",
            ),
        ],
        check: None,
    },
    Tool {
        name: "history",
        title: "Search earlier runs",
        command: "history",
        description: "Recent captured runs, or a literal search across every recorded log.",
        params: &[
            Param {
                required: false,
                ..positional(
                    "text",
                    Kind::Text,
                    "Text to search for; omit to list recent runs",
                )
            },
            option(
                "oldest",
                "oldest",
                Kind::Flag,
                "Show the oldest matches first",
            ),
            option(
                "limit",
                "limit",
                Kind::Count(1, 1000),
                "Max runs or lines (default 12)",
            ),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "recall",
        title: "Recover an archived tool result",
        command: "recall",
        description: "Original value of a tool result that Justless archived, by ID.",
        params: &[positional(
            "id",
            Kind::RecallId,
            "64-character recall ID from the placeholder",
        )],
        check: None,
    },
    Tool {
        name: "resume",
        title: "Resume earlier work",
        command: "resume",
        description: "Where earlier work left off: recent runs and outcomes, inspected source and stale evidence.",
        params: &[BUDGET],
        check: None,
    },
    Tool {
        name: "stats",
        title: "Show savings",
        command: "stats",
        description: "Bytes and estimated tokens kept out of context, by day and command.",
        params: &[],
        check: None,
    },
];

fn context_input(arguments: &Map<String, Value>) -> Result<(), String> {
    let changes = arguments.get("changes") == Some(&Value::Bool(true));
    let staged = arguments.get("staged") == Some(&Value::Bool(true));
    let at = arguments.get("at").is_some_and(|value| !value.is_null());
    if changes == at {
        return Err("pass exactly one of `at` or `changes: true`".into());
    }
    if staged && !changes {
        return Err("`staged` requires `changes: true`".into());
    }
    Ok(())
}

fn affected_input(arguments: &Map<String, Value>) -> Result<(), String> {
    let changes = arguments.get("changes") == Some(&Value::Bool(true));
    let target = arguments
        .get("target")
        .is_some_and(|value| !value.is_null());
    if changes == target {
        return Err("pass exactly one of `target` or `changes: true`".into());
    }
    Ok(())
}

impl Tool {
    fn definition(&self) -> Value {
        let mut properties = Map::new();
        for param in self.params {
            // The tool list is sent with every model request, so schemas carry only
            // types, choices and short help. Arguments are validated in full on call.
            let mut schema = match param.kind {
                Kind::Text | Kind::File | Kind::Location | Kind::RunId | Kind::RecallId => {
                    json!({"type": "string"})
                }
                Kind::Count(..) => json!({"type": "integer"}),
                Kind::Flag => json!({"type": "boolean"}),
                Kind::Choice(values) => json!({"type": "string", "enum": values}),
            };
            schema["description"] = json!(param.help);
            properties.insert(param.name.into(), schema);
        }
        let required = self
            .params
            .iter()
            .filter(|param| param.required)
            .map(|param| param.name)
            .collect::<Vec<_>>();
        let mut schema = json!({"type": "object", "properties": properties});
        if !required.is_empty() {
            schema["required"] = json!(required);
        }
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": schema,
            "annotations": {"readOnlyHint": true},
        })
    }

    /// Builds the CLI arguments after `--root`. Options use `--name=value`
    /// and positionals follow `--`, so no value can be read as an option.
    fn argv(&self, arguments: &Map<String, Value>) -> Result<Vec<String>, String> {
        for key in arguments.keys() {
            if !self.params.iter().any(|param| param.name == key) {
                let known = self
                    .params
                    .iter()
                    .map(|param| param.name)
                    .collect::<Vec<_>>();
                return Err(if known.is_empty() {
                    format!("unknown argument `{key}`: {} takes no arguments", self.name)
                } else {
                    format!(
                        "unknown argument `{key}` for {}; expected {}",
                        self.name,
                        known.join(", ")
                    )
                });
            }
        }
        let mut argv = vec![self.command.to_owned()];
        let mut positionals = Vec::new();
        for param in self.params {
            let Some(value) = arguments.get(param.name).filter(|value| !value.is_null()) else {
                if param.required {
                    return Err(format!("missing required argument `{}`", param.name));
                }
                continue;
            };
            if let Kind::Flag = param.kind {
                match value {
                    Value::Bool(true) => argv.push(format!("--{}", param.flag.unwrap())),
                    Value::Bool(false) => {}
                    _ => return Err(format!("`{}` must be true or false", param.name)),
                }
                continue;
            }
            let text = validate(param, value)?;
            match param.flag {
                Some(flag) => argv.push(format!("--{flag}={text}")),
                None => positionals.push(text),
            }
        }
        if let Some(check) = self.check {
            check(arguments)?;
        }
        if !positionals.is_empty() {
            argv.push("--".into());
            argv.extend(positionals);
        }
        Ok(argv)
    }
}

fn validate(param: &Param, value: &Value) -> Result<String, String> {
    let name = param.name;
    if let Kind::Count(minimum, maximum) = param.kind {
        return match value.as_u64() {
            Some(number) if (minimum..=maximum).contains(&number) => Ok(number.to_string()),
            _ => Err(format!(
                "`{name}` must be an integer from {minimum} to {maximum}"
            )),
        };
    }
    let Some(text) = value.as_str() else {
        return Err(format!("`{name}` must be a string"));
    };
    ensure_text(name, text)?;
    let valid = match param.kind {
        Kind::File => relative_file(text),
        Kind::Location => text.rsplit_once(':').is_some_and(|(file, lines)| {
            let mut bounds = lines.splitn(2, '-');
            relative_file(file)
                && bounds
                    .all(|bound| !bound.is_empty() && bound.bytes().all(|b| b.is_ascii_digit()))
        }),
        Kind::RunId => text.strip_prefix("run-").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        }),
        Kind::RecallId => {
            text.len() == 64
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }
        Kind::Choice(values) => values.contains(&text),
        _ => true,
    };
    if valid {
        return Ok(text.to_owned());
    }
    Err(match param.kind {
        Kind::File => format!("`{name}` must be a workspace-relative path such as src/main.rs"),
        Kind::Location => format!("`{name}` must look like src/lib.rs:40 or src/lib.rs:40-52"),
        Kind::RunId => format!("`{name}` must be a run ID such as run-1a2b3c"),
        Kind::RecallId => format!("`{name}` must be a 64-character lowercase hex recall ID"),
        Kind::Choice(values) => format!("`{name}` must be one of {}", values.join(", ")),
        _ => format!("invalid `{name}`"),
    })
}

fn ensure_text(name: &str, text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err(format!("`{name}` must not be empty"));
    }
    if text.len() > MAX_TEXT {
        return Err(format!("`{name}` is longer than {MAX_TEXT} bytes"));
    }
    if text.contains('\0') {
        return Err(format!("`{name}` must not contain NUL"));
    }
    Ok(())
}

fn relative_file(text: &str) -> bool {
    !text.is_empty()
        && Path::new(text)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

// ---------------------------------------------------------------------------
// Client setup

/// MCP clients with a verified project-level config file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Client {
    Claude,
    Cursor,
    Gemini,
    VsCode,
    Codex,
}

impl Client {
    const NAMES: [&'static str; 5] = ["claude", "cursor", "gemini", "vscode", "codex"];

    fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "claude" => Self::Claude,
            "cursor" => Self::Cursor,
            "gemini" => Self::Gemini,
            "vscode" => Self::VsCode,
            "codex" => Self::Codex,
            _ => bail!("unknown MCP client: {name}"),
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Gemini => "Gemini CLI",
            Self::VsCode => "VS Code",
            Self::Codex => "Codex",
        }
    }

    /// Project config file, relative to the workspace root.
    ///
    /// Sources, checked 2026-11-18:
    /// - Claude Code `.mcp.json` `mcpServers`, stdio when `type` is absent:
    ///   <https://code.claude.com/docs/en/mcp>
    /// - Cursor `.cursor/mcp.json` `mcpServers`, `type: "stdio"`,
    ///   `${workspaceFolder}`: <https://cursor.com/docs/context/mcp>
    /// - Gemini CLI `.gemini/settings.json` `mcpServers`:
    ///   <https://geminicli.com/docs/tools/mcp-server/>
    /// - VS Code `.vscode/mcp.json` `servers`, `type: "stdio"`,
    ///   `${workspaceFolder}`:
    ///   <https://code.visualstudio.com/docs/copilot/reference/mcp-configuration>
    /// - Codex `.codex/config.toml` `[mcp_servers.NAME]`, trusted projects only:
    ///   <https://learn.chatgpt.com/docs/extend/mcp?surface=cli>
    pub fn file(self) -> &'static str {
        match self {
            Self::Claude => ".mcp.json",
            Self::Cursor => ".cursor/mcp.json",
            Self::Gemini => ".gemini/settings.json",
            Self::VsCode => ".vscode/mcp.json",
            Self::Codex => ".codex/config.toml",
        }
    }

    fn servers_key(self) -> &'static str {
        match self {
            Self::VsCode => "servers",
            Self::Codex => "mcp_servers",
            _ => "mcpServers",
        }
    }

    /// The `justless` server entry. Cursor and VS Code expand
    /// `${workspaceFolder}`. The other clients' docs do not state the
    /// server's working directory, so their entry relies on it being inside
    /// the project, where the root is discovered like any other command.
    pub fn server_entry(self) -> Value {
        match self {
            Self::Cursor | Self::VsCode => json!({
                "type": "stdio",
                "command": "justless",
                "args": ["mcp", "--root", "${workspaceFolder}"],
            }),
            Self::Claude | Self::Gemini | Self::Codex => {
                json!({"command": "justless", "args": ["mcp"]})
            }
        }
    }
}

/// A client's config file and its merged contents, or `None` when it
/// already lists a `justless` server.
pub struct ConfigFile {
    pub client: Client,
    pub path: PathBuf,
    pub contents: Option<String>,
}

/// Renders each client's project config with the `justless` server added.
/// Every other key is preserved, and an existing `justless` entry is kept
/// as it is.
pub fn config_entries(
    workspace: &Workspace,
    clients: &BTreeSet<Client>,
) -> Result<Vec<ConfigFile>> {
    clients
        .iter()
        .map(|&client| {
            let relative = client.file();
            let path = workspace.root.join(relative);
            if let Some(directory) = Path::new(relative)
                .parent()
                .filter(|dir| *dir != Path::new(""))
            {
                match fs::symlink_metadata(workspace.root.join(directory)) {
                    Ok(metadata) => ensure!(
                        metadata.is_dir(),
                        "{} must be a directory",
                        directory.display()
                    ),
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("inspect {}", directory.display()));
                    }
                }
            }
            let original = read_regular(&path)?;
            let contents = if client == Client::Codex {
                merge_toml(relative, &original, &client.server_entry())?
            } else {
                merge_json(
                    relative,
                    &original,
                    client.servers_key(),
                    client.server_entry(),
                )?
            };
            Ok(ConfigFile {
                client,
                path,
                contents,
            })
        })
        .collect()
}

fn merge_json(relative: &str, original: &str, key: &str, entry: Value) -> Result<Option<String>> {
    let mut config: Value = if original.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(original).with_context(|| {
            format!("decode {relative}; if it has comments, add the justless entry by hand")
        })?
    };
    let servers = config
        .as_object_mut()
        .with_context(|| format!("{relative} must contain an object"))?
        .entry(key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("{key} in {relative} must contain an object"))?;
    if servers.contains_key("justless") {
        return Ok(None);
    }
    servers.insert("justless".into(), entry);
    Ok(Some(format!(
        "{}\n",
        serde_json::to_string_pretty(&config)?
    )))
}

fn merge_toml(relative: &str, original: &str, entry: &Value) -> Result<Option<String>> {
    let mut document = original
        .parse::<DocumentMut>()
        .with_context(|| format!("decode {relative}"))?;
    let servers = document
        .as_table_mut()
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_like_mut()
        .with_context(|| format!("mcp_servers in {relative} must be a table"))?;
    if servers.contains_key("justless") {
        return Ok(None);
    }
    let mut table = Table::new();
    table.insert(
        "command",
        toml_edit::value(entry["command"].as_str().unwrap()),
    );
    let args = entry["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Array>();
    table.insert("args", toml_edit::value(args));
    servers.insert("justless", Item::Table(table));
    Ok(Some(document.to_string()))
}

fn read_regular(path: &Path) -> Result<String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "{} must be a regular file",
                path.display()
            );
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn install(workspace: &Workspace, clients: &BTreeSet<Client>) -> Result<()> {
    workspace.prepare_state()?;
    let _guard = lock_state(&workspace.state.join("setup.lock"))?;
    // Validate every selected file before writing any of them.
    let files = config_entries(workspace, clients)?;
    // Absolute paths show which project was changed.
    for file in files {
        let path = file.path.display();
        match file.contents {
            Some(contents) => {
                private_dir(file.path.parent().context("config directory")?)?;
                write_text(&file.path, &contents)?;
                println!("{}: added justless to {path}", file.client.label());
            }
            None => println!("{}: {path} already lists justless", file.client.label()),
        }
    }
    Ok(())
}
