//! `discover` classifies Claude Code commands by what the current hooks, the
//! post-run hook included, would do with them. Transcripts are synthetic and every
//! configuration folder is a temporary one.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, git, has, lacks, piped, text};

struct Setup {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("work");
        let home = temp.path().join("home");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&home).unwrap();
        let root = root.canonicalize().unwrap();
        let source: String = (0..300)
            .map(|i| format!("pub fn add_{i:03}(value: u64) -> u64 {{\n    value + {i}\n}}\n"))
            .collect();
        fs::write(root.join("big.rs"), &source).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/big.rs"), &source).unwrap();
        fs::create_dir_all(temp.path().join("outside")).unwrap();
        fs::write(temp.path().join("outside/big.rs"), &source).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "big.rs", "sub"]);
        git(&root, &["commit", "-qm", "fixture"]);
        Self {
            _temp: temp,
            root,
            home,
        }
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", self.home.join("claude"))
            .env("CODEX_HOME", self.home.join("codex"))
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env(
                "JUSTLESS_TEST_CLAUDE_MANAGED_DIR",
                self.home.join("managed"),
            );
        command
    }

    fn sh(&self, script: &str) -> String {
        let output = self
            .command("/bin/sh")
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{script}");
        text(&output.stdout)
    }

    /// The post-run hook's replacement for a successful result, if it makes one.
    fn after_bash(&self, command: &str, stdout: &str) -> Option<String> {
        let input = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "session_id": "s",
            "cwd": self.root, "tool_input": {"command": command},
            "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false},
        });
        let mut hook = self.command(BIN);
        hook.args(["hook-claude", "after-bash"]);
        let output = piped(&mut hook, input.to_string().as_bytes());
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        let view = reply["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str();
        view.map(str::to_owned)
    }

    /// Writes one session transcript of `(command, output, extra fields)` calls.
    fn transcript(&self, session: &str, calls: &[(String, String, Value)]) {
        let slug: String = self
            .root
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = self.home.join("claude/projects").join(slug);
        fs::create_dir_all(&dir).unwrap();
        let mut lines = Vec::new();
        for (n, (command, output, extra)) in calls.iter().enumerate() {
            let id = format!("{session}-{n}");
            lines.push(json!({"type":"assistant","cwd":self.root,"timestamp":"2999-01-01T00:00:00Z",
                "message":{"content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":command}}]}}));
            let mut result = json!({"type":"tool_result","tool_use_id":id,"content":output});
            let mut entry =
                json!({"type":"user","cwd":self.root,"timestamp":"2999-01-01T00:00:01Z"});
            for (key, value) in extra.as_object().unwrap() {
                if key == "is_error" {
                    result[key] = value.clone();
                } else {
                    entry[key] = value.clone();
                }
            }
            entry["message"] = json!({"content":[result]});
            lines.push(entry);
        }
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(dir.join(format!("{session}.jsonl")), body.join("\n")).unwrap();
    }

    fn discover(&self, args: &[&str]) -> String {
        let output = self
            .command(BIN)
            .arg("discover")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout)
    }

    /// The group `discover` gives `command` with `output` as a session's only call.
    fn route(&self, command: &str, output: &str) -> String {
        self.transcript("only", &[(command.into(), output.into(), json!({}))]);
        let routes = self.routes();
        assert_eq!(routes.len(), 1, "{command}: {routes:?}");
        routes.into_keys().next().unwrap().0
    }

    /// Commands per `(route, label)` in `discover --json`.
    fn routes(&self) -> BTreeMap<(String, String), u64> {
        let report: Value = serde_json::from_str(&self.discover(&["--json"])).unwrap();
        let mut routes = BTreeMap::new();
        for group in report["groups"].as_array().unwrap() {
            for label in group["labels"].as_array().unwrap() {
                let key = (
                    group["route"].as_str().unwrap().to_owned(),
                    label["label"].as_str().unwrap().to_owned(),
                );
                *routes.entry(key).or_default() += label["commands"].as_u64().unwrap();
            }
        }
        routes
    }
}

const AFTER: &str = "compacted_after_running";

