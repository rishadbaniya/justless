//! `justless proxy` and `justless wrap claude` against a local mock upstream that
//! records every request and streams server-sent events. No live API calls.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};

const BIN: &str = env!("CARGO_BIN_EXE_justless");
const BETA: &str = "context-management-2025-06-27";
const PROXY_VARS: &str =
    "HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy ANTHROPIC_BASE_URL";

// ---------------------------------------------------------------------------
// HTTP helpers shared by the mock upstream and the test client

#[derive(Clone, Debug)]
struct Message {
    /// Request line or status line.
    first: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Message {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn headers_named(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn target(&self) -> &str {
        self.first.split(' ').nth(1).unwrap_or_default()
    }

    fn status(&self) -> u16 {
        self.first.split(' ').nth(1).unwrap().parse().unwrap()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

fn read_line(reader: &mut impl BufRead) -> Option<String> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).ok()? == 0 {
        return None;
    }
    Some(
        String::from_utf8_lossy(&line)
            .trim_end_matches(['\r', '\n'])
            .to_owned(),
    )
}

fn read_chunked(reader: &mut impl BufRead) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let size = read_line(reader).unwrap();
        let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16).unwrap();
        if size == 0 {
            while !read_line(reader).unwrap().is_empty() {}
            return body;
        }
        let mut chunk = vec![0; size];
        reader.read_exact(&mut chunk).unwrap();
        body.extend_from_slice(&chunk);
        assert_eq!(read_line(reader).unwrap(), "");
    }
}

/// Reads a request or response. `bodiless` is for responses to HEAD.
fn read_message(reader: &mut impl BufRead, bodiless: bool) -> Option<Message> {
    let first = read_line(reader)?;
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    let mut message = Message {
        first,
        headers,
        body: Vec::new(),
    };
    if bodiless {
        return Some(message);
    }
    if message
        .header("transfer-encoding")
        .is_some_and(|v| v.contains("chunked"))
    {
        message.body = read_chunked(reader);
    } else if let Some(length) = message.header("content-length") {
        message.body = vec![0; length.parse().unwrap()];
        reader.read_exact(&mut message.body).ok()?;
    } else if message.first.starts_with("HTTP/") {
        reader.read_to_end(&mut message.body).ok()?;
    }
    Some(message)
}

// ---------------------------------------------------------------------------
// Mock upstream

enum Reply {
    Json(u16, Value),
    Raw {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    },
    /// Server-sent events, close-delimited, written `piece` bytes at a time. With a
    /// gate, nothing after the first event is written until the gate opens.
    Events {
        events: Vec<String>,
        gate: Option<Receiver<()>>,
        piece: usize,
    },
}

type Script = Box<dyn FnMut(&Message) -> Reply + Send>;

struct Mock {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Message>>>,
}

impl Mock {
    fn start(script: impl FnMut(&Message) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let script: Arc<Mutex<Script>> = Arc::new(Mutex::new(Box::new(script)));
        let recorded = seen.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (seen, script) = (recorded.clone(), script.clone());
                thread::spawn(move || serve_mock(stream, &seen, &script));
            }
        });
        Self { addr, seen }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn seen(&self) -> Vec<Message> {
        self.seen.lock().unwrap().clone()
    }
}

fn serve_mock(stream: TcpStream, seen: &Mutex<Vec<Message>>, script: &Mutex<Script>) {
    stream.set_nodelay(true).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    while let Some(request) = read_message(&mut reader, false) {
        seen.lock().unwrap().push(request.clone());
        let reply = (script.lock().unwrap())(&request);
        let head_request = request.first.starts_with("HEAD ");
        match reply {
            Reply::Json(status, value) => {
                let body = serde_json::to_vec(&value).unwrap();
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nrequest-id: req_mock\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                writer.write_all(head.as_bytes()).unwrap();
                writer.write_all(&body).unwrap();
            }
            Reply::Raw {
                status,
                headers,
                body,
            } => {
                let mut head = format!("HTTP/1.1 {status} X\r\n");
                for (name, value) in headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
                writer.write_all(head.as_bytes()).unwrap();
                if !head_request {
                    writer.write_all(&body).unwrap();
                }
            }
            Reply::Events {
                events,
                gate,
                piece,
            } => {
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n";
                writer.write_all(head.as_bytes()).unwrap();
                for (i, event) in events.iter().enumerate() {
                    for part in event.as_bytes().chunks(piece) {
                        if writer
                            .write_all(part)
                            .and_then(|()| writer.flush())
                            .is_err()
                        {
                            return;
                        }
                    }
                    if i == 0
                        && let Some(gate) = &gate
                    {
                        let _ = gate.recv_timeout(Duration::from_secs(10));
                    }
                }
                return;
            }
        }
        writer.flush().unwrap();
    }
}

fn ok_json() -> Reply {
    Reply::Json(
        200,
        json!({"id":"msg_01","type":"message","role":"assistant","model":"claude-haiku-4-5","content":[{"type":"text","text":"ok"}],
            "usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":2}}),
    )
}

/// A realistic streamed reply, optionally reporting a tool-result clearing edit.
fn events(model: &str, cleared: Option<(u64, u64)>, cache_write: u64) -> Vec<String> {
    let mut delta = json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":20}});
    if let Some((uses, tokens)) = cleared {
        delta["context_management"] = json!({"applied_edits":[
            {"type":"clear_thinking_20251015","cleared_thinking_turns":1,"cleared_input_tokens":50},
            {"type":"clear_tool_uses_20250919","cleared_tool_uses":uses,"cleared_input_tokens":tokens}
        ]});
    }
    let start = json!({"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,
        "usage":{"input_tokens":100,"cache_read_input_tokens":2000,"cache_creation_input_tokens":cache_write,"output_tokens":1}}});
    vec![
        format!("event: message_start\ndata: {start}\n\n"),
        "event: ping\ndata: {\"type\": \"ping\"}\n\n".into(),
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".into(),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Done.\"}}\n\n".into(),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".into(),
        format!("event: message_delta\ndata: {delta}\n\n"),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into(),
    ]
}

fn stream(events: Vec<String>) -> Reply {
    Reply::Events {
        events,
        gate: None,
        piece: usize::MAX,
    }
}

// ---------------------------------------------------------------------------
// The proxy under test and a client for it

struct Proxy {
    child: Option<Child>,
    port: u16,
}

impl Proxy {
    /// Starts `justless proxy` with these space-separated flags.
    fn start(root: &Path, upstream: &str, flags: &str) -> Self {
        let mut command = Command::new(BIN);
        command
            .current_dir(root)
            .args(["--root", root.to_str().unwrap(), "proxy", "--port", "0"])
            .args(["--upstream", upstream])
            .args(words(flags))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in words(PROXY_VARS) {
            command.env_remove(name);
        }
        let mut child = command.spawn().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let Some(first) = read_line(&mut stdout) else {
            let output = child.wait_with_output().unwrap();
            panic!("proxy failed: {}", String::from_utf8_lossy(&output.stderr));
        };
        let port = first
            .strip_prefix("justless proxy on 127.0.0.1:")
            .and_then(|rest| rest.split(' ').next())
            .unwrap()
            .parse()
            .unwrap();
        // Keep reading so the banner never blocks on a full pipe.
        thread::spawn(move || {
            let mut rest = String::new();
            let _ = stdout.read_to_string(&mut rest);
        });
        Self {
            child: Some(child),
            port,
        }
    }

    /// Stops the proxy with SIGTERM, which drains and flushes before exiting.
    fn stop(mut self) -> Output {
        let child = self.child.take().unwrap();
        // SAFETY: signalling a child process this test started.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        output
    }

