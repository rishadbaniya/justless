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
const PROXY_VARS: [&str; 7] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "ANTHROPIC_BASE_URL",
];

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
    fn start(root: &Path, upstream: &str, args: &[&str]) -> Self {
        let mut command = Command::new(BIN);
        command
            .current_dir(root)
            .args([
                "--root",
                root.to_str().unwrap(),
                "proxy",
                "--port",
                "0",
                "--upstream",
                upstream,
            ])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in PROXY_VARS {
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
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
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
        self.send(
            "POST",
            "/v1/messages",
            &all,
            &serde_json::to_vec(body).unwrap(),
        )
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

fn send(port: u16, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Message {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .write_all(&request_bytes(method, target, headers, body))
        .unwrap();
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
    let mut records = Vec::new();
    let Ok(listing) = fs::read_dir(root.join(".justless/proxy")) else {
        return records;
    };
    let mut files: Vec<PathBuf> = listing
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("requests-")
        })
        .collect();
    files.sort();
    for file in files {
        for line in fs::read_to_string(file).unwrap().lines() {
            records.push(serde_json::from_str(line).unwrap());
        }
    }
    records
}

fn justless(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(root)
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap()
}

const CLEARING: [&str; 10] = [
    "--floor",
    "1000",
    "--clear-at-least",
    "100",
    "--keep",
    "2",
    "--cold-after",
    "0",
    "--payback",
    "1000",
];

fn session(
    script: impl FnMut(&Message) -> Reply + Send + 'static,
    args: &[&str],
) -> (TempDir, Mock, Proxy) {
    let dir = tempdir().unwrap();
    let mock = Mock::start(script);
    let proxy = Proxy::start(dir.path(), &mock.url(), args);
    (dir, mock, proxy)
}

// ---------------------------------------------------------------------------
// Relaying

#[test]
fn relays_unplanned_request_byte_for_byte() {
    let reply_body =
        br#"{"id":"msg_01","type":"message","usage":{"input_tokens":5,"output_tokens":2}}"#;
    let (_dir, mock, proxy) = session(
        |_| Reply::Raw {
            status: 201,
            headers: vec![
                ("content-type", "application/json".into()),
                ("request-id", "req_011CVabc".into()),
                ("anthropic-ratelimit-tokens-remaining", "123".into()),
            ],
            body: reply_body.to_vec(),
        },
        &[],
    );
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
    assert_eq!(
        response.header("anthropic-ratelimit-tokens-remaining"),
        Some("123")
    );
    assert_eq!(response.body, reply_body);
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let forwarded = &seen[0];
    assert_eq!(forwarded.first, "POST /v1/messages?beta=true HTTP/1.1");
    assert_eq!(forwarded.body, body.as_bytes());
    for (name, value) in headers {
        assert_eq!(forwarded.headers_named(name), [value], "{name}");
    }
    let names: Vec<String> = forwarded
        .headers
        .iter()
        .map(|(n, _)| n.to_ascii_lowercase())
        .collect();
    for added in ["user-agent", "accept", "connection"] {
        assert!(
            !names.contains(&added.to_string()),
            "{added} was added: {names:?}"
        );
    }
    assert_eq!(
        forwarded.header("content-length"),
        Some(body.len().to_string().as_str())
    );
    let records = {
        proxy.stop();
        ledger(_dir.path())
    };
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
        &[],
    );
    let mut stream = TcpStream::connect(("127.0.0.1", proxy.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let body = serde_json::to_vec(&conversation("claude-opus-5-5", &[100])).unwrap();
    stream
        .write_all(&request_bytes(
            "POST",
            "/v1/messages",
            &[("content-type", "application/json")],
            &body,
        ))
        .unwrap();
    let started = Instant::now();
    let mut received = Vec::new();
    let mut buffer = [0; 4096];
    while !String::from_utf8_lossy(&received).contains("message_start") {
        let read = stream.read(&mut buffer).expect("first event within 5 s");
        assert!(read > 0, "closed before the first event");
        received.extend_from_slice(&buffer[..read]);
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    let text = String::from_utf8_lossy(&received).into_owned();
    assert!(text.contains("transfer-encoding: chunked"), "{text}");
    assert!(!text.contains("message_stop"));
    gate_tx.send(()).unwrap();
    stream.read_to_end(&mut received).unwrap();
    let text = String::from_utf8_lossy(&received);
    assert!(text.contains("event: ping"));
    assert!(text.contains("message_stop"));
    assert!(text.ends_with("0\r\n\r\n"));
}

#[test]
fn observer_handles_split_events() {
    let (dir, _mock, proxy) = session(
        |_| Reply::Events {
            events: events("claude-opus-5-5", Some((7, 4321)), 555),
            gate: None,
            piece: 1,
        },
        &[],
    );
    let response = proxy.post(&[], &conversation("claude-opus-5-5", &[100]));
    let text = String::from_utf8_lossy(&response.body).into_owned();
    assert_eq!(
        text,
        events("claude-opus-5-5", Some((7, 4321)), 555).concat()
    );
    proxy.stop();
    let record = &ledger(dir.path())[0];
    assert_eq!(record["observed"], true);
    assert_eq!(record["input"], 100);
    assert_eq!(record["cache_read"], 2000);
    assert_eq!(record["cache_write"], 555);
    assert_eq!(record["output"], 20);
    assert_eq!(record["api_cleared_uses"], 7);
    assert_eq!(record["api_cleared_tokens"], 4321);
}

#[test]
fn accepts_chunked_bodies_and_expect_continue() {
    let (_dir, mock, proxy) = session(|_| ok_json(), &[]);
    let body = serde_json::to_vec(&conversation("claude-opus-5-5", &[200; 2])).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", proxy.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(b"POST /v1/messages HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    assert_eq!(read_line(&mut reader).unwrap(), "HTTP/1.1 100 Continue");
    assert_eq!(read_line(&mut reader).unwrap(), "");
    for part in body.chunks(333) {
        stream
            .write_all(format!("{:x}\r\n", part.len()).as_bytes())
            .unwrap();
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
    let (_dir, mock, proxy) = session(|_| ok_json(), &CLEARING);
    let body = serde_json::to_vec(&conversation("claude-opus-5-5", &[40])).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", proxy.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
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
    let message = response.json()["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("incomplete request body"), "{message}");
    // A later complete request still goes through.
    proxy.send("GET", "/v1/models", &[], b"");
    let targets: Vec<String> = mock.seen().iter().map(|m| m.target().to_owned()).collect();
    assert_eq!(targets, ["/v1/models"]);
}

#[test]
fn bodies_over_64_mib_keep_their_content_length() {
    let (_dir, mock, proxy) = session(|_| ok_json(), &CLEARING);
    let size = 64 * 1024 * 1024 + 1234;
    let body: Vec<u8> = (0..size).map(|i| b"abcdefghij"[i % 10]).collect();
    let response = proxy.send(
        "POST",
        "/v1/files",
        &[("content-type", "application/octet-stream")],
        &body,
    );
    assert_eq!(response.status(), 200);
    let seen = mock.seen();
    let forwarded = seen.last().unwrap();
    assert_eq!(
        forwarded.header("content-length"),
        Some(size.to_string().as_str())
    );
    assert!(forwarded.header("transfer-encoding").is_none());
    assert_eq!(
        Sha256::digest(&forwarded.body),
        Sha256::digest(&body),
        "body changed"
    );
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
        &CLEARING,
    );
    let head = proxy.send("HEAD", "/api/hello", &[("x-api-key", "k")], b"");
    assert_eq!(head.status(), 200);
    assert_eq!(head.header("content-length"), Some("13"));
    assert_eq!(head.header("x-hello"), Some("1"));
    let models = proxy.send("GET", "/v1/models?x=1", &[("x-api-key", "k")], b"");
    assert_eq!(models.status(), 200);
    let batch = serde_json::to_vec(&json!({"requests":[{"custom_id":"a","params":conversation("claude-haiku-4-5", &[2000; 6])}]})).unwrap();
    proxy.send("POST", "/v1/messages/batches", &[], &batch);
    proxy.send("POST", "/v1/messages", &[], b"not json {");
    let duplicate = br#"{"model":"claude-haiku-4-5","messages":[],"model":"claude-opus-5-5"}"#;
    proxy.send("POST", "/v1/messages", &[], duplicate);
    let large = serde_json::to_vec(&conversation("claude-haiku-4-5", &[2000; 6])).unwrap();
    proxy.send(
        "POST",
        "/v1/messages",
        &[
            ("origin", "https://example.com"),
            ("content-type", "application/json"),
        ],
        &large,
    );
    let seen = mock.seen();
    let targets: Vec<&str> = seen.iter().map(Message::target).collect();
    assert_eq!(
        targets,
        [
            "/api/hello",
            "/v1/models?x=1",
            "/v1/messages/batches",
            "/v1/messages",
            "/v1/messages",
            "/v1/messages"
        ]
    );
    assert_eq!(seen[2].body, batch);
    assert_eq!(seen[3].body, b"not json {");
    assert_eq!(seen[4].body, duplicate);
    assert_eq!(seen[5].body, large);
    assert!(seen.iter().all(|m| m.header("anthropic-beta").is_none()));
    proxy.stop();
    assert!(!dir.path().join(".justless/tool-uses").exists());
    let reasons: Vec<Value> = ledger(dir.path())
        .iter()
        .map(|r| r["reason"].clone())
        .collect();
    assert_eq!(
        reasons,
        [
            "other-path",
            "other-path",
            "other-path",
            "unparsed",
            "unparsed",
            "browser-origin"
        ]
    );
}

#[test]
fn upstream_prefix_and_unreachable() {
    let dir = tempdir().unwrap();
    let mock = Mock::start(|_| ok_json());
    let proxy = Proxy::start(dir.path(), &format!("{}/gw/", mock.url()), &[]);
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
    let proxy = Proxy::start(dir.path(), &format!("http://{dead}"), &[]);
    let response = proxy.send("POST", "/v1/messages", &[], b"{}");
    assert_eq!(response.status(), 502);
    let error = response.json();
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "api_error");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!("justless proxy could not reach http://{dead}")),
        "{message}"
    );
    proxy.stop();
    // A proxy never forwards to itself, whether or not the port is free.
    let port = dead.port();
    let refused = justless(
        dir.path(),
        &[
            "proxy",
            "--port",
            &port.to_string(),
            "--upstream",
            &format!("http://localhost:{port}"),
        ],
    );
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("is this proxy"));
    // The upstream URL is printed and logged, so credentials in it are refused.
    let secret = justless(
        dir.path(),
        &[
            "proxy",
            "--upstream",
            "https://user:SECRET3@gateway.example/v1",
        ],
    );
    assert_eq!(secret.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&secret.stderr);
    assert!(stderr.contains("cannot contain credentials") && !stderr.contains("SECRET3"));
}