/// Each command's group agrees with what the real post-run hook does with the same
/// result, and the hook's own replacement then counts as through Justless.
#[test]
fn discover_groups_commands_as_the_post_run_hook_treats_them() {
    let setup = Setup::new();
    let read = |script: &str| setup.sh(script);
    let progress = format!("{}done\n", "waiting for the build\n".repeat(600));
    let tests: String = (0..400)
        .map(|i| format!("test tests::case_{i:03} ... ok\n"))
        .chain(["\ntest result: ok. 400 passed; 0 failed\n".to_owned()])
        .collect();
    let table: Vec<(&str, String, &str, &str)> = vec![
        // Range reads: a large one is outlined, a small one keeps its text.
        (
            "sed -n 1,400p big.rs",
            read("sed -n 1,400p big.rs"),
            AFTER,
            "sed",
        ),
        (
            "sed -n 1,20p big.rs",
            read("sed -n 1,20p big.rs"),
            "passed_through",
            "sed",
        ),
        (
            "cat big.rs | head -n 500",
            read("cat big.rs | head -n 500"),
            AFTER,
            "cat",
        ),
        // A read list over the batch floor.
        (
            "sed -n 1,200p big.rs && sed -n 300,500p big.rs",
            read("sed -n 1,200p big.rs && sed -n 300,500p big.rs"),
            AFTER,
            "sed",
        ),
        // A supported tool in a pass-through form: over and under the budget.
        (
            "cargo test 2>&1 | tail -n 500",
            tests.clone(),
            AFTER,
            "cargo test",
        ),
        (
            "cargo test 2>&1 | tail -n 5",
            tests[..300].to_owned(),
            "passed_through",
            "cargo test",
        ),
        // An unsupported command: over and under its floor.
        ("python3 build.py", progress.clone(), AFTER, "python3"),
        (
            "python3 build.py",
            progress[..6000].to_owned(),
            "not_supported",
            "python3",
        ),
    ];
    let mut expected: BTreeMap<(String, String), u64> = BTreeMap::new();
    let (mut calls, mut replaced) = (Vec::new(), Vec::new());
    for (command, output, route, label) in &table {
        let hook = setup.after_bash(command, output);
        assert_eq!(hook.is_some(), *route == AFTER, "{command}: {hook:?}");
        *expected
            .entry((route.to_string(), label.to_string()))
            .or_default() += 1;
        calls.push((command.to_string(), output.clone(), json!({})));
        if let Some(view) = hook {
            replaced.push((command.to_string(), view, json!({})));
        }
    }
    // A failed command fires PostToolUseFailure, which cannot rewrite it, and an
    // interrupted result is left alone.
    let large = read("sed -n 1,400p big.rs");
    calls.push((
        "sed -n 1,400p big.rs".into(),
        large.clone(),
        json!({"is_error": true}),
    ));
    let interrupted =
        json!({"toolUseResult": {"stdout": large, "stderr": "", "interrupted": true}});
    calls.push(("sed -n 1,400p big.rs".into(), large.clone(), interrupted));
    *expected
        .entry(("not_supported".into(), "sed".into()))
        .or_default() += 2;
    // A result Claude Code saved to a file is judged by its full size.
    let saved = json!({"toolUseResult": {"stdout": "preview", "stderr": "", "interrupted": false,
        "persistedOutputPath": "/saved.txt", "persistedOutputSize": 90_000}});
    calls.push((
        "cargo test 2>&1 | tail -n 900".into(),
        "preview".into(),
        saved,
    ));
    *expected
        .entry((AFTER.into(), "cargo test".into()))
        .or_default() += 1;
    setup.transcript("s1", &calls);
    assert_eq!(setup.routes(), expected);

    // The hook's own replacements were routed at the time.
    setup.transcript("s2", &replaced);
    let routed: u64 = setup
        .routes()
        .iter()
        .filter(|((route, _), _)| route == "through_justless")
        .map(|(_, n)| n)
        .sum();
    assert_eq!(routed, replaced.len() as u64);
}

/// The text report, `--plan` and `--json` count the same commands the same way,
/// and the what-if summary splits what did not go through Justless.
#[test]
fn discover_reports_what_the_current_build_would_do() {
    let setup = Setup::new();
    let big = setup.sh("sed -n 1,400p big.rs");
    let small = setup.sh("sed -n 1,20p big.rs");
    let calls = [
        ("sed -n 1,400p big.rs", big.clone()),
        ("sed -n 1,20p big.rs", small.clone()),
        ("cargo test", "x".repeat(4000)),
        ("python3 build.py", "y".repeat(500)),
        ("npm test", "ok\n[justless run-abc · exit 0]".to_owned()),
    ];
    let calls: Vec<_> = calls
        .iter()
        .map(|(command, output)| (command.to_string(), output.clone(), json!({})))
        .collect();
    setup.transcript("s1", &calls);
    let (big, small) = (big.len() as u64, small.len() as u64);

    let report: Value = serde_json::from_str(&setup.discover(&["--json", "--plan"])).unwrap();
    let what_if = &report["what_if"];
    for (key, commands, bytes) in [
        ("routed_before_running", 1, 4000),
        ("compacted_after_running", 1, big),
        ("as_written", 2, small + 500),
    ] {
        assert_eq!(
            what_if[key],
            json!({"commands": commands, "output_bytes": bytes}),
            "{key}"
        );
    }
    assert_eq!(what_if["commands"], 4);
    let rows = report["plan"].as_array().unwrap();
    let row = |label: &str| rows.iter().find(|r| r["label"] == label).unwrap();
    assert_eq!(row("sed")["output_bytes"], big + small);
    assert_eq!(row("sed")["compactable_now_bytes"], big);
    assert_eq!(row("sed")["compacted_after_running_bytes"], big);
    assert_eq!(row("cargo test")["compactable_now_bytes"], 4000);
    assert_eq!(row("cargo test")["compacted_after_running_bytes"], 0);
    assert_eq!(row("python3")["compactable_now_bytes"], 0);

    let report = setup.discover(&["--plan"]);
    has(&report, "\nCompacted after running: 1 command(s)");
    has(&report, "\nSupported, passed through whole: 1 command(s)");
    has(
        &report,
        "With the current build, of the 4 command(s) that did not go through Justless",
    );
    has(&report, "the command hook would route 1 (3.9 KB)");
    has(&report, "sed ×2");
    has(&report, " of it after running");
    for secret in ["add_000", "big.rs", "xxxxxxxx", "1,400p"] {
        lacks(&report, secret);
    }
}