    fn send(&self, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Message {
        send(self.port, method, target, headers, body)
    }

    fn post(&self, headers: &[(&str, &str)], body: &Value) -> Message {
        let mut all = vec![
            ("content-type", "application/json"),
            ("x-api-key", "sk-ant-test"),
        ];
        all.extend_from_slice(headers);
        let body = serde_json::to_vec(body).unwrap();
        self.send("POST", "/v1/messages", &all, &body)
    }

    fn count_tokens(&self, body: &Value) {
        let mut count = body.clone();
        count.as_object_mut().unwrap().remove("stream");
        let json = [("content-type", "application/json")];
        let count = serde_json::to_vec(&count).unwrap();
        self.send("POST", "/v1/messages/count_tokens", &json, &count);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn request_bytes(method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut head =
        format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn connect(port: u16, timeout: u64) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let timeout = Duration::from_secs(timeout);
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream
}

fn send(port: u16, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Message {
    let mut stream = connect(port, 20);
    let request = request_bytes(method, target, headers, body);
    stream.write_all(&request).unwrap();
    read_message(&mut BufReader::new(stream), method == "HEAD").unwrap()
}

// ---------------------------------------------------------------------------
// Fixtures

const TOOLS: [(&str, &str); 3] = [
    ("Read", "file_path"),
    ("Bash", "command"),
    ("Grep", "pattern"),
];

fn tool_input(i: usize) -> Value {
    let (_, field) = TOOLS[i % 3];
    let mut input = Map::new();
    input.insert(field.into(), json!(format!("src/module_{i}.rs")));
    Value::Object(input)
}

fn tool_output(size: usize) -> String {
    let line = "test result line from the tool\n";
    line.repeat(size / line.len() + 1)[..size].to_owned()
}

/// The serialized size of a result's content, as the proxy counts it.
fn result_bytes(size: usize) -> u64 {
    serde_json::to_vec(&json!(tool_output(size))).unwrap().len() as u64
}

/// A Claude Code style conversation with one tool call per result size.
fn conversation(model: &str, results: &[usize]) -> Value {
    let mut messages = vec![json!({"role":"user","content":"Fix the failing test in src/lib.rs"})];
    for (i, &size) in results.iter().enumerate() {
        let id = format!("toolu_{i:02}");
        messages.push(json!({"role":"assistant","content":[
            {"type":"text","text":format!("Step {i}.")},
            {"type":"tool_use","id":id,"name":TOOLS[i % 3].0,"input":tool_input(i)}
        ]}));
        messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":tool_output(size)}]}));
    }
    json!({"model":model,"max_tokens":4096,"stream":true,
        "system":[{"type":"text","text":"You are a coding agent.","cache_control":{"type":"ephemeral"}}],
        "tools":[{"name":"Read","description":"Read a file","input_schema":{"type":"object"}}],
        "messages":messages})
}

/// Four large results the proxy can clear, then two small recent ones.
const SIZES: [usize; 6] = [2000, 2000, 2000, 2000, 40, 40];

/// `SIZES` followed by `more`.
fn grown(more: &[usize]) -> Vec<usize> {
    [&SIZES[..], more].concat()
}

/// The clearing edit the proxy appended, if any.
fn our_edit(body: &Value) -> Option<Value> {
    body["context_management"]["edits"]
        .as_array()?
        .iter()
        .find(|e| e["type"] == "clear_tool_uses_20250919")
        .cloned()
}

fn trigger_keep(edit: &Value) -> (u64, u64) {
    (
        edit["trigger"]["value"].as_u64().unwrap(),
        edit["keep"]["value"].as_u64().unwrap(),
    )
}

fn ledger(root: &Path) -> Vec<Value> {
    let Ok(listing) = fs::read_dir(root.join(".justless/proxy")) else {
        return Vec::new();
    };
    let requests = |p: &PathBuf| {
        p.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("requests-")
    };
    let mut files: Vec<PathBuf> = listing
        .map(|e| e.unwrap().path())
        .filter(requests)
        .collect();
    files.sort();
    let lines: Vec<String> = files
        .iter()
        .map(|f| fs::read_to_string(f).unwrap())
        .collect();
    let records = lines.iter().flat_map(|text| text.lines());
    records
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// One field of every ledger record, in order.
fn column(root: &Path, key: &str) -> Vec<Value> {
    ledger(root).iter().map(|r| r[key].clone()).collect()
}

/// These space-separated fields of `value`.
fn fields(value: &Value, keys: &str) -> Vec<Value> {
    words(keys).iter().map(|key| value[key].clone()).collect()
}

fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[track_caller]
fn contains(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} not in:\n{text}");
}

fn justless(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(root)
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap()
}

fn stats(root: &Path) -> Value {
    serde_json::from_slice(&justless(root, &["stats", "--json"]).stdout).unwrap()
}

const CLEARING: &str = "--floor 1000 --clear-at-least 100 --keep 2 --cold-after 0 --payback 1000";

fn session(
    script: impl FnMut(&Message) -> Reply + Send + 'static,
    flags: &str,
) -> (TempDir, Mock, Proxy) {
    let dir = tempdir().unwrap();
    let mock = Mock::start(script);
    let proxy = Proxy::start(dir.path(), &mock.url(), flags);
    (dir, mock, proxy)
}

// ---------------------------------------------------------------------------
// Relaying

#[test]
fn relays_unplanned_request_byte_for_byte() {
    let reply_body =
        br#"{"id":"msg_01","type":"message","usage":{"input_tokens":5,"output_tokens":2}}"#;
    let reply = move |_: &Message| Reply::Raw {
        status: 201,
        headers: vec![
            ("content-type", "application/json".into()),
            ("request-id", "req_011CVabc".into()),
            ("anthropic-ratelimit-tokens-remaining", "123".into()),
        ],
        body: reply_body.to_vec(),
    };
    let (dir, mock, proxy) = session(reply, "");
    // Below the default 100K floor, so nothing is added. Pretty-printed with odd
    // spacing: the upstream must see these exact bytes.
    let body = serde_json::to_string_pretty(&conversation("claude-opus-5-5", &[300; 3]))
        .unwrap()
        .replace("\"role\": ", "\"role\" :   ");
    let headers = [
        ("Content-Type", "application/json"),
        ("X-Api-Key", "sk-ant-test"),
        ("anthropic-version", "2023-06-01"),
        (
            "anthropic-beta",
            "oauth-2025-04-20, interleaved-thinking-2025-05-14",
        ),
        ("Accept-Encoding", "identity"),
        ("X-Custom", "Value  with  spaces"),
        ("x-claude-code-session-id", "4f0c-session"),
    ];
    let response = proxy.send("POST", "/v1/messages?beta=true", &headers, body.as_bytes());
    assert_eq!(response.status(), 201);
    assert_eq!(response.header("request-id"), Some("req_011CVabc"));
    let remaining = response.header("anthropic-ratelimit-tokens-remaining");
    assert_eq!(remaining, Some("123"));
    assert_eq!(response.body, reply_body);
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let forwarded = &seen[0];
    assert_eq!(forwarded.first, "POST /v1/messages?beta=true HTTP/1.1");
    assert_eq!(forwarded.body, body.as_bytes());
    for (name, value) in headers {
        assert_eq!(forwarded.headers_named(name), [value], "{name}");
    }
    for added in ["user-agent", "accept", "connection"] {
        let headers = &forwarded.headers;
        assert!(forwarded.header(added).is_none(), "{added}: {headers:?}");
    }
    let length = body.len().to_string();
    assert_eq!(forwarded.header("content-length"), Some(length.as_str()));
    proxy.stop();
    let records = ledger(dir.path());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["reason"], "below-floor");
    assert_eq!(records[0]["session"], "4f0c-session");
}

#[test]
fn streams_sse_without_buffering() {
    let (gate_tx, gate_rx) = mpsc::channel();
    let gate = Arc::new(Mutex::new(Some(gate_rx)));
    let (_dir, _mock, proxy) = session(
        move |_| Reply::Events {
            events: events("claude-opus-5-5", None, 0),
            gate: gate.lock().unwrap().take(),
            piece: usize::MAX,
        },
        "",
    );
    let mut stream = connect(proxy.port, 5);
    let body = serde_json::to_vec(&conversation("claude-opus-5-5", &[100])).unwrap();
    let json = [("content-type", "application/json")];
    let request = request_bytes("POST", "/v1/messages", &json, &body);
    stream.write_all(&request).unwrap();
    let started = Instant::now();
    let mut received = Vec::new();
    let mut buffer = [0; 4096];
    while !text(&received).contains("message_start") {
        let read = stream.read(&mut buffer).expect("first event within 5 s");
        assert!(read > 0, "closed before the first event");
        received.extend_from_slice(&buffer[..read]);
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    let head = text(&received);
    contains(&head, "transfer-encoding: chunked");
    assert!(!head.contains("message_stop"));
    gate_tx.send(()).unwrap();
    stream.read_to_end(&mut received).unwrap();
    let all = text(&received);
    contains(&all, "event: ping");
    contains(&all, "message_stop");
    assert!(all.ends_with("0\r\n\r\n"));
}

#[test]
fn observer_handles_split_events() {
    let (dir, _mock, proxy) = session(
        |_| Reply::Events {
            events: events("claude-opus-5-5", Some((7, 4321)), 555),
            gate: None,
            piece: 1,
        },
        "",
    );
    let response = proxy.post(&[], &conversation("claude-opus-5-5", &[100]));
    let expected = events("claude-opus-5-5", Some((7, 4321)), 555).concat();
    assert_eq!(text(&response.body), expected);
    proxy.stop();
    let record = &ledger(dir.path())[0];
    assert_eq!(record["observed"], true);
    let keys = "input cache_read cache_write output api_cleared_uses api_cleared_tokens";
    assert_eq!(fields(record, keys), [100, 2000, 555, 20, 7, 4321]);
}

#[test]
fn accepts_chunked_bodies_and_expect_continue() {
    let (_dir, mock, proxy) = session(|_| ok_json(), "");
    let body = serde_json::to_vec(&conversation("claude-opus-5-5", &[200; 2])).unwrap();
    let mut stream = connect(proxy.port, 10);
    stream
        .write_all(b"POST /v1/messages HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    assert_eq!(read_line(&mut reader).unwrap(), "HTTP/1.1 100 Continue");
    assert_eq!(read_line(&mut reader).unwrap(), "");
    for part in body.chunks(333) {
        let size = format!("{:x}\r\n", part.len());
        stream.write_all(size.as_bytes()).unwrap();
        stream.write_all(part).unwrap();
        stream.write_all(b"\r\n").unwrap();
    }
    stream.write_all(b"0\r\n\r\n").unwrap();
    let response = read_message(&mut reader, false).unwrap();
    assert_eq!(response.status(), 200);
    let forwarded = &mock.seen()[0];
    assert_eq!(forwarded.body, body);
    assert!(forwarded.header("expect").is_none());
    assert!(forwarded.header("transfer-encoding").is_none());
}

#[test]
fn a_body_cut_short_is_never_forwarded() {
    let (_dir, mock, proxy) = session(|_| ok_json(), CLEARING);
    let body = serde_json::to_vec(&conversation("claude-opus-5-5", &[40])).unwrap();
    let mut stream = connect(proxy.port, 20);
    let head = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(&body[..60]).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let response = read_message(&mut BufReader::new(stream), false).unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.header("connection"), Some("close"));
    let error = response.json();
    contains(
        error["error"]["message"].as_str().unwrap(),
        "incomplete request body",
    );
    // A later complete request still goes through.
    proxy.send("GET", "/v1/models", &[], b"");
    let targets: Vec<String> = mock.seen().iter().map(|m| m.target().to_owned()).collect();
    assert_eq!(targets, ["/v1/models"]);
}

#[test]
fn bodies_over_64_mib_keep_their_content_length() {
    let (_dir, mock, proxy) = session(|_| ok_json(), CLEARING);
    let size = 64 * 1024 * 1024 + 1234;
    let body: Vec<u8> = (0..size).map(|i| b"abcdefghij"[i % 10]).collect();
    let octets = [("content-type", "application/octet-stream")];
    let response = proxy.send("POST", "/v1/files", &octets, &body);
    assert_eq!(response.status(), 200);
    let seen = mock.seen();
    let forwarded = seen.last().unwrap();
    let length = size.to_string();
    assert_eq!(forwarded.header("content-length"), Some(length.as_str()));
    assert!(forwarded.header("transfer-encoding").is_none());
    let digest = Sha256::digest(&forwarded.body);
    assert_eq!(digest, Sha256::digest(&body), "body changed");
}

#[test]
fn passthrough_paths_and_invalid_bodies() {
    let (dir, mock, proxy) = session(
        |request| match request.first.split(' ').next().unwrap() {
            "HEAD" => Reply::Raw {
                status: 200,
                headers: vec![("x-hello", "1".into())],
                body: b"hello, client".to_vec(),
            },
            _ => ok_json(),
        },
        CLEARING,
    );
    let head = proxy.send("HEAD", "/api/hello", &[("x-api-key", "k")], b"");
    assert_eq!(head.status(), 200);
    assert_eq!(head.header("content-length"), Some("13"));
    assert_eq!(head.header("x-hello"), Some("1"));
    let models = proxy.send("GET", "/v1/models?x=1", &[("x-api-key", "k")], b"");
    assert_eq!(models.status(), 200);
    let params = conversation("claude-haiku-4-5", &[2000; 6]);
    let batch = json!({"requests":[{"custom_id":"a","params":params}]});
    let batch = serde_json::to_vec(&batch).unwrap();
    proxy.send("POST", "/v1/messages/batches", &[], &batch);
    proxy.send("POST", "/v1/messages", &[], b"not json {");
    let duplicate = br#"{"model":"claude-haiku-4-5","messages":[],"model":"claude-opus-5-5"}"#;
    proxy.send("POST", "/v1/messages", &[], duplicate);
    let large = serde_json::to_vec(&conversation("claude-haiku-4-5", &[2000; 6])).unwrap();
    let browser = [
        ("origin", "https://example.com"),
        ("content-type", "application/json"),
    ];
    proxy.send("POST", "/v1/messages", &browser, &large);
    let seen = mock.seen();
    let targets: Vec<&str> = seen.iter().map(Message::target).collect();
    let messages = "/v1/messages";
    let expected = ["/api/hello", "/v1/models?x=1", "/v1/messages/batches"];
    assert_eq!(targets, [&expected[..], &[messages; 3]].concat());
    assert_eq!(seen[2].body, batch);
    assert_eq!(seen[3].body, b"not json {");
    assert_eq!(seen[4].body, duplicate);
    assert_eq!(seen[5].body, large);
    assert!(seen.iter().all(|m| m.header("anthropic-beta").is_none()));
    proxy.stop();
    assert!(!dir.path().join(".justless/tool-uses").exists());
    let reasons = "other-path other-path other-path unparsed unparsed browser-origin";
    assert_eq!(column(dir.path(), "reason"), words(reasons));
}

#[test]
fn upstream_prefix_and_unreachable() {
    let dir = tempdir().unwrap();
    let mock = Mock::start(|_| ok_json());
    let proxy = Proxy::start(dir.path(), &format!("{}/gw/", mock.url()), "");
    let response = proxy.send("POST", "/v1/messages?beta=true", &[], b"{}");
    assert_eq!(response.status(), 200);
    assert_eq!(mock.seen()[0].target(), "/gw/v1/messages?beta=true");
    drop(proxy);
    // An upstream that closes every connection unanswered. It stays bound, so no
    // other test can take its port.
    let closing = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = closing.local_addr().unwrap();
    thread::spawn(move || {
        for stream in closing.incoming() {
            drop(stream);
        }
    });
    let proxy = Proxy::start(dir.path(), &format!("http://{dead}"), "");
    let response = proxy.send("POST", "/v1/messages", &[], b"{}");
    assert_eq!(response.status(), 502);
    let error = response.json();
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "api_error");
    let message = error["error"]["message"].as_str().unwrap();
    let unreachable = format!("justless proxy could not reach http://{dead}");
    assert!(message.starts_with(&unreachable), "{message}");
    proxy.stop();
    // A proxy never forwards to itself, whether or not the port is free.
    let port = dead.port();
    let itself = format!("proxy --port {port} --upstream http://localhost:{port}");
    let refused = justless(dir.path(), &words(&itself));
    assert_eq!(refused.status.code(), Some(2));
    contains(&text(&refused.stderr), "is this proxy");
    // The upstream URL is printed and logged, so credentials in it are refused.
    let upstream = "https://user:SECRET3@gateway.example/v1";
    let secret = justless(dir.path(), &["proxy", "--upstream", upstream]);
    assert_eq!(secret.status.code(), Some(2));
    let stderr = text(&secret.stderr);
    assert!(stderr.contains("cannot contain credentials") && !stderr.contains("SECRET3"));
}

// ---------------------------------------------------------------------------
// Clearing

#[test]
fn adds_clear_tool_uses_with_stable_boundary() {
    let reply = |_: &Message| stream(events("claude-haiku-4-5", None, 0));
    let (dir, mock, proxy) = session(reply, CLEARING);
    let headers = [("anthropic-beta", "oauth-2025-04-20")];
    let mut sizes = SIZES.to_vec();
    let mut expected = vec![];
    for (step, added) in [vec![], vec![40], vec![40], vec![40, 2000, 2000, 2000]]
        .into_iter()
        .enumerate()
    {
        sizes.extend(added);
        let body = conversation("claude-haiku-4-5", &sizes);
        let bytes = serde_json::to_vec(&body).unwrap();
        let response = proxy.post(&headers, &body);
        assert_eq!(response.status(), 200);
        let forwarded = mock.seen().pop().unwrap();
        // Every byte the client sent is kept; the edit is inserted before the end.
        let end = bytes.len() - 1;
        assert_eq!(forwarded.body[..end], bytes[..end], "step {step}");
        let sent = forwarded.json();
        for key in ["system", "tools", "messages", "model"] {
            assert_eq!(sent[key], body[key]);
        }
        assert_eq!(
            sent["context_management"]["edits"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let edit = our_edit(&sent).unwrap();
        let clear_at_least = json!({"type":"input_tokens","value":100});
        assert_eq!(edit["clear_at_least"], clear_at_least);
        assert!(edit.get("exclude_tools").is_none() && edit.get("clear_tool_inputs").is_none());
        let beta = forwarded.headers_named("anthropic-beta");
        assert_eq!(beta, [format!("oauth-2025-04-20,{BETA}")]);
        let uses = sizes.len() as u64;
        let (trigger, keep) = trigger_keep(&edit);
        assert_eq!(trigger, uses - 3);
        expected.push(uses - keep);
    }
    // The boundary stays at 4 while small results arrive, then moves once.
    assert_eq!(expected, [4, 4, 4, 10]);
    proxy.stop();
    let records = ledger(dir.path());
    let moves = column(dir.path(), "move");
    assert_eq!(
        moves,
        [json!("payback"), Value::Null, Value::Null, json!("payback")]
    );
    assert!(records.iter().all(|r| r["mode"] == "cleared"));
    // Between moves, the edit is still sent and the ledger says why the
    // boundary held.
    let held = json!("below-clear-at-least");
    let reasons = [Value::Null, held.clone(), held, Value::Null];
    assert_eq!(column(dir.path(), "reason"), reasons);
    assert_eq!(records[0]["newly_cleared"]["results"], 4);
    assert_eq!(records[0]["newly_cleared"]["bytes"], 4 * result_bytes(2000));
}

#[test]
fn appends_after_client_clear_thinking() {
    let (_dir, mock, proxy) = session(|_| ok_json(), CLEARING);
    let mut body = conversation("claude-haiku-4-5", &SIZES);
    let thinking = json!({"type":"clear_thinking_20251015","keep":"all"});
    body["context_management"] = json!({"edits":[thinking]});
    let beta = format!("interleaved-thinking-2025-05-14, {BETA}");
    proxy.post(&[("anthropic-beta", &beta)], &body);
    let forwarded = &mock.seen()[0];
    let edits = forwarded.json()["context_management"]["edits"].clone();
    assert_eq!(edits[0], thinking);
    assert_eq!(edits[1]["type"], "clear_tool_uses_20250919");
    assert_eq!(forwarded.headers_named("anthropic-beta"), [beta]);
}

#[test]
fn leaves_client_managed_conversations_alone() {
    let (dir, mock, proxy) = session(|_| ok_json(), CLEARING);
    let mut body = conversation("claude-haiku-4-5", &SIZES);
    body["context_management"] = json!({"edits":[{"type":"clear_tool_uses_20250919","keep":{"type":"tool_uses","value":12}}]});
    proxy.post(&[], &body);
    let later = conversation("claude-haiku-4-5", &grown(&[2000, 2000]));
    proxy.post(&[], &later);
    let seen = mock.seen();
    assert_eq!(seen[0].body, serde_json::to_vec(&body).unwrap());
    assert_eq!(seen[1].body, serde_json::to_vec(&later).unwrap());
    assert!(seen.iter().all(|m| m.header("anthropic-beta").is_none()));
    proxy.stop();
    let records = ledger(dir.path());
    let client = |r: &Value| r["mode"] == "client" && r["reason"] == "client-managed";
    assert!(records.iter().all(client));
    assert_eq!(column(dir.path(), "client_managed_new"), [true, false]);
    assert_eq!(
        stats(dir.path())["proxy"]["client_managed_conversations"],
        1
    );
}

#[test]
fn no_clear_and_no_archive_only_relay_and_record() {
    let flags = format!("{CLEARING} --no-clear --no-archive");
    let (dir, mock, proxy) = session(|_| ok_json(), &flags);
    let body = conversation("claude-haiku-4-5", &SIZES);
    assert_eq!(proxy.post(&[], &body).status(), 200);
    assert_eq!(mock.seen()[0].body, serde_json::to_vec(&body).unwrap());
    proxy.stop();
    assert!(!dir.path().join(".justless/tool-uses").exists());
    let records = ledger(dir.path());
    assert_eq!(
        fields(&records[0], "mode reason archived"),
        [json!("pass"), json!("no-clear"), json!(0)]
    );
}

#[test]
fn passes_threads_compaction_and_other_models_unchanged() {
    let (dir, mock, proxy) = session(|_| ok_json(), CLEARING);
    let mut thread = conversation("claude-haiku-4-5", &SIZES);
    thread["thread"] = json!({"id":"thr_01"});
    let mut compaction = conversation("claude-haiku-4-5", &SIZES);
    compaction["compaction"] = json!({"trigger":{"type":"input_tokens","value":150000}});
    let bodies = [
        thread,
        compaction,
        conversation("claude-3-7-sonnet-20250219", &SIZES),
        conversation("gpt-5.5", &SIZES),
        conversation("claude-haiku-4-5", &[]),
    ];
    for body in &bodies {
        assert_eq!(proxy.post(&[], body).status(), 200);
    }
    for (forwarded, body) in mock.seen().iter().zip(&bodies) {
        assert_eq!(forwarded.body, serde_json::to_vec(body).unwrap());
        assert!(forwarded.header("anthropic-beta").is_none());
    }
    proxy.stop();
    let reasons = "thread compaction-param not-claude not-claude no-tool-uses";
    assert_eq!(column(dir.path(), "reason"), words(reasons));
}

#[test]
fn cold_cache_move() {
    let flags = "--floor 1000 --clear-at-least 100 --keep 2 --cold-after 1 --payback 0";
    let (dir, mock, proxy) = session(|_| ok_json(), flags);
    let sizes = [2000; 6];
    proxy.post(&[], &conversation("claude-haiku-4-5", &sizes));
    assert!(our_edit(&mock.seen()[0].json()).is_none());
    thread::sleep(Duration::from_millis(1200));
    let grown = [&sizes[..], &[40]].concat();
    proxy.post(&[], &conversation("claude-haiku-4-5", &grown));
    let edit = our_edit(&mock.seen()[1].json()).unwrap();
    assert_eq!(trigger_keep(&edit), (4, 2));
    proxy.stop();
    let records = ledger(dir.path());
    // A first sighting counts as warm, and warm moves are off.
    assert_eq!(records[0]["reason"], "below-payback");
    assert_eq!(records[1]["move"], "cold");
    assert_eq!(records[1]["boundary"], 5);
}

#[test]
fn a_model_switch_never_rewrites_another_models_warm_cache() {
    let flags = "--floor 1000 --clear-at-least 100 --keep 2 --payback 0";
    let (dir, mock, proxy) = session(|_| ok_json(), flags);
    let mut sizes = vec![2000; 6];
    for model in ["claude-opus-5-5", "claude-haiku-4-5", "claude-opus-5-5"] {
        proxy.post(&[], &conversation(model, &sizes));
        sizes.push(2000);
    }
    assert!(mock.seen().iter().all(|m| our_edit(&m.json()).is_none()));
    proxy.stop();
    assert_eq!(column(dir.path(), "reason"), ["below-payback"; 3]);
}

#[test]
fn a_token_count_first_does_not_change_the_decision() {
    let flags = "--floor 1000 --clear-at-least 100 --keep 2 --cold-after 1 --payback 0";
    let body = conversation("claude-opus-5-5", &[2000; 8]);
    let mut decisions = Vec::new();
    for count_first in [false, true] {
        let (dir, mock, proxy) = session(|_| ok_json(), flags);
        if count_first {
            proxy.count_tokens(&body);
            thread::sleep(Duration::from_millis(1200));
        }
        proxy.post(&[], &body);
        let forwarded = mock.seen().pop().unwrap();
        assert!(our_edit(&forwarded.json()).is_none(), "{count_first}");
        proxy.stop();
        let last = ledger(dir.path()).pop().unwrap();
        decisions.push(fields(&last, "reason move"));
    }
    assert_eq!(decisions[0], decisions[1]);
    assert_eq!(decisions[0], [json!("below-payback"), Value::Null]);
}

#[test]
fn compaction_and_forks_keep_the_boundary() {
    let (dir, mock, proxy) = session(|_| ok_json(), CLEARING);
    let class = |name| ("x-claude-code-request-class", name);
    proxy.post(&[class("main")], &conversation("claude-haiku-4-5", &SIZES));
    let body = conversation("claude-haiku-4-5", &grown(&[2000; 5]));
    proxy.post(&[class("compaction")], &body);
    let fork = ("x-claude-code-agent-type", "fork");
    proxy.post(&[class("main"), fork], &body);
    proxy.count_tokens(&body);
    proxy.post(&[class("main")], &body);
    let keeps: Vec<(u64, u64)> = mock
        .seen()
        .iter()
        .map(|m| trigger_keep(&our_edit(&m.json()).unwrap()))
        .collect();
    // Boundary 4 until the main conversation moves it to 11 - 2 = 9.
    assert_eq!(keeps, [(3, 2), (8, 7), (8, 7), (8, 7), (8, 2)]);
    proxy.stop();
    let roles = "mover read-only read-only read-only mover";
    assert_eq!(column(dir.path(), "role"), words(roles));
}

#[test]
fn state_survives_restart() {
    let dir = tempdir().unwrap();
    let mock = Mock::start(|_| ok_json());
    let proxy = Proxy::start(dir.path(), &mock.url(), CLEARING);
    proxy.post(&[], &conversation("claude-haiku-4-5", &SIZES));
    proxy.stop();
    let state = fs::read(dir.path().join(".justless/proxy/conversations.json")).unwrap();
    let state: Value = serde_json::from_slice(&state).unwrap();
    let boundaries = &state["conversations"]["toolu_00"]["boundaries"];
    assert_eq!(boundaries, &json!(["toolu_03"]));
    let proxy = Proxy::start(dir.path(), &mock.url(), CLEARING);
    proxy.post(&[], &conversation("claude-haiku-4-5", &grown(&[40])));
    let edit = our_edit(&mock.seen()[1].json()).unwrap();
    assert_eq!(trigger_keep(&edit), (4, 3));
    proxy.stop();
    let records = ledger(dir.path());
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["move"], Value::Null);
    assert_eq!(records[1]["boundary"], 4);
}

/// The proxy retries without its edit only when the API refuses that edit; any
/// other 400 reaches the client as it was, with no retry.
#[test]
fn falls_back_only_when_the_edit_is_rejected() {
    let (dir, mock, proxy) = session(
        |request| {
            if text(&request.body).contains("clear_tool_uses_20250919") {
                Reply::Json(
                    400,
                    json!({"type":"error","error":{"type":"invalid_request_error",
                    "message":"claude-x-1 does not support the 'clear_tool_uses_20250919' context management strategy"}}),
                )
            } else {
                ok_json()
            }
        },
        CLEARING,
    );
    let body = conversation("claude-x-1", &SIZES);
    let response = proxy.post(&[], &body);
    assert_eq!(response.status(), 200);
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert!(our_edit(&seen[0].json()).is_some());
    assert_eq!(seen[1].body, serde_json::to_vec(&body).unwrap());
    assert!(seen[1].header("anthropic-beta").is_none());
    proxy.post(&[], &conversation("claude-x-1", &grown(&[2000])));
    let seen = mock.seen();
    assert_eq!(seen.len(), 3);
    assert!(our_edit(&seen[2].json()).is_none());
    proxy.stop();
    assert_eq!(column(dir.path(), "mode"), ["fallback", "refused"]);
    let log = fs::read_to_string(dir.path().join(".justless/proxy/proxy.log")).unwrap();
    contains(&log, "fallback: claude-x-1 answered 400");

    let error = json!({"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1000001 tokens > 1000000 maximum"}});
    let reply = error.clone();
    let (_dir, mock, proxy) = session(move |_| Reply::Json(400, reply.clone()), CLEARING);
    let response = proxy.post(&[], &conversation("claude-haiku-4-5", &SIZES));
    assert_eq!(response.status(), 400);
    assert_eq!(response.body, serde_json::to_vec(&error).unwrap());
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    assert!(our_edit(&seen[0].json()).is_some());
}

// ---------------------------------------------------------------------------
// Archive, recall and stats

fn results_request(results: Vec<(String, Value, bool)>) -> Value {
    let mut messages = vec![json!({"role":"user","content":"go"})];
    for (i, (id, content, is_error)) in results.into_iter().enumerate() {
        let (name, input) = match i % 3 {
            0 => (
                "Bash",
                json!({"command":format!("cargo test --locked -p crate{i}"),"description":"Run tests"}),
            ),
            1 => ("Read", json!({"file_path":format!("src/capture_{i}.rs")})),
            _ => ("Screenshot", json!({})),
        };
        messages.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":input}]}));
        let mut result = json!({"type":"tool_result","tool_use_id":id,"content":content});
        if is_error {
            result["is_error"] = json!(true);
        }
        messages.push(json!({"role":"user","content":[result]}));
    }
    json!({"model":"claude-opus-5-5","max_tokens":1024,"messages":messages})
}

/// `justless recall` with these space-separated arguments.
fn recall(root: &Path, args: &str) -> Output {
    justless(root, &words(&format!("recall {args}")))
}

#[test]
fn archives_only_accepted_requests_and_recalls_exactly() {
    let mut calls = 0;
    let (dir, _mock, proxy) = session(
        move |_| {
            calls += 1;
            if calls == 1 {
                Reply::Json(
                    401,
                    json!({"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}),
                )
            } else {
                ok_json()
            }
        },
        "",
    );
    let output = "exact \u{1f980} output\r\n  with spacing\n".repeat(50);
    let blocks =
        json!([{"type":"text","text":"first block"},{"type":"text","text":"second block"}]);
    let image = json!([{"type":"text","text":"screen:"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo".repeat(100)}}]);
    let failed = json!("error: test failed");
    let first = vec![
        ("toolu_01Bash".to_string(), json!(output), false),
        ("toolu_02Read".to_string(), blocks.clone(), false),
        ("toolu_03Shot".to_string(), image.clone(), false),
        ("toolu_04Bash".to_string(), failed, true),
    ];
    let status = |results| proxy.post(&[], &results_request(results)).status();
    assert_eq!(status(first.clone()), 401);
    assert_eq!(status(first.clone()), 200);
    thread::sleep(Duration::from_millis(20));
    let mut second = first.clone();
    second[0].1 = json!("changed later by the client");
    second.push(("toolu_05Read".to_string(), json!("newest result"), false));
    status(second);
    proxy.stop();
    let root = dir.path();
    assert_eq!(column(root, "archived"), [0, 4, 1]);
    let index = root.join(".justless/tool-uses");
    let digest = format!("{:x}", Sha256::digest(b"toolu_01Bash"));
    assert!(index.join(format!("{digest}.json")).is_file());
    // The first copy wins, byte for byte.
    let recalled = recall(root, "--tool-use toolu_01Bash");
    assert!(recalled.status.success());
    assert_eq!(recalled.stdout, output.as_bytes());
    let json = |output: Output| serde_json::from_slice::<Value>(&output.stdout).unwrap();
    assert_eq!(json(recall(root, "--tool-use toolu_02Read --json")), blocks);
    let read = recall(root, "--tool-use toolu_02Read").stdout;
    assert_eq!(read, b"first block\nsecond block");
    let shot = text(&recall(root, "--tool-use toolu_03Shot").stdout);
    assert_eq!(shot, "screen:\n[image/png · 1.1 KB base64 · use --json]");
    assert_eq!(json(recall(root, "--tool-use toolu_03Shot --json")), image);
    // Listing: newest first, full IDs, filter and limit.
    let list = text(&recall(root, "--list").stdout);
    let lines: Vec<&str> = list.lines().collect();
    let newest = "toolu_05Read  Read  src/capture_4.rs  15 B  ";
    assert!(lines[0].starts_with(newest), "{list}");
    contains(&list, "toolu_04Bash  Bash  cargo test --locked -p crate3  ");
    let error = |l: &&str| l.starts_with("toolu_04Bash") && l.ends_with("  error");
    assert!(lines.iter().any(error), "{list}");
    let footer = "5 archived tool results. Print one with `justless recall --tool-use ID`.";
    assert_eq!(lines.last().unwrap(), &footer);
    let cargo = text(&recall(root, "--list CARGO --limit 1").stdout);
    let rows: Vec<&str> = cargo.lines().collect();
    assert_eq!(rows.len(), 2, "{cargo}");
    contains(rows[0], "Bash  cargo test --locked -p crate");
    assert_eq!(
        rows[1],
        "2 of 5 archived tool results match \"CARGO\", newest 1 shown. Print one with `justless recall --tool-use ID`."
    );
    // Invalid and unknown IDs.
    for args in [
        "--tool-use ../x",
        "--tool-use toolu_missing",
        "--list x --json",
        "",
        "toolu_missing",
        "../x",
        "toolu_01Bash --json --grep x",
    ] {
        assert_eq!(recall(root, args).status.code(), Some(2), "{args}");
    }
    let unknown = text(&recall(root, "--tool-use toolu_missing").stderr);
    let hint =
        "justless: no archived tool result for toolu_missing; try `justless recall --list TEXT`\n";
    assert_eq!(unknown, hint);
    contains(
        &text(&recall(root, "toolu_missing").stderr),
        "recall --list TEXT",
    );
    // The positional form reaches the proxy's archive too, with the same text.
    let positional = recall(root, "toolu_01Bash");
    assert!(positional.status.success());
    assert_eq!(positional.stdout, output.as_bytes());
    // A line range is exact bytes; --grep prints numbered matching lines.
    let range = recall(root, "toolu_01Bash --from 2 --to 3").stdout;
    assert_eq!(range, b"  with spacing\nexact \xf0\x9f\xa6\x80 output\r\n");
    let grep = text(&recall(root, "--tool-use toolu_01Bash --grep spacing --limit 2").stdout);
    assert_eq!(
        grep,
        "2:   with spacing\n4:   with spacing\n[2 of 50 matching lines shown; --limit 50 shows all]\n"
    );
    let none = recall(root, "toolu_01Bash --grep absent").stdout;
    assert_eq!(none, b"No line matches \"absent\".\n");
    // MCP clients reach the same archive through the recall tool.
    let mut mcp = Command::new(BIN)
        .args(["mcp", "--root", root.to_str().unwrap()])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let mut stdin = mcp.stdin.take().unwrap();
        for line in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recall","arguments":{"id":"toolu_02Read"}}}),
        ] {
            writeln!(stdin, "{line}").unwrap();
        }
    }
    let output = text(&mcp.wait_with_output().unwrap().stdout);
    let reply: Value = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|message| message["id"] == 2)
        .unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let content = &reply["result"]["content"][0]["text"];
    assert_eq!(content, "first block\nsecond block");
}

#[test]
fn list_matches_patterns_and_read_ranges() {
    let (dir, _mock, proxy) = session(|_| ok_json(), "");
    let calls = json!([
        ["toolu_g1", "Grep", {"pattern":"parse_trade","path":"/repo/src","glob":"*.rs"}],
        ["toolu_g2", "Glob", {"pattern":"**/*_test.rs","path":"/repo"}],
        ["toolu_r1", "Read", {"file_path":"/repo/src/lib.rs","offset":1,"limit":50}],
        ["toolu_r2", "Read", {"file_path":"/repo/src/lib.rs","offset":900}],
        ["toolu_w1", "WebSearch", {"query":"rust floor_char_boundary"}]
    ]);
    let mut messages = vec![json!({"role":"user","content":"go"})];
    for call in calls.as_array().unwrap() {
        let (id, name, input) = (&call[0], &call[1], &call[2]);
        messages.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":input}]}));
        messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"result"}]}));
    }
    let body = json!({"model":"claude-opus-5-5","max_tokens":1024,"messages":messages});
    assert_eq!(proxy.post(&[], &body).status(), 200);
    proxy.stop();
    let list = |pattern: &str| text(&recall(dir.path(), &format!("--list {pattern}")).stdout);
    for (pattern, row) in [
        ("parse_trade", "toolu_g1  Grep  parse_trade in /repo/src  "),
        ("_test", "toolu_g2  Glob  **/*_test.rs in /repo  "),
        ("lib.rs", "toolu_r1  Read  /repo/src/lib.rs:1+50  "),
        ("lib.rs", "toolu_r2  Read  /repo/src/lib.rs:900  "),
        (
            "floor_char",
            "toolu_w1  WebSearch  rust floor_char_boundary  ",
        ),
    ] {
        contains(&list(pattern), row);
    }
    // TEXT also matches the rest of the input, such as a Grep glob.
    let by_glob = list("*.rs\"");
    assert!(by_glob.starts_with("toolu_g1  "), "{by_glob}");
}

