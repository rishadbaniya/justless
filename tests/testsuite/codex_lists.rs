//! Codex's command hook on the shapes its code-mode scripts write: newline lists,
//! reads inside lists, and their rewrites run beside the originals. Command hooks
//! pass every command through on Windows, so these run on Unix.
#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::common::{BIN, text};

struct Lab {
    _temp: TempDir,
    base: PathBuf,
    root: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("work");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        // These fixtures are sized for the outlines of small reads, as with a threshold of 0.
        fs::write(root.join(".justless.toml"), "read_outline_lines = 0\n").unwrap();
        fs::write(root.join("notes.md"), "# Notes\n\nshort\n").unwrap();
        fs::write(
            root.join("src/small.rs"),
            "fn a() {}\nfn b() {}\nfn c() {}\n",
        )
        .unwrap();
        let big: String = (0..60)
            .map(|i| format!("pub fn item_{i}(x: u32) -> u32 {{\n    x + {i}\n}}\n\n"))
            .collect();
        fs::write(root.join("src/big.rs"), big).unwrap();
        fs::write(root.join("delim.txt"), "a\n\\:\nb\n").unwrap();
        fs::write(base.join("outside.rs"), "fn outside() {}\n").unwrap();
        Lab {
            _temp: temp,
            base,
            root,
        }
    }

    /// A command with its own home, state and client folders, run in the root.
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        let bin = Path::new(BIN).parent().unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        command
            .current_dir(&self.root)
            .env("PATH", path)
            .env("HOME", self.base.join("home"))
            .env("XDG_STATE_HOME", self.base.join("state"))
            .env("CLAUDE_CONFIG_DIR", self.base.join("claude"))
            .env("CODEX_HOME", self.base.join("codex"))
            .env_remove("JUSTLESS_DISABLE");
        command
    }

    /// What `agent`'s hook makes of `line`: the rewritten command, or `None`.
    fn rewrite(&self, agent: &str, line: &str) -> Option<String> {
        let output = self
            .command(BIN)
            .args(["rewrite", line, "--agent", agent])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        let report = text(&output.stdout);
        let (_, rest) = report.split_once(": rewritten\n  ")?;
        Some(rest.split_once("\nWhy: ").unwrap().0.to_owned())
    }

    fn sh(&self, line: &str) -> Output {
        self.command("sh").args(["-c", line]).output().unwrap()
    }
}

