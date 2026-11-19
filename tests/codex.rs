//! Codex: the command hook for calls from code-mode scripts, and the rollouts that
//! `discover` and `stats` read. Payloads follow Codex's hook schema and the rollout
//! records described in `src/rollouts.rs`.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Lab {
    _temp: TempDir,
    root: PathBuf,
    codex: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("work");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        Lab {
            _temp: temp,
            root,
            codex: base.join("codex"),
        }
    }

    fn run(&self, args: &[&str], input: Option<&Value>) -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
            .args(args)
            .current_dir(&self.root)
            .env("CODEX_HOME", &self.codex)
            .env("CLAUDE_CONFIG_DIR", self.codex.join("claude"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        if let Some(input) = input {
            stdin.write_all(input.to_string().as_bytes()).unwrap();
        }
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stderr}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self, args: &[&str], input: Option<&Value>) -> Value {
        serde_json::from_str(&self.run(args, input)).unwrap()
    }

    fn rollout(&self, name: &str, cwd: &Path, records: &[Value]) {
        let dir = self.codex.join("sessions/2026/11/19");
        fs::create_dir_all(&dir).unwrap();
        let mut text = String::new();
        let meta = json!({"type": "session_meta", "payload": {
            "id": "thread-1", "session_id": "root-1", "cwd": cwd, "source": "cli"}});
        for record in std::iter::once(&meta).chain(records) {
            let mut record = record.clone();
            // After any `--days` window starts.
            record["timestamp"] = json!("2099-01-01T00:00:00.000Z");
            text.push_str(&format!("{record}\n"));
        }
        fs::write(
            dir.join(format!("rollout-2026-11-19T10-00-00-{name}.jsonl")),
            text,
        )
        .unwrap();
    }
}

fn item(id: &str, command: &str, cwd: &Path, code: i32, output: &str) -> Value {
    json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {
        "type": "CommandExecution", "id": id, "command": ["/bin/zsh", "-lc", command],
        "cwd": format!("file://{}", cwd.display()), "exit_code": code,
        "aggregated_output": output, "source": "unified_exec_startup"}}})
}

fn response(payload: Value) -> Value {
    json!({"type": "response_item", "payload": payload})
}

fn exec_command(id: &str, command: &str) -> Value {
    let arguments = json!({"cmd": command, "workdir": "src", "yield_time_ms": 10000});
    response(json!({"type": "function_call", "name": "exec_command",
        "arguments": arguments.to_string(), "call_id": id}))
}

fn output(id: &str, text: &str) -> Value {
    response(json!({"type": "function_call_output", "call_id": id, "output": text}))
}