#[test]
fn records_stats_from_api_usage() {
    let mut replies = vec![
        // The move: the API clears all four calls the proxy planned.
        stream(events("claude-haiku-4-5", Some((4, 2010)), 1500)),
        // The API clears a different number of calls: a mismatch.
        stream(events("claude-haiku-4-5", Some((3, 1500)), 0)),
        Reply::Raw {
            status: 200,
            headers: vec![
                ("content-type", "application/json".into()),
                ("content-encoding", "gzip".into()),
            ],
            body: vec![0x1f, 0x8b, 0, 0],
        },
    ]
    .into_iter();
    let (dir, _mock, proxy) = session(move |_| replies.next().unwrap_or_else(ok_json), CLEARING);
    proxy.post(&[], &conversation("claude-haiku-4-5", &SIZES));
    // The next call repeats cleared call 0 (same tool and input, new ID).
    let mut repeat = conversation("claude-haiku-4-5", &SIZES);
    let messages = repeat["messages"].as_array_mut().unwrap();
    messages.push(json!({"role":"assistant","content":[{"type":"tool_use","id":"toolu_again","name":"Read","input":tool_input(0)}]}));
    messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_again","content":"x"}]}));
    proxy.post(&[], &repeat);
    proxy.post(&[], &repeat);
    proxy.stop();
    let root = dir.path();
    let records = ledger(root);
    let first = fields(&records[0], "move rewrite_api mismatch");
    assert_eq!(first, [json!("payback"), json!(1500), json!(false)]);
    assert_eq!(
        fields(&records[1], "mismatch repeats"),
        [json!(true), json!(1)]
    );
    assert_eq!(records[2]["observed"], false);
    let full = recall(root, "--tool-use toolu_00");
    let part = recall(root, "toolu_01 --from 1 --to 1");
    assert!(full.status.success() && part.status.success());
    let recalled = (full.stdout.len() + part.stdout.len()) as u64;
    assert!(recalled > 0);
    let report = stats(root);
    let proxy_stats = &report["proxy"];
    let cleared = 4 * result_bytes(2000);
    for (key, expected) in [
        ("requests", json!(3)),
        ("cleared_requests", json!(3)),
        ("moves", json!({"cold":0,"payback":1})),
        ("recalled_bytes", json!(recalled)),
        ("net_cleared_bytes", json!(cleared as i64 - recalled as i64)),
        ("warm_rewrite_tokens", json!(1500)),
        ("tokens_not_sent", json!(2010 + 1500)),
        ("mismatches", json!(1)),
        ("repeats", json!(1)),
        ("unobserved", json!(1)),
        ("archived_results", json!(7)),
        ("token_estimate", json!("UTF-8 bytes / 4")),
    ] {
        assert_eq!(proxy_stats[key], expected, "{key}");
    }
    let newly = &proxy_stats["newly_cleared"];
    let expected = [
        json!(4),
        json!(cleared),
        json!(4 * result_bytes(2000).div_ceil(4)),
    ];
    assert_eq!(fields(newly, "results bytes est_tokens"), expected);
    assert_eq!(proxy_stats["usage"]["cache_write"], 1500);
    let summary = text(&justless(root, &["stats"]).stdout);
    for line in [
        "No captured runs yet",
        "Request boundary (justless proxy), last 7 day(s)",
        "3 requests · clearing requested on 3 · 0 conversations left to Claude Code's own clearing · 0 refusals",
        "4 old tool results marked for clearing (8.1 KB, about 2.07K tokens estimated) in 1 clear: 1 on a warm cache (1.5K tokens written to cache on that request, reported by the API)",
        "3.51K input tokens not sent to the model",
        "1 read repeated a cleared call with no file change in between",
        "7 tool results archived",
    ] {
        contains(&summary, line);
    }
    let empty = tempdir().unwrap();
    assert_eq!(stats(empty.path())["proxy"], Value::Null);
}

