//! Shell reads of files outside the workspace, such as an agent's scratch
//! folder. The post-run hook rereads the file the shell read and shows the
//! list's outline or excerpt over an archived run, but only for files the
//! outline of an outside Read may replace, and never through a link.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, git, has, lacks, piped, text};

/// A workspace `work`, a scratch folder beside it and a home of its own.
struct Lab {
    _temp: TempDir,
    root: PathBuf,
    scratch: PathBuf,
    home: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let (root, scratch, home) = (base.join("work"), base.join("scratch"), base.join("home"));
        let rust: String = (0..300)
            .map(|i| format!("pub fn add_{i:03}(value: u64) -> u64 {{\n    value + {i}\n}}\n"))
            .collect();
        let notes: String = (0..400)
            .map(|i| format!("note {i}: plain words, no code\n"))
            .collect();
        for dir in [
            root.clone(),
            scratch.join("sub"),
            scratch.join("tool-results"),
            home.join(".cache"),
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(root.join("big.rs"), &rust).unwrap();
        for file in [
            "big.rs",
            "sub/big.rs",
            ".hidden.rs",
            "tool-results/saved.rs",
        ] {
            fs::write(scratch.join(file), &rust).unwrap();
        }
        fs::write(scratch.join("sub/notes.txt"), notes).unwrap();
        fs::write(scratch.join("small.rs"), "pub fn one() {}\n").unwrap();
        fs::write(scratch.join("latin.txt"), b"caf\xe9\n".repeat(1200)).unwrap();
        // Built at run time, so no scanner takes it for a real token.
        let token = format!("{}{}{}", "gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let keys = format!("{rust}const TOKEN: &str = \"{token}\";\n");
        fs::write(scratch.join("keys.rs"), keys).unwrap();
        fs::write(home.join("big.rs"), &rust).unwrap();
        fs::write(home.join(".cache/big.rs"), &rust).unwrap();
        symlink(scratch.join("big.rs"), scratch.join("link.rs")).unwrap();
        symlink(&scratch, root.join("away")).unwrap();
        git(&root, &["init", "-q"]);
        Self {
            _temp: temp,
            root,
            scratch,
            home,
        }
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("CLAUDE_CONFIG_DIR", self.home.join("claude"))
            .env("CODEX_HOME", self.home.join("codex"));
        command
    }

    /// What the shell prints for `script` in the workspace.
    fn sh(&self, script: &str) -> String {
        let output = self
            .command("/bin/sh")
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{script}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The post-run hook's replacement for `command`, whose result was `stdout`.
    fn after(&self, command: &str, stdout: &str) -> Option<String> {
        let input = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "session_id": "outside",
            "cwd": self.root, "tool_input": {"command": command},
            "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false},
        });
        let mut hook = self.command(BIN);
        hook.args(["hook-claude", "after-bash"]);
        let output = piped(&mut hook, input.to_string().as_bytes());
        assert!(output.status.success(), "{}", text(&output.stderr));
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        let view = reply["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str();
        view.map(str::to_owned)
    }

    fn justless(&self, args: &[&str]) -> String {
        let output = self.command(BIN).args(args).output().unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout)
    }

    /// The source `resume` lists as read or outlined.
    fn reads(&self) -> Vec<String> {
        self.justless(&["resume", "--budget", "100000"])
            .lines()
            .filter(|line| line.starts_with("Read ") || line.starts_with("Outline "))
            .map(str::to_owned)
            .collect()
    }

    /// The archived runs, without the folder a report is staged in.
    fn runs(&self) -> usize {
        fs::read_dir(self.root.join(".justless/runs")).map_or(0, |entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("run-"))
                .count()
        })
    }
}

fn path(dir: &Path) -> &str {
    dir.to_str().unwrap()
}

