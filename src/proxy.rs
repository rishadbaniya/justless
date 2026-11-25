//! Request-boundary context management.
//!
//! `justless proxy` relays Claude API requests on 127.0.0.1 and adds Anthropic's
//! server-side tool-result clearing; `justless wrap claude` runs Claude Code through
//! it. The conversation a client sends is never edited: thinking blocks are tied to
//! the exact `system`, `tools` and `messages` before them, so the proxy only adds a
//! `context_management` edit and its beta, which that check ignores. Tool results
//! are archived by tool_use ID for `justless recall --tool-use`. Any parse failure
//! forwards the original request unchanged.

mod compact;
mod http;
mod ledger;
mod observe;
mod plan;
mod state;
mod upstream;
mod wrap;

use std::collections::HashSet;
use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Arg, ArgMatches, Command};
use ureq::http::HeaderMap;

use crate::history::{human_bytes, now_ms};
use crate::workspace::{Workspace, private_dir};
use http::{Framing, Head};
pub(crate) use ledger::report;
use ledger::{Job, LiveCompacted, Log, Record, Totals, short};
use observe::{Observed, Observer};
use plan::{Body, Request, Role};
use upstream::{Outbound, SendError, Upstream};

/// Larger request bodies are streamed through without being read.
const MAX_BODY: u64 = 64 * 1024 * 1024;
/// How much of an error response is read to recognize a refused edit.
const MAX_ERROR_BODY: u64 = 1024 * 1024;
const IDLE: Duration = Duration::from_secs(300);
const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";
const JSON: &str = "application/json";

fn clearing_args() -> Vec<Arg<'static>> {
    vec![
        crate::number("keep", "12", "The newest N tool calls are never cleared"),
        crate::number(
            "floor",
            "100000",
            "No clearing while the estimated prompt is below this many tokens",
        ),
        crate::number(
            "clear-at-least",
            "20000",
            "Minimum estimated tokens one clear must remove; also sent to the API",
        ),
        crate::number(
            "payback",
            "12",
            "A clear on a warm cache must repay its cache rewrite within N requests (0: never)",
        ),
        Arg::new("cold-after")
            .long("cold-after")
            .value_name("SECONDS")
            .help("Idle seconds after which the cache counts as expired (default: its TTL + 15; 0: never)"),
        Arg::new("no-clear")
            .long("no-clear")
            .help("Archive and record only; never ask the API to clear"),
        Arg::new("no-archive")
            .long("no-archive")
            .help("Do not archive tool results"),
        Arg::new("compact-live")
            .long("compact-live")
            .help("Compact oversized newest tool_result text blocks in the uncached final user turn (or set JUSTLESS_PROXY_COMPACT_LIVE=1)"),
        crate::number(
            "compact-live-min-bytes",
            "8192",
            "Smallest newest tool_result text block (bytes) eligible for --compact-live",
        ),
    ]
}

pub fn commands() -> Vec<Command<'static>> {
    vec![
        Command::new("proxy")
            .about("Relay Claude API requests, asking the API to clear old tool results and archiving them")
            .arg(
                Arg::new("port")
                    .long("port")
                    .value_name("N")
                    .default_value("0")
                    .help("Port on 127.0.0.1 (0 picks a free one)"),
            )
            .arg(
                Arg::new("upstream")
                    .long("upstream")
                    .value_name("URL")
                    .help("API base URL (default: $ANTHROPIC_BASE_URL, else https://api.anthropic.com)"),
            )
            .arg(
                Arg::new("verbose")
                    .long("verbose")
                    .help("Print one line per request to stderr, never bodies or header values"),
            )
            .args(clearing_args()),
        // `claude` is taken as an external subcommand so that every argument after
        // it, `--` and `--root` included, reaches Claude Code unparsed.
        Command::new("wrap")
            .about("Run Claude Code through a local proxy that clears old tool results")
            .override_usage("justless wrap [OPTIONS] claude [CLAUDE_ARGS]...")
            .after_help("Every argument after `claude` is passed to Claude Code unchanged.")
            .args(clearing_args())
            .subcommand_required(true)
            .allow_external_subcommands(true)
            .allow_invalid_utf8_for_external_subcommands(true),
    ]
}

struct Settings {
    options: plan::Options,
    archive: bool,
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let value = value.trim().to_ascii_lowercase();
        !value.is_empty() && !matches!(value.as_str(), "0" | "false" | "no" | "off")
    })
}

