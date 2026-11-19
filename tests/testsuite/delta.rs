//! `context --symbol`, `context --at` and `pack` in a session: a symbol read
//! before prints only what changed since, and prints whole again in a new
//! session or after compaction.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, has, lacks, piped, starts, text, words};

const SESSION_VARIABLES: [&str; 2] = ["CLAUDE_CODE_SESSION_ID", "JUSTLESS_SESSION"];

fn cli(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(BIN);
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
        text(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// The output of `line`, split at spaces into arguments.
fn ok(root: &Path, line: &str) -> String {
    ok_with(root, &words(line), &[])
}

fn hook(root: &Path, mode: &str, input: Value) {
    let mut command = Command::new(BIN);
    command.args(["hook-claude", mode]).current_dir(root);
    let output = piped(&mut command, input.to_string().as_bytes());
    assert!(output.status.success());
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

const PLAIN: &str = "context --symbol long_function";
const SYMBOL: &str = "context --symbol long_function --session s1";
const UNCHANGED: &str = "unchanged since you read it";

#[test]
fn rereads_print_one_line_or_only_the_changed_lines() {
    let dir = project(30);
    let root = dir.path();
    let plain = ok(root, PLAIN);
    let first = ok(root, SYMBOL);
    assert_eq!(first, plain, "the first view in a session is today's view");

    let again = ok(root, SYMBOL);
    assert_eq!(again.lines().count(), 1, "{again}");
    starts(&again, "src/lib.rs:6-38 · unchanged since you read it at ");
    // The printed command shows the whole symbol.
    let command = again.split("whole symbol: ").nth(1).unwrap();
    let command = command.lines().next().unwrap().replace('\'', "");
    let printed = ok(root, command.split_once(' ').unwrap().1);
    let expected: String = file_lines(root, (6, 38))
        .iter()
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(printed, expected);

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
    let delta = ok(root, SYMBOL);
    starts(
        &delta,
        "src/lib.rs:6-38 · 31 of 33 lines unchanged since you read it at ",
    );
    has(&delta, "\n+ 16 |     let value_10 = helper(100);\n");
    has(&delta, "\n-    |     let value_10 = helper(10);\n");
    has(&delta, "\n-    |     let value_20 = helper(20);\n");
    has(&delta, "\n+ 31 |     let extra = value_25 * 3;\n");
    // Far lines stay out.
    lacks(&delta, "value_5 ");
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

    let after = ok(root, SYMBOL);
    assert_eq!(after.lines().count(), 1, "{after}");
    has(&after, UNCHANGED);

    // A rewrite prints whole, as shown, when that is no longer than the delta.
    let text = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let rewritten = text.replace("= helper(", "= helper(1 + ");
    fs::write(root.join("src/lib.rs"), rewritten).unwrap();
    let rewrite = ok(root, SYMBOL);
    starts(&rewrite, "src/lib.rs:6-38 · changed since you read it at ");
    assert_eq!(whole_lines(&rewrite), file_lines(root, (6, 38)));
}

#[test]
fn a_line_ending_change_is_shown() {
    let dir = project(30);
    let root = dir.path();
    ok(root, SYMBOL);
    edit(root, "helper(4);\n", "helper(4);\r\n");
    let delta = ok(root, SYMBOL);
    lacks(&delta, "· unchanged since you read it at");
    has(&delta, "+ 10 |     let value_4 = helper(4);");
}

#[test]
fn moved_renamed_and_deleted_symbols() {
    let dir = project(12);
    let root = dir.path();
    ok(root, SYMBOL);
    // Moved down by two lines with the same text.
    let path = root.join("src/lib.rs");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("// moved\n\n{text}")).unwrap();
    let moved = ok(root, SYMBOL);
    starts(&moved, "src/lib.rs:8-22 · unchanged since you read it at ");
    has(&moved, ", then at lines 6-20 ·");

    // Renamed: the new name was never read, so it prints whole.
    edit(root, "pub fn long_function", "pub fn longer_function");
    let renamed = ok(root, "context --symbol longer_function --session s1");
    assert_eq!(renamed, ok(root, "context --symbol longer_function"));
    assert!(!cli(root, &words(SYMBOL), &[]).status.success());

    // Moved to another file: a different symbol, printed whole.
    edit(root, "pub fn longer_function", "pub fn long_function");
    ok(root, SYMBOL);
    fs::rename(&path, root.join("src/other.rs")).unwrap();
    let other = ok(root, SYMBOL);
    starts(&other, "src/other.rs:8-22\n");
    lacks(&other, "since you read");

    // Deleted: an error, not a stale view.
    fs::remove_file(root.join("src/other.rs")).unwrap();
    let gone = cli(root, &words(SYMBOL), &[]);
    assert!(!gone.status.success());
    assert!(gone.stdout.is_empty());
}

#[test]
fn new_sessions_and_compaction_print_whole_again() {
    let dir = project(10);
    let root = dir.path();
    let plain = ok(root, PLAIN);
    ok(root, SYMBOL);
    let other = "context --symbol long_function --session s2";
    assert_eq!(ok(root, other), plain, "another session prints whole");
    has(&ok(root, SYMBOL), UNCHANGED);
    has(&ok(root, other), UNCHANGED);
    let fresh = ok(root, &format!("{SYMBOL} --fresh"));
    starts(&fresh, "src/lib.rs:6-18\n");
    // Without a session nothing changes.
    assert_eq!(ok(root, PLAIN), plain);

    // The shell's Claude Code session ID is no session: subagents share it.
    let claude = [("CLAUDE_CODE_SESSION_ID", "8d1c")];
    assert_eq!(ok_with(root, &words(PLAIN), &claude), plain);
    assert_eq!(ok_with(root, &words(PLAIN), &claude), plain);

    // `resume` keeps the views; the compaction handoff forgets them all.
    ok(root, "resume");
    has(&ok(root, SYMBOL), UNCHANGED);
    let compact = json!({"hook_event_name":"PreCompact","trigger":"auto","cwd":root});
    hook(root, "before-compact", compact);
    assert_eq!(ok(root, SYMBOL), plain);
    assert_eq!(ok(root, other), plain);
    has(&ok(root, SYMBOL), UNCHANGED);
    let start = json!({"hook_event_name":"SessionStart","source":"compact","cwd":root});
    hook(root, "after-compact", start);
    assert_eq!(ok(root, SYMBOL), plain);
}

#[test]
fn budget_limits_never_hide_what_was_not_printed() {
    let dir = project(120);
    let root = dir.path();
    let budget = |bytes: usize| ok(root, &format!("{SYMBOL} --budget {bytes}"));
    let first = budget(700);
    assert!(first.ends_with("[context budget reached]\n"), "{first}");
    lacks(&first, "value_60 ");
    // Lines the budget cut were never seen, so they are printed now.
    let rest = budget(100_000);
    lacks(&rest, "· unchanged since you read it at");
    has(&rest, " lines unchanged since you read it at ");
    has(&rest, "|     let value_120 = helper(120);");
    has(&ok(root, SYMBOL), UNCHANGED);

    // A delta cut after its first change prints whole next time, so a
    // change it never showed, or one it showed and that went back, is seen.
    edit(root, "helper(3);", "helper(33);");
    edit(root, "helper(118);", "helper(1188);");
    let cut = budget(400);
    has(&cut, "helper(33)");
    lacks(&cut, "helper(1188)");
    edit(root, "helper(33);", "helper(3);");
    let next = budget(100_000);
    starts(&next, "src/lib.rs:6-128\n");
    has(&next, "\n  9 |     let value_3 = helper(3);\n");
    has(&next, "\n  124 |     let value_118 = helper(1188);\n");
}

#[test]
fn a_change_reverted_after_a_partial_view_is_shown() {
    let dir = project(30);
    let root = dir.path();
    ok(root, SYMBOL);
    edit(root, "helper(4);", "helper(44);");
    edit(root, "helper(24);", "helper(244);");
    // The window shows the first change but not the second.
    let window = ok(root, "context --at src/lib.rs:10 --session s1");
    has(&window, "helper(44)");
    lacks(&window, "helper(244)");
    edit(root, "helper(44);", "helper(4);");
    let view = ok(root, SYMBOL);
    has(&view, "|     let value_4 = helper(4);\n");
    has(&view, "helper(244)");
    lacks(&view, "· unchanged since");
}

#[test]
fn a_change_seen_in_an_overlapping_symbol_is_not_hidden_when_reverted() {
    let dir = project(4);
    let root = dir.path();
    let path = root.join("src/lib.rs");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("\npub mod inner {\n    pub fn first() -> u32 {\n        1\n    }\n\n    pub fn second() -> u32 {\n        2\n    }\n}\n");
    fs::write(&path, text).unwrap();
    let outer = "context --symbol inner --session s1";
    ok(root, outer);
    edit(root, "        2\n", "        22\n");
    // The method's own view shows the change; the module's record predates it.
    has(
        &ok(root, "context --symbol inner::second --session s1"),
        "        22",
    );
    edit(root, "        22\n", "        2\n");
    let again = ok(root, outer);
    lacks(&again, "· unchanged since");
    has(&again, "|         2\n");
}

#[test]
fn at_views_print_the_changed_window_lines() {
    let dir = project(20);
    let root = dir.path();
    let at = "context --at src/lib.rs:15-27 --session s1";
    let plain = ok(root, "context --at src/lib.rs:15-27");
    assert_eq!(ok(root, at), plain);
    let again = ok(root, at);
    has(&again, "\n  lines 12-26 unchanged since you read them at ");
    starts(&again, plain.lines().next().unwrap());
    edit(root, "helper(19);", "helper(199);");
    let delta = ok(root, at);
    has(
        &delta,
        "\n  14 of 15 lines unchanged since you read them at ",
    );
    has(&delta, "\n+ 25 |     let value_19 = helper(199);\n");
    has(&delta, "\n-    |     let value_19 = helper(19);\n");
    lacks(&delta, "value_10 ");

    // A window of a few short lines prints whole when that is no longer.
    let short = "context --at src/lib.rs:15 --session s1";
    ok(root, short);
    edit(root, "helper(9);", "helper(99);");
    let window = ok(root, short);
    assert_eq!(window, ok(root, "context --at src/lib.rs:15"));
    has(&window, "\n  15 |     let value_9 = helper(99);\n");
}

#[test]
fn packs_shorten_excerpts_shown_before() {
    let dir = project(6);
    let root = dir.path();
    let question = "what does long_function compute";
    let task = ["pack", question, "--session", "s1"];
    let pack = |args: &[&str]| ok_with(root, args, &[]);
    let first = pack(&task);
    assert_eq!(first, pack(&["pack", question]));
    has(&first, "| let doubled = value * 2;");
    let second = pack(&task);
    has(&second, "    1-3 | unchanged since you read them\n");
    lacks(&second, "| let doubled = value * 2;");
    assert_eq!(pack(&[&task[..], &["--fresh"]].concat()), first);
    edit(root, "value * 2", "value * 4");
    has(&pack(&task), "| let doubled = value * 4;");
    // Changed back, the lines differ from the last ones printed.
    edit(root, "value * 4", "value * 2");
    has(&pack(&task), "| let doubled = value * 2;");
}

#[test]
fn twins_that_share_a_name_are_remembered_apart() {
    let dir = project(4);
    let root = dir.path();
    let body = "    let first = 1;\n    let second = 2;\n    let third = 3;\n    first + second + third\n}\n";
    let path = root.join("src/lib.rs");
    let mut source = fs::read_to_string(&path).unwrap();
    for cfg in ["unix", "not(unix)"] {
        source.push_str(&format!("\n#[cfg({cfg})]\npub fn twin() -> u32 {{\n{body}"));
    }
    fs::write(&path, source).unwrap();
    let ambiguous = cli(root, &["context", "--symbol", "twin"], &[]);
    let message = text(&ambiguous.stderr);
    let selectors: Vec<&str> = message
        .split_whitespace()
        .filter(|word| word.starts_with("demo::twin@"))
        .map(|word| word.trim_end_matches(')'))
        .collect();
    assert_eq!(selectors.len(), 2, "{message}");
    let view = |selector: &str| ok(root, &format!("context --symbol {selector} --session s1"));
    view(selectors[0]);
    let second = view(selectors[1]);
    starts(&second, "src/lib.rs:22-28\n");
    lacks(&second, "since you read");
    has(&view(selectors[0]), UNCHANGED);
}

/// The Claude Code command hook's answer for `command`, with only `settings`
/// as the project's permission settings.
fn claude_command_hook(root: &Path, command: &str, extra: Value, settings: Option<&str>) -> Value {
    let home = TempDir::new().unwrap();
    let claude = root.join(".claude/settings.json");
    match settings {
        Some(settings) => {
            fs::create_dir_all(claude.parent().unwrap()).unwrap();
            fs::write(&claude, settings).unwrap();
        }
        None => {
            let _ = fs::remove_file(&claude);
        }
    }
    let mut input = json!({"hook_event_name":"PreToolUse","tool_name":"Bash",
        "cwd":root,"session_id":"0f3a-session","tool_input":{"command":command}});
    for (key, value) in extra.as_object().unwrap() {
        input[key] = value.clone();
    }
    let mut hook = Command::new(BIN);
    hook.args(["hook-claude", "before-command"])
        .current_dir(root)
        .env("CLAUDE_PROJECT_DIR", root)
        .env(
            "JUSTLESS_TEST_CLAUDE_MANAGED_DIR",
            home.path().join("managed"),
        )
        .env("HOME", home.path())
        .env_remove("CLAUDE_CONFIG_DIR");
    let output = piped(&mut hook, input.to_string().as_bytes());
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn the_claude_hook_names_the_session_and_subagent() {
    let dir = project(10);
    let root = dir.path();
    let read = "justless context --symbol long_function";
    let rewritten = |answer: &Value| {
        answer["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("{answer}"))
            .to_owned()
    };
    let auto = json!({"permission_mode":"auto"});
    let parent = rewritten(&claude_command_hook(root, read, auto.clone(), None));
    let digest = parent.rsplit(' ').next().unwrap().to_owned();
    assert_eq!(parent, format!("{read} --session {digest}"));
    let subagent = json!({"permission_mode":"auto","agent_id":"a41c","agent_type":"Explore"});
    let child = rewritten(&claude_command_hook(root, read, subagent, None));
    assert_ne!(child, parent, "a subagent gets its own session");
    let pack = "justless --root . pack \"what is long_function\"";
    assert_eq!(
        rewritten(&claude_command_hook(
            root,
            &format!("{pack} 2>&1"),
            auto.clone(),
            None
        )),
        format!("{pack} --session {digest} 2>&1")
    );
    // The parent's read does not stand for the subagent's, nor the reverse.
    let run = |line: &str| ok(root, line.split_once(' ').unwrap().1);
    let plain = ok(root, PLAIN);
    assert_eq!(run(&parent), plain);
    assert_eq!(run(&child), plain);
    has(&run(&parent), UNCHANGED);
    has(&run(&child), UNCHANGED);

    // Without a reported mode the rules are not read, so nothing changes.
    assert_eq!(claude_command_hook(root, read, json!({}), None), json!({}));
    // Other commands, compound lines and named sessions stay as they are.
    for command in [
        "justless query long_function",
        "justless context --symbol long_function --session mine",
        "cd src && justless context --symbol long_function",
        "justless pack -- what",
        "cargo-justless context --symbol long_function",
    ] {
        let answer = claude_command_hook(root, command, auto.clone(), None);
        assert_eq!(answer, json!({}), "{command}");
    }
    // The original command is judged against the permission rules first.
    let default = json!({"permission_mode":"default"});
    let answer = claude_command_hook(root, read, default.clone(), None);
    assert_eq!(answer, json!({}));
    let allow = r#"{"permissions":{"allow":["Bash(justless context:*)"]}}"#;
    let allowed = claude_command_hook(root, read, default, Some(allow));
    assert_eq!(rewritten(&allowed), parent);
    assert_eq!(allowed["hookSpecificOutput"]["permissionDecision"], "allow");
    let deny = r#"{"permissions":{"deny":["Bash(justless context:*)"]}}"#;
    assert_eq!(claude_command_hook(root, read, auto, Some(deny)), json!({}));
}

#[test]
fn stats_count_the_bytes_deltas_kept_out() {
    let dir = project(30);
    let root = dir.path();
    let plain = ok(root, PLAIN);
    ok(root, SYMBOL);
    let unchanged = ok(root, SYMBOL);
    edit(root, "helper(12);", "helper(120);");
    let delta = ok(root, SYMBOL);
    let stats: Value = serde_json::from_str(&ok(root, "stats --json")).unwrap();
    let counted = &stats["changed_since_read"];
    assert_eq!(counted["views"], 2, "{counted}");
    let shown = (unchanged.len() + delta.len()) as u64;
    assert_eq!(counted["shown_bytes"], shown, "{counted}");
    // What the views without a session print.
    let whole = (plain.len() + ok(root, PLAIN).len()) as u64;
    assert_eq!(counted["whole_bytes"], whole, "{counted}");
    has(&ok(root, "stats"), "Changed since read: 2 view(s) printed ");
}