// ---------------------------------------------------------------------------
// Clearing

#[test]
fn adds_clear_tool_uses_with_stable_boundary() {
    let (dir, mock, proxy) = session(|_| stream(events("claude-haiku-4-5", None, 0)), &CLEARING);
    let headers = [("anthropic-beta", "oauth-2025-04-20")];
    let mut sizes: Vec<usize> = vec![2000, 2000, 2000, 2000, 40, 40];
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
        assert_eq!(
            forwarded.body[..bytes.len() - 1],
            bytes[..bytes.len() - 1],
            "step {step}"
        );
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
        assert_eq!(
            edit["clear_at_least"],
            json!({"type":"input_tokens","value":100})
        );
        assert!(edit.get("exclude_tools").is_none() && edit.get("clear_tool_inputs").is_none());
        assert_eq!(
            forwarded.headers_named("anthropic-beta"),
            [format!("oauth-2025-04-20,{BETA}")]
        );
        let uses = sizes.len() as u64;
        let (trigger, keep) = trigger_keep(&edit);
        assert_eq!(trigger, uses - 3);
        expected.push(uses - keep);
    }
    // The boundary stays at 4 while small results arrive, then moves once.
    assert_eq!(expected, [4, 4, 4, 10]);
    proxy.stop();
    let records = ledger(dir.path());
    let moves: Vec<Value> = records.iter().map(|r| r["move"].clone()).collect();
    assert_eq!(
        moves,
        [json!("payback"), Value::Null, Value::Null, json!("payback")]
    );
    assert!(records.iter().all(|r| r["mode"] == "cleared"));
    // Between moves, the edit is still sent and the ledger says why the
    // boundary held.
    let reasons: Vec<Value> = records.iter().map(|r| r["reason"].clone()).collect();
    assert_eq!(
        reasons,
        [
            Value::Null,
            json!("below-clear-at-least"),
            json!("below-clear-at-least"),
            Value::Null
        ]
    );
    assert_eq!(records[0]["newly_cleared"]["results"], 4);
    assert_eq!(records[0]["newly_cleared"]["bytes"], 4 * result_bytes(2000));
}