#[test]
fn codex_rewrites_lists_of_reads_and_supported_commands_only() {
    let lab = Lab::new();
    let root = lab.root.display().to_string();
    let read = |range: &str, file: &str| {
        format!("justless --root {root} read --shell-range {range} -- {file}")
    };
    // (line, the rewrite, or None to leave it as written)
    let cases: Vec<(&str, Option<String>)> = vec![
        (
            "sed -n '1,2p' src/small.rs\nsed -n '1,200p' src/big.rs",
            Some(format!(
                "{}\n{}",
                read("1-2", "src/small.rs"),
                read("1-200", "src/big.rs")
            )),
        ),
        (
            "cat notes.md && rg -n fn src",
            Some(format!(
                "{} && justless run --hook -- rg -n fn src",
                read("+1", "notes.md")
            )),
        ),
        (
            "cd src\n\ncat small.rs; head -n 2 big.rs\n",
            Some(format!(
                "cd src\n\n{}; {}\n",
                read("+1", "src/small.rs"),
                read("1-2", "src/big.rs")
            )),
        ),
        (
            "cat src/small.rs | head -n 1\ngit status 2>&1\n",
            Some(format!(
                "{}\njustless run --hook --merge -- git status 2>&1\n",
                read("1-1", "src/small.rs")
            )),
        ),
        (
            "git status\ngit diff",
            Some("justless run --hook -- git status\njustless run --hook -- git diff".into()),
        ),
        // A supported command piped only into `head` keeps the `head`.
        (
            "rg -n fn src | head -n 5",
            Some("justless run --hook --window -- rg -n fn src | head -n 5".into()),
        ),
        (
            "git status\nrg -n 'fn|use' src|head -20",
            Some(
                "justless run --hook -- git status\njustless run --hook --window -- rg -n 'fn|use' src|head -20"
                    .into(),
            ),
        ),
        // `nl -ba FILE | sed -n` is a numbered read.
        (
            "nl -ba src/big.rs | sed -n '10,20p'",
            Some(read("10-20 --shell-numbered nl", "src/big.rs")),
        ),
        (
            "cat -n src/big.rs | head -n 30\ncat -n src/small.rs | sed -n '2,3p'",
            Some(format!(
                "{}\n{}",
                read("1-30 --shell-numbered cat", "src/big.rs"),
                read("2-3 --shell-numbered cat", "src/small.rs")
            )),
        ),
        (
            "nl -ba src/small.rs | sed -n '2,9p'\ngit status",
            Some(format!(
                "{}\njustless run --hook -- git status",
                read("2-3 --shell-numbered nl", "src/small.rs")
            )),
        ),
        // A read counted from the end, an unsupported command, a file outside the
        // workspace or missing, a line that goes on, a newline inside quotes, a
        // pipe into anything but a read, a here-document and a comment.
        ("cat notes.md; tail -n 5 src/small.rs", None),
        ("cat notes.md\npython3 x.py", None),
        ("cat ../outside.rs\ngit status", None),
        ("cat missing.rs && git status", None),
        ("rg -n fn src &&\nsed -n 1,2p src/small.rs", None),
        ("rg -n fn src |\nhead -n 2", None),
        ("rg -n 'a\nb' src", None),
        ("git status\nrg -n fn src | wc -l", None),
        ("git status\ncat <<EOF\nx\nEOF", None),
        ("git status\n# note\ngit diff", None),
        ("git status\ncd ..\ngit diff", None),
        // A `cd` the shell may skip leaves a read after it as written.
        ("rg -n fn src || cd src\ncat small.rs", None),
        ("rg -n nomatch src && cd src\ncat small.rs", None),
        // `head` with a file, without a count or after `2>&1`, another filter, two
        // pipes, an option that runs a program, and a JSON read.
        ("rg -n fn src | head -n 5 src/small.rs", None),
        ("rg -n fn src | head", None),
        ("rg -n fn src 2>&1 | head -n 5", None),
        ("rg -n fn src | tail -n 5", None),
        ("rg -n fn src | sed -n '1,5p'", None),
        ("rg -n fn src | head -n 5 | cat", None),
        ("rg --pre cat fn src | head -n 5", None),
        ("cargo metadata --format-version 1 | head -n 5", None),
        // `nl` without `-ba`, numbering after `sed`, and a section delimiter.
        ("nl src/small.rs | sed -n '1,2p'", None),
        ("sed -n '1,2p' src/small.rs | nl -ba", None),
        ("nl -ba delim.txt | sed -n '3,3p'", None),
    ];
    for (line, expected) in &cases {
        assert_eq!(&lab.rewrite("codex", line), expected, "codex: {line:?}");
        assert_eq!(&lab.rewrite("agy", line), expected, "agy: {line:?}");
    }
    // Newline lists and reads inside lists belong to Codex and Antigravity CLI.
    for agent in ["claude", "cursor", "gemini"] {
        assert_eq!(lab.rewrite(agent, "git status\ngit diff"), None, "{agent}");
    }
    assert_eq!(
        lab.rewrite("cursor", "cat notes.md && git status"),
        None,
        "cursor"
    );
}

#[test]
fn an_excluded_command_on_a_later_line_leaves_the_list() {
    let lab = Lab::new();
    fs::write(
        lab.root.join(".justless.toml"),
        "exclude_commands = [\"git diff\"]\n",
    )
    .unwrap();
    assert_eq!(lab.rewrite("codex", "git status\ngit diff"), None);
    assert_eq!(lab.rewrite("codex", "cat notes.md\ngit diff HEAD"), None);
    assert_eq!(
        lab.rewrite("codex", "git status\ngit diff | head -n 5"),
        None
    );
    assert!(lab.rewrite("codex", "cat notes.md\ngit status").is_some());
}

#[test]
fn a_rewritten_list_of_small_reads_prints_the_same_bytes() {
    let lab = Lab::new();
    for line in [
        "cat notes.md\nsed -n '2,3p' src/small.rs\ncd src && cat small.rs",
        "head -n 2 src/big.rs; cat src/small.rs | sed -n '1,2p;3,3p' | cat",
        "nl -ba src/big.rs | sed -n '9,30p;200,240p'\nnl -ba src/small.rs | sed -n '2,3p'",
        "cat -n src/big.rs | sed -n '9,30p'\ncat -n delim.txt | head -n 3",
        "cat notes.md; cd src && cat small.rs\nsed -n '1,2p' big.rs",
    ] {
        let rewritten = lab.rewrite("codex", line).expect(line);
        let (before, after) = (lab.sh(line), lab.sh(&rewritten));
        assert_eq!(after.status.code(), before.status.code(), "{rewritten}");
        assert_eq!(text(&after.stdout), text(&before.stdout), "{rewritten}");
        assert_eq!(text(&after.stderr), "", "{rewritten}");
    }
}

