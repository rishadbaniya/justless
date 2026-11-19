//! Machine-wide setup: hook kill switches and `justless rewrite`, the project
//! registry, and user-level hooks. Every command runs with `HOME`, the state home
//! and the Claude Code and Codex config homes inside the test's own folder.
//! Command hooks pass every command through on Windows, so these run on Unix.
#![cfg(unix)]
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Lab {
    _temp: TempDir,
    base: PathBuf,
    home: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        Lab {
            _temp: temp,
            base,
            home,
        }
    }

    /// A Cargo project under the lab, with optional `.justless.toml` settings.
    fn project(&self, name: &str, settings: &str) -> PathBuf {
        let root = self.base.join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        if !settings.is_empty() {
            fs::write(root.join(".justless.toml"), settings).unwrap();
        }
        root
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_justless"));
        command
            .args(args)
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env(
                "JUSTLESS_TEST_CLAUDE_MANAGED_DIR",
                self.base.join("managed"),
            )
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("JUSTLESS_DISABLE");
        command
    }

    fn output(
        &self,
        cwd: &Path,
        args: &[&str],
        input: Option<&Value>,
        env: &[(&str, &str)],
    ) -> Output {
        let mut child = self
            .command(cwd, args)
            .envs(env.iter().copied())
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
        child.wait_with_output().unwrap()
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.output(cwd, args, None, &[]);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn hook(&self, cwd: &Path, args: &[&str], input: &Value, env: &[(&str, &str)]) -> String {
        let output = self.output(cwd, args, Some(input), env);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

fn bash(cwd: &Path, command: &str, mode: Option<&str>) -> Value {
    let mut input = json!({
        "hook_event_name": "PreToolUse", "tool_name": "Bash", "session_id": "",
        "cwd": cwd, "tool_input": {"command": command},
    });
    if let Some(mode) = mode {
        input["permission_mode"] = json!(mode);
    }
    input
}

fn routed(answer: &str) -> Option<String> {
    let answer: Value = serde_json::from_str(answer).unwrap();
    answer["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .map(str::to_owned)
}

#[test]
fn kill_switches_leave_commands_alone_and_rewrite_explains_the_hook() {
    let lab = Lab::new();
    let root = lab.project(
        "work",
        "exclude_commands = [\"cargo bench\", \"^git (log|show)\"]\n",
    );
    let codex = |command: &str| {
        lab.hook(
            &root,
            &["hook-codex", "before-command"],
            &bash(&root, command, None),
            &[],
        )
    };
    assert_eq!(
        routed(&codex("cargo test")).as_deref(),
        Some("justless run --hook -- cargo test")
    );
    for command in [
        "cargo bench",
        "RUST_LOG=1 cargo bench --all",
        "git log -3",
        "cargo bench | tail",
    ] {
        assert_eq!(codex(command).trim(), "{}", "{command}");
    }
    // Codex runs a rewritten list without asking, so one excluded command keeps the
    // whole list as written. Claude Code checks the rewrite, so the rest is routed.
    assert_eq!(codex("cargo bench && cargo test").trim(), "{}");
    let claude = lab.hook(
        &root,
        &["hook-claude", "before-command"],
        &bash(
            &root,
            "cargo bench && cargo test",
            Some("bypassPermissions"),
        ),
        &[],
    );
    assert_eq!(
        routed(&claude).as_deref(),
        Some("cargo bench && justless run --hook -- cargo test")
    );

    // The post-run hook leaves an excluded command's output alone.
    let long: String = (0..400)
        .map(|line| format!("test case_{line} ... ok\n"))
        .collect();
    let after = |command: &str| {
        let input = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "session_id": "s", "cwd": root,
            "tool_input": {"command": command},
            "tool_response": {"stdout": long, "stderr": "", "interrupted": false, "isImage": false},
        });
        lab.hook(&root, &["hook-claude", "after-bash"], &input, &[])
    };
    assert_ne!(after("cargo test 2>&1 | tail -n 400").trim(), "{}");
    assert_eq!(after("cargo bench 2>&1 | tail -n 400").trim(), "{}");

    // `JUSTLESS_DISABLE` answers every hook with its client's no-op.
    let off = [("JUSTLESS_DISABLE", "1")];
    let input = bash(&root, "cargo test", Some("bypassPermissions"));
    assert_eq!(
        lab.hook(&root, &["hook-claude", "before-command"], &input, &off)
            .trim(),
        "{}"
    );
    assert_eq!(
        lab.hook(
            &root,
            &["hook-claude", "before-compact"],
            &json!({"hook_event_name": "PreCompact"}),
            &off
        ),
        ""
    );
    assert_eq!(
        lab.hook(&root, &["hook-agy", "before-command"], &json!({}), &off)
            .trim(),
        r#"{"decision":"allow"}"#
    );
    assert_eq!(
        lab.hook(&root, &["hook-cursor", "before-command"], &json!({}), &off)
            .trim(),
        "{}"
    );
    assert!(
        lab.hook(
            &root,
            &["hook-claude", "before-command"],
            &input,
            &[("JUSTLESS_DISABLE", "0")]
        )
        .contains("justless run")
    );

    // `rewrite` prints the hook's own rewrite and records nothing.
    fs::create_dir_all(root.join(".justless")).unwrap();
    let report = lab.run(&root, &["rewrite", "--agent", "codex", "cargo test"]);
    assert!(
        report.starts_with("Codex hook: rewritten\n  justless run --hook -- cargo test\n"),
        "{report}"
    );
    let report = lab.run(
        &root,
        &["rewrite", "--agent", "codex", "cargo bench && cargo test"],
    );
    assert!(
        report.contains("only when every command in it is supported and none is excluded"),
        "{report}"
    );
    assert!(
        report.contains("Excluded: `cargo bench` matches `cargo bench`"),
        "{report}"
    );
    let report = lab.run(&root, &["rewrite", "git show HEAD"]);
    assert!(
        report.contains("matches `^git (log|show)` in exclude_commands"),
        "{report}"
    );
    let report = lab.run(&root, &["rewrite", "ls", "--agent", "codex"]);
    assert!(
        report.contains("no command in it is one the hook routes"),
        "{report}"
    );
    // Claude Code's verdict: no rule allows `cargo test` in default mode.
    let report = lab.run(&root, &["rewrite", "cargo test"]);
    assert!(
        report.contains("unchanged\nWhy: Claude Code's permission rules"),
        "{report}"
    );
    assert!(
        report.contains("Permission: no rule allows it in default mode"),
        "{report}"
    );
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::write(
        root.join(".claude/settings.json"),
        r#"{"permissions":{"allow":["Bash(cargo test:*)"]}}"#,
    )
    .unwrap();
    let report = lab.run(&root, &["rewrite", "cargo test", "--mode", "acceptEdits"]);
    assert!(
        report.contains("rewritten\n  justless run --hook -- cargo test\n"),
        "{report}"
    );
    assert!(report.contains("allow rules cover every command in it in acceptEdits mode, so the hook allows the rewrite"), "{report}");
    assert!(!root.join(".justless/activity.json").exists());

    // An invalid expression is a settings error, not a silent pass.
    fs::write(root.join(".justless.toml"), "exclude_commands = [\"^(\"]\n").unwrap();
    let output = lab.output(&root, &["run", "--", "true"], None, &[]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("invalid exclude_commands regex"), "{error}");
}