#[test]
fn excluded_commands_stay_excluded_after_running() {
    let setup = Setup::new();
    let settings = "exclude_commands = [\"cargo test\", \"sed\"]\n";
    fs::write(setup.root.join(".justless.toml"), settings).unwrap();
    let tests = "test a ... ok\n".repeat(600);
    let calls = [
        ("cargo test 2>&1 | tail -n 600", tests, "cargo test"),
        (
            "sed -n 1,400p big.rs",
            setup.sh("sed -n 1,400p big.rs"),
            "sed",
        ),
    ];
    let mut expected = BTreeMap::new();
    for (command, output, label) in &calls {
        assert_eq!(setup.after_bash(command, output), None, "{command}");
        expected.insert(("excluded".to_owned(), label.to_string()), 1);
    }
    let calls: Vec<_> = calls
        .into_iter()
        .map(|(command, output, _)| (command.to_owned(), output, json!({})))
        .collect();
    setup.transcript("s1", &calls);
    assert_eq!(setup.routes(), expected);
}

/// Reads and searches in the forms agents use, inside the workspace and outside
/// it, count as compacted after running exactly when the hook compacts them.
#[test]
fn discover_agrees_with_the_hook_on_where_a_read_runs() {
    let setup = Setup::new();
    let outside = setup.root.parent().unwrap().join("outside");
    let outside = outside.display();
    for (command, compacts) in [
        ("sed -n 1,400p big.rs".to_owned(), true),
        ("head -n 400 big.rs".to_owned(), true),
        ("tail -n 400 big.rs".to_owned(), true),
        ("cd sub && sed -n 1,400p big.rs".to_owned(), true),
        ("git show HEAD:big.rs".to_owned(), true),
        // A file read reaches a file outside the workspace, but not by a
        // parent folder, and no search is rerun out there.
        (format!("sed -n 1,400p {outside}/big.rs"), true),
        (format!("cd {outside} && sed -n 1,400p big.rs"), true),
        (format!("cd {outside} && cat big.rs; echo done"), true),
        ("sed -n 1,400p ../outside/big.rs".to_owned(), false),
        (
            format!("cd {outside} && grep -rn value . | head -n 900"),
            false,
        ),
    ] {
        let output = setup.sh(&command);
        let hook = setup.after_bash(&command, &output).is_some();
        let route = setup.route(&command, &output);
        assert_eq!(hook, compacts, "{command}: the hook");
        // The command hook routes a `git show` of a file before it runs.
        let before = command.starts_with("git show");
        assert_eq!(
            route == "supported_not_routed",
            before,
            "{command}: {route}"
        );
        assert_eq!(route == AFTER, compacts && !before, "{command}: {route}");
    }
}

/// The post-run hook does not consult the permission rules, so a large result of
/// a command the command hook left to them is compacted after running.
#[test]
fn commands_left_to_permissions_are_compacted_after_running_when_large() {
    let setup = Setup::new();
    let tests: String = (0..400)
        .map(|i| format!("test tests::case_{i:03} ... ok\n"))
        .collect();
    assert!(setup.after_bash("cargo test", &tests).is_some());
    let mode = json!({"permissionMode": "default"});
    let calls = [
        ("true", String::new(), mode),
        ("cargo test", tests, json!({})),
        ("cargo test", "test result: ok".to_owned(), json!({})),
    ];
    let calls: Vec<_> = calls
        .into_iter()
        .map(|(command, output, extra)| (command.to_owned(), output, extra))
        .collect();
    setup.transcript("s1", &calls);
    let routes = setup.routes();
    let key = |route: &str| (route.to_owned(), "cargo test".to_owned());
    assert_eq!(routes.get(&key(AFTER)), Some(&1), "{routes:?}");
    assert_eq!(
        routes.get(&key("left_by_permissions")),
        Some(&1),
        "{routes:?}"
    );
}
