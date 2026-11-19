//! `context --symbol`, `context --at` and `pack` in a session: a symbol read
//! before prints only what changed since, and prints whole again in a new
//! session or after compaction.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const SESSION_VARIABLES: [&str; 4] = [
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "JUSTLESS_ORIGIN_UNKNOWN",
    "JUSTLESS_SESSION",
];

fn cli(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_justless"));
    command.args(args).current_dir(root);
    for name in SESSION_VARIABLES {
        command.env_remove(name);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn ok_with(root: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let output = cli(root, args, env);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn ok(root: &Path, args: &[&str]) -> String {
    ok_with(root, args, &[])
}

fn hook(root: &Path, mode: &str, input: serde_json::Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(["hook-claude", mode])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
}

/// A crate whose `long_function` has `lines` numbered statements and calls
/// `helper`, which spans three lines.
fn project(lines: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    let mut text = String::from(
        "pub fn helper(value: u32) -> u32 {\n    let doubled = value * 2;\n    doubled + 1\n}\n\npub fn long_function() -> u32 {\n",
    );
    for i in 1..=lines {
        text.push_str(&format!("    let value_{i} = helper({i});\n"));
    }
    text.push_str(&format!("    value_1 + value_{lines}\n}}\n"));
    fs::write(dir.path().join("src/lib.rs"), text).unwrap();
    dir
}

fn edit(root: &Path, from: &str, to: &str) {
    let path = root.join("src/lib.rs");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(from), "{from}");
    fs::write(path, text.replacen(from, to, 1)).unwrap();
}

/// `FILE:A-B` from the first line of a symbol view.
fn span(view: &str) -> (usize, usize) {
    let first = view.lines().next().unwrap();
    let range = first.split(' ').next().unwrap().rsplit(':').next().unwrap();
    let (a, b) = range.split_once('-').unwrap();
    (a.parse().unwrap(), b.parse().unwrap())
}

/// The text of each `  N | text` line of a whole view.
fn whole_lines(view: &str) -> Vec<String> {
    view.lines()
        .skip(1)
        .map(|line| line.split_once(" | ").unwrap().1.to_owned())
        .collect()
}

/// Runs the `whole symbol:` command a view printed.
fn expand_printed(root: &Path, view: &str) -> String {
    let command = view
        .split("whole symbol: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap();
    let words: Vec<String> = command
        .split(' ')
        .skip(1)
        .map(|word| word.trim_matches('\'').to_owned())
        .collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    ok(root, &words)
}

/// Rebuilds the symbol's new lines from the lines of an earlier whole view and
/// a delta view, checking each printed line against its place.
fn apply(old: &[String], delta: &str) -> Vec<String> {
    let (a, b) = span(delta);
    let mut inserted = vec![None; b - a + 1];
    let mut context = Vec::new();
    let mut removed = Vec::new();
    for line in delta.lines().skip(1) {
        if line.trim() == "…" {
            continue;
        }
        let (mark, rest) = line.split_at(1);
        let (number, text) = rest[1..].split_once(" | ").unwrap();
        match mark {
            "+" => inserted[number.trim().parse::<usize>().unwrap() - a] = Some(text.to_owned()),
            "-" => removed.push(text.to_owned()),
            _ => context.push((number.trim().parse::<usize>().unwrap(), text.to_owned())),
        }
    }
    let mut kept = Vec::new();
    let mut removed = removed.into_iter().peekable();
    for line in old {
        if removed.peek() == Some(line) {
            removed.next();
        } else {
            kept.push(line.clone());
        }
    }
    assert!(
        removed.next().is_none(),
        "every removed line was in the old view"
    );
    let mut kept = kept.into_iter();
    let new: Vec<String> = inserted
        .into_iter()
        .map(|line| line.unwrap_or_else(|| kept.next().unwrap()))
        .collect();
    assert!(kept.next().is_none(), "every kept line has a place");
    for (number, text) in context {
        assert_eq!(new[number - a], text, "line {number}");
    }
    new
}

fn file_lines(root: &Path, (a, b): (usize, usize)) -> Vec<String> {
    fs::read_to_string(root.join("src/lib.rs"))
        .unwrap()
        .lines()
        .skip(a - 1)
        .take(b - a + 1)
        .map(str::to_owned)
        .collect()
}

const SYMBOL: [&str; 5] = ["context", "--symbol", "long_function", "--session", "s1"];

#[test]
fn rereads_print_one_line_or_only_the_changed_lines() {
    let dir = project(30);
    let root = dir.path();
    let plain = ok(root, &["context", "--symbol", "long_function"]);
    let first = ok(root, &SYMBOL);
    assert_eq!(first, plain, "the first view in a session is today's view");

    let again = ok(root, &SYMBOL);
    assert_eq!(again.lines().count(), 1, "{again}");
    assert!(
        again.starts_with("src/lib.rs:6-38 · unchanged since you read it at "),
        "{again}"
    );
    let printed = expand_printed(root, &again);
    let expected: String = file_lines(root, (6, 38))
        .iter()
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(
        printed, expected,
        "the printed command shows the whole symbol"
    );

    // Change one line, delete another and add one.
    edit(
        root,
        "let value_10 = helper(10);",
        "let value_10 = helper(100);",
    );
    edit(root, "    let value_20 = helper(20);\n", "");
    edit(
        root,
        "    let value_25 = helper(25);\n",
        "    let value_25 = helper(25);\n    let extra = value_25 * 3;\n",
    );
    let delta = ok(root, &SYMBOL);
    assert!(
        delta.starts_with("src/lib.rs:6-38 · 31 of 33 lines unchanged since you read it at "),
        "{delta}"
    );
    assert!(
        delta.contains("\n+ 16 |     let value_10 = helper(100);\n"),
        "{delta}"
    );
    assert!(
        delta.contains("\n-    |     let value_10 = helper(10);\n"),
        "{delta}"
    );
    assert!(
        delta.contains("\n-    |     let value_20 = helper(20);\n"),
        "{delta}"
    );
    assert!(
        delta.contains("\n+ 31 |     let extra = value_25 * 3;\n"),
        "{delta}"
    );
    assert!(!delta.contains("value_5 "), "far lines stay out:\n{delta}");
    assert!(
        delta.len() < plain.len(),
        "{} vs {}",
        delta.len(),
        plain.len()
    );
    assert_eq!(
        apply(&whole_lines(&first), &delta),
        file_lines(root, span(&delta)),
        "the delta rebuilds the new text exactly"
    );

    let after = ok(root, &SYMBOL);
    assert_eq!(after.lines().count(), 1, "{after}");
    assert!(after.contains("unchanged since you read it"), "{after}");

    // A rewrite prints whole, as shown, when that is no longer than the delta.
    let text = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        text.replace("= helper(", "= helper(1 + "),
    )
    .unwrap();
    let rewrite = ok(root, &SYMBOL);
    assert!(
        rewrite.starts_with("src/lib.rs:6-38 · changed since you read it at "),
        "{rewrite}"
    );
    assert_eq!(whole_lines(&rewrite), file_lines(root, (6, 38)));
}

#[test]
fn a_line_ending_change_is_shown() {
    let dir = project(30);
    let root = dir.path();
    ok(root, &SYMBOL);
    edit(root, "helper(4);\n", "helper(4);\r\n");
    let delta = ok(root, &SYMBOL);
    assert!(
        !delta.contains("· unchanged since you read it at"),
        "{delta}"
    );
    assert!(
        delta.contains("+ 10 |     let value_4 = helper(4);"),
        "{delta}"
    );
}

#[test]
fn moved_renamed_and_deleted_symbols() {
    let dir = project(12);
    let root = dir.path();
    ok(root, &SYMBOL);
    // Moved down by two lines with the same text.
    let path = root.join("src/lib.rs");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("// moved\n\n{text}")).unwrap();
    let moved = ok(root, &SYMBOL);
    assert!(
        moved.starts_with("src/lib.rs:8-22 · unchanged since you read it at "),
        "{moved}"
    );
    assert!(moved.contains(", then at lines 6-20 ·"), "{moved}");

    // Renamed: the new name was never read.
    edit(root, "pub fn long_function", "pub fn longer_function");
    let renamed = ok(
        root,
        &["context", "--symbol", "longer_function", "--session", "s1"],
    );
    assert_eq!(
        renamed,
        ok(root, &["context", "--symbol", "longer_function"]),
        "a renamed symbol prints whole"
    );
    assert!(!cli(root, &SYMBOL, &[]).status.success());

    // Moved to another file: a different symbol, printed whole.
    edit(root, "pub fn longer_function", "pub fn long_function");
    ok(root, &SYMBOL);
    fs::rename(&path, root.join("src/other.rs")).unwrap();
    let other = ok(root, &SYMBOL);
    assert!(other.starts_with("src/other.rs:8-22\n"), "{other}");
    assert!(!other.contains("since you read"), "{other}");

    // Deleted: an error, not a stale view.
    fs::remove_file(root.join("src/other.rs")).unwrap();
    let gone = cli(root, &SYMBOL, &[]);
    assert!(!gone.status.success());
    assert!(gone.stdout.is_empty());
}

#[test]
fn new_sessions_and_compaction_print_whole_again() {
    let dir = project(10);
    let root = dir.path();
    let plain = ok(root, &["context", "--symbol", "long_function"]);
    ok(root, &SYMBOL);
    let other = ["context", "--symbol", "long_function", "--session", "s2"];
    assert_eq!(ok(root, &other), plain, "another session prints whole");
    assert!(ok(root, &SYMBOL).contains("unchanged since you read it"));
    assert!(ok(root, &other).contains("unchanged since you read it"));
    assert!(
        ok(
            root,
            &[
                "context",
                "--symbol",
                "long_function",
                "--session",
                "s1",
                "--fresh"
            ]
        )
        .starts_with("src/lib.rs:6-18\n"),
        "--fresh prints whole"
    );
    // Without a session nothing changes.
    assert_eq!(ok(root, &["context", "--symbol", "long_function"]), plain);

    // Claude Code's session ID, except in a subagent or the MCP server.
    let claude = [("CLAUDE_CODE_SESSION_ID", "8d1c")];
    let symbol = ["context", "--symbol", "long_function"];
    assert_eq!(ok_with(root, &symbol, &claude), plain);
    assert!(ok_with(root, &symbol, &claude).contains("unchanged since you read it"));
    let child = [
        ("CLAUDE_CODE_SESSION_ID", "8d1c"),
        ("CLAUDE_CODE_CHILD_SESSION", "1"),
    ];
    assert_eq!(ok_with(root, &symbol, &child), plain);
    let mcp = [
        ("CLAUDE_CODE_SESSION_ID", "8d1c"),
        ("JUSTLESS_ORIGIN_UNKNOWN", "1"),
    ];
    assert!(!ok_with(root, &symbol, &mcp).contains("since you read"));

    // `resume` keeps the views; the compaction handoff forgets them all.
    ok(root, &["resume"]);
    assert!(ok(root, &SYMBOL).contains("unchanged since you read it"));
    hook(
        root,
        "before-compact",
        serde_json::json!({"hook_event_name":"PreCompact","trigger":"auto","cwd":root}),
    );
    assert_eq!(ok(root, &SYMBOL), plain);
    assert_eq!(ok_with(root, &symbol, &claude), plain);
    assert!(ok(root, &SYMBOL).contains("unchanged since you read it"));
    hook(
        root,
        "after-compact",
        serde_json::json!({"hook_event_name":"SessionStart","source":"compact","cwd":root}),
    );
    assert_eq!(ok(root, &SYMBOL), plain);
}

#[test]
fn budget_limits_never_hide_what_was_not_printed() {
    let dir = project(120);
    let root = dir.path();
    let small = [
        "context",
        "--symbol",
        "long_function",
        "--session",
        "s1",
        "--budget",
        "700",
    ];
    let first = ok(root, &small);
    assert!(first.ends_with("[context budget reached]\n"), "{first}");
    assert!(!first.contains("value_60 "));
    // Lines the budget cut were never seen, so they are printed now.
    let rest = ok(
        root,
        &[
            "context",
            "--symbol",
            "long_function",
            "--session",
            "s1",
            "--budget",
            "100000",
        ],
    );
    assert!(!rest.contains("· unchanged since you read it at"), "{rest}");
    assert!(
        rest.contains(" lines unchanged since you read it at "),
        "{rest}"
    );
    assert!(
        rest.contains("|     let value_120 = helper(120);"),
        "{rest}"
    );
    assert!(ok(root, &SYMBOL).contains("unchanged since you read it"));

    // A delta cut after its first change prints whole next time, so a
    // change it never showed, or one it showed and that went back, is seen.
    edit(root, "helper(3);", "helper(33);");
    edit(root, "helper(118);", "helper(1188);");
    let cut = ok(
        root,
        &[
            "context",
            "--symbol",
            "long_function",
            "--session",
            "s1",
            "--budget",
            "400",
        ],
    );
    assert!(cut.contains("helper(33)"), "{cut}");
    assert!(!cut.contains("helper(1188)"), "{cut}");
    edit(root, "helper(33);", "helper(3);");
    let whole = [
        "context",
        "--symbol",
        "long_function",
        "--session",
        "s1",
        "--budget",
        "100000",
    ];
    let next = ok(root, &whole);
    assert!(next.starts_with("src/lib.rs:6-128\n"), "{next}");
    assert!(
        next.contains("\n  9 |     let value_3 = helper(3);\n"),
        "{next}"
    );
    assert!(
        next.contains("\n  124 |     let value_118 = helper(1188);\n"),
        "{next}"
    );
}

#[test]
fn a_change_reverted_after_a_partial_view_is_shown() {
    let dir = project(30);
    let root = dir.path();
    ok(root, &SYMBOL);
    edit(root, "helper(4);", "helper(44);");
    edit(root, "helper(24);", "helper(244);");
    // The window shows the first change but not the second.
    let window = ok(
        root,
        &["context", "--at", "src/lib.rs:10", "--session", "s1"],
    );
    assert!(window.contains("helper(44)"), "{window}");
    assert!(!window.contains("helper(244)"), "{window}");
    edit(root, "helper(44);", "helper(4);");
    let view = ok(root, &SYMBOL);
    assert!(view.contains("|     let value_4 = helper(4);\n"), "{view}");
    assert!(view.contains("helper(244)"), "{view}");
    assert!(!view.contains("· unchanged since"), "{view}");
}

#[test]
fn a_change_seen_in_an_overlapping_symbol_is_not_hidden_when_reverted() {
    let dir = project(4);
    let root = dir.path();
    let path = root.join("src/lib.rs");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("\npub mod inner {\n    pub fn first() -> u32 {\n        1\n    }\n\n    pub fn second() -> u32 {\n        2\n    }\n}\n");
    fs::write(&path, text).unwrap();
    let outer = ["context", "--symbol", "inner", "--session", "s1"];
    let method = ["context", "--symbol", "inner::second", "--session", "s1"];
    ok(root, &outer);
    edit(root, "        2\n", "        22\n");
    // The method's own view shows the change; the module's record predates it.
    assert!(ok(root, &method).contains("        22"));
    edit(root, "        22\n", "        2\n");
    let again = ok(root, &outer);
    assert!(!again.contains("· unchanged since"), "{again}");
    assert!(again.contains("|         2\n"), "{again}");
}

#[test]
fn at_views_print_the_changed_window_lines() {
    let dir = project(20);
    let root = dir.path();
    let at = ["context", "--at", "src/lib.rs:15-27", "--session", "s1"];
    let plain = ok(root, &["context", "--at", "src/lib.rs:15-27"]);
    assert_eq!(ok(root, &at), plain);
    let again = ok(root, &at);
    assert!(
        again.contains("\n  lines 12-26 unchanged since you read them at "),
        "{again}"
    );
    assert!(again.starts_with(plain.lines().next().unwrap()), "{again}");
    edit(root, "helper(19);", "helper(199);");
    let delta = ok(root, &at);
    assert!(
        delta.contains("\n  14 of 15 lines unchanged since you read them at "),
        "{delta}"
    );
    assert!(
        delta.contains("\n+ 25 |     let value_19 = helper(199);\n"),
        "{delta}"
    );
    assert!(
        delta.contains("\n-    |     let value_19 = helper(19);\n"),
        "{delta}"
    );
    assert!(!delta.contains("value_10 "), "{delta}");

    // A window of a few short lines prints whole when that is no longer.
    let short = ["context", "--at", "src/lib.rs:15", "--session", "s1"];
    ok(root, &short);
    edit(root, "helper(9);", "helper(99);");
    let window = ok(root, &short);
    assert_eq!(window, ok(root, &["context", "--at", "src/lib.rs:15"]));
    assert!(
        window.contains("\n  15 |     let value_9 = helper(99);\n"),
        "{window}"
    );
}

#[test]
fn packs_shorten_excerpts_shown_before() {
    let dir = project(6);
    let root = dir.path();
    let task = ["pack", "what does long_function compute", "--session", "s1"];
    let first = ok(root, &task);
    assert_eq!(
        first,
        ok(root, &["pack", "what does long_function compute"])
    );
    assert!(first.contains("| let doubled = value * 2;"), "{first}");
    let second = ok(root, &task);
    assert!(
        second.contains("    1-3 | unchanged since you read them\n"),
        "{second}"
    );
    assert!(!second.contains("| let doubled = value * 2;"), "{second}");
    let mut fresh = task.to_vec();
    fresh.push("--fresh");
    assert_eq!(ok(root, &fresh), first);
    edit(root, "value * 2", "value * 4");
    assert!(ok(root, &task).contains("| let doubled = value * 4;"));
    // Changed back, the lines differ from the last ones printed.
    edit(root, "value * 4", "value * 2");
    let back = ok(root, &task);
    assert!(back.contains("| let doubled = value * 2;"), "{back}");
}

#[test]
fn twins_that_share_a_name_are_remembered_apart() {
    let dir = project(4);
    let root = dir.path();
    let body = "    let first = 1;\n    let second = 2;\n    let third = 3;\n    first + second + third\n}\n";
    let path = root.join("src/lib.rs");
    let mut text = fs::read_to_string(&path).unwrap();
    for cfg in ["unix", "not(unix)"] {
        text.push_str(&format!("\n#[cfg({cfg})]\npub fn twin() -> u32 {{\n{body}"));
    }
    fs::write(&path, text).unwrap();
    let ambiguous = cli(root, &["context", "--symbol", "twin"], &[]);
    let message = String::from_utf8_lossy(&ambiguous.stderr).into_owned();
    let selectors: Vec<&str> = message
        .split_whitespace()
        .filter(|word| word.starts_with("demo::twin@"))
        .map(|word| word.trim_end_matches(')'))
        .collect();
    assert_eq!(selectors.len(), 2, "{message}");
    let view = |selector: &str| ok(root, &["context", "--symbol", selector, "--session", "s1"]);
    view(selectors[0]);
    let second = view(selectors[1]);
    assert!(second.starts_with("src/lib.rs:22-28\n"), "{second}");
    assert!(!second.contains("since you read"), "{second}");
    assert!(view(selectors[0]).contains("unchanged since you read it"));
}