#[test]
fn never_persists_credentials() {
    let (dir, _mock, proxy) = session(
        |_| stream(events("claude-haiku-4-5", Some((4, 2000)), 10)),
        CLEARING,
    );
    let headers = [
        ("x-api-key", "sk-ant-test-SECRET1"),
        ("authorization", "Bearer SECRET2"),
        ("x-claude-code-session-id", "s1"),
    ];
    let body = conversation("claude-haiku-4-5", &SIZES);
    assert_eq!(proxy.post(&headers, &body).status(), 200);
    proxy.stop();
    fn walk(path: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, files);
            } else {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&dir.path().join(".justless"), &mut files);
    assert!(files.iter().any(|f| f.ends_with("proxy/proxy.log")));
    assert!(files.len() > 5, "{files:?}");
    for file in files {
        let text = text(&fs::read(&file).unwrap());
        let leaked = text.contains("SECRET1") || text.contains("SECRET2");
        assert!(!leaked, "{}", file.display());
    }
}

#[test]
fn concurrent_streams() {
    let gates: Arc<Mutex<Vec<Receiver<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut senders = Vec::new();
    for _ in 0..2 {
        let (tx, rx) = mpsc::channel();
        senders.push(tx);
        gates.lock().unwrap().push(rx);
    }
    let shared = gates.clone();
    let (_dir, _mock, proxy) = session(
        move |_| Reply::Events {
            events: events("claude-opus-5-5", None, 0),
            gate: shared.lock().unwrap().pop(),
            piece: 64,
        },
        "",
    );
    let port = proxy.port;
    let clients: Vec<_> = (0..2)
        .map(|_| {
            thread::spawn(move || {
                let body = serde_json::to_vec(&conversation("claude-opus-5-5", &[50])).unwrap();
                let json = [("content-type", "application/json")];
                send(port, "POST", "/v1/messages", &json, &body)
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(300));
    for tx in senders {
        tx.send(()).unwrap();
    }
    for client in clients {
        let response = client.join().unwrap();
        let expected = events("claude-opus-5-5", None, 0).concat();
        assert_eq!(text(&response.body), expected);
    }
}

#[test]
fn verbose_lines_name_the_decision_without_values() {
    let (_dir, _mock, proxy) = session(
        |_| stream(events("claude-haiku-4-5", Some((4, 2010)), 1500)),
        &format!("{CLEARING} --verbose"),
    );
    let headers = [
        ("x-api-key", "sk-ant-test-SECRET1"),
        ("x-claude-code-request-class", "main"),
    ];
    proxy.post(&headers, &conversation("claude-haiku-4-5", &SIZES));
    let stderr = text(&proxy.stop().stderr);
    let decision = "200 messages claude-haiku-4-5 main · 6 uses · cleared 4 (moved, warm, C/M ";
    assert!(stderr.starts_with(decision), "{stderr}");
    contains(&stderr, "API cleared 4 calls, 2,010 tokens");
    assert!(!stderr.contains("SECRET1"));
}

// ---------------------------------------------------------------------------
// wrap claude

fn fake_claude(dir: &Path, script: &str) -> PathBuf {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let path = bin.join("claude");
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn wrap_command(root: &Path, bin: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command
        .current_dir(root)
        .args(["--root", root.to_str().unwrap(), "wrap"])
        .args(args)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", root.join("home"));
    for name in [
        "CLAUDE_CONFIG_DIR",
        "JUSTLESS_PROXY",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING",
        "CLAUDE_CODE_GATEWAY_HINT_HEADERS",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_MANTLE",
        "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
    ] {
        command.env_remove(name);
    }
    for name in words(PROXY_VARS) {
        command.env_remove(name);
    }
    command
}

/// A fake `claude` that writes each argument as `arg=VALUE`, then the environment
/// variables `pattern` matches, to `root/record`, and exits with `status`.
fn recording_claude(root: &Path, pattern: &str, status: u8) -> (PathBuf, PathBuf) {
    let record = root.join("record");
    let script = format!(
        "{{ for a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done; env | grep -E '{pattern}'; }} > '{}'\nexit {status}",
        record.display()
    );
    (fake_claude(root, &script), record)
}

fn recorded_args(record: &str) -> Vec<&str> {
    record
        .lines()
        .filter_map(|l| l.strip_prefix("arg="))
        .collect()
}

#[test]
fn wrap_launches_claude_with_proxy_env() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let pattern = "^(ANTHROPIC_BASE_URL|JUSTLESS_PROXY|CLAUDE_CODE_)";
    let (bin, record) = recording_claude(root, pattern, 7);
    let mock = Mock::start(|_| ok_json());
    fs::create_dir(root.join(".claude")).unwrap();
    let settings = r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example"}}"#;
    fs::write(root.join(".claude/settings.json"), settings).unwrap();
    let args = [
        "--keep", "5", "claude", "--resume", "abc", "-p", "x y", "--help",
    ];
    let output = wrap_command(root, &bin, &args)
        .env("ANTHROPIC_BASE_URL", mock.url())
        .env("CLAUDE_CODE_GATEWAY_HINT_HEADERS", "0")
        .output()
        .unwrap();
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(7), "{stderr}");
    let recorded = fs::read_to_string(&record).unwrap();
    let expected = ["--resume", "abc", "-p", "x y", "--help"];
    assert_eq!(recorded_args(&recorded), expected);
    let env: Vec<&str> = recorded
        .lines()
        .filter(|l| !l.starts_with("arg="))
        .collect();
    let base = env
        .iter()
        .find_map(|l| l.strip_prefix("ANTHROPIC_BASE_URL="))
        .unwrap();
    let port = base.strip_prefix("http://127.0.0.1:").unwrap();
    let numeric = !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit());
    assert!(numeric, "{base}");
    assert!(env.contains(&"JUSTLESS_PROXY=1"));
    assert!(env.contains(&"CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING=1"));
    // An inherited value is kept.
    assert!(env.contains(&"CLAUDE_CODE_GATEWAY_HINT_HEADERS=0"));
    let canonical = root.canonicalize().unwrap();
    contains(
        &stderr,
        ".claude/settings.json sets ANTHROPIC_BASE_URL; Claude Code may bypass the proxy.",
    );
    let banner = format!(
        " → {} · old tool results cleared from 100K tokens · archive {}/.justless · justless stats shows what changed",
        mock.url(),
        canonical.display()
    );
    contains(&stderr, "justless: Claude Code → 127.0.0.1:");
    contains(&stderr, &banner);
    contains(&stderr, "justless: the proxy received no requests.");
}

#[test]
fn wrap_passes_every_later_argument_and_exempts_loopback_from_proxies() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let (bin, record) = recording_claude(root, "^(NO_PROXY|no_proxy)=", 0);
    let mock = Mock::start(|_| ok_json());
    let recorded = |args: &str, proxied: bool| {
        let args = words(args);
        let mut command = wrap_command(root, &bin, &args);
        command
            .env("ANTHROPIC_BASE_URL", mock.url())
            .env_remove("NO_PROXY")
            .env_remove("no_proxy");
        if proxied {
            command
                .env("HTTPS_PROXY", "http://proxy.corp.example:3128")
                .env("NO_PROXY", "corp.example, .internal")
                .env("no_proxy", "localhost");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        fs::read_to_string(&record).unwrap()
    };
    // `--` and options Justless also has reach Claude Code unparsed.
    let plain = recorded("claude -- -p hi --root x", false);
    assert_eq!(recorded_args(&plain), ["--", "-p", "hi", "--root", "x"]);
    assert!(!plain.contains("NO_PROXY"), "{plain}");
    let proxied = recorded("--keep 4 claude --root x", true);
    assert_eq!(recorded_args(&proxied), ["--root", "x"]);
    let hosts = "corp.example,.internal,localhost,127.0.0.1";
    contains(&proxied, &format!("NO_PROXY={hosts}\n"));
    contains(&proxied, &format!("no_proxy={hosts}\n"));
    let other = wrap_command(root, &bin, &["codex"]).output().unwrap();
    assert_eq!(other.status.code(), Some(2));
    contains(&text(&other.stderr), "wrap launches Claude Code");
}

#[test]
fn wrap_relays_requests_and_summarizes_them() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let body = root.join("body.json");
    let request = conversation("claude-haiku-4-5", &SIZES);
    fs::write(&body, serde_json::to_vec(&request).unwrap()).unwrap();
    // The fake Claude Code sends one request through the proxy with curl, which
    // also exercises `Expect: 100-continue` for a large body.
    if Command::new("curl").arg("--version").output().is_err() {
        eprintln!("skipped: curl is not on PATH");
        return;
    }
    let curl = Command::new("sh")
        .args(["-c", "command -v curl"])
        .output()
        .unwrap();
    let bin = fake_claude(
        root,
        &format!(
            "'{}' -s -o /dev/null -H 'content-type: application/json' --data-binary @'{}' \"$ANTHROPIC_BASE_URL/v1/messages\"",
            text(&curl.stdout).trim(),
            body.display()
        ),
    );
    let mock = Mock::start(|_| stream(events("claude-haiku-4-5", Some((4, 2010)), 1500)));
    let flags = format!("{CLEARING} claude");
    let output = wrap_command(root, &bin, &words(&flags))
        .env("ANTHROPIC_BASE_URL", mock.url())
        .output()
        .unwrap();
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let canonical = root.canonicalize().unwrap();
    let summary = format!(
        "justless: 1 request · 1 clear (0 on an expired cache) · 4 old tool results marked for clearing · 2.01K input tokens not sent to the model · 6 tool results archived in {}/.justless\n",
        canonical.display()
    );
    assert!(stderr.ends_with(&summary), "{stderr}");
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    assert!(our_edit(&seen[0].json()).is_some());
}