fn settings(args: &ArgMatches) -> Result<Settings> {
    let cold_after = args
        .value_of("cold-after")
        .map(|value| {
            value
                .parse::<u64>()
                .context("--cold-after expects a number of seconds")
        })
        .transpose()?;
    let compact_live = args.is_present("compact-live") || env_truthy("JUSTLESS_PROXY_COMPACT_LIVE");
    let compact_live_min_bytes = crate::read_num(args, "compact-live-min-bytes", 1)?;
    Ok(Settings {
        options: plan::Options {
            keep: crate::read_num(args, "keep", 0)?,
            floor: crate::read_num(args, "floor", 0)? as u64,
            clear_at_least: crate::read_num(args, "clear-at-least", 0)? as u64,
            payback: crate::read_num(args, "payback", 0)? as u64,
            cold_after,
            clear: !args.is_present("no-clear"),
            compact_live,
            compact_live_min_bytes,
            compact_live_budget: plan::DEFAULT_COMPACT_LIVE_BUDGET,
        },
        archive: !args.is_present("no-archive"),
    })
}

fn default_upstream() -> String {
    std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_UPSTREAM.to_owned())
}

pub fn run(ws: &Workspace, name: &str, args: &ArgMatches) -> Result<i32> {
    let settings = settings(args)?;
    if name == "wrap" {
        return wrap::run(ws, args, settings);
    }
    let port: u16 = args
        .value_of("port")
        .unwrap_or("0")
        .parse()
        .context("--port expects a port number")?;
    let upstream = args
        .value_of("upstream")
        .map_or_else(default_upstream, str::to_owned);
    let options = settings.options.clone();
    let proxy = Proxy::start(
        ws,
        port,
        &upstream,
        settings.options,
        settings.archive,
        args.is_present("verbose"),
    )?;
    let addr = proxy.addr();
    println!("justless proxy on {addr} → {}", proxy.upstream());
    let archive = if settings.archive {
        ws.state.display().to_string()
    } else {
        "off".into()
    };
    if options.clear {
        println!(
            "Clears old tool results from {} tokens, keeping the newest {} calls. Archive: {archive}",
            short(options.floor),
            options.keep
        );
    } else {
        println!("Archives tool results without asking the API to clear them. Archive: {archive}");
    }
    if options.compact_live && settings.archive {
        println!(
            "Live-zone compaction: newest uncached tool results >= {} compacted to <= {} with recall archive.",
            human_bytes(options.compact_live_min_bytes as u64),
            human_bytes(options.compact_live_budget as u64),
        );
    }
    // Claude Code turns these off behind a custom base URL; with them on, its
    // requests match an unproxied session's and carry their request class.
    println!(
        "Claude Code:   export ANTHROPIC_BASE_URL=http://{addr} {}",
        wrap::PARITY.map(|name| format!("{name}=1")).join(" ")
    );
    println!("Other Anthropic clients: base URL http://{addr}");
    signals::wait();
    proxy.stop();
    Ok(0)
}

#[cfg(unix)]
mod signals {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    static STOP: AtomicBool = AtomicBool::new(false);

    extern "C" fn stop(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }

    /// Blocks until SIGINT or SIGTERM. The handler only sets a flag.
    pub fn wait() {
        let handler = stop as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: the handler is async-signal-safe; it only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
        while !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(not(unix))]
mod signals {
    /// Runs until the process is ended; state is saved every second meanwhile.
    pub fn wait() {
        loop {
            std::thread::park();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

struct Shared {
    ws: Workspace,
    options: plan::Options,
    archive: bool,
    verbose: bool,
    upstream: Upstream,
    store: Mutex<state::Store>,
    /// Models that refused the clearing edit in this process.
    refused: Mutex<HashSet<String>>,
    jobs: Sender<Job>,
    totals: Arc<Mutex<Totals>>,
    log: Log,
    stopping: AtomicBool,
    active: AtomicUsize,
}

pub struct Proxy {
    shared: Arc<Shared>,
    addr: SocketAddr,
    accept: JoinHandle<()>,
    flusher: JoinHandle<()>,
}

impl Proxy {
    pub fn start(
        ws: &Workspace,
        port: u16,
        upstream: &str,
        options: plan::Options,
        archive: bool,
        verbose: bool,
    ) -> Result<Self> {
        let upstream = Upstream::new(upstream)?;
        let is_self = |port: u16| {
            upstream.authority().is_some_and(|(host, upstream_port)| {
                upstream_port == port
                    && matches!(
                        host.as_str(),
                        "127.0.0.1" | "localhost" | "[::1]" | "::1" | "0.0.0.0"
                    )
            })
        };
        // Checked before binding a given port too, so the answer never depends
        // on whether the port is free.
        if port != 0 && is_self(port) {
            bail!("the upstream {} is this proxy", upstream.base);
        }
        let listener = TcpListener::bind(("127.0.0.1", port))
            .with_context(|| format!("listen on 127.0.0.1:{port}"))?;
        let addr = listener.local_addr()?;
        if is_self(addr.port()) {
            bail!("the upstream {} is this proxy", upstream.base);
        }
        ws.prepare_state()?;
        let dir = ws.state.join("proxy");
        private_dir(&dir)?;
        let log = Log::open(dir.join("proxy.log"));
        // Under `wrap`, a panic message on stderr would land in Claude Code's
        // terminal, so the proxy logs it instead.
        let panics = log.clone();
        std::panic::set_hook(Box::new(move |info| {
            panics.line(&format!("panic: {info}"));
        }));
        log.line(&format!(
            "start {addr} → {} · keep {} · floor {} · clear-at-least {} · payback {} · cold-after {} · clear {} · archive {archive} · compact-live {} · compact-live-min-bytes {}",
            upstream.base,
            options.keep,
            options.floor,
            options.clear_at_least,
            options.payback,
            options
                .cold_after
                .map_or_else(|| "auto".to_owned(), |s| s.to_string()),
            options.clear,
            options.compact_live,
            options.compact_live_min_bytes,
        ));
        let (jobs, receiver) = mpsc::channel();
        let totals = Arc::new(Mutex::new(Totals::default()));
        let clone_ws = || Workspace {
            root: ws.root.clone(),
            cwd: ws.cwd.clone(),
            state: ws.state.clone(),
            origin: ws.origin.clone(),
        };
        let worker = ledger::Worker::new(clone_ws(), archive, log.clone(), totals.clone());
        thread::Builder::new()
            .name("justless-ledger".into())
            .spawn(move || worker.run(receiver))?;
        let shared = Arc::new(Shared {
            ws: clone_ws(),
            options,
            archive,
            verbose,
            upstream,
            store: Mutex::new(state::Store::load(dir.join("conversations.json"))),
            refused: Mutex::new(HashSet::new()),
            jobs,
            totals,
            log,
            stopping: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        });
        let accept = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("justless-accept".into())
                .spawn(move || accept_loop(&listener, &shared))?
        };
        let flusher = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("justless-state".into())
                .spawn(move || {
                    // Saves at most once a second; `stop` saves the rest.
                    let stopping = || shared.stopping.load(Ordering::SeqCst);
                    while !stopping() {
                        for _ in 0..10 {
                            if stopping() {
                                return;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                        let mut store = lock(&shared.store);
                        if store.dirty()
                            && let Err(error) = store.save()
                        {
                            shared.log.line(&format!("state: {error:#}"));
                        }
                    }
                })?
        };
        Ok(Self {
            shared,
            addr,
            accept,
            flusher,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn upstream(&self) -> &str {
        &self.shared.upstream.base
    }

    /// Stops accepting, waits up to 2 seconds for requests in flight, then saves
    /// state and waits up to 5 seconds for the archive and ledger.
    pub fn stop(self) -> Totals {
        let shared = self.shared;
        shared.stopping.store(true, Ordering::SeqCst);
        // Wake the blocking accept.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        let _ = self.accept.join();
        let deadline = Instant::now() + Duration::from_secs(2);
        while shared.active.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.flusher.join();
        if let Err(error) = lock(&shared.store).save() {
            shared.log.line(&format!("state: {error:#}"));
        }
        let (done, flushed) = mpsc::channel();
        if shared.jobs.send(Job::Flush(done)).is_ok() {
            let _ = flushed.recv_timeout(Duration::from_secs(5));
        }
        shared.log.line("stop");
        lock(&shared.totals).clone()
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else {
            continue;
        };
        let shared = shared.clone();
        let spawned = thread::Builder::new()
            .name("justless-connection".into())
            .spawn(move || {
                if catch_unwind(AssertUnwindSafe(|| serve(&shared, stream))).is_err() {
                    shared.log.line("connection: internal error");
                }
            });
        if spawned.is_err() {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

struct Active<'a>(&'a AtomicUsize);

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn serve(shared: &Shared, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(IDLE));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    loop {
        let head = match http::read_head(&mut reader) {
            Ok(Some(head)) => head,
            Ok(None) => return,
            Err(error) => {
                let status = match error.kind() {
                    io::ErrorKind::OutOfMemory => 431,
                    io::ErrorKind::InvalidData => 400,
                    _ => return,
                };
                let body =
                    http::error_body("invalid_request_error", &format!("justless proxy: {error}"));
                let _ = http::write_simple(&mut writer, status, JSON, &body, true);
                return;
            }
        };
        shared.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(&shared.active);
        let keep = handle(shared, &head, &mut reader, &mut writer).unwrap_or(false);
        if !keep || shared.stopping.load(Ordering::SeqCst) {
            return;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Messages,
    CountTokens,
    Other,
}

impl Endpoint {
    fn of(head: &Head) -> Self {
        match (head.method.as_str(), head.path()) {
            ("POST", "/v1/messages") => Self::Messages,
            ("POST", "/v1/messages/count_tokens") => Self::CountTokens,
            _ => Self::Other,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::CountTokens => "count_tokens",
            Self::Other => "other",
        }
    }
}

/// A Claude Code hint header kept in the ledger: session, agent and request class.
fn hint(head: &Head, name: &str) -> Option<String> {
    head.header_str(name)
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 128
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        })
        .map(str::to_owned)
}

/// The outcome of planning one request.
#[derive(Default)]
struct Planned {
    /// The edited body, when clearing and/or live-zone compaction rewrote it.
    body: Option<Vec<u8>>,
    /// Whether Anthropic's `clear_tool_uses_20250919` edit was spliced into `body`.
    clearing_added: bool,
    /// Live-zone compaction stats when newest uncached `tool_result` blocks were compacted.
    live_compacted: Option<LiveCompacted>,
    request: Option<Request>,
    model: Option<String>,
    mode: &'static str,
    reason: Option<&'static str>,
    role: Option<Role>,
    uses: Option<usize>,
    boundary: Option<usize>,
    movement: Option<plan::Move>,
    ratio: Option<f64>,
    threshold: Option<f64>,
    cold: bool,
    /// The `(conversation_key, boundary_tool_use_id)` pushed by this request's
    /// move, so a refused edit or unreachable upstream can roll it back.
    moved_boundary: Option<(String, String)>,
    repeats: u64,
    client_new: bool,
}

impl Planned {
    fn pass(reason: &'static str) -> Self {
        Self {
            mode: "pass",
            reason: Some(reason),
            ..Default::default()
        }
    }
}

/// Read-only tools: one repeated after its result was cleared suggests the model
/// needed that result back.
const READS: [&str; 6] = [
    "Read",
    "Grep",
    "Glob",
    "WebFetch",
    "WebSearch",
    "NotebookRead",
];
/// Tools that change files, after which reading again is expected.
const WRITES: [&str; 4] = ["Edit", "MultiEdit", "Write", "NotebookEdit"];

/// Counts new read-only calls that repeat a cleared one with no file change in
/// between, then remembers the calls cleared up to the boundary. Reruns of tests
/// or builds are not counted: their output is expected to change.
fn count_repeats(calls: &mut state::Calls, req: &Request, boundary: usize) -> u64 {
    let input = |u: &plan::Use| &req.block(u.call)["input"];
    let hash = |u: &plan::Use| plan::call_hash(&u.name, input(u));
    let path = |u: &plan::Use| {
        let input = input(u);
        input["file_path"]
            .as_str()
            .or_else(|| input["notebook_path"].as_str())
    };
    let mut repeats = 0;
    if calls.started {
        for (at, tool_use) in req.uses.iter().enumerate() {
            if !calls.seen.insert(tool_use.id.clone()) || !READS.contains(&tool_use.name.as_str()) {
                continue;
            }
            let Some(&cleared) = calls.cleared.get(&hash(tool_use)) else {
                continue;
            };
            // A Read after an edit of its file, or a search after any edit,
            // reads new content.
            let changed = req
                .uses
                .get(cleared + 1..at)
                .unwrap_or_default()
                .iter()
                .any(|between| {
                    WRITES.contains(&between.name.as_str())
                        && (tool_use.name != "Read" || path(between) == path(tool_use))
                });
            repeats += u64::from(!changed);
        }
    } else {
        // Calls already in the conversation when this process first sees it are
        // not new.
        calls.started = true;
        calls
            .seen
            .extend(req.uses.iter().map(|tool_use| tool_use.id.clone()));
    }
    for (at, tool_use) in req
        .uses
        .iter()
        .enumerate()
        .take(boundary)
        .skip(calls.hashed)
    {
        calls.cleared.insert(hash(tool_use), at);
    }
    calls.hashed = calls.hashed.max(boundary.min(req.uses.len()));
    repeats
}

impl Shared {
    fn plan(&self, head: &Head, body: &[u8], endpoint: Endpoint, now: u64) -> Planned {
        let Some(parsed) = Body::parse(body) else {
            return Planned::pass("unparsed");
        };
        let req = match Request::read(&parsed) {
            Ok(req) => req,
            Err(reason) => return Planned::pass(reason),
        };
        let mut planned = Planned {
            model: req.model.clone(),
            uses: Some(req.uses.len()),
            mode: "pass",
            ..Default::default()
        };
        let live_body = if self.options.compact_live
            && self.archive
            && endpoint == Endpoint::Messages
            && !req.thread
        {
            compact::compact_live_zone(&self.ws, &parsed, &self.options).map(|live| {
                planned.live_compacted = Some(live.stats);
                live.body
            })
        } else {
            None
        };
        self.decide(
            head,
            &parsed,
            live_body.as_deref(),
            &req,
            endpoint,
            now,
            &mut planned,
        );
        if planned.body.is_none()
            && let Some(compacted_body) = live_body
        {
            planned.body = Some(compacted_body);
            if planned.mode == "pass" {
                planned.mode = "compact";
            }
        }
        planned.request = Some(req);
        planned
    }

    #[allow(clippy::too_many_arguments)]
    fn decide(
        &self,
        head: &Head,
        parsed: &Body,
        live_body: Option<&[u8]>,
        req: &Request,
        endpoint: Endpoint,
        now: u64,
        planned: &mut Planned,
    ) {
        let Some(model) = req
            .model
            .as_deref()
            .filter(|m| m.starts_with("claude-") && !m.starts_with("claude-3-"))
        else {
            planned.reason = Some("not-claude");
            return;
        };
        if lock(&self.refused).contains(model) {
            planned.mode = "refused";
            return;
        }
        if req.thread {
            planned.reason = Some("thread");
            return;
        }
        if req.compaction {
            planned.reason = Some("compaction-param");
            return;
        }
        let key = req.key();
        let mut store = lock(&self.store);
        let sticky = key.is_some_and(|k| store.get(k).is_some_and(|c| c.client_managed));
        if sticky || req.client_edits() {
            // Two clearers never alternate: the client keeps the conversation.
            planned.mode = "client";
            planned.reason = Some("client-managed");
            if let Some(key) = key
                && !sticky
            {
                store.update(key, now, |c| c.client_managed = true);
                planned.client_new = true;
            }
            return;
        }
        if !self.options.clear {
            planned.reason = Some("no-clear");
            return;
        }
        let Some(key) = key else {
            planned.reason = Some("no-tool-uses");
            return;
        };
        let role = Role::of(
            endpoint == Endpoint::Messages,
            head.header_str("x-claude-code-request-class"),
            head.header_str("x-claude-code-agent-type"),
        );
        planned.role = Some(role);
        let conversation = store.get(key).cloned();
        let decision = plan::decide(req, model, conversation.as_ref(), role, &self.options, now);
        if role == Role::Mover {
            planned.repeats = count_repeats(store.calls(key), req, decision.boundary);
        }
        // Token counts never touch the prompt cache, so they leave the state alone.
        if endpoint == Endpoint::Messages {
            let boundary_id = decision
                .moved
                .is_some()
                .then(|| req.uses[decision.boundary - 1].id.clone());
            store.update(key, now, |c| {
                c.last_start.insert(model.to_owned(), now);
                if let Some(id) = &boundary_id {
                    c.boundaries.push(id.clone());
                }
            });
            if let Some(id) = boundary_id {
                planned.moved_boundary = Some((key.to_owned(), id));
            }
        }
        drop(store);
        planned.ratio = decision.ratio;
        planned.threshold = decision.threshold;
        planned.cold = decision.cold;
        planned.movement = decision.moved.clone();
        if decision.boundary < plan::MIN_BOUNDARY {
            planned.reason = Some(decision.idle_reason(req, &self.options));
            return;
        }
        let edit = plan::edit(
            req.uses.len(),
            decision.boundary,
            self.options.clear_at_least,
        );
        let spliced = match live_body {
            Some(bytes) => Body::parse(bytes).and_then(|live_parsed| {
                plan::splice(&live_parsed, req.context_management.as_ref(), edit)
            }),
            None => plan::splice(parsed, req.context_management.as_ref(), edit),
        };
        match spliced {
            Some(body) => {
                planned.body = Some(body);
                planned.clearing_added = true;
                planned.mode = "cleared";
                planned.boundary = Some(decision.boundary);
                // Why the boundary stayed where it is, when more could clear.
                planned.reason = decision.held;
            }
            None => planned.reason = Some("unparsed"),
        }
    }

    /// Undoes a boundary this request pushed before upstream answered when the
    /// request never reached upstream or the model refused the clearing edit.
    fn rollback_boundary(&self, planned: &mut Planned, now: u64) {
        let Some((key, boundary_id)) = planned.moved_boundary.take() else {
            return;
        };
        let mut store = lock(&self.store);
        store.update(&key, now, |c| {
            if let Some(pos) = c.boundaries.iter().rposition(|id| id == &boundary_id) {
                c.boundaries.remove(pos);
            }
        });
    }

    fn send_failed(&self, writer: &mut TcpStream, error: SendError) -> io::Result<u16> {
        let (status, body) = match error {
            SendError::Request(message) => (
                400,
                http::error_body(
                    "invalid_request_error",
                    &format!("justless proxy could not forward the request: {message}"),
                ),
            ),
            SendError::Upstream(message) => (
                502,
                http::error_body(
                    "api_error",
                    &format!(
                        "justless proxy could not reach {}: {message}",
                        self.upstream.base
                    ),
                ),
            ),
        };
        http::write_simple(writer, status, JSON, &body, false)?;
        Ok(status)
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a [u8]> {
    headers.get(name).map(|value| value.as_bytes())
}

/// Handles one request and returns whether the connection can be reused.
fn handle(
    shared: &Shared,
    head: &Head,
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
) -> io::Result<bool> {
    let started = Instant::now();
    let now = now_ms();
    let framing = match head.framing() {
        Ok(framing) => framing,
        Err(error) => {
            let body =
                http::error_body("invalid_request_error", &format!("justless proxy: {error}"));
            http::write_simple(writer, 400, JSON, &body, true)?;
            return Ok(false);
        }
    };
    if !matches!(framing, Framing::Empty) && head.has_token("expect", "100-continue") {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        writer.flush()?;
    }
    let endpoint = Endpoint::of(head);
    let mut source = http::body_reader(&mut *reader, &framing);
    let mut body = Vec::new();
    if let Err(error) = source.by_ref().take(MAX_BODY + 1).read_to_end(&mut body) {
        // A body cut short or badly chunked is never forwarded.
        if matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
        ) {
            let message = format!("justless proxy: incomplete request body: {error}");
            let reply = http::error_body("invalid_request_error", &message);
            let _ = http::write_simple(writer, 400, JSON, &reply, true);
            return Ok(false);
        }
        return Err(error);
    }
    let too_large = body.len() as u64 > MAX_BODY;
    let browser = head.header("origin").is_some();
    let candidate = endpoint != Endpoint::Other && !too_large && !browser;
    let mut planned = if too_large {
        Planned::pass("too-large")
    } else if endpoint == Endpoint::Other {
        Planned::pass("other-path")
    } else if browser {
        // A browser page could reach a loopback port; relay it, but never plan or
        // archive its requests.
        Planned::pass("browser-origin")
    } else {
        // Fail open: a planning bug forwards the original request.
        catch_unwind(AssertUnwindSafe(|| shared.plan(head, &body, endpoint, now)))
            .unwrap_or_else(|_| Planned::pass("internal-error"))
    };
    let keep_alive = head.keep_alive() && !too_large;
    let sent = if too_large {
        // The declared length is forwarded with the streamed body, so the
        // upstream sees the framing the client chose.
        let length = match &framing {
            Framing::Length(length) => Some(*length),
            Framing::Empty | Framing::Chunked => None,
        };
        let mut rest = io::Cursor::new(std::mem::take(&mut body)).chain(&mut source);
        shared.upstream.send(
            &head.method,
            &head.target,
            &head.headers,
            Outbound::Stream(&mut rest, length),
        )
    } else if let Some(edited) = &planned.body {
        if planned.clearing_added {
            let mut headers = head.headers.clone();
            plan::merge_beta(&mut headers);
            shared.upstream.send(
                &head.method,
                &head.target,
                &headers,
                Outbound::Bytes(edited),
            )
        } else {
            shared.upstream.send(
                &head.method,
                &head.target,
                &head.headers,
                Outbound::Bytes(edited),
            )
        }
    } else {
        shared.upstream.send(
            &head.method,
            &head.target,
            &head.headers,
            Outbound::Bytes(&body),
        )
    };
    drop(source);
    let mut record = Record {
        ts: now,
        session: hint(head, "x-claude-code-session-id"),
        agent: hint(head, "x-claude-code-agent-id"),
        class: hint(head, "x-claude-code-request-class"),
        path: endpoint.name().into(),
        ..Default::default()
    };
    let response = match sent {
        Ok(response) => response,
        Err(error) => {
            shared.rollback_boundary(&mut planned, now);
            record.status = shared.send_failed(writer, error)?;
            finish(shared, record, planned, None, false, started);
            return Ok(keep_alive);
        }
    };
    let (mut parts, response_body) = response.into_parts();
    let mut upstream_body = response_body.into_reader();
    let mut prefix = Vec::new();
    if planned.clearing_added && matches!(parts.status.as_u16(), 400 | 422) {
        let _ = (&mut upstream_body)
            .take(MAX_ERROR_BODY)
            .read_to_end(&mut prefix);
        let text = String::from_utf8_lossy(&prefix);
        if ["context_management", "clear_tool_uses", plan::BETA]
            .iter()
            .any(|needle| text.contains(needle))
        {
            // The model refused the edit: send the original once and leave the
            // edit off for that model from now on.
            let model = planned.model.clone().unwrap_or_default();
            lock(&shared.refused).insert(model.clone());
            shared.rollback_boundary(&mut planned, now);
            shared.log.line(&format!(
                "fallback: {model} answered {} to the clearing edit; resent without it",
                parts.status.as_u16()
            ));
            planned.mode = "fallback";
            planned.body = None;
            planned.clearing_added = false;
            planned.live_compacted = None;
            planned.movement = None;
            planned.boundary = None;
            match shared.upstream.send(
                &head.method,
                &head.target,
                &head.headers,
                Outbound::Bytes(&body),
            ) {
                Ok(retry) => {
                    let (retry_parts, retry_body) = retry.into_parts();
                    parts = retry_parts;
                    upstream_body = retry_body.into_reader();
                    prefix.clear();
                }
                Err(error) => {
                    record.status = shared.send_failed(writer, error)?;
                    finish(shared, record, planned, None, false, started);
                    return Ok(keep_alive);
                }
            }
        }
    }
    let status = parts.status.as_u16();
    let mut observer = if candidate {
        Observer::new(
            header(&parts.headers, "content-type"),
            header(&parts.headers, "content-encoding"),
        )
    } else {
        Observer::Ignored
    };
    let relayed = relay(
        writer,
        head,
        status,
        &parts.headers,
        &prefix,
        &mut upstream_body,
        &mut observer,
        keep_alive,
    );
    record.status = status;
    let observed = observer.finish();
    if candidate {
        record.observed = Some(observed.is_some());
    }
    finish(shared, record, planned, observed, candidate, started);
    relayed.map(|()| keep_alive)
}

/// Fills in the ledger record and hands it to the worker.
fn finish(
    shared: &Shared,
    mut record: Record,
    mut planned: Planned,
    observed: Option<Observed>,
    candidate: bool,
    started: Instant,
) {
    record.mode = planned.mode.into();
    record.reason = planned.reason.map(str::to_owned);
    record.role = planned.role.map(|role| role.name().to_owned());
    record.repeats = planned.repeats;
    record.client_managed_new = planned.client_new;
    record.live_compacted = planned.live_compacted.clone();
    record.uses = planned.uses.map(|u| u as u64);
    let edited = planned.boundary.zip(planned.uses);
    if let Some((boundary, uses)) = edited {
        record.boundary = Some(boundary as u64);
        record.trigger = Some(uses.saturating_sub(3).max(1) as u64);
        record.keep = Some((uses - boundary) as u64);
    }
    if let Some(movement) = &planned.movement {
        record.movement = Some(movement.kind.name().to_owned());
        record.newly_cleared = Some(movement.into());
        record.rewrite_est = Some(movement.rewrite);
    }
    let ok = (200..300).contains(&record.status);
    if let Some(observed) = &observed {
        record.model = observed.model.clone().or(planned.model.clone());
        record.input = observed.input;
        record.cache_read = observed.cache_read;
        record.cache_write = observed.cache_write;
        record.output = observed.output;
        record.api_cleared_uses = observed.cleared_uses;
        record.api_cleared_tokens = observed.cleared_tokens;
        if planned.movement.is_some() {
            record.rewrite_api = observed.cache_write;
        }
        if ok
            && record.path == "messages"
            && let Some((boundary, _)) = edited
        {
            record.mismatch = observed.cleared_uses.unwrap_or(0) != boundary as u64;
        }
    } else {
        record.model = planned.model.clone();
    }
    record.ms = started.elapsed().as_millis() as u64;
    if shared.verbose {
        eprintln!("{}", verbose_line(&record, &planned, started.elapsed()));
    }
    let archive = (shared.archive && candidate && ok)
        .then(|| planned.request.take())
        .flatten();
    let _ = shared.jobs.send(Job::Done(Box::new((record, archive))));
}

fn verbose_line(record: &Record, planned: &Planned, elapsed: Duration) -> String {
    let mut parts = vec![format!(
        "{} {} {} {}",
        record.status,
        record.path,
        record.model.as_deref().unwrap_or("-"),
        record.class.as_deref().unwrap_or("-"),
    )];
    if let Some(uses) = record.uses {
        parts.push(format!("{uses} uses"));
    }
    match (record.boundary, &planned.movement) {
        (Some(boundary), Some(movement)) => parts.push(format!(
            "cleared {boundary} (moved, {}, C/M {:.2})",
            if planned.cold { "cold" } else { "warm" },
            movement.tokens as f64 / movement.rewrite.max(1) as f64
        )),
        (Some(boundary), None) => parts.push(match &record.reason {
            Some(held) => format!("cleared {boundary}, held {held}"),
            None => format!("cleared {boundary}"),
        }),
        (None, _) => parts.push(format!(
            "{} {}",
            record.mode,
            record.reason.as_deref().unwrap_or_default()
        )),
    }
    if let Some(ratio) = planned.ratio.filter(|_| planned.movement.is_none()) {
        match planned.threshold {
            Some(threshold) => parts.push(format!("C/M {ratio:.2} of {threshold:.2}")),
            None => parts.push(format!("C/M {ratio:.2}")),
        }
    }
    if let (Some(uses), Some(tokens)) = (record.api_cleared_uses, record.api_cleared_tokens) {
        parts.push(format!(
            "API cleared {uses} calls, {} tokens",
            ledger::thousands(tokens)
        ));
    }
    if let Some(live) = &record.live_compacted {
        parts.push(format!(
            "live compacted {} ({} → {}, {} saved)",
            live.results,
            human_bytes(live.bytes_before),
            human_bytes(live.bytes_after),
            human_bytes(live.saved_bytes),
        ));
    }
    parts.push(format!("{:.1}s", elapsed.as_secs_f64()));
    parts.join(" · ")
}

/// Relays a response as it arrives: each upstream read is written and flushed at
/// once, so server-sent events and pings pass through unbuffered.
#[allow(clippy::too_many_arguments)]
fn relay(
    writer: &mut TcpStream,
    head: &Head,
    status: u16,
    headers: &HeaderMap,
    prefix: &[u8],
    body: &mut dyn Read,
    observer: &mut Observer,
    keep_alive: bool,
) -> io::Result<()> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut length = None;
    let mut chunked = false;
    for (name, value) in headers {
        match name.as_str() {
            "content-length" => {
                length = std::str::from_utf8(value.as_bytes())
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok());
            }
            "transfer-encoding" => chunked = true,
            name if http::hop_by_hop(name) => {}
            name => out.push((name.to_owned(), value.as_bytes().to_vec())),
        }
    }
    let close = !keep_alive || head.http10;
    if close {
        out.push(("connection".into(), b"close".to_vec()));
    }
    let bodiless = head.method == "HEAD" || matches!(status, 100..=199 | 204 | 304);
    if bodiless {
        if let Some(length) = length {
            out.push(("content-length".into(), length.to_string().into_bytes()));
        }
        return http::write_head(writer, status, &out);
    }
    // Content-Length is kept only when exactly that many bytes are relayed.
    let fixed = length.filter(|_| !chunked);
    let rechunk = fixed.is_none() && !head.http10;
    if let Some(length) = fixed {
        out.push(("content-length".into(), length.to_string().into_bytes()));
    } else if rechunk {
        out.push(("transfer-encoding".into(), b"chunked".to_vec()));
    }
    http::write_head(writer, status, &out)?;
    let mut sent = 0u64;
    let mut emit = |data: &[u8], writer: &mut TcpStream| -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        observer.feed(data);
        sent += data.len() as u64;
        if rechunk {
            http::write_chunk(writer, data)
        } else {
            writer.write_all(data)?;
            writer.flush()
        }
    };
    emit(prefix, writer)?;
    let mut buffer = vec![0; 16 * 1024];
    loop {
        match body.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => emit(&buffer[..read], writer)?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    if rechunk {
        http::finish_chunks(writer)?;
    }
    if fixed.is_some_and(|length| length != sent) {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn request(calls: &[(&str, Value)]) -> Request {
        let mut messages = vec![json!({"role":"user","content":"go"})];
        for (i, (name, input)) in calls.iter().enumerate() {
            let id = format!("toolu_{i:02}");
            messages.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":input}]}));
            messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"x"}]}));
        }
        let body =
            serde_json::to_vec(&json!({"model":"claude-opus-5-5","messages":messages})).unwrap();
        Request::read(&Body::parse(&body).unwrap()).unwrap()
    }

    #[test]
    fn repeats_count_only_reads_of_unchanged_content() {
        let test = ("Bash", json!({"command":"cargo test --locked"}));
        let read = ("Read", json!({"file_path":"src/parser.rs"}));
        let other = ("Read", json!({"file_path":"src/lib.rs"}));
        let grep = ("Grep", json!({"pattern":"parse"}));
        let edit = (
            "Edit",
            json!({"file_path":"src/parser.rs","old_string":"a","new_string":"b"}),
        );
        let mut calls = state::Calls::default();
        let first = [test.clone(), read.clone(), other.clone(), grep.clone()];
        assert_eq!(count_repeats(&mut calls, &request(&first), 4), 0);
        // Rerunning the tests is expected, and so is reading parser.rs or
        // searching again after parser.rs was edited. Reading the unedited
        // lib.rs again is the one repeat.
        let mut later = first.to_vec();
        later.extend([test, edit, read, other, grep]);
        assert_eq!(count_repeats(&mut calls, &request(&later), 4), 1);
        // A call is counted at most once.
        assert_eq!(count_repeats(&mut calls, &request(&later), 4), 0);
    }

    #[test]
    fn rollback_boundary_removes_only_the_unapplied_move() {
        let dir = tempfile::tempdir().unwrap();
        let (jobs, _rx) = mpsc::channel();
        let shared = Shared {
            ws: Workspace {
                root: dir.path().to_owned(),
                cwd: dir.path().to_owned(),
                state: dir.path().join(".justless"),
                origin: dir.path().to_owned(),
            },
            options: plan::Options::default(),
            archive: false,
            verbose: false,
            upstream: Upstream::new("http://127.0.0.1:9").unwrap(),
            store: Mutex::new(state::Store::load(dir.path().join("conversations.json"))),
            refused: Mutex::new(HashSet::new()),
            jobs,
            totals: Arc::new(Mutex::new(Totals::default())),
            log: Log::open(dir.path().join("proxy.log")),
            stopping: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        };
        lock(&shared.store).update("toolu_00", 10, |c| {
            c.boundaries.push("toolu_03".into());
            c.boundaries.push("toolu_07".into());
        });
        let mut planned = Planned {
            moved_boundary: Some(("toolu_00".into(), "toolu_07".into())),
            ..Default::default()
        };
        shared.rollback_boundary(&mut planned, 20);
        assert!(planned.moved_boundary.is_none());
        let store = lock(&shared.store);
        assert_eq!(store.get("toolu_00").unwrap().boundaries, ["toolu_03"]);
    }
}
