//! Read shapes the hooks take as the reads they print: a read followed by a
//! no-op `| cat`, and a `cat` of several workspace files.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, has, piped, text};

/// A workspace with an outlined Rust file, a wide shell script no outline can
/// show, a small file, a file without a final newline, and copies in `sub/`.
fn fixture() -> TempDir {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    let rust: String = (0..200)
        .map(|i| format!("pub fn add_{i:03}(value: u64) -> u64 {{\n    value + {i}\n}}\n"))
        .collect();
    let script: String = (0..400)
        .map(|i| format!("echo \"step {i} of the deployment script\"\n"))
        .collect();
    let settings: String = (0..60)
        .map(|i| format!("key_{i:02} = \"the value of setting number {i}\"\n"))
        .collect();
    // A lockfile, whose whole-file outline a wide `cat` is rerouted to.
    let lock = fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock")).unwrap();
    fs::create_dir(dir.join("sub")).unwrap();
    for at in [dir.to_path_buf(), dir.join("sub")] {
        fs::write(at.join("big.rs"), &rust).unwrap();
        fs::write(at.join("wide.sh"), &script).unwrap();
        fs::write(at.join("small.txt"), "alpha\nβeta\n").unwrap();
        fs::write(at.join("notes.toml"), &settings).unwrap();
        fs::write(at.join("Cargo.lock"), &lock).unwrap();
    }
    fs::write(dir.join("part"), "no final newline").unwrap();
    sh(
        dir,
        "git init -q && git add . && git -c user.email=a@localhost -c user.name=t -c commit.gpgsign=false commit -qm f",
    );
    temp
}