#[test]
fn wrap_survives_sigint_until_child_exits() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let ready = root.join("ready");
    let interrupted = root.join("interrupted");
    let bin = fake_claude(
        root,
        &format!(
            "trap 'echo yes > \"{}\"' INT\n: > '{}'\ni=0\nwhile [ $i -lt 30 ] && [ ! -e '{}' ]; do sleep 0.1; i=$((i+1)); done\nsleep 0.3\nexit 0",
            interrupted.display(),
            ready.display(),
            interrupted.display()
        ),
    );
    let mut command = wrap_command(root, &bin, &["claude"]);
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "fake claude never started");
        thread::sleep(Duration::from_millis(20));
    }
    // Let the wrapper ignore terminal signals, then interrupt the whole group the
    // way a terminal does.
    thread::sleep(Duration::from_millis(200));
    // SAFETY: signalling the process group this test created.
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGINT) };
    let output = child.wait_with_output().unwrap();
    assert!(interrupted.exists(), "the child did not see SIGINT");
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    contains(&stderr, "the proxy received no requests");
}

#[test]
fn wrap_refuses_unsupported_setups() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let marker = root.join("ran");
    let bin = fake_claude(root, &format!(": > '{}'", marker.display()));
    let wrap = |name: &str, value: &str| {
        let command = &mut wrap_command(root, &bin, &["claude"]);
        command.env(name, value).output().unwrap()
    };
    for (name, message) in [
        (
            "CLAUDE_CODE_USE_BEDROCK",
            "wrap needs the Claude API, but CLAUDE_CODE_USE_BEDROCK is set. Run claude directly.",
        ),
        ("JUSTLESS_PROXY", "already runs through a Justless proxy"),
    ] {
        let output = wrap(name, "1");
        assert_eq!(output.status.code(), Some(2), "{name}");
        contains(&text(&output.stderr), message);
        assert!(!marker.exists(), "{name}");
    }
    // A falsy provider variable does not refuse.
    assert!(wrap("CLAUDE_CODE_USE_VERTEX", "0").status.success());
    assert!(marker.exists());
    fs::remove_file(&marker).unwrap();
    let empty = root.join("empty");
    fs::create_dir(&empty).unwrap();
    let missing = wrap_command(root, &empty, &["claude"]).output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
    contains(&text(&missing.stderr), "claude is not on PATH");
    let betas = wrap("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1");
    assert!(betas.status.success());
    contains(
        &text(&betas.stderr),
        "archive only, because CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS is set",
    );
}

