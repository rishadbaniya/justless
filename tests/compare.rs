//! `justless test --compare REF` on disposable repositories.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.email=qa@example.com", "-c", "user.name=qa"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", stderr(&output));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Runs Justless with its own temporary directory, to check what is left there.
fn cli(root: &Path, temp: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(args)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("TMPDIR", temp)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The temporary worktree is gone from the disk and from Git.
fn assert_clean(root: &Path, temp: &Path) {
    assert_eq!(
        fs::read_dir(temp).unwrap().count(),
        0,
        "temporary files left"
    );
    let worktrees = git(root, &["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.matches("worktree ").count(), 1, "{worktrees}");
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
    let root = repo.path();
    git(root, &["init", "-q"]);
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join(".gitignore"), "target\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), BASE_LIB).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["branch", "-M", "main"]);
    git(root, &["checkout", "-qb", "feature"]);
    fs::write(root.join("src/lib.rs"), BRANCH_LIB).unwrap();
    git(root, &["commit", "-qam", "branch"]);

    let unknown = cli(root, temp.path(), &["test", "--compare", "nope"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        stderr(&unknown).contains("--compare nope: no such commit"),
        "{}",
        stderr(&unknown)
    );

    let command = [
        "test",
        "--compare",
        "main",
        "--",
        "cargo",
        "test",
        "--offline",
    ];
    let compared = cli(root, temp.path(), &command);
    let (out, err) = (stdout(&compared), stderr(&compared));
    // The working tree's exit status, unchanged.
    assert_eq!(compared.status.code(), Some(101), "{out}\n{err}");
    assert!(
        out.contains("failures compared with main (merge-base "),
        "{out}"
    );
    assert!(
        out.contains(
            "temporary worktree: cargo test --offline --no-fail-fast -- --exact <4 test names>"
        ),
        "{out}"
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
        assert!(err.contains(line), "missing {line:?} in\n{err}");
    }
    assert_clean(root, temp.path());
    // The base built in its own target directory, never the working tree's.
    assert!(root.join(".justless/compare/cargo-target").is_dir());
    let id = err
        .split_once("[justless ")
        .unwrap()
        .1
        .split_once(']')
        .unwrap()
        .0;
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join(".justless/runs").join(id).join("compare.json")).unwrap(),
    )
    .unwrap();
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
    let stale = temp.path().join("justless-compare-stale");
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
    let uncaptured = cli(
        root,
        temp.path(),
        &[
            "test",
            "--compare",
            "main",
            "--",
            "cargo",
            "test",
            "--offline",
            "--",
            "--nocapture",
        ],
    );
    assert_eq!(uncaptured.status.code(), Some(101));
    let err = stderr(&uncaptured);
    assert!(
        err.contains(
            "Failing at the base too, no message to compare (2): tests::changed, tests::old_bug"
        ),
        "{err}"
    );
    assert_clean(root, temp.path());

    // Nothing failed, so nothing runs at the base.
    let passing = cli(
        root,
        temp.path(),
        &[
            "test",
            "--compare",
            "main",
            "--",
            "cargo",
            "test",
            "--offline",
            "stable",
        ],
    );
    assert_eq!(passing.status.code(), Some(0), "{}", stderr(&passing));
    assert!(
        !stdout(&passing).contains("Run at main"),
        "{}",
        stdout(&passing)
    );
    assert!(
        stderr(&passing).contains("no test failed, so nothing ran at the base."),
        "{}",
        stderr(&passing)
    );
    assert_clean(root, temp.path());

    // A base that does not build cannot group anything, and says so.
    git(root, &["checkout", "-qb", "broken", "main"]);
    fs::write(root.join("src/lib.rs"), "pub fn value() -> i32 { 1 \n").unwrap();
    git(root, &["commit", "-qam", "broken"]);
    git(root, &["checkout", "-qb", "fixed"]);
    fs::write(root.join("src/lib.rs"), BRANCH_LIB).unwrap();
    git(root, &["commit", "-qam", "fixed"]);
    let command = [
        "test",
        "--compare",
        "broken",
        "--",
        "cargo",
        "test",
        "--offline",
    ];
    let unbuilt = cli(root, temp.path(), &command);
    assert_eq!(unbuilt.status.code(), Some(101));
    let err = stderr(&unbuilt);
    assert!(
        err.contains("the base run reported no test result, exit 101 (error")
            && err.contains("failures are not grouped"),
        "{err}"
    );
    assert!(!err.contains("New failures"), "{err}");
    assert_clean(root, temp.path());
}

#[cfg(unix)]
#[test]
fn pytest_node_ids_compare_and_other_runners_are_unsupported() {
    use std::os::unix::fs::PermissionsExt;
    let repo = TempDir::new().unwrap();
    let temp = TempDir::new().unwrap();
    let tools = TempDir::new().unwrap();
    let root = repo.path();
    // A stand-in for pytest: it reports what the checkout's `results` file says
    // and logs its arguments outside the repository.
    let log = tools.path().join("args.log");
    let pytest = tools.path().join("pytest");
    fs::write(
        &pytest,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\ncat results\nexit \"$(cat status)\"\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&pytest, fs::Permissions::from_mode(0o700)).unwrap();
    git(root, &["init", "-q"]);
    fs::create_dir(root.join("tests")).unwrap();
    fs::write(root.join("tests/test_a.py"), "").unwrap();
    fs::write(
        root.join("results"),
        "PASSED tests/test_a.py::test_breaks\nFAILED tests/test_a.py::test_old - assert 1 == 2\n1 failed, 1 passed in 0.10s\n",
    )
    .unwrap();
    fs::write(root.join("status"), "1\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["branch", "-M", "main"]);
    git(root, &["checkout", "-qb", "feature"]);
    fs::write(root.join("tests/test_b.py"), "").unwrap();
    fs::write(
        root.join("results"),
        "FAILED tests/test_a.py::test_breaks - assert 3 == 4\nFAILED tests/test_a.py::test_old - assert 1 == 2\nFAILED tests/test_b.py::test_new - RuntimeError: boom\n3 failed in 0.10s\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "branch"]);

    let program = pytest.to_str().unwrap();
    let compared = cli(
        root,
        temp.path(),
        &["test", "--compare", "main", "--", program],
    );
    let err = stderr(&compared);
    assert_eq!(compared.status.code(), Some(1), "{err}");
    for line in [
        "New failures, passing at the base (1): tests/test_a.py::test_breaks",
        "no result at the base (added or renamed since?) (1): tests/test_b.py::test_new",
        "Already failing at the base, same message (1): tests/test_a.py::test_old",
    ] {
        assert!(err.contains(line), "missing {line:?} in\n{err}");
    }
    // The base ran only the failing node IDs whose file exists there.
    let args = fs::read_to_string(&log).unwrap();
    let base = args.lines().nth(1).unwrap();
    assert_eq!(
        base,
        "-rA --maxfail=0 tests/test_a.py::test_breaks tests/test_a.py::test_old"
    );
    assert_clean(root, temp.path());

    let other = cli(
        root,
        temp.path(),
        &[
            "test",
            "--compare",
            "main",
            "--",
            "sh",
            "-c",
            "echo broken; exit 3",
        ],
    );
    assert_eq!(other.status.code(), Some(3));
    assert!(
        stderr(&other).contains("not supported for command; nothing ran at the base."),
        "{}",
        stderr(&other)
    );
    assert_clean(root, temp.path());
}