fn sh(dir: &Path, script: &str) -> String {
    let output = Command::new("/bin/sh")
        .args(["-c", script])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{script}: {}",
        text(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Runs a hook in `dir`, with every user-level folder in `home`.
fn hook(home: &Path, dir: &Path, args: &str, input: &Value) -> Value {
    let mut command = Command::new(BIN);
    command
        .args(["--root", dir.to_str().unwrap()])
        .args(args.split(' '))
        .current_dir(dir)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("CODEX_HOME", home.join("codex"));
    let out = piped(&mut command, input.to_string().as_bytes());
    assert!(out.status.success(), "{}", text(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[derive(Clone, Copy, Debug)]
enum Hook {
    ClaudePre,
    CodexPre,
    ClaudePost,
}

/// The hook's answer for `command` run in `dir`, given the bytes `stdout`
/// printed after the command ran.
fn answer(home: &Path, dir: &Path, which: Hook, command: &str, stdout: &str) -> Value {
    let mut input = json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":dir,
        "session_id":"shapes","tool_input":{"command":command}});
    let args = match which {
        Hook::ClaudePre => "hook-claude before-command",
        Hook::CodexPre => "hook-codex before-command",
        Hook::ClaudePost => {
            input["hook_event_name"] = json!("PostToolUse");
            input["tool_response"] = json!({"stdout":stdout,"stderr":"","interrupted":false});
            "hook-claude after-bash"
        }
    };
    hook(home, dir, args, &input)
}

/// The run ids the answer names, and the answer with its folder and run ids
/// replaced, so answers from two workspaces compare.
fn normalized(answer: &Value, dir: &Path) -> (Vec<String>, String) {
    let text = answer.to_string().replace(dir.to_str().unwrap(), "ROOT");
    let ids = regex::Regex::new(r"run-[A-Za-z0-9]+").unwrap();
    let found = ids
        .find_iter(&text)
        .map(|m| m.as_str().to_owned())
        .collect();
    (found, ids.replace_all(&text, "run-ID").into_owned())
}

/// What `justless log ID` replays in `dir`.
fn replay(home: &Path, dir: &Path, id: &str) -> Vec<u8> {
    let out = Command::new(BIN)
        .args(["log", id])
        .current_dir(dir)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    out.stdout
}

fn runs(dir: &Path) -> usize {
    fs::read_dir(dir.join(".justless/runs")).map_or(0, |entries| entries.count())
}

#[test]
fn a_trailing_cat_is_answered_as_the_read_alone() {
    let home = TempDir::new().unwrap();
    let forms = [
        "sed -n '1,170p' FILE",
        "sed -n '1,20p;300,330p' FILE",
        "head -n 160 FILE",
        "tail -n 170 FILE",
        "cat FILE",
        "cat FILE | head -n 170",
        "cd sub && cat FILE",
        "git show HEAD:FILE",
    ];
    let (mut compacted, mut rerouted) = (0, [0, 0]);
    for form in forms {
        for (file, suffix) in [
            ("big.rs", " | cat"),
            ("wide.sh", "|cat -"),
            ("small.txt", " | cat"),
            ("Cargo.lock", " | cat"),
        ] {
            if file == "Cargo.lock" && !form.ends_with("cat FILE") {
                continue;
            }
            let read = form.replace("FILE", file);
            let piped = format!("{read}{suffix}");
            // Each side in its own workspace, so neither sees the other's runs.
            let (alone, with_cat) = (fixture(), fixture());
            let stdout = sh(alone.path(), &read);
            assert_eq!(sh(with_cat.path(), &piped), stdout);
            for which in [Hook::ClaudePre, Hook::CodexPre, Hook::ClaudePost] {
                let a = answer(home.path(), alone.path(), which, &read, &stdout);
                let b = answer(home.path(), with_cat.path(), which, &piped, &stdout);
                let (ids, a) = normalized(&a, alone.path());
                let (_, b) = normalized(&b, with_cat.path());
                // A blob no outline shortens keeps the run route, which would
                // drop the `| cat` and with it the pipeline's exit status.
                if matches!(which, Hook::CodexPre) && a.contains("justless run ") {
                    assert!(read.starts_with("git show") && b == "{}", "{a}");
                    continue;
                }
                assert_eq!(a, b, "{which:?} {piped}");
                match which {
                    Hook::ClaudePre => rerouted[0] += usize::from(a.contains(" read -- ")),
                    Hook::CodexPre => rerouted[1] += usize::from(a.contains("--shell-range")),
                    Hook::ClaudePost => {}
                }
                // A compacted read archives the exact bytes the pipe printed.
                if let (Hook::ClaudePost, Some(id)) = (which, ids.first()) {
                    let replayed = replay(home.path(), with_cat.path(), id);
                    assert_eq!(text(&replayed), stdout, "{piped}");
                    compacted += 1;
                }
            }
        }
    }
    // Every client's route is exercised, not only answers that leave the read.
    assert!(
        compacted >= 10 && rerouted[0] >= 1 && rerouted[1] >= 10,
        "{compacted} {rerouted:?}"
    );
}

#[test]
fn a_cat_of_several_files_reads_as_one_piece_per_file() {
    let home = TempDir::new().unwrap();
    let post = |dir: &Path, command: &str| {
        let stdout = sh(dir, command);
        let answer = answer(home.path(), dir, Hook::ClaudePost, command, &stdout);
        let view = answer["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .map(str::to_owned);
        (stdout, view)
    };
    for (command, headers) in [
        (
            "cat big.rs small.txt",
            &[
                "$ cat big.rs [stdout:1-600]",
                "$ cat small.txt [stdout:601-602]",
            ][..],
        ),
        (
            "cd sub && cat small.txt big.rs wide.sh",
            &[
                "$ cat small.txt [stdout:1-2]",
                "$ cat big.rs [stdout:3-602]",
                "$ cat wide.sh [stdout:603-1002]",
            ],
        ),
        (
            "cat part big.rs | cat",
            &["$ cat part [stdout:1-1]", "$ cat big.rs [stdout:1-600]"],
        ),
        (
            "cat notes.toml big.rs wide.sh",
            &[
                "$ cat notes.toml [stdout:1-60]",
                "$ cat big.rs [stdout:61-660]",
            ],
        ),
        (
            "grep -n absent small.txt || cat big.rs small.txt; head -n 1 small.txt",
            &[
                "$ cat big.rs [stdout:1-600]",
                "$ head -n 1 small.txt [stdout:603-603]",
            ],
        ),
    ] {
        let temp = fixture();
        let dir = temp.path();
        assert_eq!(
            answer(home.path(), dir, Hook::ClaudePre, command, ""),
            json!({})
        );
        let (stdout, view) = post(dir, command);
        let view = view.unwrap_or_else(|| panic!("{command} was not compacted"));
        for header in headers {
            has(&view, header);
        }
        assert!(view.len() * 4 < stdout.len(), "{view}");
        // The small file is shown as it is, the Rust file as its outline.
        if command.contains("small.txt") {
            has(&view, "alpha\nβeta\n");
        }
        if command.contains("notes.toml") {
            has(&view, &fs::read_to_string(dir.join("notes.toml")).unwrap());
        }
        has(&view, "big.rs lines 1-600\n  L1-3 function add_000");
        let (ids, _) = normalized(&json!(view), dir);
        assert_eq!(text(&replay(home.path(), dir, &ids[0])), stdout);
    }

    // Each file is recorded as a whole-file read would record it.
    let temp = fixture();
    let dir = temp.path();
    post(dir, "cat small.txt big.rs notes.toml").1.unwrap();
    let out = Command::new(BIN)
        .args(["resume", "--budget", "100000"])
        .current_dir(dir)
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .output()
        .unwrap();
    let resume = text(&out.stdout);
    has(&resume, "Read small.txt:1-2");
    has(&resume, "Outline big.rs:1-600");
    has(&resume, "Read notes.toml:1-60");

    // A pre-command client has no read that prints several files.
    assert_eq!(
        answer(home.path(), dir, Hook::CodexPre, "cat big.rs small.txt", ""),
        json!({})
    );

    // Outside the workspace, not UTF-8, filtered or flagged: left as printed.
    let outside = TempDir::new().unwrap();
    fs::write(
        outside.path().join("secret.rs"),
        "pub fn a() {}\n".repeat(400),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.rs"), dir.join("link.rs")).unwrap();
    fs::write(dir.join("latin.txt"), b"caf\xe9\n").unwrap();
    let before = runs(dir);
    for command in [
        "cat big.rs link.rs",
        "cat big.rs small.txt | head -n 300",
        "cat -n big.rs small.txt",
        "cat big.rs - small.txt",
        "cat big.rs ../x",
    ] {
        let stdout = Command::new("/bin/sh")
            .args(["-c", command])
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
            .stdout;
        let answer = answer(home.path(), dir, Hook::ClaudePost, command, &text(&stdout));
        assert_eq!(answer, json!({}), "{command}");
    }
    let lossy = format!(
        "{}caf\u{fffd}\n",
        fs::read_to_string(dir.join("big.rs")).unwrap()
    );
    let answer = answer(
        home.path(),
        dir,
        Hook::ClaudePost,
        "cat big.rs latin.txt",
        &lossy,
    );
    assert_eq!(answer, json!({}));
    assert_eq!(runs(dir), before);
}