// ---------------------------------------------------------------------------
// Live-zone prefix-cache-safe newest-result compaction

fn extract_recall_id(compacted: &str) -> String {
    let footer = compacted.lines().last().expect("footer line");
    let rest = footer
        .strip_prefix("[justless recall ")
        .unwrap_or_else(|| panic!("missing recall prefix in {footer:?}"));
    let (id, _) = rest
        .split_once(" · compacted ")
        .unwrap_or_else(|| panic!("missing compacted metadata in {footer:?}"));
    assert_eq!(id.len(), 64, "expected 64-hex observation id, got {id:?}");
    id.to_owned()
}

fn live_json_payload() -> String {
    let rows: Vec<Value> = (0..180)
        .map(|i| {
            json!({
                "id": i,
                "service": "orders-api",
                "region": "us-east-1",
                "environment": "production",
                "status": if i == 90 { "degraded" } else { "healthy" },
                "latency_ms": 14 + (i % 45),
                "version": "3.2.1"
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap()
}

fn live_structured_log_payload() -> String {
    let mut out = String::new();
    for i in 0..220 {
        if i == 115 {
            out.push_str(
                "api-1 | 2026-11-25T10:01:55Z ERROR database connection refused host=pg-primary:5432 err=ECONNREFUSED\n",
            );
        } else {
            out.push_str(&format!(
                "api-1 | 2026-11-25T10:{:02}:{:02}Z INFO GET /v1/healthz status=200 request_id=req-{i:05} latency={}ms\n",
                (i / 60) % 60,
                i % 60,
                4 + (i % 6)
            ));
        }
    }
    out
}

fn live_ascii_table_payload() -> String {
    let mut out = String::from(
        "+----+----------------+-------------+-----------+----------+----------+\n\
         | id | service        | region      | env       | status   | replicas |\n\
         +----+----------------+-------------+-----------+----------+----------+\n",
    );
    for i in 0..160 {
        out.push_str(&format!(
            "| {:<2} | worker-{:<7} | us-east-1   | prod      | running  | 3        |\n",
            i, i
        ));
    }
    out.push_str("+----+----------------+-------------+-----------+----------+----------+\n");
    out
}

fn live_traceback_payload(root: &Path) -> String {
    let mut out = String::from("Traceback (most recent call last):\n");
    out.push_str(&format!(
        "  File \"{}/src/app.py\", line 42, in handle_request\n    return dispatch(payload)\n",
        root.display()
    ));
    for i in 0..95 {
        out.push_str(&format!(
            "  File \"/usr/lib/python3.12/site-packages/starlette/middleware/layer_{i:02}.py\", line {}, in __call__\n    await self.app(scope, receive, send)\n",
            100 + i
        ));
    }
    out.push_str(&format!(
        "  File \"{}/src/billing.py\", line 118, in charge_invoice\n    raise ValueError('negative invoice total')\n",
        root.display()
    ));
    out.push_str("ValueError: negative invoice total\n");
    out
}

fn multi_turn_live_request(older_big: &str, newest_id: &str, newest_payload: &str) -> Value {
    json!({
        "model": "claude-opus-5-5",
        "max_tokens": 4096,
        "system": [{"type": "text", "text": "You are a coding agent.", "cache_control": {"type": "ephemeral"}}],
        "tools": [{"name": "Bash", "description": "Run command", "input_schema": {"type": "object"}}],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Diagnose the failing service.", "cache_control": {"type": "ephemeral"}}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "First inspect the older run output.", "signature": "sig_thinking_preserved_01"},
                {"type": "redacted_thinking", "data": "opaque_redacted_sig_blob"},
                {"type": "tool_use", "id": "toolu_older_01", "name": "Bash", "input": {"command": "cat older.log"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_older_01", "content": older_big, "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Now fetch the newest diagnostic payload.", "signature": "sig_thinking_preserved_02"},
                {"type": "tool_use", "id": newest_id, "name": "Bash", "input": {"command": format!("fetch {newest_id}")}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": newest_id, "content": newest_payload}
            ]}
        ]
    })
}

#[test]
fn live_zone_compacts_json_log_table_and_traceback_with_exact_recall_and_untouched_prefix() {
    let (dir, mock, proxy) = session(|_| ok_json(), "--compact-live");
    let root = dir.path();
    let older_big = live_structured_log_payload();
    let cases = [
        ("json", "toolu_live_json", live_json_payload()),
        ("log", "toolu_live_log", live_structured_log_payload()),
        ("table", "toolu_live_table", live_ascii_table_payload()),
        ("traceback", "toolu_live_tb", live_traceback_payload(root)),
    ];

    for (kind, tool_id, payload) in &cases {
        assert!(payload.len() > 8192, "{kind} must exceed 8 KB");
        let body = multi_turn_live_request(&older_big, tool_id, payload);
        // Pretty-print with odd spacing so any unintended re-serialization fails loudly.
        let raw_text = serde_json::to_string_pretty(&body)
            .unwrap()
            .replace("\"role\": ", "\"role\" :   ");
        let headers = [
            ("content-type", "application/json"),
            ("x-api-key", "sk-ant-test"),
        ];
        let response = proxy.send("POST", "/v1/messages", &headers, raw_text.as_bytes());
        assert_eq!(response.status(), 200, "{kind}");

        let forwarded = mock.seen().pop().unwrap();
        // Live-zone compaction alone must NOT inject Anthropic's context-management beta.
        assert!(
            forwarded.header("anthropic-beta").is_none(),
            "{kind} unexpectedly added anthropic-beta"
        );

        // Every byte before the final tool_result's content value (system, tools,
        // cache_control breakpoints, thinking & redacted_thinking blocks, older 22 KB
        // tool_result) MUST be 100% byte-for-byte identical.
        let marker = format!("\"tool_use_id\": \"{tool_id}\"");
        let prefix_end = raw_text.find(&marker).expect("final tool_use_id marker") + marker.len();
        assert_eq!(
            &forwarded.body[..prefix_end],
            &raw_text.as_bytes()[..prefix_end],
            "{kind}: cached prefix or thinking blocks were modified!"
        );

        let sent = forwarded.json();
        // Older messages and thinking blocks are untouched.
        for idx in 0..4 {
            assert_eq!(
                sent["messages"][idx], body["messages"][idx],
                "{kind} msg {idx}"
            );
        }
        let compacted = sent["messages"][4]["content"][0]["content"]
            .as_str()
            .unwrap();
        let reduction_pct = (payload.len() - compacted.len()) * 100 / payload.len();
        assert!(
            reduction_pct >= 70,
            "{kind}: expected >=70% reduction, got {reduction_pct}% ({} -> {})",
            payload.len(),
            compacted.len()
        );
        assert!(
            compacted.len() <= 4096,
            "{kind}: compacted {} B exceeded 4096 B budget",
            compacted.len()
        );

        // Semantic checks for structured compactors.
        if *kind == "log" {
            contains(compacted, "ECONNREFUSED");
        }
        if *kind == "traceback" {
            contains(compacted, "charge_invoice");
            contains(compacted, "ValueError: negative invoice total");
        }

        // Exact byte-for-byte recovery via `justless recall <id>`.
        let recall_id = extract_recall_id(compacted);
        let recalled = recall(root, &recall_id);
        assert!(
            recalled.status.success(),
            "{kind}: {}",
            text(&recalled.stderr)
        );
        assert_eq!(
            recalled.stdout,
            payload.as_bytes(),
            "{kind}: recalled bytes did not match original!"
        );
    }

    proxy.stop();

    // Exact recovery via `justless recall --tool-use <tool_use_id>` also keeps uncompacted original.
    let via_tool_use = recall(root, "--tool-use toolu_live_json");
    assert!(via_tool_use.status.success());
    assert_eq!(via_tool_use.stdout, cases[0].2.as_bytes());

    // Ledger and stats record live-zone savings cleanly without breaking existing schema.
    let records = ledger(root);
    assert_eq!(records.len(), 4);
    let mut total_saved = 0u64;
    for (r, (kind, _, payload)) in records.iter().zip(&cases) {
        assert_eq!(r["mode"], "compact", "{kind}");
        assert_eq!(r["live_compacted"]["results"], 1, "{kind}");
        let before = r["live_compacted"]["bytes_before"].as_u64().unwrap();
        let after = r["live_compacted"]["bytes_after"].as_u64().unwrap();
        let saved = r["live_compacted"]["saved_bytes"].as_u64().unwrap();
        assert_eq!(before, payload.len() as u64, "{kind}");
        assert_eq!(saved, before - after, "{kind}");
        assert!(
            saved * 100 / before >= 70,
            "{kind}: expected >=70% saved in ledger, got {saved}/{before}"
        );
        total_saved += saved;
    }
    let report = stats(root);
    assert_eq!(report["proxy"]["live_compacted"]["results"], 4);
    assert_eq!(
        report["proxy"]["live_compacted"]["saved_bytes"],
        total_saved
    );
    assert!(total_saved > 50_000, "total_saved={total_saved}");
    let summary = text(&justless(root, &["stats"]).stdout);
    contains(
        &summary,
        "4 newest tool results compacted in the uncached live zone",
    );
}

#[test]
fn live_zone_leaves_small_already_compacted_cache_control_and_non_text_untouched() {
    let (dir, mock, proxy) = session(|_| ok_json(), "--compact-live");
    let big = live_json_payload();
    let already_compacted = format!("[justless recall {} · prior]\n{}", "a".repeat(64), big);
    let image_data = "iVBORw0KGgo".repeat(1200);

    let untouched_bodies = [
        // 1. Small result (< 8192 B)
        multi_turn_live_request("small older", "toolu_small", &"line\n".repeat(800)),
        // 2. Already starting with `[justless `
        multi_turn_live_request("small older", "toolu_already", &already_compacted),
        // 3. `cache_control` set directly on the final tool_result
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 1024,
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_cc", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_cc", "content": big, "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        }),
        // 4. `cache_control` on a trailing sibling block in the same final user turn
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 1024,
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_trail_cc", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_trail_cc", "content": big},
                    {"type": "text", "text": "checkpoint", "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        }),
        // 5. Image & document blocks only (non-text)
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 1024,
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_img", "name": "Screenshot", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_img", "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": image_data}},
                        {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQ=".repeat(1200)}}
                    ]}
                ]}
            ]
        }),
        // 6. Final message is assistant (never touch!)
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_old", "content": big}]},
                {"role": "assistant", "content": [{"type": "text", "text": "prefill"}]}
            ]
        }),
    ];

    for (idx, body) in untouched_bodies.iter().enumerate() {
        let bytes = serde_json::to_vec(body).unwrap();
        let headers = [("content-type", "application/json")];
        let resp = proxy.send("POST", "/v1/messages", &headers, &bytes);
        assert_eq!(resp.status(), 200, "case {idx}");
        let forwarded = mock.seen().pop().unwrap();
        assert_eq!(
            forwarded.body, bytes,
            "case {idx} must stay byte-for-byte untouched"
        );
    }

    // Also verify `/v1/messages/count_tokens` stays untouched.
    let large_req = multi_turn_live_request("ok", "toolu_cnt", &big);
    proxy.count_tokens(&large_req);
    let forwarded_count = mock.seen().pop().unwrap();
    let mut expected_count = large_req.clone();
    expected_count.as_object_mut().unwrap().remove("stream");
    assert_eq!(forwarded_count.json(), expected_count);

    proxy.stop();
    let records = ledger(dir.path());
    assert!(records.iter().all(|r| r.get("live_compacted").is_none()));

    // Verify `--no-archive` refuses to mutate even with `--compact-live`.
    let (dir_no_arch, mock_no_arch, proxy_no_arch) =
        session(|_| ok_json(), "--compact-live --no-archive");
    let raw = serde_json::to_vec(&large_req).unwrap();
    let headers = [("content-type", "application/json")];
    assert_eq!(
        proxy_no_arch
            .send("POST", "/v1/messages", &headers, &raw)
            .status(),
        200
    );
    assert_eq!(mock_no_arch.seen()[0].body, raw);
    proxy_no_arch.stop();
    assert!(!dir_no_arch.path().join(".justless/observations").exists());
    assert!(!dir_no_arch.path().join(".justless/tool-uses").exists());
}