#[test]
fn appends_after_client_clear_thinking() {
    let (_dir, mock, proxy) = session(|_| ok_json(), &CLEARING);
    let mut body = conversation("claude-haiku-4-5", &[2000, 2000, 2000, 2000, 40, 40]);
    body["context_management"] = json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]});
    proxy.post(
        &[(
            "anthropic-beta",
            &format!("interleaved-thinking-2025-05-14, {BETA}"),
        )],
        &body,
    );
    let forwarded = &mock.seen()[0];
    let edits = forwarded.json()["context_management"]["edits"].clone();
    assert_eq!(
        edits[0],
        json!({"type":"clear_thinking_20251015","keep":"all"})
    );
    assert_eq!(edits[1]["type"], "clear_tool_uses_20250919");
    assert_eq!(
        forwarded.headers_named("anthropic-beta"),
        [format!("interleaved-thinking-2025-05-14, {BETA}")]
    );
}

#[test]
fn leaves_client_managed_conversations_alone() {
    let (dir, mock, proxy) = session(|_| ok_json(), &CLEARING);
    let mut body = conversation("claude-haiku-4-5", &[2000, 2000, 2000, 2000, 40, 40]);
    body["context_management"] = json!({"edits":[{"type":"clear_tool_uses_20250919","keep":{"type":"tool_uses","value":12}}]});
    proxy.post(&[], &body);
    let later = conversation(
        "claude-haiku-4-5",
        &[2000, 2000, 2000, 2000, 40, 40, 2000, 2000],
    );
    proxy.post(&[], &later);
    let seen = mock.seen();
    assert_eq!(seen[0].body, serde_json::to_vec(&body).unwrap());
    assert_eq!(seen[1].body, serde_json::to_vec(&later).unwrap());
    assert!(seen.iter().all(|m| m.header("anthropic-beta").is_none()));
    proxy.stop();
    let records = ledger(dir.path());
    assert!(
        records
            .iter()
            .all(|r| r["mode"] == "client" && r["reason"] == "client-managed")
    );
    assert_eq!(records[0]["client_managed_new"], true);
    assert_eq!(records[1]["client_managed_new"], false);
    let stats: Value =
        serde_json::from_slice(&justless(dir.path(), &["stats", "--json"]).stdout).unwrap();
    assert_eq!(stats["proxy"]["client_managed_conversations"], 1);
}

#[test]
fn no_clear_and_no_archive_only_relay_and_record() {
    let args = [&CLEARING[..], &["--no-clear", "--no-archive"]].concat();
    let (dir, mock, proxy) = session(|_| ok_json(), &args);
    let body = conversation("claude-haiku-4-5", &[2000, 2000, 2000, 2000, 40, 40]);
    assert_eq!(proxy.post(&[], &body).status(), 200);
    assert_eq!(mock.seen()[0].body, serde_json::to_vec(&body).unwrap());
    proxy.stop();
    assert!(!dir.path().join(".justless/tool-uses").exists());
    let records = ledger(dir.path());
    assert_eq!(
        (records[0]["mode"].as_str(), records[0]["reason"].as_str()),
        (Some("pass"), Some("no-clear"))
    );
    assert_eq!(records[0]["archived"], 0);
}

