//! Claude Code Reads of files in a checkout nested under the project, such as a
//! Git worktree under `.claude/worktrees/`, a submodule or a nested repository,
//! are outlined for that checkout, whichever directory the hook runs in.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, git, has, piped, text};

/// Runs the binary in `cwd` with the home, Claude Code, Codex and state
/// directories in `home`.
fn justless(home: &Path, cwd: &Path, args: &[&str], input: Option<&Value>) -> Output {
    let mut command = Command::new(BIN);
    command
        .current_dir(cwd)
        .args(args)
        .env("HOME", home)
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("CODEX_HOME", home.join("codex"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("JUSTLESS_SESSION")
        .env_remove("JUSTLESS_DISABLE");
    let input = input.map_or_else(String::new, Value::to_string);
    piped(&mut command, input.as_bytes())
}

fn hook(home: &Path, cwd: &Path, input: &Value) -> Value {
    let output = justless(home, cwd, &["hook-claude", "after-read"], Some(input));
    assert!(output.status.success(), "{}", text(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A Rust file of 100 ten-line functions.
fn source() -> String {
    (0..100)
        .map(|i| {
            let body: String = (0..7)
                .map(|j| format!("    let step_{j} = value.wrapping_mul({j}).wrapping_add({i});\n"))
                .collect();
            format!("/// Function {i}.\npub fn function_{i}(value: u64) -> u64 {{\n{body}}}\n")
        })
        .collect()
}

/// What Claude Code's Read returns for lines `offset..offset + limit` of `text`,
/// or for all of it without an offset, asked from `cwd`.
fn read(cwd: &Path, path: &Path, text: &str, window: Option<(usize, usize)>) -> Value {
    let lines: Vec<&str> = text.split('\n').collect();
    let (args, first, shown) = match window {
        Some((offset, limit)) => (
            json!({"file_path":path,"offset":offset,"limit":limit}),
            offset,
            &lines[offset - 1..offset - 1 + limit],
        ),
        None => (json!({"file_path":path}), 1, &lines[..]),
    };
    json!({"session_id":"s1","tool_name":"Read","hook_event_name":"PostToolUse","cwd":cwd,
        "tool_input":args,"tool_response":{"type":"text","file":{"filePath":path,
        "content":shown.join("\n"),"numLines":shown.len(),"startLine":first,
        "totalLines":lines.len()}}})
}

/// The `REV#ID` of the outline's second row and the hint's command start.
fn second_row(answer: &Value) -> (String, String) {
    let output = &answer["hookSpecificOutput"];
    let content = output["updatedToolOutput"]["file"]["content"]
        .as_str()
        .unwrap_or_else(|| panic!("no outline: {answer}"));
    let revision = content.split_whitespace().nth(1).unwrap();
    let row = content.lines().filter(|line| line.starts_with('#')).nth(1);
    let id = row.unwrap().split_whitespace().next().unwrap();
    let context = output["additionalContext"].as_str().unwrap();
    let start = context.find("justless").unwrap();
    let end = context[start..].find(" expand ").unwrap();
    (
        format!("{revision}{id}"),
        context[start..start + end].to_owned(),
    )
}

/// A Git project with a worktree at `.claude/worktrees/x` that holds `src/big.rs`.
fn worktree(dir: &Path) -> (PathBuf, PathBuf, String) {
    let root = dir.canonicalize().unwrap();
    git(&root, &["init", "-q"]);
    fs::write(root.join(".gitignore"), "/.claude/worktrees/\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "init"]);
    let tree = root.join(".claude/worktrees/x");
    git(
        &root,
        &["worktree", "add", "-q", tree.to_str().unwrap(), "-b", "x"],
    );
    let body = source();
    fs::create_dir_all(tree.join("src")).unwrap();
    fs::write(tree.join("src/big.rs"), &body).unwrap();
    (root, tree, body)
}

#[test]
fn reads_in_a_worktree_bind_their_outline_to_the_worktree() {
    let (dir, home) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let home = home.path();
    let (root, tree, body) = worktree(dir.path());
    let path = tree.join("src/big.rs");
    fs::create_dir_all(tree.join(".justless")).unwrap();
    for (hook_dir, agent_dir) in [(&root, &root), (&root, &tree), (&tree, &tree)] {
        for window in [None, Some((300, 650))] {
            let answer = hook(home, hook_dir, &read(agent_dir, &path, &body, window));
            let (row, start) = second_row(&answer);
            // From the main checkout the hint names the worktree's root.
            assert_eq!(
                start.contains(tree.to_str().unwrap()),
                agent_dir == &root,
                "{start}"
            );
            let expanded = justless(home, &tree, &["expand", "src/big.rs", &row], None);
            assert!(expanded.status.success(), "{}", text(&expanded.stderr));
            let tree_root = tree.to_str().unwrap();
            let rooted = ["--root", tree_root, "expand", path.to_str().unwrap(), &row];
            let expanded = justless(home, &root, &rooted, None);
            assert!(expanded.status.success(), "{}", text(&expanded.stderr));
        }
    }
    let resume = justless(home, &tree, &["resume"], None);
    has(&text(&resume.stdout), "src/big.rs:300-949");
}

#[test]
fn reads_in_a_nested_repository_bind_to_it_but_never_through_a_symlink() {
    let (dir, home, outside) = (
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    );
    let home = home.path();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "-q"]);
    let nested = root.join("vendor/lib");
    fs::create_dir_all(nested.join("src")).unwrap();
    git(&nested, &["init", "-q"]);
    let body = source();
    let path = nested.join("src/big.rs");
    fs::write(&path, &body).unwrap();
    let (row, _) = second_row(&hook(home, &root, &read(&root, &path, &body, None)));
    let expanded = justless(home, &nested, &["expand", "src/big.rs", &row], None);
    has(&text(&expanded.stdout), "pub fn function_1(");
    // A linked directory that leaves the workspace, or one that enters the nested
    // repository from the outer root, keeps Read's own result.
    let away = outside.path().canonicalize().unwrap();
    fs::write(away.join("big.rs"), &body).unwrap();
    std::os::unix::fs::symlink(&away, nested.join("away")).unwrap();
    std::os::unix::fs::symlink(nested.join("src"), root.join("into")).unwrap();
    for linked in [nested.join("away/big.rs"), root.join("into/big.rs")] {
        let answer = hook(home, &root, &read(&root, &linked, &body, None));
        assert_eq!(answer, json!({}), "{}", linked.display());
    }
}
