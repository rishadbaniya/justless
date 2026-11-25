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
use std::sync::atomic::{AtomicU64, Ordering};

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
config that best match it (add logs=true to include captured logs). `pack` gathers budgeted \
evidence for a task, with links, changes and recent failures. `context` shows source around a \
location, symbol, Git diff or captured run; repeated `context`/`pack` reads in a session return \
deltas unless fresh=true. `query` and `path` follow symbols and the links between them. \
`affected` lists what depends on a symbol, file or the Git changes, and the tests to run. \
`find` searches literal text. `read` (file or files) with outline=true lists symbols so `expand` \
or `code` can fetch only the lines you need. `log`, `search`, `history`, `recall` and `resume` \
reopen captured runs, archived tool outputs and recent activity. File paths are relative to the \
repository root; `budget` is max output bytes.";

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

/// A paged log is accounted for by the server once it knows the delivered size.
pub(crate) const PAGED_REPLAY: &str = "JUSTLESS_PAGED_REPLAY";

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

fn new_session() -> String {
    let seq = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let seed = format!(
        "mcp-{}-{}-{seq}",
        std::process::id(),
        crate::history::now_ms()
    );
    format!(
        "mcp-{}",
        &crate::capture::source_digest(seed.as_bytes())[..24]
    )
}