#[test]
fn passes_threads_compaction_and_other_models_unchanged() {
    let (dir, mock, proxy) = session(|_| ok_json(), &CLEARING);
    let sizes = [2000, 2000, 2000, 2000, 40, 40];
    let mut thread = conversation("claude-haiku-4-5", &sizes);
    thread["thread"] = json!({"id":"thr_01"});
    let mut compaction = conversation("claude-haiku-4-5", &sizes);
    compaction["compaction"] = json!({"trigger":{"type":"input_tokens","value":150000}});
    let bodies = [
        thread,
        compaction,
        conversation("claude-3-7-sonnet-20250219", &sizes),
        conversation("gpt-5.5", &sizes),
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
    let reasons: Vec<Value> = ledger(dir.path())
        .iter()
        .map(|r| r["reason"].clone())
        .collect();
    assert_eq!(
        reasons,
        [
            "thread",
            "compaction-param",
            "not-claude",
            "not-claude",
            "no-tool-uses"
        ]
    );
}

#[test]
fn cold_cache_move() {
    let args = [
        "--floor",
        "1000",
        "--clear-at-least",
        "100",
        "--keep",
        "2",
        "--cold-after",
        "1",
        "--payback",
        "0",
    ];
    let (dir, mock, proxy) = session(|_| ok_json(), &args);
    let sizes = [2000, 2000, 2000, 2000, 2000, 2000];
    proxy.post(&[], &conversation("claude-haiku-4-5", &sizes));
    assert!(our_edit(&mock.seen()[0].json()).is_none());
    thread::sleep(Duration::from_millis(1200));
    let mut grown = sizes.to_vec();
    grown.push(40);
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
    let args = [
        "--floor",
        "1000",
        "--clear-at-least",
        "100",
        "--keep",
        "2",
        "--payback",
        "0",
    ];
    let (dir, mock, proxy) = session(|_| ok_json(), &args);
    let mut sizes = vec![2000; 6];
    for model in ["claude-opus-5-5", "claude-haiku-4-5", "claude-opus-5-5"] {
        proxy.post(&[], &conversation(model, &sizes));
        sizes.push(2000);
    }
    assert!(mock.seen().iter().all(|m| our_edit(&m.json()).is_none()));
    proxy.stop();
    let reasons: Vec<Value> = ledger(dir.path())
        .iter()
        .map(|r| r["reason"].clone())
        .collect();
    assert_eq!(reasons, ["below-payback"; 3]);
}

#[test]
fn a_token_count_first_does_not_change_the_decision() {
    let args = [
        "--floor",
        "1000",
        "--clear-at-least",
        "100",
        "--keep",
        "2",
        "--cold-after",
        "1",
        "--payback",
        "0",
    ];
    let body = conversation("claude-opus-5-5", &[2000; 8]);
    let mut count = body.clone();
    count.as_object_mut().unwrap().remove("stream");
    let count = serde_json::to_vec(&count).unwrap();
    let mut decisions = Vec::new();
    for count_first in [false, true] {
        let (dir, mock, proxy) = session(|_| ok_json(), &args);
        if count_first {
            proxy.send(
                "POST",
                "/v1/messages/count_tokens",
                &[("content-type", "application/json")],
                &count,
            );
            thread::sleep(Duration::from_millis(1200));
        }
        proxy.post(&[], &body);
        let forwarded = mock.seen().pop().unwrap();
        assert!(our_edit(&forwarded.json()).is_none(), "{count_first}");
        proxy.stop();
        let last = ledger(dir.path()).pop().unwrap();
        decisions.push((last["reason"].clone(), last["move"].clone()));
    }
    assert_eq!(decisions[0], decisions[1]);
    assert_eq!(decisions[0], (json!("below-payback"), Value::Null));
}

#[test]
fn compaction_and_forks_keep_the_boundary() {
    let (dir, mock, proxy) = session(|_| ok_json(), &CLEARING);
    let base = [2000, 2000, 2000, 2000, 40, 40];
    proxy.post(
        &[("x-claude-code-request-class", "main")],
        &conversation("claude-haiku-4-5", &base),
    );
    let mut grown = base.to_vec();
    grown.extend([2000; 5]);
    let body = conversation("claude-haiku-4-5", &grown);
    proxy.post(&[("x-claude-code-request-class", "compaction")], &body);
    proxy.post(
        &[
            ("x-claude-code-request-class", "main"),
            ("x-claude-code-agent-type", "fork"),
        ],
        &body,
    );
    let mut count = body.clone();
    count.as_object_mut().unwrap().remove("stream");
    proxy.send(
        "POST",
        "/v1/messages/count_tokens",
        &[("content-type", "application/json")],
        &serde_json::to_vec(&count).unwrap(),
    );
    proxy.post(&[("x-claude-code-request-class", "main")], &body);
    let keeps: Vec<(u64, u64)> = mock
        .seen()
        .iter()
        .map(|m| trigger_keep(&our_edit(&m.json()).unwrap()))
        .collect();
    // Boundary 4 until the main conversation moves it to 11 - 2 = 9.
    assert_eq!(keeps, [(3, 2), (8, 7), (8, 7), (8, 7), (8, 2)]);
    proxy.stop();
    let roles: Vec<Value> = ledger(dir.path())
        .iter()
        .map(|r| r["role"].clone())
        .collect();
    assert_eq!(
        roles,
        ["mover", "read-only", "read-only", "read-only", "mover"]
    );
}

#[test]
fn state_survives_restart() {
    let dir = tempdir().unwrap();
    let mock = Mock::start(|_| ok_json());
    let sizes = [2000, 2000, 2000, 2000, 40, 40];
    let proxy = Proxy::start(dir.path(), &mock.url(), &CLEARING);
    proxy.post(&[], &conversation("claude-haiku-4-5", &sizes));
    proxy.stop();
    let state: Value = serde_json::from_slice(
        &fs::read(dir.path().join(".justless/proxy/conversations.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["conversations"]["toolu_00"]["boundaries"],
        json!(["toolu_03"])
    );
    let proxy = Proxy::start(dir.path(), &mock.url(), &CLEARING);
    let mut grown = sizes.to_vec();
    grown.push(40);
    proxy.post(&[], &conversation("claude-haiku-4-5", &grown));
    let edit = our_edit(&mock.seen()[1].json()).unwrap();
    assert_eq!(trigger_keep(&edit), (4, 3));
    proxy.stop();
    let records = ledger(dir.path());
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["move"], Value::Null);
    assert_eq!(records[1]["boundary"], 4);
}

#[test]
fn falls_back_only_when_the_edit_is_rejected() {
    let (dir, mock, proxy) = session(
        |request| {
            if String::from_utf8_lossy(&request.body).contains("clear_tool_uses_20250919") {
                Reply::Json(
                    400,
                    json!({"type":"error","error":{"type":"invalid_request_error",
                    "message":"claude-x-1 does not support the 'clear_tool_uses_20250919' context management strategy"}}),
                )
            } else {
                ok_json()
            }
        },
        &CLEARING,
    );
    let sizes = [2000, 2000, 2000, 2000, 40, 40];
    let body = conversation("claude-x-1", &sizes);
    let response = proxy.post(&[], &body);
    assert_eq!(response.status(), 200);
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert!(our_edit(&seen[0].json()).is_some());
    assert_eq!(seen[1].body, serde_json::to_vec(&body).unwrap());
    assert!(seen[1].header("anthropic-beta").is_none());
    let mut grown = sizes.to_vec();
    grown.push(2000);
    proxy.post(&[], &conversation("claude-x-1", &grown));
    let seen = mock.seen();
    assert_eq!(seen.len(), 3);
    assert!(our_edit(&seen[2].json()).is_none());
    proxy.stop();
    let modes: Vec<Value> = ledger(dir.path())
        .iter()
        .map(|r| r["mode"].clone())
        .collect();
    assert_eq!(modes, ["fallback", "refused"]);
    let log = fs::read_to_string(dir.path().join(".justless/proxy/proxy.log")).unwrap();
    assert!(log.contains("fallback: claude-x-1 answered 400"), "{log}");
}

#[test]
fn unrelated_400_is_relayed_without_retry() {
    let error = json!({"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1000001 tokens > 1000000 maximum"}});
    let reply = error.clone();
    let (_dir, mock, proxy) = session(move |_| Reply::Json(400, reply.clone()), &CLEARING);
    let response = proxy.post(
        &[],
        &conversation("claude-haiku-4-5", &[2000, 2000, 2000, 2000, 40, 40]),
    );
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
        &[],
    );
    let text = "exact \u{1f980} output\r\n  with spacing\n".repeat(50);
    let blocks =
        json!([{"type":"text","text":"first block"},{"type":"text","text":"second block"}]);
    let image = json!([{"type":"text","text":"screen:"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo".repeat(100)}}]);
    let first = vec![
        ("toolu_01Bash".to_string(), json!(text), false),
        ("toolu_02Read".to_string(), blocks.clone(), false),
        ("toolu_03Shot".to_string(), image.clone(), false),
        (
            "toolu_04Bash".to_string(),
            json!("error: test failed"),
            true,
        ),
    ];
    assert_eq!(
        proxy.post(&[], &results_request(first.clone())).status(),
        401
    );
    assert_eq!(
        proxy.post(&[], &results_request(first.clone())).status(),
        200
    );
    thread::sleep(Duration::from_millis(20));
    let mut second = first.clone();
    second[0].1 = json!("changed later by the client");
    second.push(("toolu_05Read".to_string(), json!("newest result"), false));
    proxy.post(&[], &results_request(second));
    proxy.stop();
    let records = ledger(dir.path());
    assert_eq!(records[0]["archived"], 0);
    assert_eq!(records[1]["archived"], 4);
    assert_eq!(records[2]["archived"], 1);
    let index = dir.path().join(".justless/tool-uses");
    let digest = format!("{:x}", Sha256::digest(b"toolu_01Bash"));
    assert!(index.join(format!("{digest}.json")).is_file());
    // The first copy wins, byte for byte.
    let recalled = justless(dir.path(), &["recall", "--tool-use", "toolu_01Bash"]);
    assert!(recalled.status.success());
    assert_eq!(recalled.stdout, text.as_bytes());
    let typed = justless(
        dir.path(),
        &["recall", "--tool-use", "toolu_02Read", "--json"],
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&typed.stdout).unwrap(),
        blocks
    );
    assert_eq!(
        justless(dir.path(), &["recall", "--tool-use", "toolu_02Read"]).stdout,
        b"first block\nsecond block"
    );
    let shot =
        String::from_utf8(justless(dir.path(), &["recall", "--tool-use", "toolu_03Shot"]).stdout)
            .unwrap();
    assert_eq!(shot, "screen:\n[image/png · 1.1 KB base64 · use --json]");
    let image_json = justless(
        dir.path(),
        &["recall", "--tool-use", "toolu_03Shot", "--json"],
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&image_json.stdout).unwrap(),
        image
    );
    // Listing: newest first, full IDs, filter and limit.
    let list = String::from_utf8(justless(dir.path(), &["recall", "--list"]).stdout).unwrap();
    let lines: Vec<&str> = list.lines().collect();
    assert!(
        lines[0].starts_with("toolu_05Read  Read  src/capture_4.rs  15 B  "),
        "{list}"
    );
    assert!(
        list.contains("toolu_04Bash  Bash  cargo test --locked -p crate3  "),
        "{list}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("toolu_04Bash") && l.ends_with("  error"))
    );
    assert_eq!(
        lines.last().unwrap(),
        &"5 archived tool results. Print one with `justless recall --tool-use ID`."
    );
    let cargo = String::from_utf8(
        justless(dir.path(), &["recall", "--list", "CARGO", "--limit", "1"]).stdout,
    )
    .unwrap();
    let rows: Vec<&str> = cargo.lines().collect();
    assert_eq!(rows.len(), 2, "{cargo}");
    assert!(
        rows[0].contains("Bash  cargo test --locked -p crate"),
        "{cargo}"
    );
    assert_eq!(
        rows[1],
        "2 of 5 archived tool results match \"CARGO\", newest 1 shown. Print one with `justless recall --tool-use ID`."
    );
    // Invalid and unknown IDs.
    let invalid = justless(dir.path(), &["recall", "--tool-use", "../x"]);
    assert_eq!(invalid.status.code(), Some(2));
    let unknown = justless(dir.path(), &["recall", "--tool-use", "toolu_missing"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&unknown.stderr),
        "justless: no archived tool result for toolu_missing; try `justless recall --list TEXT`\n"
    );
    assert_eq!(
        justless(dir.path(), &["recall", "--list", "x", "--json"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(justless(dir.path(), &["recall"]).status.code(), Some(2));
    // The positional form reaches the proxy's archive too, with the same text.
    let positional = justless(dir.path(), &["recall", "toolu_01Bash"]);
    assert!(positional.status.success());
    assert_eq!(positional.stdout, text.as_bytes());
    let missing = justless(dir.path(), &["recall", "toolu_missing"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("recall --list TEXT"));
    let bad = justless(dir.path(), &["recall", "../x"]);
    assert_eq!(bad.status.code(), Some(2));
    // A line range is exact bytes; --grep prints numbered matching lines.
    let range = justless(
        dir.path(),
        &["recall", "toolu_01Bash", "--from", "2", "--to", "3"],
    );
    assert_eq!(
        range.stdout,
        b"  with spacing\nexact \xf0\x9f\xa6\x80 output\r\n"
    );
    let grep = justless(
        dir.path(),
        &[
            "recall",
            "--tool-use",
            "toolu_01Bash",
            "--grep",
            "spacing",
            "--limit",
            "2",
        ],
    );
    assert_eq!(
        String::from_utf8(grep.stdout).unwrap(),
        "2:   with spacing\n4:   with spacing\n"
    );
    let none = justless(dir.path(), &["recall", "toolu_01Bash", "--grep", "absent"]);
    assert_eq!(none.stdout, b"No line matches \"absent\".\n");
    let conflict = justless(
        dir.path(),
        &["recall", "toolu_01Bash", "--json", "--grep", "x"],
    );
    assert_eq!(conflict.status.code(), Some(2));
    // MCP clients reach the same archive through the recall tool.
    let mut mcp = Command::new(BIN)
        .args(["mcp", "--root", dir.path().to_str().unwrap()])
        .current_dir(dir.path())
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
    let output = mcp.wait_with_output().unwrap();
    let reply: Value = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|message| message["id"] == 2)
        .unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(
        reply["result"]["content"][0]["text"],
        "first block\nsecond block"
    );
}

#[test]
fn list_matches_patterns_and_read_ranges() {
    let (dir, _mock, proxy) = session(|_| ok_json(), &[]);
    let calls = [
        (
            "toolu_g1",
            "Grep",
            json!({"pattern":"parse_trade","path":"/repo/src","glob":"*.rs"}),
        ),
        (
            "toolu_g2",
            "Glob",
            json!({"pattern":"**/*_test.rs","path":"/repo"}),
        ),
        (
            "toolu_r1",
            "Read",
            json!({"file_path":"/repo/src/lib.rs","offset":1,"limit":50}),
        ),
        (
            "toolu_r2",
            "Read",
            json!({"file_path":"/repo/src/lib.rs","offset":900}),
        ),
        (
            "toolu_w1",
            "WebSearch",
            json!({"query":"rust floor_char_boundary"}),
        ),
    ];
    let mut messages = vec![json!({"role":"user","content":"go"})];
    for (id, name, input) in &calls {
        messages.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":input}]}));
        messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"result"}]}));
    }
    let body = json!({"model":"claude-opus-5-5","max_tokens":1024,"messages":messages});
    assert_eq!(proxy.post(&[], &body).status(), 200);
    proxy.stop();
    let list = |text: &str| {
        String::from_utf8(justless(dir.path(), &["recall", "--list", text]).stdout).unwrap()
    };
    let grep = list("parse_trade");
    assert!(
        grep.contains("toolu_g1  Grep  parse_trade in /repo/src  "),
        "{grep}"
    );
    let glob = list("_test");
    assert!(
        glob.contains("toolu_g2  Glob  **/*_test.rs in /repo  "),
        "{glob}"
    );
    let reads = list("lib.rs");
    assert!(
        reads.contains("toolu_r1  Read  /repo/src/lib.rs:1+50  "),
        "{reads}"
    );
    assert!(
        reads.contains("toolu_r2  Read  /repo/src/lib.rs:900  "),
        "{reads}"
    );
    // TEXT also matches the rest of the input, such as a Grep glob.
    let by_glob = list("*.rs\"");
    assert!(by_glob.starts_with("toolu_g1  "), "{by_glob}");
    assert!(list("floor_char").contains("toolu_w1  WebSearch  rust floor_char_boundary  "));
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
    let (dir, _mock, proxy) = session(move |_| replies.next().unwrap_or_else(ok_json), &CLEARING);
    let sizes = [2000, 2000, 2000, 2000, 40, 40];
    proxy.post(&[], &conversation("claude-haiku-4-5", &sizes));
    // The next call repeats cleared call 0 (same tool and input, new ID).
    let mut repeat = conversation("claude-haiku-4-5", &sizes);
    let messages = repeat["messages"].as_array_mut().unwrap();
    messages.push(json!({"role":"assistant","content":[{"type":"tool_use","id":"toolu_again","name":"Read","input":tool_input(0)}]}));
    messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_again","content":"x"}]}));
    proxy.post(&[], &repeat);
    proxy.post(&[], &repeat);
    proxy.stop();
    let records = ledger(dir.path());
    assert_eq!(records[0]["move"], "payback");
    assert_eq!(records[0]["rewrite_api"], 1500);
    assert_eq!(records[0]["mismatch"], false);
    assert_eq!(records[1]["mismatch"], true);
    assert_eq!(records[1]["repeats"], 1);
    assert_eq!(records[2]["observed"], false);
    let stats: Value =
        serde_json::from_slice(&justless(dir.path(), &["stats", "--json"]).stdout).unwrap();
    let proxy_stats = &stats["proxy"];
    assert_eq!(proxy_stats["requests"], 3);
    assert_eq!(proxy_stats["cleared_requests"], 3);
    assert_eq!(proxy_stats["moves"], json!({"cold":0,"payback":1}));
    assert_eq!(proxy_stats["newly_cleared"]["results"], 4);
    assert_eq!(
        proxy_stats["newly_cleared"]["bytes"],
        4 * result_bytes(2000)
    );
    assert_eq!(
        proxy_stats["newly_cleared"]["est_tokens"],
        4 * result_bytes(2000).div_ceil(4)
    );
    assert_eq!(proxy_stats["warm_rewrite_tokens"], 1500);
    assert_eq!(proxy_stats["tokens_not_sent"], 2010 + 1500);
    assert_eq!(proxy_stats["mismatches"], 1);
    assert_eq!(proxy_stats["repeats"], 1);
    assert_eq!(proxy_stats["unobserved"], 1);
    assert_eq!(proxy_stats["archived_results"], 7);
    assert_eq!(proxy_stats["token_estimate"], "UTF-8 bytes / 4");
    assert_eq!(proxy_stats["usage"]["cache_write"], 1500);
    let text = String::from_utf8(justless(dir.path(), &["stats"]).stdout).unwrap();
    assert!(text.contains("No captured runs yet"), "{text}");
    assert!(
        text.contains("Request boundary (justless proxy), last 7 day(s)"),
        "{text}"
    );
    assert!(text.contains("3 requests · clearing requested on 3 · 0 conversations left to Claude Code's own clearing · 0 refusals"), "{text}");
    assert!(text.contains("4 old tool results marked for clearing (8.1 KB, about 2.07K tokens estimated) in 1 clear: 1 on a warm cache (1.5K tokens written to cache on that request, reported by the API)"), "{text}");
    assert!(
        text.contains("3.51K input tokens not sent to the model"),
        "{text}"
    );
    assert!(
        text.contains("1 read repeated a cleared call with no file change in between"),
        "{text}"
    );
    assert!(text.contains("7 tool results archived"), "{text}");
    let empty = tempdir().unwrap();
    let none: Value =
        serde_json::from_slice(&justless(empty.path(), &["stats", "--json"]).stdout).unwrap();
    assert_eq!(none["proxy"], Value::Null);
}

