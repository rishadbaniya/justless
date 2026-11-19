//! Helpers the suites share.

use std::io::Write;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

pub const BIN: &str = env!("CARGO_BIN_EXE_justless");

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `line` split at single spaces, for arguments that hold none.
pub fn words(line: &str) -> Vec<&str> {
    line.split(' ').collect()
}

/// Runs Git in `root` with a fixed identity and returns its stdout.
pub fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        text(&output.stderr)
    );
    text(&output.stdout)
}

/// Runs `command` with `input` on its stdin and both outputs captured.
pub fn piped(command: &mut Command, input: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

/// The run named by the last `[justless run-…` footer on `output`'s stderr.
pub fn run_id(output: &Output) -> String {
    let stderr = text(&output.stderr);
    let start = stderr.rfind("[justless run-").expect("a run footer") + "[justless ".len();
    let mut id = stderr[start..].split(|c: char| !c.is_ascii_alphanumeric() && c != '-');
    id.next().unwrap().to_owned()
}

/// Sends `lines` to a `justless mcp` server, closes its stdin and returns its stderr
/// and its stdout lines parsed. The server must exit cleanly and print only JSON-RPC
/// messages, one per line or in batch arrays.
pub fn mcp_session(command: &mut Command, lines: &[String]) -> (String, Vec<Value>) {
    let input: String = lines.iter().map(|line| format!("{line}\n")).collect();
    let output = piped(command, input.as_bytes());
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(stdout.ends_with('\n') || stdout.is_empty(), "{stdout}");
    let parse = |line: &str| -> Value {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("non-JSON stdout line {line:?}: {error}"));
        let messages = value
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![value.clone()]);
        for message in messages {
            assert_eq!(message["jsonrpc"], "2.0", "{line}");
            let result = message.get("result").is_some();
            assert!(result != message.get("error").is_some(), "{line}");
        }
        value
    };
    (text(&output.stderr), stdout.lines().map(parse).collect())
}

/// Writes `script` at `path`, executable on Unix.
pub fn executable(path: &std::path::Path, script: &str) {
    std::fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[track_caller]
pub fn has(haystack: &str, needle: &str) {
    assert!(
        haystack.contains(needle),
        "missing {needle:?} in:\n{haystack}"
    );
}

#[track_caller]
pub fn lacks(haystack: &str, needle: &str) {
    assert!(
        !haystack.contains(needle),
        "unexpected {needle:?} in:\n{haystack}"
    );
}

#[track_caller]
pub fn starts(haystack: &str, prefix: &str) {
    assert!(
        haystack.starts_with(prefix),
        "expected {prefix:?} at the start of:\n{haystack}"
    );
}