#[derive(Clone, Copy)]
enum Paged<'a> {
    Log(&'a Map<String, Value>),
    Recall(&'a Map<String, Value>),
}

struct Server {
    root: PathBuf,
    /// The served project, where a run a `log` page replays is looked up first.
    workspace: Workspace,
    exe: PathBuf,
    /// Stable per-server session key passed to `context` and `pack` so repeated
    /// reads return `seen::Memory` deltas instead of reprinting unchanged code.
    session: String,
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
        workspace: workspace.clone(),
        exe: std::env::current_exe().context("locate the justless executable")?,
        session: new_session(),
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
            Ok(mut argv) => {
                if uses_session(tool.name, arguments) {
                    argv.insert(1, format!("--session={}", self.session));
                }
                let paged = match tool.name {
                    "log" => Some(Paged::Log(arguments)),
                    "recall" => Some(Paged::Recall(arguments)),
                    _ => None,
                };
                self.execute(&argv, paged)
            }
            // Invalid arguments are tool execution errors, so the model can
            // correct them and retry.
            Err(message) => tool_result(vec![message], true),
        })
    }

    /// Runs one command. `log` and `recall` pass their arguments so long
    /// outputs are paged on line boundaries.
    fn execute(&self, argv: &[String], paged: Option<Paged<'_>>) -> Value {
        // The agent's shell may start anywhere, so a command a result prints for it,
        // such as lookup's `Next:` line, names this root.
        let mut command = Process::new(&self.exe);
        if matches!(paged, Some(Paged::Log(_))) {
            command.env(PAGED_REPLAY, "1");
        }
        let output = command
            .arg("--root")
            .arg(&self.root)
            .args(argv)
            .current_dir(&self.root)
            .env(crate::workspace::ORIGIN_UNKNOWN, "1")
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
        match paged.filter(|_| !failed) {
            Some(Paged::Log(arguments)) => {
                texts = log_pages(&output, arguments, &self.root);
            }
            Some(Paged::Recall(arguments)) => {
                texts = recall_pages(&output, arguments, &self.root);
            }
            None => {
                for stream in [&output.stdout, &output.stderr] {
                    if !stream.is_empty() {
                        texts.push(String::from_utf8_lossy(stream).into_owned());
                    }
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
        if let Some(Paged::Log(arguments)) = paged.filter(|_| !failed) {
            self.measure_page(arguments, texts.iter().map(String::len).sum());
        }
        tool_result(texts, failed)
    }

    /// Records the page and its delivered byte count together after pagination,
    /// against the run in whichever registered project holds it, as `log` found it.
    fn measure_page(&self, arguments: &Map<String, Value>, bytes: usize) {
        let Some(Ok(run)) = arguments
            .get("run_id")
            .map(|value| validate(&RUN_ID, value))
        else {
            return;
        };
        let Ok((_, directory, _)) = crate::registry::locate(&self.workspace, &run) else {
            return;
        };
        let whole = arguments.get("from").and_then(Value::as_u64).unwrap_or(1) == 1
            && arguments.get("to").is_none_or(Value::is_null);
        let retrieval = if whole {
            crate::history::Retrieval::Full
        } else {
            crate::history::Retrieval::Range
        };
        crate::history::recall(&directory, retrieval, Some(bytes as u64));
    }
}

/// Bytes of replayed output in one `log` result. Clients limit tool results
/// (Claude Code to 25,000 tokens by default), so longer output is paged by line.
const LOG_PAGE: usize = 32 * 1024;

/// A successful `log` replay as one text per stream. A replay that exceeds
/// `LOG_PAGE` ends on a line boundary with the arguments for the next page.
/// `root` is the server's: the shell the agent runs a printed command in may start
/// anywhere, so that command always names it.
fn log_pages(
    output: &std::process::Output,
    arguments: &Map<String, Value>,
    root: &std::path::Path,
) -> Vec<String> {
    let run = arguments["run_id"].as_str().unwrap_or_default();
    let root = crate::workspace::shell_quote(root);
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
                 Run `justless --root {root} log {run} --stream {stream} --from {from} --to {from}` in a shell \
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

/// Whether this MCP tool call supports per-process `--session` dedup via `seen::Memory`.
fn uses_session(tool: &str, arguments: &Map<String, Value>) -> bool {
    match tool {
        "pack" => true,
        "context" => {
            arguments.get("at").is_some_and(|v| !v.is_null())
                || arguments.get("symbol").is_some_and(|v| !v.is_null())
        }
        _ => false,
    }
}

/// Largest byte index <= `limit` that does not split a UTF-8 code point.
fn floor_utf8_boundary(bytes: &[u8], limit: usize) -> usize {
    let mut end = limit.min(bytes.len());
    if end == bytes.len() {
        return end;
    }
    let mut stepped = 0;
    while end > 0 && stepped < 3 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
        stepped += 1;
    }
    if end == 0 || (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        limit.min(bytes.len())
    } else {
        end
    }
}

/// A successful `recall` result, paged to `LOG_PAGE` on line boundaries so
/// large archived observations cannot flood context in one tool response.
fn recall_pages(
    output: &std::process::Output,
    arguments: &Map<String, Value>,
    root: &std::path::Path,
) -> Vec<String> {
    let mut texts = Vec::new();
    let bytes = &output.stdout;
    if !bytes.is_empty() {
        if bytes.len() <= LOG_PAGE {
            texts.push(String::from_utf8_lossy(bytes).into_owned());
        } else {
            let cut = match bytes[..LOG_PAGE].iter().rposition(|&b| b == b'\n') {
                Some(newline) => newline + 1,
                None => floor_utf8_boundary(bytes, LOG_PAGE),
            };
            let shown = &bytes[..cut];
            let lines = shown.iter().filter(|&&b| b == b'\n').count() as u64;
            let mut text = String::from_utf8_lossy(shown).into_owned();
            let list = arguments.get("list") == Some(&Value::Bool(true));
            let grep = arguments.get("grep").and_then(Value::as_str);
            let id = arguments
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let from = arguments
                .get("from")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .max(1);
            let to = arguments.get("to").and_then(Value::as_u64);
            let to_hint = to.map(|to| format!(", to={to}")).unwrap_or_default();
            let hint = if list {
                let prefix = if text.ends_with('\n') { "" } else { "\n" };
                format!(
                    "{prefix}[justless: recall list truncated after {LOG_PAGE} bytes. Call recall with list=true and a smaller limit.]"
                )
            } else if let Some(needle) = grep {
                let prefix = if text.ends_with('\n') { "" } else { "\n" };
                let next_from = text
                    .lines()
                    .rev()
                    .find_map(|line| line.split_once(": ")?.0.parse::<u64>().ok())
                    .map(|n| n + 1);
                match next_from {
                    Some(next) => format!(
                        "{prefix}[justless: recall grep truncated after {LOG_PAGE} bytes. Call recall with id=\"{id}\", grep={needle:?}, from={next}{to_hint} for more.]"
                    ),
                    None => format!(
                        "{prefix}[justless: recall grep truncated after {LOG_PAGE} bytes. Call recall with id=\"{id}\", grep={needle:?} and a smaller limit.]"
                    ),
                }
            } else if lines == 0 {
                let root = crate::workspace::shell_quote(root);
                format!(
                    "\n[justless: recall line {from} is longer than {LOG_PAGE} bytes and was cut. \
                     Run `justless --root {root} recall {id} --from {from} --to {from}` in a shell \
                     for the whole line, or call recall with id=\"{id}\", from={}{to_hint} to continue.]",
                    from + 1
                )
            } else {
                let last = from + lines - 1;
                format!(
                    "[justless: recall page ends at line {last} after {LOG_PAGE} bytes. \
                     Call recall with id=\"{id}\", from={}{to_hint} for more, or grep=\"TEXT\" to filter.]",
                    last + 1
                )
            };
            text.push_str(&hint);
            texts.push(text);
        }
    }
    if !output.stderr.is_empty() {
        texts.push(String::from_utf8_lossy(&output.stderr).into_owned());
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
    /// One or more workspace-relative file paths passed as positionals.
    Files,
    /// `PATH:LINE` or `PATH:START-END`.
    Location,
    RunId,
    RecallId,
    Count(u64, u64),
    Flag,
    Choice(&'static [&'static str]),
    /// One or more workspace-relative files or directories, each passed as
    /// its own option.
    Paths,
    /// One or more strings, each passed as its own option.
    Texts,
}

#[derive(Clone, Copy)]
struct Param {
    name: &'static str,
    /// Long CLI option; `None` passes the value as a positional argument.
    flag: Option<&'static str>,
    kind: Kind,
    required: bool,
    /// Short help text; omitted from the JSON schema when empty and the
    /// parameter name/type/enum is self-explanatory.
    help: &'static str,
}

/// Validates how a tool's arguments combine.
type Check = fn(&Map<String, Value>) -> Result<(), String>;

struct Tool {
    name: &'static str,
    command: &'static str,
    description: &'static str,
    params: &'static [Param],
    check: Option<Check>,
}

const MAX_TEXT: usize = 4096;
const MAX_LINE: u64 = 100_000_000;
/// The largest `limit` a tool takes. A cut notice that a tool can return never
/// suggests more.
pub(crate) const MAX_LIMIT: usize = 1000;
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

const BUDGET: Param = option("budget", "budget", Kind::Count(128, 1_000_000), "");
const FILE: Param = positional("file", Kind::File, "");
const RUN_ID: Param = positional("run_id", Kind::RunId, "");
const STREAM: Param = option("stream", "stream", Kind::Choice(STREAMS), "");
const BASE: Param = option("base", "base", Kind::Text, "Base ref");

const TOOLS: &[Tool] = &[
    Tool {
        name: "query",
        command: "query",
        description: "Symbol definition, refs, callers, tests or deps.",
        params: &[
            positional("symbol", Kind::Text, ""),
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
                "",
            ),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "path",
        command: "path",
        description: "Shortest call/import/SQL path between symbols.",
        params: &[
            positional("from", Kind::Text, ""),
            positional("to", Kind::Text, ""),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "find",
        command: "find",
        description: "Exact literal text search (file:line matches).",
        params: &[
            positional("text", Kind::Text, ""),
            option("ignore_case", "ignore-case", Kind::Flag, ""),
            option("path", "path", Kind::Paths, "Dirs/files"),
            option("glob", "glob", Kind::Texts, "rg -g globs"),
            option("limit", "limit", Kind::Count(1, MAX_LIMIT as u64), ""),
            option("after", "after", Kind::Text, "Page cursor"),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "lookup",
        command: "lookup",
        description: "Rank symbols, docs and config for a question.",
        params: &[
            positional("question", Kind::Text, ""),
            option("limit", "limit", Kind::Count(1, 50), ""),
            option("logs", "logs", Kind::Flag, "Search logs"),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "context",
        command: "context",
        description: "Source for at, symbol, Git changes or run_id (deduped).",
        params: &[
            option("at", "at", Kind::Location, "PATH:LINE[-END]"),
            option("symbol", "symbol", Kind::Text, ""),
            option("changes", "changes", Kind::Flag, ""),
            option("run_id", "run", Kind::RunId, ""),
            option("staged", "staged", Kind::Flag, ""),
            BASE,
            option("fresh", "fresh", Kind::Flag, "Skip dedup"),
            BUDGET,
        ],
        check: Some(context_input),
    },
    Tool {
        name: "code",
        command: "code",
        description: "Exact numbered lines from a known file range.",
        params: &[
            FILE,
            option("from", "from", Kind::Count(1, MAX_LINE), ""),
            Param {
                required: true,
                ..option("to", "to", Kind::Count(1, MAX_LINE), "")
            },
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "read",
        command: "read",
        description: "Small file(s) verbatim, or outline with expand rev.",
        params: &[
            Param {
                required: false,
                ..FILE
            },
            Param {
                required: false,
                ..positional("files", Kind::Files, "")
            },
            option("outline", "outline", Kind::Flag, "Force outline"),
            option("match", "match", Kind::Text, "Filter (a|b)"),
            option("limit", "limit", Kind::Count(1, MAX_LIMIT as u64), ""),
            option("after", "after", Kind::Text, "REV#ROW"),
            BUDGET,
        ],
        check: Some(read_input),
    },
    Tool {
        name: "expand",
        command: "expand",
        description: "Exact source of symbol, section, REV#ROW or L10-20 range.",
        params: &[
            FILE,
            positional("selector", Kind::Text, "Symbol, REV#ROW,#ROW or L10-20"),
            option("revision", "revision", Kind::Text, "Rev check"),
        ],
        check: None,
    },
    Tool {
        name: "pack",
        command: "pack",
        description: "Budgeted task evidence: defs, links, diff, runs.",
        params: &[
            positional("question", Kind::Text, ""),
            option("fresh", "fresh", Kind::Flag, "Skip dedup"),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "map",
        command: "map",
        description: "Repo map: areas, hubs, cross-lang links, tests.",
        params: &[BUDGET],
        check: None,
    },
    Tool {
        name: "affected",
        command: "affected",
        description: "Dependents and tests for target or Git changes.",
        params: &[
            Param {
                required: false,
                ..positional("target", Kind::Text, "Symbol or file")
            },
            option("changes", "changes", Kind::Flag, ""),
            BASE,
            option("depth", "depth", Kind::Count(1, 32), "Max hops"),
            BUDGET,
        ],
        check: Some(affected_input),
    },
    Tool {
        name: "log",
        command: "log",
        description: "Captured run output (paged, secrets redacted).",
        params: &[
            RUN_ID,
            STREAM,
            option("from", "from", Kind::Count(1, MAX_LINE), ""),
            option("to", "to", Kind::Count(1, MAX_LINE), ""),
        ],
        check: None,
    },
    Tool {
        name: "search",
        command: "search",
        description: "Search literal text in one captured run log.",
        params: &[
            RUN_ID,
            positional("text", Kind::Text, ""),
            STREAM,
            option("limit", "limit", Kind::Count(1, MAX_LIMIT as u64), ""),
        ],
        check: None,
    },
    Tool {
        name: "history",
        command: "history",
        description: "List recent runs, or search text across logs.",
        params: &[
            Param {
                required: false,
                ..positional("text", Kind::Text, "Omit to list")
            },
            option("oldest", "oldest", Kind::Flag, ""),
            option("limit", "limit", Kind::Count(1, MAX_LIMIT as u64), ""),
            BUDGET,
        ],
        check: None,
    },
    Tool {
        name: "recall",
        command: "recall",
        description: "Recover archived tool result (paged/grep) or list IDs.",
        params: &[
            Param {
                required: false,
                ..positional("id", Kind::RecallId, "Recall/tool_use ID")
            },
            option("from", "from", Kind::Count(1, MAX_LINE), ""),
            option("to", "to", Kind::Count(1, MAX_LINE), ""),
            option("grep", "grep", Kind::Text, "Filter lines"),
            option("limit", "limit", Kind::Count(1, MAX_LIMIT as u64), ""),
            option("list", "list", Kind::Flag, "List IDs"),
        ],
        check: Some(recall_input),
    },
    Tool {
        name: "resume",
        command: "resume",
        description: "Recent runs, inspected source and stale files.",
        params: &[BUDGET],
        check: None,
    },
    Tool {
        name: "stats",
        command: "stats",
        description: "Context bytes/tokens saved by day and command.",
        params: &[],
        check: None,
    },
];

fn context_input(arguments: &Map<String, Value>) -> Result<(), String> {
    let changes = base_input(arguments)?;
    let staged = arguments.get("staged") == Some(&Value::Bool(true));
    let at = arguments.get("at").is_some_and(|value| !value.is_null());
    let symbol = arguments
        .get("symbol")
        .is_some_and(|value| !value.is_null());
    let run = arguments
        .get("run_id")
        .is_some_and(|value| !value.is_null());
    if usize::from(changes) + usize::from(at) + usize::from(symbol) + usize::from(run) != 1 {
        return Err("pass exactly one of `at`, `symbol`, `changes: true` or `run_id`".into());
    }
    if staged && !changes {
        return Err("`staged` requires `changes: true`".into());
    }
    if staged && arguments.get("base").is_some_and(|value| !value.is_null()) {
        return Err("pass `staged` or `base`, not both".into());
    }
    Ok(())
}

fn read_input(arguments: &Map<String, Value>) -> Result<(), String> {
    let file = arguments.get("file").is_some_and(|value| !value.is_null());
    let files = arguments.get("files").is_some_and(|value| !value.is_null());
    if file == files {
        return Err(if file {
            "pass `file` or `files`, not both".into()
        } else {
            "missing required argument `file` or `files`".into()
        });
    }
    Ok(())
}

fn recall_input(arguments: &Map<String, Value>) -> Result<(), String> {
    let list = arguments.get("list") == Some(&Value::Bool(true));
    let id = arguments.get("id").is_some_and(|value| !value.is_null());
    if list == id {
        return Err("pass exactly one of `id` or `list: true`".into());
    }
    let from = arguments.get("from").and_then(Value::as_u64);
    let to = arguments.get("to").and_then(Value::as_u64);
    let grep = arguments.get("grep").is_some_and(|value| !value.is_null());
    if list && (from.is_some() || to.is_some() || grep) {
        return Err("`from`, `to` and `grep` apply to `id`, not `list: true`".into());
    }
    if let (Some(from), Some(to)) = (from, to)
        && from > to
    {
        return Err("`from` must not be after `to`".into());
    }
    Ok(())
}

/// Whether `changes` is true, after checking that a `base` comes with it.
fn base_input(arguments: &Map<String, Value>) -> Result<bool, String> {
    let changes = arguments.get("changes") == Some(&Value::Bool(true));
    if !changes && arguments.get("base").is_some_and(|value| !value.is_null()) {
        return Err("`base` requires `changes: true`".into());
    }
    Ok(changes)
}

fn affected_input(arguments: &Map<String, Value>) -> Result<(), String> {
    let changes = base_input(arguments)?;
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
            // types, choices and short help where the name/type is not self-evident.
            // Arguments are validated in full on call.
            let mut schema = match param.kind {
                Kind::Text | Kind::File | Kind::Location | Kind::RunId | Kind::RecallId => {
                    json!({"type": "string"})
                }
                Kind::Count(..) => json!({"type": "integer"}),
                Kind::Flag => json!({"type": "boolean"}),
                Kind::Choice(values) => json!({"type": "string", "enum": values}),
                Kind::Paths | Kind::Texts | Kind::Files => {
                    json!({"type": "array", "items": {"type": "string"}})
                }
            };
            if !param.help.is_empty() {
                schema["description"] = json!(param.help);
            }
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
            if let Kind::Paths | Kind::Texts | Kind::Files = param.kind {
                let items = match value {
                    Value::String(_) => std::slice::from_ref(value),
                    Value::Array(items) => items.as_slice(),
                    _ => {
                        return Err(format!(
                            "`{}` must be a string or a list of strings",
                            param.name
                        ));
                    }
                };
                if matches!(param.kind, Kind::Files) && items.is_empty() {
                    return Err(format!("`{}` must not be empty", param.name));
                }
                for item in items {
                    let text = validate(param, item)?;
                    match param.flag {
                        Some(flag) => argv.push(format!("--{flag}={text}")),
                        None => positionals.push(text),
                    }
                }
                continue;
            }
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
    let normalized = match param.kind {
        Kind::File | Kind::Files => normalize_relative(text, false),
        Kind::Paths => normalize_relative(text, true),
        Kind::Location => text.rsplit_once(':').and_then(|(file, lines)| {
            let mut bounds = lines.splitn(2, '-');
            let file = normalize_relative(file, false)?;
            bounds
                .all(|bound| !bound.is_empty() && bound.bytes().all(|b| b.is_ascii_digit()))
                .then(|| format!("{file}:{lines}"))
        }),
        Kind::RunId => text
            .strip_prefix("run-")
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .then(|| text.to_owned()),
        // A 64-character recall ID, or a tool_use ID the proxy archived.
        Kind::RecallId => (text.len() <= 200
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
        .then(|| text.to_owned()),
        Kind::Choice(values) => values.contains(&text).then(|| text.to_owned()),
        _ => Some(text.to_owned()),
    };
    if let Some(value) = normalized {
        return Ok(value);
    }
    Err(match param.kind {
        Kind::File => format!("`{name}` must be a workspace-relative path such as src/main.rs"),
        Kind::Files => {
            format!("`{name}` must be workspace-relative paths such as src/main.rs")
        }
        Kind::Paths => format!(
            "`{name}` must be a workspace-relative path or a list of them, such as src or src/main.rs"
        ),
        Kind::Location => format!("`{name}` must look like src/lib.rs:40 or src/lib.rs:40-52"),
        Kind::RunId => format!("`{name}` must be a run ID such as run-1a2b3c"),
        Kind::RecallId => format!("`{name}` must be a recall ID or a tool_use ID"),
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

/// Normalizes a workspace-relative path by dropping redundant `.` segments
/// while rejecting absolute paths and `..`. When `allow_dot` is true, `.`
/// itself resolves to `"."` (for directory scopes such as `find --path`).
fn normalize_relative(text: &str, allow_dot: bool) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for part in Path::new(text).components() {
        match part {
            Component::Normal(segment) => parts.push(segment.to_str()?),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.is_empty() {
        allow_dot.then(|| ".".to_owned())
    } else {
        Some(parts.join("/"))
    }
}

// ---------------------------------------------------------------------------
// Client setup

/// MCP clients with a verified config file (project-level, or user-level for
/// Antigravity CLI's `~/.gemini/config/mcp_config.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Client {
    Claude,
    Cursor,
    Gemini,
    VsCode,
    Codex,
    Agy,
}

impl Client {
    pub(crate) const NAMES: [&'static str; 6] =
        ["claude", "cursor", "gemini", "vscode", "codex", "agy"];

    pub(crate) fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "claude" => Self::Claude,
            "cursor" => Self::Cursor,
            "gemini" => Self::Gemini,
            "vscode" => Self::VsCode,
            "codex" => Self::Codex,
            "agy" => Self::Agy,
            _ => bail!("unknown MCP client: {name}"),
        })
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Gemini => "Gemini CLI",
            Self::VsCode => "VS Code",
            Self::Codex => "Codex",
            Self::Agy => "Antigravity CLI",
        }
    }

    /// Whether this client's MCP config is user-level rather than project-local.
    pub(crate) fn is_global(self) -> bool {
        self == Self::Agy
    }

    /// Project config file, relative to the workspace root (or the global file
    /// name for `Client::Agy`; use [`Self::path`] for the resolved path).
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
    /// - Antigravity CLI `~/.gemini/config/mcp_config.json` `mcpServers` (global)
    pub fn file(self) -> &'static str {
        match self {
            Self::Claude => ".mcp.json",
            Self::Cursor => ".cursor/mcp.json",
            Self::Gemini => ".gemini/settings.json",
            Self::VsCode => ".vscode/mcp.json",
            Self::Codex => ".codex/config.toml",
            Self::Agy => "~/.gemini/config/mcp_config.json",
        }
    }

    pub(crate) fn path(self, workspace: &Workspace) -> Result<PathBuf> {
        if self == Self::Agy {
            return crate::global::config_dir(crate::agents::Agent::Agy)
                .context("no home directory to find Antigravity MCP config in")
                .map(|dir| dir.join("mcp_config.json"));
        }
        Ok(workspace.root.join(self.file()))
    }

    pub(crate) fn servers_key(self) -> &'static str {
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
            Self::Claude | Self::Gemini | Self::Codex | Self::Agy => {
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

/// Renders each client's config with the `justless` server added.
/// Every other key is preserved, and an existing `justless` entry is kept
/// as it is.
pub fn config_entries(
    workspace: &Workspace,
    clients: &BTreeSet<Client>,
) -> Result<Vec<ConfigFile>> {
    clients
        .iter()
        .map(|&client| {
            let path = client.path(workspace)?;
            let display = if client.is_global() {
                path.display().to_string()
            } else {
                client.file().to_owned()
            };
            if let Some(directory) = path.parent().filter(|dir| *dir != workspace.root) {
                match fs::symlink_metadata(directory) {
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
                merge_toml(&display, &original, &client.server_entry())?
            } else {
                merge_json(
                    &display,
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
    let project_local = clients.iter().any(|client| !client.is_global());
    let _guard = if project_local {
        workspace.prepare_state()?;
        Some(lock_state(&workspace.state.join("setup.lock"))?)
    } else {
        None
    };
    // Validate every selected file before writing any of them.
    let files = config_entries(workspace, clients)?;
    // Absolute paths show which project or user config was changed.
    for file in files {
        let path = file.path.display();
        match file.contents {
            Some(contents) => {
                let parent = file.path.parent().context("config directory")?;
                if file.client.is_global() {
                    if fs::symlink_metadata(parent).is_err() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("create {}", parent.display()))?;
                    }
                } else {
                    crate::uninstall::remember(workspace, &file.path, &contents);
                    private_dir(parent)?;
                }
                write_text(&file.path, &contents)?;
                println!("{}: added justless to {path}", file.client.label());
            }
            None => println!("{}: {path} already lists justless", file.client.label()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitStatus;

    fn tool(name: &str) -> &'static Tool {
        TOOLS.iter().find(|tool| tool.name == name).unwrap()
    }

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn ok_output(stdout: Vec<u8>, stderr: Vec<u8>) -> std::process::Output {
        std::process::Output {
            status: ExitStatus::default(),
            stdout,
            stderr,
        }
    }

    #[test]
    fn tools_list_schema_is_valid_and_compact() {
        let defs: Vec<Value> = TOOLS.iter().map(Tool::definition).collect();
        assert_eq!(defs.len(), 17);
        let serialized = serde_json::to_string(&defs).unwrap();
        assert!(
            serialized.len() < 6_000,
            "slimmed tools/list should stay compact, got {} bytes",
            serialized.len()
        );
        for def in &defs {
            assert_eq!(def["inputSchema"]["type"], "object", "{def:#}");
            assert_eq!(def["annotations"]["readOnlyHint"], true, "{def:#}");
            let desc = def["description"].as_str().unwrap();
            assert!(
                (40..=200).contains(&desc.len()),
                "{}: {} bytes ({desc:?})",
                def["name"],
                desc.len()
            );
            let props = def["inputSchema"]["properties"].as_object().unwrap();
            assert!(!props.contains_key("unredacted"), "{def:#}");
        }
        let by_name = |name: &str| defs.iter().find(|d| d["name"] == name).unwrap();
        assert!(
            by_name("log")["description"]
                .as_str()
                .unwrap()
                .contains("secrets redacted")
        );

        let recall = &by_name("recall")["inputSchema"];
        assert!(recall.get("required").is_none());
        let rprops = recall["properties"].as_object().unwrap();
        assert_eq!(rprops["id"]["type"], "string");
        assert_eq!(rprops["from"]["type"], "integer");
        assert_eq!(rprops["to"]["type"], "integer");
        assert_eq!(rprops["grep"]["type"], "string");
        assert_eq!(rprops["limit"]["type"], "integer");
        assert_eq!(rprops["list"]["type"], "boolean");

        let read = &by_name("read")["inputSchema"];
        assert!(read.get("required").is_none());
        assert_eq!(read["properties"]["file"]["type"], "string");
        assert_eq!(read["properties"]["files"]["type"], "array");
        assert_eq!(read["properties"]["files"]["items"]["type"], "string");

        let ctx = &by_name("context")["inputSchema"]["properties"];
        assert_eq!(ctx["symbol"]["type"], "string");
        assert_eq!(ctx["fresh"]["type"], "boolean");

        assert_eq!(
            by_name("pack")["inputSchema"]["properties"]["fresh"]["type"],
            "boolean"
        );
        assert_eq!(
            by_name("lookup")["inputSchema"]["properties"]["logs"]["type"],
            "boolean"
        );
    }

    #[test]
    fn argv_handles_parity_and_recall_options_and_validation() {
        // Multi-file read
        assert_eq!(
            tool("read")
                .argv(&map(
                    json!({"files": ["./src/a.rs", "src/b.rs"], "outline": true})
                ))
                .unwrap(),
            ["read", "--outline", "--", "src/a.rs", "src/b.rs"]
        );
        assert!(tool("read").argv(&map(json!({}))).is_err());
        assert!(
            tool("read")
                .argv(&map(json!({"file": "a.rs", "files": ["b.rs"]})))
                .is_err()
        );
        assert!(tool("read").argv(&map(json!({"files": []}))).is_err());
        assert!(
            tool("read")
                .argv(&map(json!({"files": ["../outside.rs"]})))
                .is_err()
        );
        assert!(tool("read").argv(&map(json!({"files": ["."]}))).is_err());

        // Context symbol + fresh
        assert_eq!(
            tool("context")
                .argv(&map(json!({"symbol": "foo::bar", "fresh": true})))
                .unwrap(),
            ["context", "--symbol=foo::bar", "--fresh"]
        );
        assert!(
            tool("context")
                .argv(&map(json!({"symbol": "foo", "at": "a.rs:1"})))
                .is_err()
        );

        // Pack fresh & Lookup logs
        assert_eq!(
            tool("pack")
                .argv(&map(json!({"question": "how", "fresh": true})))
                .unwrap(),
            ["pack", "--fresh", "--", "how"]
        );
        assert_eq!(
            tool("lookup")
                .argv(&map(json!({"question": "err", "logs": true})))
                .unwrap(),
            ["lookup", "--logs", "--", "err"]
        );

        // Recall filters and list
        assert_eq!(
            tool("recall")
                .argv(&map(
                    json!({"id": "toolu_01", "from": 2, "to": 10, "grep": "err", "limit": 5})
                ))
                .unwrap(),
            [
                "recall",
                "--from=2",
                "--to=10",
                "--grep=err",
                "--limit=5",
                "--",
                "toolu_01"
            ]
        );
        assert_eq!(
            tool("recall")
                .argv(&map(json!({"list": true, "limit": 5})))
                .unwrap(),
            ["recall", "--limit=5", "--list"]
        );
        assert!(tool("recall").argv(&map(json!({}))).is_err());
        assert!(
            tool("recall")
                .argv(&map(json!({"id": "toolu_01", "list": true})))
                .is_err()
        );
        assert!(
            tool("recall")
                .argv(&map(json!({"list": true, "from": 1})))
                .is_err()
        );
        assert!(
            tool("recall")
                .argv(&map(json!({"list": true, "grep": "x"})))
                .is_err()
        );
        assert!(
            tool("recall")
                .argv(&map(json!({"id": "toolu_01", "from": 10, "to": 2})))
                .is_err()
        );
    }

    #[test]
    fn session_selection_and_key_uniqueness() {
        assert!(uses_session("pack", &map(json!({"question": "x"}))));
        assert!(uses_session(
            "pack",
            &map(json!({"question": "x", "fresh": true}))
        ));
        assert!(uses_session("context", &map(json!({"symbol": "foo"}))));
        assert!(uses_session("context", &map(json!({"at": "a.rs:1"}))));
        assert!(!uses_session("context", &map(json!({"changes": true}))));
        assert!(!uses_session("context", &map(json!({"run_id": "run-1"}))));
        assert!(!uses_session("read", &map(json!({"file": "a.rs"}))));

        let a = new_session();
        let b = new_session();
        assert!(a.starts_with("mcp-") && a.len() == 28);
        assert_ne!(a, b);
    }

    #[test]
    fn recall_pages_bounds_multiline_single_line_grep_and_list() {
        let root = Path::new("/repo");

        // Short output passes through verbatim.
        let short = recall_pages(
            &ok_output(b"hello\nworld\n".to_vec(), Vec::new()),
            &map(json!({"id": "toolu_1"})),
            root,
        );
        assert_eq!(short, ["hello\nworld\n"]);

        // Large multiline output pages cleanly on a line boundary with `from=` + `to=`.
        let mut rows = String::new();
        for i in 1..=5000 {
            rows.push_str(&format!("row {i:05}\n"));
        }
        let paged = recall_pages(
            &ok_output(rows.into_bytes(), Vec::new()),
            &map(json!({"id": "toolu_big", "from": 10, "to": 4000})),
            root,
        );
        assert_eq!(paged.len(), 1);
        assert!(paged[0].starts_with("row 00001\n"));
        assert!(
            paged[0].contains("recall page ends at line 3285 after 32768 bytes"),
            "{}",
            &paged[0][paged[0].len() - 200..]
        );
        assert!(
            paged[0].contains("id=\"toolu_big\", from=3286, to=4000 for more"),
            "{}",
            &paged[0][paged[0].len() - 200..]
        );

        // Single line exceeding LOG_PAGE with multi-byte UTF-8 cuts cleanly at a char boundary.
        let mut long_line = "a".repeat(LOG_PAGE - 2);
        long_line.push('🦀');
        long_line.push_str(&"b".repeat(1000));
        let cut = recall_pages(
            &ok_output(long_line.into_bytes(), Vec::new()),
            &map(json!({"id": "toolu_long"})),
            root,
        );
        assert_eq!(cut.len(), 1);
        assert!(!cut[0].contains('\u{FFFD}'));
        assert!(cut[0].contains("recall line 1 is longer than 32768 bytes and was cut"));
        assert!(cut[0].contains("justless --root /repo recall toolu_long --from 1 --to 1"));
        assert!(cut[0].contains("id=\"toolu_long\", from=2 to continue"));

        // Large grep output truncates cleanly and hints the next line number.
        let mut grep_out = String::new();
        for i in 1..=3000 {
            grep_out.push_str(&format!("{}: match text here\n", i * 2));
        }
        let grepped = recall_pages(
            &ok_output(grep_out.into_bytes(), Vec::new()),
            &map(json!({"id": "toolu_g", "grep": "match"})),
            root,
        );
        assert_eq!(grepped.len(), 1);
        assert!(
            grepped[0].contains("recall grep truncated after 32768 bytes"),
            "{}",
            &grepped[0][grepped[0].len() - 200..]
        );
        assert!(grepped[0].contains("from="));

        // Large list output truncates cleanly.
        let list_out = "toolu_x  Bash  echo hi  1.0 KB  1m ago\n".repeat(1000);
        let listed = recall_pages(
            &ok_output(list_out.into_bytes(), Vec::new()),
            &map(json!({"list": true})),
            root,
        );
        assert_eq!(listed.len(), 1);
        assert!(listed[0].contains("recall list truncated after 32768 bytes"));
    }
}
