//! Compact views of ordinary previews: JSON tables, Cargo.lock dependency changes and
//! the generic floor. Each keeps the exact output one `justless log` away.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn workspace(settings: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".justless.toml"), settings).unwrap();
    dir
}

fn justless(root: &Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    output
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The run named by the footer on stderr.
fn run_id(output: &Output) -> String {
    let stderr = text(&output.stderr);
    let start = stderr.find("[justless run-").expect("run marker") + "[justless ".len();
    stderr[start..]
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .next()
        .unwrap()
        .to_owned()
}

#[test]
fn json_output_is_shown_as_tables_that_log_reverses() {
    let rows: Vec<String> = (0..30)
        .map(|i| {
            format!(
                "  {{\n    \"id\": {i},\n    \"name\": \"crate-{i}\",\n    \"kind\": \"lib\",\n    \"tag\": {}\n  }}",
                if i < 15 { "\"1\"" } else { "1" }
            )
        })
        .collect();
    let json = format!("[\n{}\n]\n", rows.join(",\n"));
    for (settings, header, first) in [
        (
            "",
            "30 objects; fields: id,name,kind=\"lib\",tag\n",
            "0,\"crate-0\",\"1\"\n",
        ),
        (
            "json_grouping = true\n",
            "30 objects; fields: id,name,kind=\"lib\",@tag\n",
            "@ \"1\"\n0,\"crate-0\"\n",
        ),
    ] {
        let dir = workspace(settings);
        let root = dir.path();
        fs::write(root.join("data.json"), &json).unwrap();
        // A hook-routed command reaches the agent directly.
        let output = justless(root, &["run", "--hook", "--", "cat", "data.json"]);
        let shown = text(&output.stdout);
        assert!(shown.starts_with("[JSON array as a table: "), "{shown}");
        assert!(shown.contains(header) && shown.contains(first), "{shown}");
        assert!(shown.len() < json.len() / 2, "{shown}");
        let log = justless(root, &["log", &run_id(&output)]);
        assert_eq!(text(&log.stdout), json);
        // A typed run that fits could feed a pipe, so it stays verbatim.
        let typed = justless(root, &["run", "--fresh", "--", "cat", "data.json"]);
        assert_eq!(text(&typed.stdout), json);
    }
}

#[test]
fn previews_collapse_progress_encoded_runs_and_repeats() {
    let dir = workspace("");
    let root = dir.path();
    let frames: String = (0..=100).map(|i| format!("\r{i:>3}%")).collect();
    let blob: String = (0..400).map(|i| ["Qm", "x9", "aZ", "k+"][i % 4]).collect();
    let noisy = format!(
        "fetch{frames}\n{}error: kept\nerror: kept\nkey {blob}\n",
        "waiting\n".repeat(200)
    );
    fs::write(root.join("noisy.txt"), &noisy).unwrap();
    let floored = format!(
        "100% [101 earlier progress frame(s) cut]\nwaiting\n[last line repeated 199 more time(s)]\n\
         error: kept\nerror: kept\nkey {}[… 776 base64 characters cut …]{}\n",
        &blob[..12],
        &blob[788..]
    );
    let output = justless(root, &["run", "--hook", "--", "cat", "noisy.txt"]);
    assert_eq!(text(&output.stdout), floored);
    let log = justless(root, &["log", &run_id(&output)]);
    assert_eq!(text(&log.stdout), noisy);
    // A typed run that fits could feed a pipe, so it stays verbatim, but a clipped
    // one folds its head and tail.
    let typed = justless(root, &["run", "--fresh", "--", "cat", "noisy.txt"]);
    assert_eq!(text(&typed.stdout), noisy);
    let clipped = justless(
        root,
        &[
            "run",
            "--fresh",
            "--max-bytes",
            "1024",
            "--",
            "cat",
            "noisy.txt",
        ],
    );
    let shown = text(&clipped.stdout);
    assert!(
        shown.starts_with(
            "100% [101 earlier progress frame(s) cut]\nwaiting\n[last line repeated 199"
        ),
        "{shown}"
    );
    assert!(
        shown.contains("error: kept\nerror: kept\nkey Qmx9"),
        "{shown}"
    );
}

/// The Claude Code post-run hook's rewrite of a successful Bash result, if any.
fn after_bash(root: &Path, command: &str, stdout: &str) -> Option<String> {
    use std::io::Write;
    let input = serde_json::json!({
        "hook_event_name": "PostToolUse", "tool_name": "Bash", "session_id": "s",
        "cwd": root, "tool_input": {"command": command},
        "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false},
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(["hook-claude", "after-bash"])
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    reply["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
        .as_str()
        .map(str::to_owned)
}

#[test]
fn the_post_run_hook_floors_large_output_of_unknown_commands() {
    let dir = workspace("");
    let root = dir.path().canonicalize().unwrap();
    let issues: Vec<String> = (0..200)
        .map(|i| {
            format!(
                r#"{{"number":{i},"state":"open","title":"Issue {i}","user":{{"login":"u{}"}}}}"#,
                i % 3
            )
        })
        .collect();
    let issues = format!("[{}]\n", issues.join(","));
    let progress = format!("{}done\n", "waiting for the build\n".repeat(600));
    for (command, stdout, shown) in [
        (
            "gh api repos/acme/widget/issues",
            issues.as_str(),
            Some("[JSON array as a table: "),
        ),
        (
            "python3 build.py",
            progress.as_str(),
            Some("waiting for the build\n[last line repeated 599 more time(s)]\ndone\n"),
        ),
        // Output an agent may edit or copy, and short output, stay whole.
        ("strings build.bin", progress.as_str(), None),
        ("python3 build.py", &progress[..6000], None),
        // A pipe or list is left to the supported-command rules.
        ("python3 build.py | tail -n 700", progress.as_str(), None),
    ] {
        let rewritten = after_bash(&root, command, stdout);
        match shown {
            Some(start) => {
                let compacted = rewritten.unwrap_or_else(|| panic!("{command} was not compacted"));
                assert!(compacted.starts_with(start), "{command}: {compacted}");
                assert!(compacted.len() < stdout.len() / 2, "{command}: {compacted}");
                let id = compacted
                    .rsplit_once("[justless ")
                    .unwrap()
                    .1
                    .split(' ')
                    .next()
                    .unwrap();
                let log = justless(&root, &["log", id]);
                assert_eq!(text(&log.stdout), stdout);
            }
            None => assert_eq!(rewritten, None, "{command}"),
        }
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    text(&output.stdout)
}

/// A Cargo.lock holding `(name, version, checksum, dependencies)` packages.
fn lockfile(packages: &[(&str, &str, &str, &str)]) -> String {
    let mut text = String::from("version = 4\n");
    for (name, version, checksum, deps) in packages {
        text.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\ndependencies = [{deps}]\n"
        ));
    }
    text
}

#[test]
fn cargo_lock_patches_name_dependency_changes_and_flag_substitutions() {
    let dir = workspace("");
    let root = dir.path();
    git(root, &["init", "-q"]);
    // Every fifth filler package is updated, so the patch has many hunks.
    let lock = |updated: bool, head: &[(&str, &str, &str, &str)]| {
        let names: Vec<String> = (0..40).map(|i| format!("pkg{i}")).collect();
        let mut packages = head.to_vec();
        packages.extend(names.iter().enumerate().map(|(i, name)| {
            let version = if updated && i % 5 == 0 {
                "1.0.1"
            } else {
                "1.0.0"
            };
            (name.as_str(), version, "00", "")
        }));
        lockfile(&packages)
    };
    let old = lock(
        false,
        &[
            ("tokio", "1.40.0", "aa", "\"mio\""),
            ("mio", "1.0.0", "bb", ""),
        ],
    );
    fs::write(root.join("Cargo.lock"), old).unwrap();
    fs::write(root.join("lib.rs"), "fn a() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    let new = lock(
        true,
        &[
            ("tokio", "1.41.0", "cc", "\"mio\", \"signal-hook-registry\""),
            ("mio", "1.0.0", "dd", ""),
            ("signal-hook-registry", "1.4.2", "ee", ""),
        ],
    );
    fs::write(root.join("Cargo.lock"), new).unwrap();
    let summary = "Cargo.lock dependencies: 1 added, 9 updated, 1 with changed dependencies, 1 source or checksum change(s)";
    let flag = "checksum changed on existing version mio 1.0.0: bb -> dd";
    // The working tree has no blob for the new side, so the file is read.
    let output = justless(root, &["run", "--hook", "--", "git", "diff"]);
    let shown = text(&output.stdout);
    for expected in [
        summary,
        flag,
        "pkg5 1.0.0 -> 1.0.1, tokio 1.40.0 -> 1.41.0\n",
        "dependencies: tokio +signal-hook-registry\n",
    ] {
        assert!(shown.contains(expected), "{expected}: {shown}");
    }
    let log = justless(root, &["log", &run_id(&output)]);
    assert_eq!(text(&log.stdout), git(root, &["diff"]));
    // A commit, and a patch too large to show whole, whose hunk summary describes
    // the lockfile with the flag on top.
    let large: String = (0..300).map(|i| format!("fn f{i}() {{}}\n")).collect();
    fs::write(root.join("lib.rs"), large).unwrap();
    git(root, &["commit", "-qam", "bump"]);
    let show = [
        "run",
        "--hook",
        "--",
        "git",
        "show",
        "HEAD",
        "--",
        "Cargo.lock",
    ];
    assert!(text(&justless(root, &show).stdout).contains(summary));
    let diff = ["run", "--hook", "--", "git", "diff", "HEAD~1"];
    let shown = text(&justless(root, &diff).stdout);
    assert!(
        shown.contains(&format!(" diff section(s)\n! Cargo.lock: {flag}\n")),
        "{shown}"
    );
    assert!(
        shown.contains(summary) && !shown.contains("Cargo.lock @@"),
        "{shown}"
    );
}
