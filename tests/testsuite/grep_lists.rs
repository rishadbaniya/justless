#![cfg(unix)]
//! Read lists with `grep` pieces of any common shape: context, recursion,
//! several patterns, counts, file lists and a final `head`. Each is rerun with
//! the grep the shell ran and kept only when its bytes match.

use serde_json::json;
use std::{fs, os::unix::fs::symlink, path::Path, process::Command};
use tempfile::TempDir;

use crate::common::{BIN, executable, has, lacks, piped, text};

/// A workspace with a large file to read and small sources to search, a link
/// to a directory outside it, and a home of its own.
struct Fixture {
    temp: TempDir,
    home: TempDir,
    outside: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            temp: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
            outside: TempDir::new().unwrap(),
        };
        let dir = fixture.dir();
        let source = (0..200)
            .map(|i| format!("pub fn add_{i:03}(value: u64) -> u64 {{\n    value + {i}\n}}\n"))
            .collect::<String>();
        fs::write(dir.join("big.rs"), source).unwrap();
        fs::create_dir_all(dir.join("src/deep")).unwrap();
        for (file, n) in [("src/a.rs", 3), ("src/b.rs", 5), ("src/deep/c.rs", 2)] {
            let body = (0..n)
                .map(|i| format!("// line {i}\nfn needle_{i}() {{}}\nlet Needle = {i};\n"))
                .collect::<String>();
            fs::write(dir.join(file), body).unwrap();
        }
        fs::write(dir.join("patterns"), "needle_1\nneedle_2\n").unwrap();
        fs::write(
            fixture.outside.path().join("secret.rs"),
            "fn needle_9() {}\n",
        )
        .unwrap();
        symlink(fixture.outside.path(), dir.join("src/away")).unwrap();
        symlink(fixture.outside.path(), dir.join("away")).unwrap();
        fixture
    }

    fn dir(&self) -> &Path {
        self.temp.path()
    }

    fn isolate<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        let home = self.home.path();
        command
            .env("HOME", home)
            .env("XDG_STATE_HOME", home.join("state"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("CODEX_HOME", home.join("codex"))
            .env_remove("CLAUDE_CODE_EXECPATH")
            .env("PWD", self.dir())
            .current_dir(self.dir())
    }

    /// The stdout of `script` run by the shell with the system grep.
    fn sh(&self, script: &str) -> String {
        let output = self
            .isolate(&mut Command::new("/bin/sh"))
            .args(["-c", script])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    }

    /// Claude Code's view of the list after its post-run hook, if compacted.
    fn view(&self, command: &str, stdout: &str) -> Option<String> {
        let input = json!({"hook_event_name": "PostToolUse", "tool_name": "Bash",
            "cwd": self.dir(), "session_id": "grep", "tool_input": {"command": command},
            "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false}});
        let mut cmd = Command::new(BIN);
        self.isolate(&mut cmd).args([
            "--root",
            self.dir().to_str().unwrap(),
            "hook-claude",
            "after-bash",
        ]);
        let out = piped(&mut cmd, input.to_string().as_bytes());
        assert!(out.status.success(), "{}", text(&out.stderr));
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        value["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .map(str::to_owned)
    }

    /// Stand in for the ugrep Claude Code's shell runs, where its installer
    /// keeps it: drop the options its `grep` function adds, and run grep.
    fn embedded(&self) {
        let versions = self.home.path().join(".local/share/claude/versions");
        fs::create_dir_all(&versions).unwrap();
        executable(
            &versions.join("1.0.0"),
            "#!/bin/sh\nshift 11\nexec /usr/bin/grep \"$@\"\n",
        );
        fs::create_dir_all(self.home.path().join(".local/bin")).unwrap();
        symlink(
            versions.join("1.0.0"),
            self.home.path().join(".local/bin/claude"),
        )
        .unwrap();
    }
}

const READ: &str = "sed -n 1,600p big.rs";

#[test]
fn each_grep_shape_keeps_its_exact_range_and_replay() {
    let fixture = Fixture::new();
    let read = fixture.sh(READ);
    for piece in [
        "grep -n 'fn needle_1' -A2 src/b.rs",
        "grep -n needle_1 -C1 src/a.rs src/b.rs | head -5",
        "grep -rn needle src",
        "grep -rnw -e needle_0 -e needle_2 src | head -n 3",
        "grep -ri NEEDLE src/deep",
        "grep -c needle src/a.rs src/b.rs",
        "grep -rl Needle src",
        "grep -rno 'needle_[0-9]' src | cut -c 1-12",
        "grep -nE 'needle_(1|3)' -m1 src/b.rs",
        "grep -n -f patterns src/b.rs",
        "grep -v needle src/a.rs | cat",
        "env -u GREP_COLOR grep -rhn needle src",
    ] {
        let list = format!("{READ}; {piece}");
        let raw = fixture.sh(&list);
        let own = fixture.sh(piece);
        assert!(!own.is_empty() && raw == format!("{read}{own}"), "{piece}");
        let view = fixture
            .view(&list, &raw)
            .unwrap_or_else(|| panic!("{piece} passed through"));
        let first = read.lines().count() + 1;
        let last = first + own.lines().count() - 1;
        has(&view, &format!("$ {piece} [stdout:{first}-{last}]\n{own}"));
        // The link out of the workspace is never followed.
        lacks(&view, "needle_9");
        let id = view
            .lines()
            .find_map(|l| l.strip_prefix("Exact: justless log "))
            .unwrap();
        let replay = fixture
            .isolate(&mut Command::new(BIN))
            .args(["log", id])
            .output()
            .unwrap();
        assert_eq!(text(&replay.stdout), raw, "{piece}");
    }
}

#[test]
fn a_grep_that_differs_or_reaches_outside_passes_the_list_through() {
    let fixture = Fixture::new();
    let outside = fixture.outside.path().join("secret.rs");
    let outside = outside.to_str().unwrap();
    for (piece, from, to) in [
        ("grep -rn needle src", "src/b.rs:5:", "src/b.rs:6:"),
        (
            "grep -c needle src/a.rs src/b.rs",
            "src/b.rs:5",
            "src/b.rs:4",
        ),
        ("grep -rn needle src | head -4", "needle_1", "needle_7"),
        // Claude Code's ugrep would not print a binary file's name.
        ("grep -rl Needle src", "src/deep/c.rs\n", ""),
    ] {
        let list = format!("{READ}; {piece}");
        let raw = fixture.sh(&list);
        assert!(fixture.view(&list, &raw).is_some(), "{piece}");
        let at = raw.rfind(from).unwrap();
        let changed = format!("{}{to}{}", &raw[..at], &raw[at + from.len()..]);
        assert_eq!(fixture.view(&list, &changed), None, "{piece}");
    }
    for piece in [
        // Paths and pattern files outside, and a walk that follows links.
        "grep -rn needle away".to_owned(),
        format!("grep -n needle {outside}"),
        format!("grep -n -f {outside} src/a.rs"),
        "grep -Rn needle src".into(),
        // Options that change what grep prints.
        "grep -n --color=always needle src/a.rs".into(),
        "LC_ALL=C grep -n needle src/a.rs".into(),
    ] {
        let list = format!("{READ}; {piece}");
        let raw = fixture.sh(&list);
        assert_eq!(fixture.view(&list, &raw), None, "{piece}");
    }
}

#[test]
fn files_claude_codes_ugrep_printed_in_any_order_are_checked_one_by_one() {
    let fixture = Fixture::new();
    let read = fixture.sh(READ);
    let block = |file: &str| fixture.sh(&format!("grep -Hn -A1 needle_2 {file}"));
    let (a, b) = (block("src/a.rs"), block("src/b.rs"));
    // The order the threads finished in, which no single rerun reproduces.
    let piece = "grep -rn -A1 needle_2 src";
    let list = format!("{READ}; {piece}");
    let shuffled = format!("{read}{b}{a}");
    let head = format!("{READ}; {piece} | head -3");
    // Two lines of one file and the first of the other.
    let cut = format!("{b}{}\n", a.lines().next().unwrap());
    assert_eq!(cut.lines().count(), 3);
    // The system grep prints files in its own order only.
    assert_eq!(fixture.view(&list, &shuffled), None);
    fixture.embedded();
    let view = fixture.view(&list, &shuffled).expect("compacted");
    has(&view, &format!("$ {piece} [stdout:"));
    assert!(fixture.view(&head, &format!("{read}{cut}")).is_some());
    // A file shown twice, or one left out, is not what ugrep printed.
    for stdout in [format!("{read}{b}{b}{a}"), format!("{read}{b}")] {
        assert_eq!(fixture.view(&list, &stdout), None);
    }
    // A grep a wrapper starts is the system grep, in its own order.
    let wrapped = format!("{READ}; env -u GREP_COLOR {piece}");
    assert_eq!(fixture.view(&wrapped, &shuffled), None);
    let raw = fixture.sh(&wrapped);
    assert!(fixture.view(&wrapped, &raw).is_some());
}