#[test]
fn never_persists_credentials() {
    let (dir, _mock, proxy) = session(
        |_| stream(events("claude-haiku-4-5", Some((4, 2000)), 10)),
        &CLEARING,
    );
    let body = conversation("claude-haiku-4-5", &[2000, 2000, 2000, 2000, 40, 40]);
    let response = proxy.post(
        &[
            ("x-api-key", "sk-ant-test-SECRET1"),
            ("authorization", "Bearer SECRET2"),
            ("x-claude-code-session-id", "s1"),
        ],
        &body,
    );
    assert_eq!(response.status(), 200);
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
        let bytes = fs::read(&file).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("SECRET1") && !text.contains("SECRET2"),
            "{}",
            file.display()
        );
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
        &[],
    );
    let port = proxy.port;
    let clients: Vec<_> = (0..2)
        .map(|_| {
            thread::spawn(move || {
                let body = serde_json::to_vec(&conversation("claude-opus-5-5", &[50])).unwrap();
                send(
                    port,
                    "POST",
                    "/v1/messages",
                    &[("content-type", "application/json")],
                    &body,
                )
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(300));
    for tx in senders {
        tx.send(()).unwrap();
    }
    for client in clients {
        let response = client.join().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            events("claude-opus-5-5", None, 0).concat()
        );
    }
}

#[test]
fn verbose_lines_name_the_decision_without_values() {
    let (_dir, _mock, proxy) = session(
        |_| stream(events("claude-haiku-4-5", Some((4, 2010)), 1500)),
        &[&CLEARING[..], &["--verbose"]].concat(),
    );
    proxy.post(
        &[
            ("x-api-key", "sk-ant-test-SECRET1"),
            ("x-claude-code-request-class", "main"),
        ],
        &conversation("claude-haiku-4-5", &[2000, 2000, 2000, 2000, 40, 40]),
    );
    let output = proxy.stop();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with(
            "200 messages claude-haiku-4-5 main · 6 uses · cleared 4 (moved, warm, C/M "
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("API cleared 4 calls, 2,010 tokens"),
        "{stderr}"
    );
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
        .env("HOME", root.join("home"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("JUSTLESS_PROXY")
        .env_remove("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS")
        .env_remove("CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING")
        .env_remove("CLAUDE_CODE_GATEWAY_HINT_HEADERS");
    for name in [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_MANTLE",
        "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
    ] {
        command.env_remove(name);
    }
    for name in PROXY_VARS {
        command.env_remove(name);
    }
    command
}

#[test]
fn wrap_launches_claude_with_proxy_env() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let record = root.join("record");
    let bin = fake_claude(
        root,
        &format!(
            "{{ for a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done; env | grep -E '^(ANTHROPIC_BASE_URL|JUSTLESS_PROXY|CLAUDE_CODE_)'; }} > '{}'\nexit 7",
            record.display()
        ),
    );
    let mock = Mock::start(|_| ok_json());
    fs::create_dir(root.join(".claude")).unwrap();
    fs::write(
        root.join(".claude/settings.json"),
        r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example"}}"#,
    )
    .unwrap();
    let output = wrap_command(
        root,
        &bin,
        &[
            "--keep", "5", "claude", "--resume", "abc", "-p", "x y", "--help",
        ],
    )
    .env("ANTHROPIC_BASE_URL", mock.url())
    .env("CLAUDE_CODE_GATEWAY_HINT_HEADERS", "0")
    .output()
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = fs::read_to_string(&record).unwrap();
    let args: Vec<&str> = recorded
        .lines()
        .filter_map(|l| l.strip_prefix("arg="))
        .collect();
    assert_eq!(args, ["--resume", "abc", "-p", "x y", "--help"]);
    let env: Vec<&str> = recorded
        .lines()
        .filter(|l| !l.starts_with("arg="))
        .collect();
    let base = env
        .iter()
        .find_map(|l| l.strip_prefix("ANTHROPIC_BASE_URL="))
        .unwrap();
    let port = base.strip_prefix("http://127.0.0.1:").unwrap();
    assert!(
        !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
        "{base}"
    );
    assert!(env.contains(&"JUSTLESS_PROXY=1"));
    assert!(env.contains(&"CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING=1"));
    // An inherited value is kept.
    assert!(env.contains(&"CLAUDE_CODE_GATEWAY_HINT_HEADERS=0"));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let canonical = root.canonicalize().unwrap();
    assert!(
        stderr.contains(
            ".claude/settings.json sets ANTHROPIC_BASE_URL; Claude Code may bypass the proxy."
        ),
        "{stderr}"
    );
    let banner = format!(
        " → {} · old tool results cleared from 100K tokens · archive {}/.justless · justless stats shows what changed",
        mock.url(),
        canonical.display()
    );
    assert!(
        stderr.contains("justless: Claude Code → 127.0.0.1:"),
        "{stderr}"
    );
    assert!(stderr.contains(&banner), "{stderr}");
    assert!(
        stderr.contains("justless: the proxy received no requests."),
        "{stderr}"
    );
}

#[test]
fn wrap_passes_every_later_argument_and_exempts_loopback_from_proxies() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let record = root.join("record");
    let bin = fake_claude(
        root,
        &format!(
            "{{ for a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done; env | grep -E '^(NO_PROXY|no_proxy)='; }} > '{}'\nexit 0",
            record.display()
        ),
    );
    let mock = Mock::start(|_| ok_json());
    let recorded = |args: &[&str], proxied: bool| {
        let mut command = wrap_command(root, &bin, args);
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
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read_to_string(&record).unwrap()
    };
    let args = |text: &str| -> Vec<String> {
        text.lines()
            .filter_map(|l| l.strip_prefix("arg=").map(str::to_owned))
            .collect()
    };
    // `--` and options Justless also has reach Claude Code unparsed.
    let plain = recorded(&["claude", "--", "-p", "hi", "--root", "x"], false);
    assert_eq!(args(&plain), ["--", "-p", "hi", "--root", "x"]);
    assert!(!plain.contains("NO_PROXY"), "{plain}");
    let proxied = recorded(&["--keep", "4", "claude", "--root", "x"], true);
    assert_eq!(args(&proxied), ["--root", "x"]);
    let hosts = "corp.example,.internal,localhost,127.0.0.1";
    assert!(
        proxied.contains(&format!("NO_PROXY={hosts}\n")),
        "{proxied}"
    );
    assert!(
        proxied.contains(&format!("no_proxy={hosts}\n")),
        "{proxied}"
    );
    let other = wrap_command(root, &bin, &["codex"]).output().unwrap();
    assert_eq!(other.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&other.stderr).contains("wrap launches Claude Code"));
}

