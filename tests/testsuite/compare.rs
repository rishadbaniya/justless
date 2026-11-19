//! `justless test --compare REF` on disposable repositories.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::common::{BIN, git, has, lacks, text, words};

/// Runs Justless with its own temporary directory, to check what is left there.
fn cli(root: &Path, temp: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("TMPDIR", temp)
        .output()
        .unwrap()
}

/// The temporary worktree is gone from the disk and from Git.
fn assert_clean(root: &Path, temp: &Path) {
    let left = fs::read_dir(temp).unwrap().count();
    assert_eq!(left, 0, "temporary files left");
    let worktrees = git(root, &["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.matches("worktree ").count(), 1, "{worktrees}");
}

/// `main` with the base files, then `feature` with the branch files.
fn repository(root: &Path, base: &[(&str, &str)], branch: &[(&str, &str)]) {
    let commit = |files: &[(&str, &str)], message: &str| {
        for (file, contents) in files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", message]);
    };
    git(root, &["init", "-q"]);
    commit(base, "base");
    git(root, &["branch", "-M", "main"]);
    git(root, &["checkout", "-qb", "feature"]);
    commit(branch, "branch");
}

const BASE_LIB: &str = r#"pub fn value() -> i32 { 1 }
pub fn total() -> i32 { 5 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_bug() { assert_eq!(1 + 1, 3); }
    #[test]
    fn changed() { assert_eq!(value(), 10); }
    #[test]
    fn breaks() { assert_eq!(total(), 5); }
    #[test]
    fn stable() { assert!(true); }
}
"#;

// Every line moves down, so the panics' locations differ from the base's.
const BRANCH_LIB: &str = r#"// Totals now include shipping.
pub fn value() -> i32 { 2 }
pub fn total() -> i32 { 6 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_bug() { assert_eq!(1 + 1, 3); }
    #[test]
    fn changed() { assert_eq!(value(), 10); }
    #[test]
    fn breaks() { assert_eq!(total(), 5); }
    #[test]
    fn stable() { assert!(true); }
    #[test]
    fn added() { assert!(false, "new test fails"); }
}
"#;

#[test]
fn cargo_failures_are_grouped_against_the_merge_base() {
    let repo = TempDir::new().unwrap();
    let temp = TempDir::new().unwrap();
    let (root, temp) = (repo.path(), temp.path());
    let manifest = "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    let base = [
        ("Cargo.toml", manifest),
        (".gitignore", "target\n"),
        ("src/lib.rs", BASE_LIB),
    ];
    repository(root, &base, &[("src/lib.rs", BRANCH_LIB)]);

    let unknown = cli(root, temp, &words("test --compare nope"));
    assert_eq!(unknown.status.code(), Some(2));
    has(&text(&unknown.stderr), "--compare nope: no such commit");

    let compared = cli(
        root,
        temp,
        &words("test --compare main -- cargo test --offline"),
    );
    let (out, err) = (text(&compared.stdout), text(&compared.stderr));
    // The working tree's exit status, unchanged.
    assert_eq!(compared.status.code(), Some(101), "{out}\n{err}");
    has(&out, "failures compared with main (merge-base ");
    has(
        &out,
        "temporary worktree: cargo test --offline --no-fail-fast -- --exact <4 test names>",
    );
    for line in [
        "reran 4 tests failing here by exact name at the base",
        "New failures, passing at the base (1): tests::breaks",
        "no result at the base (added or renamed since?) (1): tests::added",
        "Changed failures, failing at the base with another message (1): tests::changed",
        "tests::changed · here: assertion `left == right` failed left: 2 right: 10",
        "tests::changed · base: assertion `left == right` failed left: 1 right: 10",
        "Already failing at the base, same message (1): tests::old_bug",
    ] {
        has(&err, line);
    }
    assert_clean(root, temp);
    // The base built in its own target directory, never the working tree's.
    assert!(root.join(".justless/compare/cargo-target").is_dir());
    let id = crate::common::run_id(&compared);
    let record = fs::read(root.join(".justless/runs").join(id).join("compare.json")).unwrap();
    let record: serde_json::Value = serde_json::from_slice(&record).unwrap();
    assert_eq!(record["ref"], "main");
    let groups: Vec<(&str, &str)> = record["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|test| {
            (
                test["name"].as_str().unwrap(),
                test["group"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(
        groups.contains(&("tests::old_bug", "already-failing")),
        "{groups:?}"
    );

    // A worktree an interrupted comparison left behind goes with the next one.
    let stale = temp.join("justless-compare-stale");
    fs::create_dir(&stale).unwrap();
    let owner = root.canonicalize().unwrap().join(".justless/compare");
    fs::write(stale.join("owner"), owner.to_string_lossy().as_bytes()).unwrap();
    let left = stale.join("shop");
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            left.to_str().unwrap(),
            "main",
        ],
    );
    // Without captured output there is no message to compare.
    let line = "test --compare main -- cargo test --offline -- --nocapture";
    let uncaptured = cli(root, temp, &words(line));
    assert_eq!(uncaptured.status.code(), Some(101));
    has(
        &text(&uncaptured.stderr),
        "Failing at the base too, no message to compare (2): tests::changed, tests::old_bug",
    );
    assert_clean(root, temp);

    // Nothing failed, so nothing runs at the base.
    let line = "test --compare main -- cargo test --offline stable";
    let passing = cli(root, temp, &words(line));
    assert_eq!(passing.status.code(), Some(0), "{}", text(&passing.stderr));
    lacks(&text(&passing.stdout), "Run at main");
    has(
        &text(&passing.stderr),
        "no test failed, so nothing ran at the base.",
    );
    assert_clean(root, temp);

    // A base that does not build cannot group anything, and says so.
    git(root, &["checkout", "-qb", "broken", "main"]);
    fs::write(root.join("src/lib.rs"), "pub fn value() -> i32 { 1 \n").unwrap();
    git(root, &["commit", "-qam", "broken"]);
    git(root, &["checkout", "-qb", "fixed"]);
    fs::write(root.join("src/lib.rs"), BRANCH_LIB).unwrap();
    git(root, &["commit", "-qam", "fixed"]);
    let unbuilt = cli(
        root,
        temp,
        &words("test --compare broken -- cargo test --offline"),
    );
    assert_eq!(unbuilt.status.code(), Some(101));
    let err = text(&unbuilt.stderr);
    has(
        &err,
        "the base run reported no test result, exit 101 (error",
    );
    has(&err, "failures are not grouped");
    lacks(&err, "New failures");
    assert_clean(root, temp);
}

#[cfg(unix)]
#[test]
fn pytest_node_ids_compare_and_other_runners_are_unsupported() {
    let repo = TempDir::new().unwrap();
    let temp = TempDir::new().unwrap();
    let tools = TempDir::new().unwrap();
    let (root, temp) = (repo.path(), temp.path());
    // A stand-in for pytest: it reports what the checkout's `results` file says
    // and logs its arguments outside the repository.
    let log = tools.path().join("args.log");
    let pytest = tools.path().join("pytest");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\ncat results\nexit \"$(cat status)\"\n",
        log.display()
    );
    crate::common::executable(&pytest, &script);
    let base = [
        ("tests/test_a.py", ""),
        (
            "results",
            "PASSED tests/test_a.py::test_breaks\nFAILED tests/test_a.py::test_old - assert 1 == 2\n1 failed, 1 passed in 0.10s\n",
        ),
        ("status", "1\n"),
    ];
    let branch = [
        ("tests/test_b.py", ""),
        (
            "results",
            "FAILED tests/test_a.py::test_breaks - assert 3 == 4\nFAILED tests/test_a.py::test_old - assert 1 == 2\nFAILED tests/test_b.py::test_new - RuntimeError: boom\n3 failed in 0.10s\n",
        ),
    ];
    repository(root, &base, &branch);

    let compared = cli(
        root,
        temp,
        &["test", "--compare", "main", "--", pytest.to_str().unwrap()],
    );
    let err = text(&compared.stderr);
    assert_eq!(compared.status.code(), Some(1), "{err}");
    for line in [
        "New failures, passing at the base (1): tests/test_a.py::test_breaks",
        "no result at the base (added or renamed since?) (1): tests/test_b.py::test_new",
        "Already failing at the base, same message (1): tests/test_a.py::test_old",
    ] {
        has(&err, line);
    }
    // The base ran only the failing node IDs whose file exists there.
    let args = fs::read_to_string(&log).unwrap();
    assert_eq!(
        args.lines().nth(1).unwrap(),
        "-rA --maxfail=0 tests/test_a.py::test_breaks tests/test_a.py::test_old"
    );
    assert_clean(root, temp);

    let other = [
        "test",
        "--compare",
        "main",
        "--",
        "sh",
        "-c",
        "echo broken; exit 3",
    ];
    let other = cli(root, temp, &other);
    assert_eq!(other.status.code(), Some(3));
    has(
        &text(&other.stderr),
        "not supported for command; nothing ran at the base.",
    );
    assert_clean(root, temp);
}
