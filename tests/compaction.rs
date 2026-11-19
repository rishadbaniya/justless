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
