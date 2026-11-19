//! Claude Code's post-run hook on a supported command piped into a display filter,
//! such as `git diff | sed -n 'A,Bp'` or `grep -rn X src | cut -c1-120`: the shown
//! text is archived and replaced by its family's report, every item of which points
//! at lines of that archive.
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, has, lacks, piped, text};

/// A workspace and a separate home for the hook's user-level state.
fn workspace() -> (TempDir, TempDir) {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".justless.toml"), "").unwrap();
    (dir, TempDir::new().unwrap())
}

fn justless(root: &Path, home: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut command = Command::new(BIN);
    command.args(args).current_dir(root);
    for (key, folder) in [
        ("HOME", ""),
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("CODEX_HOME", "codex"),
        ("XDG_STATE_HOME", "state"),
    ] {
        command.env(key, home.join(folder));
    }
    let output = piped(&mut command, input);
    assert!(output.status.success(), "{args:?}: {output:?}");
    output.stdout
}

/// What the hook shows for `command` whose Bash result was `stdout`, if it
/// replaced the result.
fn after_bash(root: &Path, home: &Path, command: &str, response: Value) -> Option<String> {
    let input = json!({
        "hook_event_name": "PostToolUse", "tool_name": "Bash", "session_id": "s",
        "cwd": root, "tool_input": {"command": command}, "tool_response": response,
    });
    let reply = justless(
        root,
        home,
        &["hook-claude", "after-bash"],
        input.to_string().as_bytes(),
    );
    let reply: Value = serde_json::from_slice(&reply).unwrap();
    let shown = reply["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str();
    shown.map(str::to_owned)
}

fn response(stdout: &str) -> Value {
    json!({"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false})
}

fn runs(root: &Path) -> usize {
    fs::read_dir(root.join(".justless/runs")).map_or(0, |dir| {
        dir.flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("run-"))
            .count()
    })
}

/// A patch of `files` files, each with one hunk of `lines` changed lines.
fn patch(files: usize, lines: usize) -> String {
    (0..files)
        .map(|i| {
            let removed: String = (0..lines).map(|n| format!("-    old_value({i}, {n});\n")).collect();
            let added: String = (0..lines).map(|n| format!("+    new_value({i}, {n});\n")).collect();
            format!(
                "diff --git a/src/f{i}.rs b/src/f{i}.rs\nindex 1111111..2222222 100644\n--- a/src/f{i}.rs\n\
                 +++ b/src/f{i}.rs\n@@ -1,{n} +1,{n} @@ fn f{i}() {{\n fn f{i}() {{\n{removed}{added} }}\n",
                n = lines + 2
            )
        })
        .collect()
}

/// The first `count` lines of `text`, as `head -n COUNT` passes them on.
fn first_lines(text: &str, count: usize) -> String {
    text.split_inclusive('\n').take(count).collect()
}

/// `grep -rn` matches with long lines, in `files` files.
fn matches(files: usize, per_file: usize) -> String {
    let filler = "x".repeat(150);
    (0..files)
        .flat_map(|f| (1..=per_file).map(move |n| (f, n)))
        .map(|(f, n)| format!("src/mod{f}/file.rs:{n}: let needle_{n} = load(\"{filler}\");\n"))
        .collect()
}

/// Claude Code drops a result's trailing newline.
fn shown(text: &str) -> &str {
    text.trim_end_matches('\n')
}

#[cfg(unix)]
#[test]
fn windows_of_supported_output_get_the_family_report() {
    let full = patch(40, 6);
    let cut = first_lines(&full, 300);
    let tests: String = (0..300)
        .map(|i| format!("test pricing::case_{i} ... ok\n"))
        .chain(["test pricing::total ... FAILED\n".to_owned()])
        .collect();
    let search = matches(30, 8);
    let clipped: String = search
        .lines()
        .map(|line| format!("{}\n", &line[..120]))
        .collect();
    let log = format!(
        "commit {}\nAuthor: a <a@example.com>\nDate:   today\n\n{}    done\n",
        "1".repeat(40),
        "    waiting for the build\n".repeat(600)
    );
    let cases: [(&str, &str, &[&str], &[&str]); 6] = [
        // A whole patch within the window: one report line per hunk, no note.
        (
            "git diff | sed -n 1,2000p",
            &full,
            &["40 diff section(s)\nsrc/f0.rs @@ -1,8 +1,8 @@ fn f0() { [stdout:5-19]\n"],
            &["Covers only", "(cut off)"],
        ),
        // `head` stopped inside a hunk, which the report says.
        (
            "git diff | head -n 300",
            &cut,
            &[
                "Covers only the lines `head -n 300` passed on",
                "16 diff section(s)\n",
                "src/f14.rs @@ -1,8 +1,8 @@ fn f14() { [stdout:271-285]\n",
                "src/f15.rs @@ -1,8 +1,8 @@ fn f15() { (cut off) [stdout:290-300]\n",
            ],
            &[],
        ),
        // A test log cut before its result line names the failure it showed.
        (
            "cargo test 2>&1 | head -301",
            &tests,
            &[
                "Covers only the lines `head -301` passed on",
                "1 reported failing test(s)\npricing::total  [stdout:301-301]\n",
            ],
            &["case_7 ... ok"],
        ),
        // Clipped matches keep their path and line, so they group by file.
        (
            "grep -rn needle src | cut -c1-120",
            &clipped,
            &["src/mod0/file.rs ×8 [stdout:1-8]\n"],
            &["Covers only"],
        ),
        (
            "rg -n needle src | sed -n '1,500p' | cat",
            &search,
            &["rg: 240 matches in 30 files\nsrc/mod0/file.rs ×8 [stdout:1-8]\n"],
            &["Covers only"],
        ),
        // Output no report reads gets the floor, never a clipped preview.
        (
            "git log | sed -n 1,3000p",
            &log,
            &["    waiting for the build\n[last line repeated 599 more time(s)]\n    done\n"],
            &["clipped", "Covers only"],
        ),
    ];
    for (command, stdout, has_all, lacks_all) in cases {
        let (dir, home) = workspace();
        let root = dir.path().canonicalize().unwrap();
        for f in 0..30 {
            fs::create_dir_all(root.join(format!("src/mod{f}"))).unwrap();
            fs::write(
                root.join(format!("src/mod{f}/file.rs")),
                "needle\n".repeat(9),
            )
            .unwrap();
        }
        let replaced = after_bash(&root, home.path(), command, response(shown(stdout)));
        let replaced = replaced.unwrap_or_else(|| panic!("{command} was left alone"));
        for needle in has_all {
            has(&replaced, needle);
        }
        for needle in lacks_all {
            lacks(&replaced, needle);
        }
        assert!(replaced.len() * 2 < stdout.len(), "{command}: {replaced}");
        // The archive replays the shown text byte for byte, and every item points
        // at lines it holds.
        let id = replaced
            .rsplit("[justless ")
            .next()
            .and_then(|tail| tail.split([' ', ']']).next())
            .unwrap();
        let log = justless(&root, home.path(), &["log", id], b"");
        assert_eq!(text(&log), shown(stdout), "{command}");
        let lines = shown(stdout).lines().count();
        for range in replaced.split("[stdout:").skip(1) {
            let range = range.split(']').next().unwrap();
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            let (first, last): (usize, usize) = (first.parse().unwrap(), last.parse().unwrap());
            assert!(
                1 <= first && first <= last && last <= lines,
                "{command}: {range}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn pipes_into_programs_and_unreadable_windows_are_left_alone() {
    let full = patch(40, 6);
    let mid = shown(&full).lines().skip(6).collect::<Vec<_>>().join("\n");
    let long = full.replace("fn f3()", &format!("fn f3_{}()", "y".repeat(120)));
    let cases: &[(&str, &str)] = &[
        ("git diff | xargs echo", &full),
        ("git diff | sh", &full),
        ("git diff | python3 -c 'import sys'", &full),
        ("git diff | jq -R .", &full),
        ("git diff | wc -l", &full),
        ("git diff | sed 's/old/new/'", &full),
        ("git diff | sed -n '1,10p;20,30p'", &full),
        ("git diff | sed -n -i 1,400p", &full),
        ("git diff | cut -d: -f1", &full),
        ("git diff | cut -c5-200", &full),
        ("git diff | sed -n 1,400p > patch.txt", &full),
        ("echo \"$(git diff | sed -n 1,400p)\"", &full),
        ("git status; git diff | sed -n 1,2000p", &full),
        (
            "git diff | sed -n 1,2000p; git diff | sed -n 1,2000p",
            &full,
        ),
        // A window that starts inside a patch is no patch, and a clipped hunk
        // header could misname its file.
        ("git diff | sed -n 7,2000p", &mid),
        ("git diff | cut -c1-100", &long),
    ];
    for (command, stdout) in cases {
        let (dir, home) = workspace();
        let root = dir.path().canonicalize().unwrap();
        let replaced = after_bash(&root, home.path(), command, response(shown(stdout)));
        assert_eq!(replaced, None, "{command}");
        assert_eq!(runs(&root), 0, "{command}");
    }
    // A window Claude Code saved to a file keeps Claude Code's own preview.
    let (dir, home) = workspace();
    let root = dir.path().canonicalize().unwrap();
    let results = home.path().join("tool-results");
    fs::create_dir_all(&results).unwrap();
    let saved = results.join("b1.txt");
    fs::write(&saved, shown(&full)).unwrap();
    let mut saved_response = response(&shown(&full)[..2000]);
    saved_response["persistedOutputPath"] = json!(saved);
    saved_response["persistedOutputSize"] = json!(shown(&full).len());
    let replaced = after_bash(
        &root,
        home.path(),
        "git diff | sed -n 1,2000p",
        saved_response,
    );
    assert_eq!(replaced, None);
    assert_eq!(runs(&root), 0);
}