#[test]
fn a_read_outside_the_workspace_is_verified_archived_and_replayed() {
    let lab = Lab::new();
    let (scratch, home) = (path(&lab.scratch), path(&lab.home));
    for (command, code) in [
        (format!("sed -n 1,400p {scratch}/big.rs"), true),
        (format!("cat {scratch}/big.rs"), true),
        (format!("head -n 400 {scratch}/big.rs | cut -c1-60"), true),
        (format!("sed -n 1,400p {scratch}/big.rs | cat"), true),
        (format!("nl -ba {scratch}/big.rs | sed -n 1,300p"), true),
        (
            format!("cat {scratch}/big.rs {scratch}/sub/notes.txt"),
            true,
        ),
        (format!("cd {scratch} && sed -n 1,400p big.rs"), true),
        (format!("cd {scratch}/sub && cat notes.txt"), false),
        (
            format!("cd {scratch} && sed -n 1,200p big.rs; echo ----; head -n 300 sub/notes.txt"),
            true,
        ),
        // Under the home folder, but not in one of its dot folders.
        (format!("sed -n 1,400p {home}/big.rs"), true),
    ] {
        let raw = lab.sh(&command);
        let view = lab
            .after(&command, &raw)
            .unwrap_or_else(|| panic!("{command} stayed as printed"));
        assert!(view.len() * 2 < raw.len(), "{command}: {view}");
        if code {
            has(&view, "  L1-3 function add_000\n");
        }
        // `expand`, `read` and `edit` take workspace files only, so the view
        // names the archived run and nothing else.
        lacks(&view, "justless expand");
        lacks(&view, "justless read");
        let exact = view
            .lines()
            .find(|line| line.starts_with("Exact: "))
            .unwrap();
        let id = exact.split_whitespace().last().unwrap();
        assert_eq!(lab.justless(&["log", id]), raw, "{command}");
    }
    // A file outside the workspace is never recorded as read.
    assert_eq!(lab.reads(), Vec::<String>::new());

    // In one list with a workspace file, only the workspace file is recorded.
    let mixed = format!("sed -n 1,200p big.rs; sed -n 1,200p {scratch}/big.rs");
    let raw = lab.sh(&mixed);
    let view = lab.after(&mixed, &raw).unwrap();
    has(&view, "big.rs lines 1-200\n");
    has(&view, &format!("{scratch}/big.rs lines 1-200\n"));
    assert_eq!(lab.reads(), ["Outline big.rs:1-200"]);
}

#[test]
fn files_the_outside_read_rules_exclude_stay_as_printed() {
    let lab = Lab::new();
    let (scratch, home) = (path(&lab.scratch), path(&lab.home));
    for command in [
        // A dotfile, a saved tool result and a dot folder of the home folder.
        format!("cat {scratch}/.hidden.rs"),
        format!("cd {scratch} && cat .hidden.rs"),
        format!("cat {scratch}/tool-results/saved.rs"),
        format!("cat {home}/.cache/big.rs"),
        // A link outside the workspace, and a link out of it.
        format!("cat {scratch}/link.rs"),
        "cd away && sed -n 1,400p big.rs".to_owned(),
        "cat away/big.rs".to_owned(),
        "cat big.rs away/big.rs".to_owned(),
        // Back out of the workspace by a parent folder.
        "cat ../scratch/big.rs".to_owned(),
        "cd ../scratch && cat big.rs".to_owned(),
        // Only file reads leave the workspace: nothing is rerun out there.
        format!("cd {scratch} && grep -n add_ big.rs"),
        format!("grep -n add_ {scratch}/big.rs"),
        format!("cd {scratch} && wc -l big.rs; cat big.rs"),
        format!("cd {scratch} && ls; cat big.rs"),
        format!("cd {scratch} && pwd; cat big.rs"),
        // A secret the archive's preview would redact, bytes that are not
        // UTF-8, and a read small enough to keep.
        format!("cat {scratch}/keys.rs"),
        format!("cat {scratch}/latin.txt"),
        format!("cat {scratch}/small.rs"),
    ] {
        let raw = lab.sh(&command);
        assert_eq!(lab.after(&command, &raw), None, "{command}");
        assert_eq!(lab.runs(), 0, "{command} left a run");
    }
    // Output that is not the file's lines is not attributed to it.
    let command = format!("sed -n 1,400p {scratch}/big.rs");
    let raw = lab.sh(&command).replacen("add_007", "add_700", 1);
    assert_eq!(lab.after(&command, &raw), None);
    assert_eq!(lab.runs(), 0);
    assert_eq!(lab.reads(), Vec::<String>::new());
}