#[test]
fn live_zone_env_var_min_bytes_and_clearing_coexist_cleanly() {
    // 1. JUSTLESS_PROXY_COMPACT_LIVE=1 + --compact-live-min-bytes 2048
    let dir = tempdir().unwrap();
    let mock = Mock::start(|_| ok_json());
    let mut cmd = Command::new(BIN);
    cmd.current_dir(dir.path())
        .args([
            "--root",
            dir.path().to_str().unwrap(),
            "proxy",
            "--port",
            "0",
            "--upstream",
            &mock.url(),
            "--compact-live-min-bytes",
            "2048",
        ])
        .env("JUSTLESS_PROXY_COMPACT_LIVE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in words(PROXY_VARS) {
        cmd.env_remove(name);
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let first = read_line(&mut stdout).unwrap();
    let port: u16 = first
        .strip_prefix("justless proxy on 127.0.0.1:")
        .and_then(|rest| rest.split(' ').next())
        .unwrap()
        .parse()
        .unwrap();
    thread::spawn(move || {
        let mut rest = String::new();
        let _ = stdout.read_to_string(&mut rest);
    });
    let proxy = Proxy {
        child: Some(child),
        port,
    };

    // 4 KB structured log (below default 8 KB, above custom 2048 B threshold)
    let mut mid_log = String::new();
    for i in 0..50 {
        mid_log.push_str(&format!(
            "api-1 | 2026-11-25T10:00:{:02}Z INFO GET /v1/items status=200 request_id=req-{i:04} latency=5ms\n",
            i % 60
        ));
    }
    assert!(mid_log.len() > 2048 && mid_log.len() < 8192);
    let req = multi_turn_live_request("ok", "toolu_env", &mid_log);
    assert_eq!(proxy.post(&[], &req).status(), 200);
    let forwarded = mock.seen().pop().unwrap().json();
    let compacted = forwarded["messages"][4]["content"][0]["content"]
        .as_str()
        .unwrap();
    let recall_id = extract_recall_id(compacted);
    assert!(compacted.len() * 10 <= mid_log.len() * 7);
    assert_eq!(recall(dir.path(), &recall_id).stdout, mid_log.as_bytes());
    proxy.stop();

    // 2. Coexistence of --compact-live and clear_tool_uses_20250919 on one request.
    let flags = format!("{CLEARING} --compact-live");
    let (dir, mock, proxy) = session(
        |_| stream(events("claude-haiku-4-5", Some((4, 2010)), 1500)),
        &flags,
    );
    let mut body = conversation("claude-haiku-4-5", &SIZES);
    let big_json = live_json_payload();
    body["messages"].as_array_mut().unwrap().last_mut().unwrap()["content"][0]["content"] =
        json!(big_json);
    let raw_bytes = serde_json::to_vec(&body).unwrap();
    let response = proxy.send(
        "POST",
        "/v1/messages",
        &[("content-type", "application/json")],
        &raw_bytes,
    );
    assert_eq!(response.status(), 200);
    let forwarded = mock.seen().pop().unwrap();
    assert_eq!(forwarded.headers_named("anthropic-beta"), [BETA]);
    let sent = forwarded.json();
    assert!(our_edit(&sent).is_some());
    // Every older message before the final user message is untouched.
    let orig_msgs = body["messages"].as_array().unwrap();
    let sent_msgs = sent["messages"].as_array().unwrap();
    assert_eq!(orig_msgs.len(), sent_msgs.len());
    for i in 0..orig_msgs.len() - 1 {
        assert_eq!(sent_msgs[i], orig_msgs[i], "msg {i}");
    }
    let compacted = sent_msgs.last().unwrap()["content"][0]["content"]
        .as_str()
        .unwrap();
    let recall_id = extract_recall_id(compacted);
    assert_eq!(recall(dir.path(), &recall_id).stdout, big_json.as_bytes());
    proxy.stop();
    let rec = &ledger(dir.path())[0];
    assert_eq!(rec["mode"], "cleared");
    assert_eq!(rec["live_compacted"]["results"], 1);
}

#[test]
fn live_zone_openai_chat_and_responses_compact_preserve_prefix_and_recall() {
    let openai_ok = |_: &Message| {
        Reply::Json(
            200,
            json!({"id": "resp_01", "model": "gpt-5.4", "choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
    };
    let (dir, mock, proxy) = session(openai_ok, "--compact-live");
    let root = dir.path();
    let big_json = live_json_payload();
    let big_log = live_structured_log_payload();
    let big_table = {
        let mut out = String::from(
            "+-----+----------------+-------------+-----------+----------+----------+\n\
             | id  | service        | region      | env       | status   | replicas |\n\
             +-----+----------------+-------------+-----------+----------+----------+\n",
        );
        for i in 0..320 {
            out.push_str(&format!(
                "| {:<3} | worker-{:<7} | us-east-1   | prod      | running  | 3        |\n",
                i, i
            ));
        }
        out.push_str("+-----+----------------+-------------+-----------+----------+----------+\n");
        out
    };
    let big_tb = live_traceback_payload(root);
    let headers = [
        ("content-type", "application/json"),
        ("authorization", "Bearer sk-openai-test"),
    ];

    // 1. OpenAI Chat Completions (`/v1/chat/completions`, trailing `role: "tool"`).
    let chat_body = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "You are a coding agent."},
            {"role": "user", "content": "Diagnose orders-api."},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_prev", "type": "function", "function": {"name": "bash", "arguments": "{\"cmd\":\"cat old.log\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_prev", "content": big_log},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_json", "type": "function", "function": {"name": "bash", "arguments": "{\"cmd\":\"curl /orders\"}"}},
                {"id": "call_log", "type": "function", "function": {"name": "bash", "arguments": "{\"cmd\":\"tail api.log\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_json", "content": big_json},
            {"role": "tool", "tool_call_id": "call_log", "content": big_log}
        ]
    });
    let chat_raw = serde_json::to_string_pretty(&chat_body)
        .unwrap()
        .replace("\"role\": ", "\"role\" :   ");
    let resp = proxy.send(
        "POST",
        "/v1/chat/completions",
        &headers,
        chat_raw.as_bytes(),
    );
    assert_eq!(resp.status(), 200);
    let forwarded_chat = mock.seen().pop().unwrap();
    assert!(forwarded_chat.header("anthropic-beta").is_none());
    let chat_prefix_end = chat_raw.rfind("\"tool_call_id\": \"call_json\"").unwrap();
    assert_eq!(
        &forwarded_chat.body[..chat_prefix_end],
        &chat_raw.as_bytes()[..chat_prefix_end],
        "earlier assistant and tool prefix must stay byte-for-byte identical"
    );
    let sent_chat = forwarded_chat.json();
    assert!(sent_chat.get("context_management").is_none());
    for i in 0..5 {
        assert_eq!(sent_chat["messages"][i], chat_body["messages"][i]);
    }
    for (idx, orig) in [(5usize, &big_json), (6usize, &big_log)] {
        let compacted = sent_chat["messages"][idx]["content"].as_str().unwrap();
        let pct = (orig.len() - compacted.len()) * 100 / orig.len();
        assert!(
            pct > 85,
            "chat tool {idx}: {pct}% ({} -> {})",
            orig.len(),
            compacted.len()
        );
        assert!(compacted.len() <= 4096);
        let id = extract_recall_id(compacted);
        let recalled = recall(root, &id);
        assert!(recalled.status.success(), "{}", text(&recalled.stderr));
        assert_eq!(recalled.stdout, orig.as_bytes());
    }
    contains(
        sent_chat["messages"][6]["content"].as_str().unwrap(),
        "ECONNREFUSED",
    );

    // 2. OpenAI Responses API (`/v1/responses`, trailing `type: "function_call_output"`).
    let resp_body = json!({
        "model": "gpt-5.4",
        "instructions": "You are Codex.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Fix billing crash."}]},
            {"type": "reasoning", "id": "rs_01", "encrypted_content": "opaque_sig_blob_01"},
            {"type": "function_call", "call_id": "call_prev", "name": "shell", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_prev", "output": big_table},
            {"type": "function_call", "call_id": "call_tbl", "name": "shell", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_tb", "name": "shell", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_tbl", "output": big_table},
            {"type": "function_call_output", "call_id": "call_tb", "output": big_tb}
        ]
    });
    let resp_raw = serde_json::to_string_pretty(&resp_body)
        .unwrap()
        .replace("\"type\": ", "\"type\" :   ");
    let resp = proxy.send("POST", "/v1/responses", &headers, resp_raw.as_bytes());
    assert_eq!(resp.status(), 200);
    let forwarded_resp = mock.seen().pop().unwrap();
    assert!(forwarded_resp.header("anthropic-beta").is_none());
    let resp_prefix_end = resp_raw.rfind("\"call_id\": \"call_tbl\"").unwrap();
    assert_eq!(
        &forwarded_resp.body[..resp_prefix_end],
        &resp_raw.as_bytes()[..resp_prefix_end],
        "earlier responses prefix must stay byte-for-byte identical"
    );
    let sent_resp = forwarded_resp.json();
    assert!(sent_resp.get("context_management").is_none());
    for i in 0..6 {
        assert_eq!(sent_resp["input"][i], resp_body["input"][i]);
    }
    for (idx, orig) in [(6usize, &big_table), (7usize, &big_tb)] {
        let compacted = sent_resp["input"][idx]["output"].as_str().unwrap();
        let pct = (orig.len() - compacted.len()) * 100 / orig.len();
        assert!(
            pct > 80,
            "responses output {idx}: {pct}% ({} -> {})",
            orig.len(),
            compacted.len()
        );
        assert!(compacted.len() <= 4096);
        let id = extract_recall_id(compacted);
        let recalled = recall(root, &id);
        assert!(recalled.status.success(), "{}", text(&recalled.stderr));
        assert_eq!(recalled.stdout, orig.as_bytes());
    }
    contains(
        sent_resp["input"][7]["output"].as_str().unwrap(),
        "ValueError: negative invoice total",
    );

    proxy.stop();
    let records = ledger(root);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["path"], "chat_completions");
    assert_eq!(records[0]["mode"], "compact");
    assert_eq!(records[0]["model"], "gpt-4o");
    assert_eq!(records[0]["live_compacted"]["results"], 2);
    assert_eq!(records[1]["path"], "responses");
    assert_eq!(records[1]["mode"], "compact");
    assert_eq!(records[1]["model"], "gpt-5.4");
    assert_eq!(records[1]["live_compacted"]["results"], 2);
    let report = stats(root);
    assert_eq!(report["proxy"]["live_compacted"]["results"], 4);
    assert!(
        report["proxy"]["live_compacted"]["saved_bytes"]
            .as_u64()
            .unwrap()
            > 70_000
    );
}

#[test]
fn openai_chat_and_responses_pass_through_verbatim_without_compact_live() {
    let (dir, mock, proxy) = session(|_| ok_json(), "");
    let big_json = live_json_payload();
    let chat_raw = serde_json::to_string_pretty(&json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": big_json}
        ]
    }))
    .unwrap()
    .replace("\"role\": ", "\"role\" :   ");
    let resp_raw = serde_json::to_string_pretty(&json!({
        "model": "gpt-5.4",
        "input": [
            {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": big_json}
        ]
    }))
    .unwrap()
    .replace("\"type\": ", "\"type\" :   ");
    let headers = [
        ("content-type", "application/json"),
        ("authorization", "Bearer sk-openai-test"),
    ];

    assert_eq!(
        proxy
            .send(
                "POST",
                "/v1/chat/completions",
                &headers,
                chat_raw.as_bytes()
            )
            .status(),
        200
    );
    assert_eq!(
        proxy
            .send("POST", "/v1/responses", &headers, resp_raw.as_bytes())
            .status(),
        200
    );
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, chat_raw.as_bytes());
    assert_eq!(seen[1].body, resp_raw.as_bytes());
    assert!(seen.iter().all(|m| m.header("anthropic-beta").is_none()));
    proxy.stop();
    let records = ledger(dir.path());
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|r| r["mode"] == "pass" && r.get("live_compacted").is_none())
    );
    assert!(!dir.path().join(".justless/observations").exists());
}

