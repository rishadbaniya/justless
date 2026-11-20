#![cfg(unix)]
//! Read lists that mix in output the shell prints itself, listings, `cat -n`,
//! `git status` and `git log`: each piece is reproduced and kept exactly.

use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

use crate::common::{BIN, has, piped, text};

/// A repository with a committed source file, ignored and untracked files, and
/// a home directory of its own for Git and the agents.
struct Fixture {
    temp: TempDir,
    home: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            temp: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
        };
        let dir = fixture.dir();
        // These fixtures are sized for the outlines of small reads, as with a threshold of 0.
        fs::write(dir.join(".justless.toml"), "read_outline_lines = 0\n").unwrap();
        fs::create_dir_all(fixture.home().join(".config/git")).unwrap();
        fs::write(fixture.home().join(".config/git/ignore"), "*.tmp\n").unwrap();
        fs::create_dir_all(dir.join("sub/deep")).unwrap();
        let source = (0..200)
            .map(|i| format!("pub fn add_{i:03}(value: u64) -> u64 {{\n    value + {i}\n}}\n"))
            .collect::<String>();
        fs::write(dir.join("big.rs"), source).unwrap();
        fs::write(dir.join("notes"), "alpha\nβeta\n").unwrap();
        fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        fixture.sh(
            "git init -q && git add big.rs notes .gitignore && \
             git -c user.email=a@localhost -c user.name=fixture -c commit.gpgsign=false commit -qm first && \
             echo 'scratch/' >> .git/info/exclude",
        );
        for file in [
            "new.txt",
            "skip.log",
            "skip.tmp",
            "scratch/x",
            "sub/.hidden",
        ] {
            fs::create_dir_all(dir.join(file).parent().unwrap()).unwrap();
            fs::write(dir.join(file), "x\n").unwrap();
        }
        fs::write(dir.join("notes"), "alpha\nβeta\ngamma\n").unwrap();
        fixture
    }

    fn dir(&self) -> &Path {
        self.temp.path()
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn isolate<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        let home = self.home();
        command
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("CODEX_HOME", home.join("codex"))
            .env("PWD", self.dir())
            .current_dir(self.dir())
    }

    /// The stdout of `script` run by the shell as Claude Code would run it.
    fn sh(&self, script: &str) -> String {
        let output = self
            .isolate(&mut Command::new("/bin/sh"))
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    }

    fn hook(&self, command: &str, stdout: Option<&str>) -> Value {
        let (mode, event) = match stdout {
            Some(_) => ("after-bash", "PostToolUse"),
            None => ("before-command", "PreToolUse"),
        };
        let response = stdout.map(|s| json!({"stdout": s, "stderr": "", "interrupted": false}));
        let input = json!({"hook_event_name": event, "tool_name": "Bash", "cwd": self.dir(),
            "session_id": "mixed", "tool_input": {"command": command}, "tool_response": response});
        let mut cmd = Command::new(BIN);
        self.isolate(&mut cmd)
            .args(["--root", self.dir().to_str().unwrap(), "hook-claude", mode]);
        let out = piped(&mut cmd, input.to_string().as_bytes());
        assert!(out.status.success(), "{}", text(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn view(&self, command: &str, stdout: &str) -> Option<String> {
        let out = self.hook(command, Some(stdout));
        out["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .map(str::to_owned)
    }
}

#[test]
fn each_kind_of_piece_keeps_its_exact_range_and_replay() {
    let fixture = Fixture::new();
    // Each piece's header, and the script that prints its bytes alone.
    let pieces = [
        ("git status --short", "git status --short"),
        ("git log --oneline -1", "git log --oneline -1"),
        ("echo ----", "echo ----"),
        ("printf 'a\\tb%%\\n'", "printf 'a\\tb%%\\n'"),
        ("pwd", "pwd"),
        ("true", "true"),
        ("wc -l big.rs notes", "wc -l big.rs notes"),
        ("ls", "ls"),
        ("ls -A sub", "ls -A sub"),
        ("cat -n big.rs", "cat -n big.rs"),
        ("echo", "echo"),
        ("pwd", "cd sub && pwd"),
    ];
    let list = pieces.map(|(_, script)| script).join("; ");
    let raw = fixture.sh(&list);
    // The shell's own view of the untracked files the recheck has to match.
    assert!(raw.contains("?? new.txt\n"), "{raw}");
    assert!(
        !raw.contains("?? skip") && !raw.contains("?? scratch"),
        "{raw}"
    );
    let view = fixture
        .view(&list, &raw)
        .unwrap_or_else(|| panic!("{list} passed through"));
    assert!(view.len() < raw.len() / 2, "{view}");
    // Every piece names its lines of the archived stdout, and small ones stay whole.
    let mut line = 1;
    for (header, script) in pieces {
        let bytes = fixture.sh(script);
        let span = match bytes.lines().count() {
            0 => " [no output]".to_owned(),
            n => format!(" [stdout:{line}-{}]", line + n - 1),
        };
        has(&view, &format!("$ {header}{span}\n"));
        if bytes.len() < 200 {
            has(&view, &format!("{span}\n{bytes}"));
        }
        line += bytes.lines().count();
    }
    has(&view, "big.rs lines 1-600\n");
    let id = view
        .lines()
        .find_map(|l| l.strip_prefix("Exact: justless log "))
        .unwrap();
    let replay = fixture
        .isolate(&mut Command::new(BIN))
        .args(["log", id])
        .output()
        .unwrap();
    assert_eq!(text(&replay.stdout), raw);
}

#[test]
fn the_command_hook_keeps_its_route_for_a_mixed_list() {
    let fixture = Fixture::new();
    // Left as written, as a list of reads alone always was.
    for list in [
        "sed -n 1,600p big.rs; echo ----; pwd; wc -l notes; ls; cat -n notes",
        "cat big.rs && printf 'x\\n' && true",
    ] {
        assert_eq!(fixture.hook(list, None), json!({}), "{list}");
    }
    // A note before one supported command keeps that command's report.
    let changed = fs::read_to_string(fixture.dir().join("big.rs")).unwrap();
    fs::write(fixture.dir().join("big.rs"), changed.replace('+', "-")).unwrap();
    let noted = "echo note; git diff";
    let view = fixture.view(noted, &fixture.sh(noted)).unwrap();
    assert!(!view.contains("$ echo note"), "{view}");
    // A command the hook routes on its own is still routed in the list.
    let routed = fixture.hook("sed -n 1,600p big.rs; git status --short", None);
    let command = routed["hookSpecificOutput"]["updatedInput"]["command"].as_str();
    let command = command.unwrap_or_default();
    assert!(
        command.starts_with("sed -n 1,600p big.rs; justless run --hook ")
            && command.ends_with(" -- git status --short"),
        "{routed}"
    );
}

#[test]
fn a_piece_whose_output_differs_passes_the_list_through() {
    let fixture = Fixture::new();
    let read = "sed -n 1,600p big.rs";
    let tamper = |piece: &str, from: &str, to: &str| {
        let list = format!("{read}; {piece}");
        let raw = fixture.sh(&list);
        assert!(raw.contains(from), "{piece}: {from:?} not in output");
        // The untampered list compacts, so only the change keeps it whole.
        assert!(fixture.view(&list, &raw).is_some(), "{list}");
        // Change the piece's own output, after the read's.
        let at = raw.rfind(from).unwrap();
        (
            list,
            format!("{}{to}{}", &raw[..at], &raw[at + from.len()..]),
        )
    };
    for (list, stdout) in [
        tamper("echo ----", "----\n", "---\n"),
        tamper("printf 'x\\n'", "x\n", "y\n"),
        tamper("cd sub && pwd", "/sub\n", "/elsewhere\n"),
        tamper("wc -l notes", "3", "2"),
        tamper("ls", "new.txt\n", ""),
        tamper("cat -n notes", "     3\tgamma", "     3\tdelta"),
        tamper("git status --short", "?? new.txt\n", ""),
        tamper("git log --oneline -1", "first", "second"),
    ] {
        assert_eq!(fixture.view(&list, &stdout), None, "{list}");
    }
    // A listing of a path outside the workspace is never rerun.
    let outside = format!("{read}; ls /");
    let raw = fixture.sh(&outside);
    assert_eq!(fixture.view(&outside, &raw), None);
}

#[test]
fn rerun_pieces_leave_the_repository_as_it_was() {
    let fixture = Fixture::new();
    let dir = fixture.dir();
    let list = "sed -n 1,600p big.rs; git status --short; git log --oneline -1 -- big.rs";
    let raw = fixture.sh(list);
    // An fsmonitor and an index hook the real repository configures never run,
    // and the real index is not refreshed for a file whose mtime changed.
    fixture.sh("git config core.fsmonitor 'touch fsmonitor-ran; false' && \
         printf '#!/bin/sh\\ntouch hook-ran\\n' > .git/hooks/post-index-change && \
         chmod +x .git/hooks/post-index-change && touch -t 202001010000 big.rs");
    let index = fs::read(dir.join(".git/index")).unwrap();
    assert!(fixture.view(list, &raw).is_some());
    assert!(!dir.join("fsmonitor-ran").exists());
    assert!(!dir.join("hook-ran").exists());
    assert_eq!(fs::read(dir.join(".git/index")).unwrap(), index);
}