#[test]
fn wrap_relays_requests_and_summarizes_them() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let body = root.join("body.json");
    fs::write(
        &body,
        serde_json::to_vec(&conversation(
            "claude-haiku-4-5",
            &[2000, 2000, 2000, 2000, 40, 40],
        ))
        .unwrap(),
    )
    .unwrap();
    // The fake Claude Code sends one request through the proxy with curl, which
    // also exercises `Expect: 100-continue` for a large body.
    if Command::new("curl").arg("--version").output().is_err() {
        eprintln!("skipped: curl is not on PATH");
        return;
    }
    let curl = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v curl"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let bin = fake_claude(
        root,
        &format!(
            "'{}' -s -o /dev/null -H 'content-type: application/json' --data-binary @'{}' \"$ANTHROPIC_BASE_URL/v1/messages\"",
            curl.trim(),
            body.display()
        ),
    );
    let mock = Mock::start(|_| stream(events("claude-haiku-4-5", Some((4, 2010)), 1500)));
    let mut args = CLEARING.to_vec();
    args.push("claude");
    let output = wrap_command(root, &bin, &args)
        .env("ANTHROPIC_BASE_URL", mock.url())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let canonical = root.canonicalize().unwrap();
    assert!(
        stderr.ends_with(&format!(
            "justless: 1 request · 1 clear (0 on an expired cache) · 4 old tool results marked for clearing · 2.01K input tokens not sent to the model · 6 tool results archived in {}/.justless\n",
            canonical.display()
        )),
        "{stderr}"
    );
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
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("the proxy received no requests"),
        "{stderr}"
    );
}