#[test]
fn a_window_keeps_head_and_the_exit_status_and_is_never_compared() {
    let lab = Lab::new();
    for i in 0..40 {
        let body: String = (0..30)
            .map(|j| format!("pub fn f_{i}_{j}() -> usize {{ {j} }}\n"))
            .collect();
        fs::write(lab.root.join(format!("src/m{i}.rs")), body).unwrap();
    }
    let line = "rg -n fn src | head -n 4";
    let rewritten = lab.rewrite("codex", line).unwrap();
    for _ in 0..2 {
        let after = lab.sh(&rewritten);
        assert_eq!(after.status.code(), lab.sh(line).status.code());
        let stdout = text(&after.stdout);
        assert_eq!(stdout.lines().count(), 4, "{stdout}");
        // The footer reaches the agent on stderr, which `head` does not cut.
        let stderr = text(&after.stderr);
        assert!(stderr.contains("[justless run-"), "{stderr}");
        assert!(
            !format!("{stdout}{stderr}").contains("as run-"),
            "{stdout}{stderr}"
        );
    }
    // A plain run after them compares with none of them, and then with itself.
    let plain = || lab.sh("justless run --hook -- rg -n fn src");
    let first = plain();
    assert!(!text(&first.stdout).contains("as run-"));
    let second = plain();
    assert!(
        text(&second.stdout).contains("as run-"),
        "{}",
        text(&second.stdout)
    );
    // `head` sets the status, with or without Justless.
    let missing = "rg -n fn nowhere | head -n 4";
    let rewritten = lab.rewrite("codex", missing).unwrap();
    assert_eq!(
        lab.sh(&rewritten).status.code(),
        lab.sh(missing).status.code()
    );
}

/// The run a compact read or report names, from its `Exact:` line or footer.
fn run_named(output: &str) -> Option<String> {
    let at = output.find("run-")?;
    let id = output[at..].split(|c: char| !c.is_ascii_alphanumeric() && c != '-');
    id.into_iter().next().map(str::to_owned)
}

#[test]
fn numbered_reads_print_what_cat_n_and_nl_print() {
    let lab = Lab::new();
    let long: String = (0..90)
        .map(|i| format!("pub fn wide_{i}(x: u32) -> u32 {{\n    x * {i}\n}}\n\n"))
        .collect();
    fs::write(lab.root.join("src/long.rs"), long).unwrap();
    let replay = |id: &str| {
        let output = lab.command(BIN).args(["log", id]).output().unwrap();
        text(&output.stdout)
    };
    for line in [
        "cat -n src/long.rs | sed -n '1,170p'",
        "cat -n src/long.rs | head -n 160",
        "nl -ba src/long.rs | sed -n '20,200p'",
        "cat -n src/small.rs | sed -n '2,3p'",
        "nl -ba src/small.rs | head -n 2",
    ] {
        let stdout = text(&lab.sh(line).stdout);
        let wide = stdout.lines().count() > 150;
        // Codex runs the read in its place.
        let rewritten = lab.rewrite("codex", line).expect(line);
        let after = lab.sh(&rewritten);
        let shown = text(&after.stdout);
        if wide {
            assert!(shown.len() < stdout.len() / 2, "{rewritten}: {shown}");
            let id = run_named(&shown).expect(&shown);
            assert_eq!(replay(&id), stdout, "{line}");
        } else {
            assert_eq!(shown, stdout, "{rewritten}");
        }
        // Claude Code's post-run hook outlines the printed bytes.
        let input = serde_json::json!({"hook_event_name": "PostToolUse", "tool_name": "Bash",
            "cwd": lab.root, "session_id": "numbered", "tool_input": {"command": line},
            "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false}});
        let mut hook = lab.command(BIN);
        hook.args(["hook-claude", "after-bash"]);
        let output = crate::common::piped(&mut hook, input.to_string().as_bytes());
        let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let replaced = answer["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str();
        match replaced {
            Some(outline) if wide => {
                let id = run_named(outline).expect(outline);
                assert_eq!(replay(&id), stdout, "{line}");
            }
            None if !wide => {}
            _ => panic!("{line}: {answer}"),
        }
    }
}