#[test]
fn code_mode_calls_reach_the_bash_hook_and_are_routed() {
    let lab = Lab::new();
    lab.run(&["init", "--hooks", "--agent", "codex"], None);
    let config = fs::read_to_string(lab.root.join(".codex/hooks.json")).unwrap();
    let hooks: Value = serde_json::from_str(&config).unwrap();
    // `exec_command` matches as `Bash`, from a script too; Codex has no after-bash hook.
    assert_eq!(hooks["hooks"]["PreToolUse"][0]["matcher"], "^Bash$");
    assert!(hooks["hooks"].get("PostToolUse").is_none(), "{config}");

    // Codex's PreToolUse input for a nested call: `cwd` is the call's directory, and
    // the source shows `exec-` call IDs for calls from a script.
    let call = |command: &str, extra: Value| {
        let mut input = json!({
            "session_id": "root-1", "turn_id": "turn-1", "transcript_path": null,
            "cwd": lab.root.join("src"), "hook_event_name": "PreToolUse", "model": "gpt-5",
            "permission_mode": "default", "tool_name": "Bash",
            "tool_input": {"command": command}, "tool_use_id": "exec-0b6d",
        });
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        lab.json(&["hook-codex", "before-command"], Some(&input))
    };
    let routed = call("cargo test", json!({}));
    let hook = &routed["hookSpecificOutput"];
    assert_eq!(hook["permissionDecision"], "allow");
    let command = hook["updatedInput"]["command"].as_str().unwrap();
    assert!(
        command.starts_with("justless run --hook --session ")
            && command.ends_with(" -- cargo test"),
        "{command}"
    );
    assert_eq!(hook["updatedInput"].as_object().unwrap().len(), 1);
    // An older payload names the directory as a `tool_cwd` URI.
    let uri = json!({"cwd": lab.root, "tool_cwd": format!("file://{}", lab.root.display())});
    assert_eq!(
        call("git status", uri)["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    // Codex runs a rewrite without asking, so a list with an unsupported command stays.
    assert_eq!(call("cargo test && git push", json!({})), json!({}));
}

#[test]
fn rollouts_feed_discover_and_stats() {
    let lab = Lab::new();
    let (root, src) = (&lab.root, &lab.root.join("src"));
    let needles = "src/lib.rs:1:needle\n".repeat(200);
    let nested = item("exec-1", "rg needle src", root, 0, &needles);
    let status = "M src/lib.rs\n[justless run-Ab1 · exit 0]\n";
    lab.rollout(
        "a",
        root,
        &[
            json!({"type": "turn_context", "payload": {"cwd": root}}),
            // A direct call is recorded as a call, an item and the result the model read.
            exec_command("call_1", "cargo test"),
            item("call_1", "cargo test", src, 101, "error[E0425]\n"),
            output(
                "call_1",
                "Chunk ID: a\nWall time: 1.0 seconds\nProcess exited with code 101\nOutput:\nerror[E0425]\n",
            ),
            response(json!({"type": "custom_tool_call", "name": "exec", "call_id": "call_2",
                "input": "text(await tools.exec_command({cmd: 'rg needle src'}))"})),
            nested.clone(),
            item(
                "exec-2",
                "justless run --hook -- git status",
                root,
                0,
                status,
            ),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {
                "type": "CommandExecution", "id": "exec-3", "command": ["/bin/zsh", "-lc", "ls"],
                "cwd": root, "exit_code": 0, "aggregated_output": "src\n",
                "source": "user_shell"}}}),
            response(json!({"type": "custom_tool_call_output", "call_id": "call_2",
                "output": [{"type": "input_text", "text": "Script completed\nOutput:\nfound 200"}]})),
            exec_command("call_3", "cargo build"),
            output(
                "call_3",
                "Chunk ID: b\nWall time: 10.0 seconds\nProcess running with session ID 5\nOutput:\nCompiling\n",
            ),
            exec_command("call_4", "git push"),
            output(
                "call_4",
                "Command blocked by PreToolUse hook: no. Command: git push",
            ),
        ],
    );
    // A forked thread's copy of a call counts once, and a thread elsewhere not at all.
    lab.rollout("b", root, &[nested]);
    let elsewhere = root.parent().unwrap();
    lab.rollout(
        "c",
        elsewhere,
        &[item("exec-9", "cargo test", elsewhere, 0, "ok\n")],
    );

    let report = lab.json(&["discover", "--json"], None);
    let codex = &report["codex"];
    let counts = |keys: &[&str]| {
        keys.iter()
            .map(|key| codex[key].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        counts(&[
            "sessions",
            "commands",
            "code_mode_commands",
            "code_mode_results"
        ]),
        [json!(1), json!(4), json!(2), json!(1)],
        "{codex:#}"
    );
    assert_eq!(
        codex["code_mode_output_bytes"],
        needles.len() + status.len()
    );
    assert_eq!(codex["repeated_calls"], 1);
    let labels = |route: &str| {
        let groups = codex["groups"].as_array().unwrap();
        let group = groups.iter().find(|group| group["route"] == route).unwrap();
        let labels = group["labels"].as_array().unwrap();
        labels
            .iter()
            .map(|l| l["label"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(labels("through_justless"), ["justless run"]);
    assert_eq!(labels("supported_not_routed"), ["cargo test", "rg"]);
    assert_eq!(labels("not_supported"), ["git push"]);

    let text = lab.run(&["discover"], None);
    assert!(
        text.contains("Codex ran 4 shell command(s) here")
            && text.contains("2 of them ran inside code-mode scripts")
            && text.contains("init --hooks --agent codex")
            && !text.contains("needle"),
        "{text}"
    );
    let stats = lab.json(&["stats", "--json"], None);
    assert_eq!(stats["codex"]["commands"], 4, "{stats:#}");
    assert_eq!(stats["codex"]["through_justless_commands"], 1);
    assert!(
        lab.run(&["stats"], None)
            .contains("Codex shell commands here: 4")
    );
}