#[test]
fn wrap_refuses_unsupported_setups() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let marker = root.join("ran");
    let bin = fake_claude(root, &format!(": > '{}'", marker.display()));
    for (name, value, message) in [
        (
            "CLAUDE_CODE_USE_BEDROCK",
            "1",
            "wrap needs the Claude API, but CLAUDE_CODE_USE_BEDROCK is set. Run claude directly.",
        ),
        (
            "JUSTLESS_PROXY",
            "1",
            "already runs through a Justless proxy",
        ),
    ] {
        let output = wrap_command(root, &bin, &["claude"])
            .env(name, value)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{name}"
        );
        assert!(!marker.exists(), "{name}");
    }
    // A falsy provider variable does not refuse.
    let output = wrap_command(root, &bin, &["claude"])
        .env("CLAUDE_CODE_USE_VERTEX", "0")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(marker.exists());
    fs::remove_file(&marker).unwrap();
    let empty = root.join("empty");
    fs::create_dir(&empty).unwrap();
    let missing = wrap_command(root, &empty, &["claude"]).output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("claude is not on PATH"));
    let betas = wrap_command(root, &bin, &["claude"])
        .env("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1")
        .output()
        .unwrap();
    assert!(betas.status.success());
    assert!(
        String::from_utf8_lossy(&betas.stderr)
            .contains("archive only, because CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS is set")
    );
}
