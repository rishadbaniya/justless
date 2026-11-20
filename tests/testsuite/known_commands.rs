//! Commands the hooks route for the ordinary preview alone, and the progress
//! lines their previews leave out. The tool here is a script that prints lines
//! written from Terraform's usual output, not captured from a run.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, executable, git, has, lacks, piped, run_id, text};

/// A workspace with a `terraform` on the path that refreshes 300 resources and
/// then prints a plan.
struct Lab {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
    tools: PathBuf,
}

const TERRAFORM: &str = r#"#!/bin/sh
i=0
while [ "$i" -lt 300 ]; do
  echo "aws_instance.web[$i]: Refreshing state... [id=i-0abc$i]"
  i=$((i + 1))
done
echo
echo "  # aws_instance.web[3] will be updated in-place"
echo "Plan: 0 to add, 1 to change, 0 to destroy."
"#;

/// A plan that refreshes one resource, warns 400 times, fails once and then
/// prints 400 more lines, so its preview is clipped whatever is left out.
const TOFU: &str = r#"#!/bin/sh
echo "aws_instance.web: Refreshing state... [id=i-0abc]"
i=0
while [ "$i" -lt 400 ]; do
  echo "Warning: Deprecated attribute in module.net[$i]"
  i=$((i + 1))
done
echo "Error: Unsupported argument on main.tf line 12"
i=0
while [ "$i" -lt 400 ]; do
  echo "  + resource \"aws_subnet\" \"s$i\" { cidr = known after apply }"
  i=$((i + 1))
done
exit 1
"#;

impl Lab {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let (root, home, tools) = (base.join("work"), base.join("home"), base.join("tools"));
        for dir in [&root, &home, &tools] {
            fs::create_dir_all(dir).unwrap();
        }
        executable(&tools.join("terraform"), TERRAFORM);
        executable(&tools.join("tofu"), TOFU);
        git(&root, &["init", "-q"]);
        Self {
            _temp: temp,
            root,
            home,
            tools,
        }
    }

    fn command(&self, program: &str) -> Command {
        let path = std::env::var("PATH").unwrap_or_default();
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("PATH", format!("{}:{path}", self.tools.display()))
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("CLAUDE_CONFIG_DIR", self.home.join("claude"))
            .env("CLAUDE_PROJECT_DIR", &self.root)
            .env("CODEX_HOME", self.home.join("codex"));
        command
    }

    fn hook(&self, agent: &str, mode: &str, input: Value) -> Value {
        let mut hook = self.command(BIN);
        hook.args([agent, mode]);
        let output = piped(&mut hook, input.to_string().as_bytes());
        assert!(output.status.success(), "{}", text(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
    }

    /// The command a client's command hook runs in place of `command`, if any.
    /// Codex names its permission mode; Claude Code's hook is asked as in a
    /// session whose rules already allow the command.
    fn before(&self, agent: &str, command: &str) -> Option<String> {
        let mut input = json!({
            "session_id": "known", "cwd": self.root, "hook_event_name": "PreToolUse",
            "tool_name": "Bash", "tool_input": {"command": command}, "tool_use_id": "call-1",
        });
        if agent == "hook-codex" {
            input["permission_mode"] = json!("default");
        }
        self.hook(agent, "before-command", input)["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .map(str::to_owned)
    }

    /// The post-run hook's replacement for `command`, whose result was `stdout`.
    fn after(&self, command: &str, stdout: &str) -> Option<String> {
        let input = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "session_id": "known",
            "cwd": self.root, "tool_input": {"command": command},
            "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false},
        });
        self.hook("hook-claude", "after-bash", input)["hookSpecificOutput"]["updatedToolOutput"]
            ["stdout"]
            .as_str()
            .map(str::to_owned)
    }

    /// What the script prints.
    fn printed(&self) -> String {
        let output = self.command("terraform").arg("plan").output().unwrap();
        text(&output.stdout)
    }
}

#[test]
fn the_command_hook_routes_a_listed_command_and_leaves_its_other_forms() {
    let lab = Lab::new();
    for command in [
        "terraform plan",
        "pnpm install",
        "npx prettier --check .",
        "gh pr list",
    ] {
        let routed = lab.before("hook-claude", command).expect(command);
        assert!(
            routed.starts_with("justless run --hook") && routed.ends_with(&format!("-- {command}")),
            "{routed}"
        );
    }
    // A command that changes what it manages, follows a stream or opens a browser
    // runs as written.
    for command in [
        "terraform apply",
        "docker compose logs -f",
        "gh pr view 7 --web",
        "npm run dev",
    ] {
        assert_eq!(lab.before("hook-claude", command), None, "{command}");
    }
    // Codex runs a rewritten command without asking, so it takes none of these.
    let tested = lab.before("hook-codex", "cargo test").expect("cargo test");
    assert!(tested.ends_with("-- cargo test"), "{tested}");
    for command in [
        "terraform plan",
        "pnpm install",
        "cargo fmt",
        "df -h",
        "gh pr list",
    ] {
        assert_eq!(lab.before("hook-codex", command), None, "{command}");
    }
}

#[test]
fn a_run_leaves_progress_lines_out_of_the_preview_and_keeps_them_in_the_log() {
    let lab = Lab::new();
    let output = lab
        .command(BIN)
        .args(["run", "--hook", "--", "terraform", "plan"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    let preview = text(&output.stdout);
    has(&preview, "Plan: 0 to add, 1 to change, 0 to destroy.");
    has(&preview, "# aws_instance.web[3] will be updated in-place");
    has(&preview, "300 filtered");
    lacks(&preview, "Refreshing state...");
    assert!(preview.len() < 600, "{preview}");

    let log = lab
        .command(BIN)
        .args(["log", &run_id(&output)])
        .output()
        .unwrap();
    assert_eq!(text(&log.stdout), lab.printed());
}

#[test]
fn the_post_run_hook_previews_a_listed_command_that_ran_as_written() {
    let lab = Lab::new();
    let printed = lab.printed();
    let shown = lab.after("terraform plan", &printed).expect("a preview");
    has(&shown, "Plan: 0 to add, 1 to change, 0 to destroy.");
    has(&shown, "[justless run-");
    lacks(&shown, "aws_instance.web[150]: Refreshing state...");
    assert!(shown.len() < printed.len() / 4, "{shown}");

    // A line that also prints how the command ended keeps the clipped preview a
    // command no hook knows gets, with that last line in it.
    let ended = format!("{printed}exit=0\n");
    let shown = lab
        .after("terraform plan; echo \"exit=$?\"", &ended)
        .expect("a preview");
    has(&shown, "exit=0");
    has(&shown, "[justless run-");
    assert!(shown.len() < ended.len() / 2, "{shown}");
}

#[test]
fn a_clipped_run_keeps_the_ordinary_preview_with_its_failure_first() {
    let lab = Lab::new();
    let output = lab
        .command(BIN)
        .args(["run", "--hook", "--", "tofu", "plan"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let preview = text(&output.stdout);
    // Leaving out the one progress line does not make the rest fit, so the
    // preview is the one any run gets, which shows the failure among the warnings.
    has(&preview, "Error: Unsupported argument on main.tf line 12");
    lacks(&preview, "filtered");
}