#[test]
fn live_zone_gemini_stream_generate_content_compact_recall_and_passthrough() {
    let gemini_sse = |_: &Message| {
        stream(vec![
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Found \"}]}}]}\n\n"
                .into(),
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ECONNREFUSED.\"}]},\"finishReason\":\"STOP\"}]}\n\n"
                .into(),
        ])
    };
    let big_json = live_json_payload();
    let big_log = live_structured_log_payload();
    let req_body = json!({
        "systemInstruction": {"parts": [{"text": "You are a coding agent."}]},
        "cachedContent": "cachedContents/gemini-ctx-01",
        "tools": [{"functionDeclarations": [{"name": "run_shell", "description": "Run shell"}]}],
        "contents": [
            {"role": "user", "parts": [{"text": "Investigate orders-api."}]},
            {"role": "model", "parts": [
                {"thoughtSignature": "sig_prev", "functionCall": {"name": "run_shell", "args": {"command": "cat prev.log"}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "run_shell", "response": {"output": big_log}}}
            ]},
            {"role": "model", "parts": [
                {"thoughtSignature": "sig_curr", "functionCall": {"name": "get_orders", "args": {}}},
                {"functionCall": {"name": "run_shell", "args": {"command": "tail -n 220 api.log"}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "get_orders", "response": {"output": big_json, "exitCode": 0}}},
                {"functionResponse": {"name": "run_shell", "response": {"output": big_log}}}
            ]}
        ]
    });
    let req_raw = serde_json::to_string_pretty(&req_body)
        .unwrap()
        .replace("\"role\": ", "\"role\" :   ");
    let headers = [
        ("content-type", "application/json"),
        ("x-goog-api-key", "AIza-test-key"),
    ];
    let target = "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse";

    // 1. With `--compact-live`: compacts final functionResponse outputs, preserves prefix, recalls verbatim.
    let (dir, mock, proxy) = session(gemini_sse, "--compact-live");
    let root = dir.path();
    let resp = proxy.send("POST", target, &headers, req_raw.as_bytes());
    assert_eq!(resp.status(), 200);
    contains(&text(&resp.body), "ECONNREFUSED.");
    let forwarded = mock.seen().pop().unwrap();
    assert_eq!(forwarded.target(), target);
    assert!(forwarded.header("anthropic-beta").is_none());
    let prefix_end = req_raw.rfind("\"get_orders\"").unwrap();
    assert_eq!(
        &forwarded.body[..prefix_end],
        &req_raw.as_bytes()[..prefix_end],
        "Gemini prefix (systemInstruction, cachedContent, earlier turns) must stay byte-for-byte identical"
    );
    let sent = forwarded.json();
    assert!(sent.get("context_management").is_none());
    assert_eq!(sent["systemInstruction"], req_body["systemInstruction"]);
    assert_eq!(sent["cachedContent"], req_body["cachedContent"]);
    for i in 0..4 {
        assert_eq!(sent["contents"][i], req_body["contents"][i], "turn {i}");
    }
    for (part_idx, orig) in [(0usize, &big_json), (1usize, &big_log)] {
        let compacted =
            sent["contents"][4]["parts"][part_idx]["functionResponse"]["response"]["output"]
                .as_str()
                .unwrap();
        let pct = (orig.len() - compacted.len()) * 100 / orig.len();
        assert!(
            pct > 85,
            "gemini part {part_idx}: {pct}% ({} -> {})",
            orig.len(),
            compacted.len()
        );
        assert!(compacted.len() <= 4096);
        let id = extract_recall_id(compacted);
        let recalled = recall(root, &id);
        assert!(recalled.status.success(), "{}", text(&recalled.stderr));
        assert_eq!(recalled.stdout, orig.as_bytes());
    }
    contains(
        sent["contents"][4]["parts"][1]["functionResponse"]["response"]["output"]
            .as_str()
            .unwrap(),
        "ECONNREFUSED",
    );
    // Also exercise `/v1/models/<model>:generateContent` (non-streaming).
    let non_stream_target = "/v1/models/gemini-2.5-flash:generateContent";
    let resp_v1 = proxy.send("POST", non_stream_target, &headers, req_raw.as_bytes());
    assert_eq!(resp_v1.status(), 200);
    assert!(mock.seen().pop().unwrap().body.len() < req_raw.len());
    proxy.stop();
    let records = ledger(root);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["path"], "gemini");
    assert_eq!(records[0]["model"], "gemini-2.5-pro");
    assert_eq!(records[0]["mode"], "compact");
    assert_eq!(records[0]["live_compacted"]["results"], 2);
    assert_eq!(records[1]["path"], "gemini");
    assert_eq!(records[1]["model"], "gemini-2.5-flash");
    assert_eq!(records[1]["mode"], "compact");
    assert_eq!(records[1]["live_compacted"]["results"], 2);
    let report = stats(root);
    assert_eq!(report["proxy"]["live_compacted"]["results"], 4);
    assert!(
        report["proxy"]["live_compacted"]["saved_bytes"]
            .as_u64()
            .unwrap()
            > 80_000
    );

    // 2. Without `--compact-live`: 100% byte-for-byte verbatim passthrough, no observations archived.
    let (dir_off, mock_off, proxy_off) = session(gemini_sse, "");
    let resp_off = proxy_off.send("POST", target, &headers, req_raw.as_bytes());
    assert_eq!(resp_off.status(), 200);
    let forwarded_off = mock_off.seen().pop().unwrap();
    assert_eq!(forwarded_off.target(), target);
    assert_eq!(forwarded_off.body, req_raw.as_bytes());
    assert!(forwarded_off.header("anthropic-beta").is_none());
    proxy_off.stop();
    let records_off = ledger(dir_off.path());
    assert_eq!(records_off.len(), 1);
    assert_eq!(records_off[0]["path"], "gemini");
    assert_eq!(records_off[0]["model"], "gemini-2.5-pro");
    assert_eq!(records_off[0]["mode"], "pass");
    assert_eq!(records_off[0]["reason"], "compact-live-off");
    assert!(records_off[0].get("live_compacted").is_none());
    assert!(!dir_off.path().join(".justless/observations").exists());
}

#[test]
fn prefix_drift_detection_leaves_wire_bytes_untouched_and_reports_in_ledger_and_stats() {
    let (dir, mock, proxy) = session(|_| ok_json(), "");
    let root = dir.path();
    let anth_headers = [
        ("content-type", "application/json"),
        ("x-api-key", "sk-ant-test"),
    ];
    let openai_headers = [
        ("content-type", "application/json"),
        ("authorization", "Bearer sk-openai-test"),
    ];
    let read_tool =
        json!({"name": "Read", "description": "Read a file", "input_schema": {"type": "object"}});
    let bash_tool = json!({"name": "Bash", "description": "Run a shell command", "input_schema": {"type": "object"}});
    let secret_sys = "SECRET_SYSTEM_PROMPT_DO_NOT_LOG";
    let secret_msg = "SECRET_USER_MESSAGE_DO_NOT_LOG";

    let raw_odd = |val: &Value| {
        serde_json::to_string_pretty(val)
            .unwrap()
            .replace("\"role\": ", "\"role\" :   ")
    };

    // 1. Anthropic baseline + stable growing turn -> 0 prefix drift.
    let anth_t1 = json!({
        "model": "claude-opus-5-5",
        "max_tokens": 1024,
        "system": format!("Now: 2026-11-26T12:00:00Z {secret_sys}"),
        "tools": [read_tool, bash_tool],
        "messages": [
            {"role": "user", "content": "Inspect parser.rs"},
            {"role": "assistant", "content": "Looking at parser.rs"},
            {"role": "user", "content": "Check line 42"}
        ]
    });
    let anth_t2_stable = json!({
        "model": "claude-opus-5-5",
        "max_tokens": 1024,
        "system": format!("Now: 2026-11-26T12:00:00Z {secret_sys}"),
        "tools": [read_tool, bash_tool],
        "messages": [
            {"role": "user", "content": "Inspect parser.rs"},
            {"role": "assistant", "content": "Looking at parser.rs"},
            {"role": "user", "content": "Check line 42"},
            {"role": "assistant", "content": "Found off-by-one"},
            {"role": "user", "content": "Patch it"}
        ]
    });
    // 2. Anthropic system timestamp mutation -> drift: system changed.
    let mut anth_t3_sys_drift = anth_t2_stable.clone();
    anth_t3_sys_drift["system"] = json!(format!("Now: 2026-11-26T12:00:05Z {secret_sys}"));
    // 3. Anthropic reordered tools -> drift: tools reordered.
    let mut anth_t4_tools_drift = anth_t3_sys_drift.clone();
    anth_t4_tools_drift["tools"] = json!([bash_tool, read_tool]);

    // 4. OpenAI Chat baseline + mutated messages[1] -> drift: message[1] changed.
    let openai_t1 = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "You are a coding assistant."},
            {"role": "user", "content": format!("Original question {secret_msg}")},
            {"role": "assistant", "content": "Working on it."},
            {"role": "user", "content": "Any update?"}
        ]
    });
    let mut openai_t2_msg_drift = openai_t1.clone();
    openai_t2_msg_drift["messages"][1]["content"] =
        json!(format!("Edited earlier question {secret_msg}"));

    let sent_cases = [
        ("/v1/messages", &anth_headers[..], raw_odd(&anth_t1)),
        ("/v1/messages", &anth_headers[..], raw_odd(&anth_t2_stable)),
        (
            "/v1/messages",
            &anth_headers[..],
            raw_odd(&anth_t3_sys_drift),
        ),
        (
            "/v1/messages",
            &anth_headers[..],
            raw_odd(&anth_t4_tools_drift),
        ),
        (
            "/v1/chat/completions",
            &openai_headers[..],
            raw_odd(&openai_t1),
        ),
        (
            "/v1/chat/completions",
            &openai_headers[..],
            raw_odd(&openai_t2_msg_drift),
        ),
    ];

    for (idx, (path, headers, raw)) in sent_cases.iter().enumerate() {
        let resp = proxy.send("POST", path, headers, raw.as_bytes());
        assert_eq!(resp.status(), 200, "case {idx}");
        let forwarded = mock.seen().pop().unwrap();
        assert_eq!(
            forwarded.body,
            raw.as_bytes(),
            "case {idx}: prefix-drift detection must never mutate wire bytes"
        );
    }

    proxy.stop();

    // Ledger captures exact drifts and omits prefix_drift on cache-aligned requests.
    let records = ledger(root);
    assert_eq!(records.len(), 6);
    assert!(records[0].get("prefix_drift").is_none());
    assert!(records[1].get("prefix_drift").is_none());
    assert_eq!(
        records[2]["prefix_drift"],
        json!({"section": "system", "reason": "changed"})
    );
    assert_eq!(
        records[3]["prefix_drift"],
        json!({"section": "tools", "reason": "reordered"})
    );
    assert!(records[4].get("prefix_drift").is_none());
    assert_eq!(
        records[5]["prefix_drift"],
        json!({"section": "message[1]", "reason": "changed"})
    );

    // Zero raw prompt text or secrets leaked into ledger/logs.
    for entry in fs::read_dir(root.join(".justless/proxy")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let content = fs::read_to_string(&path).unwrap();
            assert!(!content.contains(secret_sys), "{}", path.display());
            assert!(!content.contains(secret_msg), "{}", path.display());
        }
    }

    // `justless stats --json` and `justless stats` surface drift count & top causes.
    let report = stats(root);
    assert_eq!(report["proxy"]["prefix_drifts"], 3);
    assert_eq!(
        report["proxy"]["prefix_drift_causes"],
        json!([
            {"section": "message[1]", "reason": "changed", "count": 1},
            {"section": "system", "reason": "changed", "count": 1},
            {"section": "tools", "reason": "reordered", "count": 1},
        ])
    );
    let summary = text(&justless(root, &["stats"]).stdout);
    contains(
        &summary,
        "3 prefix drifts detected (prompt cache prefix diverged: message[1] changed (1), system changed (1), tools reordered (1))",
    );
}
