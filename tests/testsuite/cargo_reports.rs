//! What a plain Cargo run shows in place of the pipes agents write after it, such
//! as `cargo test 2>&1 | grep '^test result'` or `cargo clippy 2>&1 | tail -3`:
//! the test totals with one line per test binary, and a clean build or lint in one
//! line. Every line of the report points back at the archive, which replays the
//! output exactly, and the command's exit status reaches the caller.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::common::{BIN, executable, has, starts, text};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/cargo");

/// A workspace with its own state and a `cargo` stub that replays `$OUT` on
/// stdout and `$ERR` on stderr, then exits with `$CODE`.
struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        for sub in ["root", "bin", "home", "state"] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        fs::write(dir.path().join("root/.justless.toml"), "").unwrap();
        executable(
            &dir.path().join("bin/cargo"),
            "#!/bin/sh\ncat \"$OUT\"\ncat \"$ERR\" >&2\nexit \"$CODE\"\n",
        );
        Self { dir }
    }

    fn justless(&self, args: &[&str], streams: (&Path, &Path), code: i32) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![self.dir.path().join("bin")];
        dirs.extend(std::env::split_paths(&path));
        Command::new(BIN)
            .args(args)
            .current_dir(self.dir.path().join("root"))
            .env("PATH", std::env::join_paths(dirs).unwrap())
            .env("HOME", self.dir.path().join("home"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("OUT", streams.0)
            .env("ERR", streams.1)
            .env("CODE", code.to_string())
            .output()
            .unwrap()
    }

    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, content).unwrap();
        path
    }

    /// The archived lines `from` to `to` of `stream`, as `justless log` replays them.
    fn log(&self, output: &Output, stream: &str, (from, to): (usize, usize)) -> String {
        let id = crate::common::run_id(output);
        let empty = Path::new("/dev/null");
        let (from, to) = (from.to_string(), to.to_string());
        let args = ["log", &id, "--stream", stream, "--from", &from, "--to", &to];
        let replay = self.justless(&args, (empty, empty), 0);
        text(if stream == "stdout" {
            &replay.stdout
        } else {
            &replay.stderr
        })
    }
}

fn fixture(name: &str) -> String {
    fs::read_to_string(Path::new(FIXTURES).join(name)).unwrap()
}

fn lines(text: &str, (from, to): (usize, usize)) -> String {
    text.split_inclusive('\n')
        .skip(from - 1)
        .take(to + 1 - from)
        .collect()
}

/// The two streams as `2>&1` joins them: each binary's `Running` line before
/// its block.
fn merged(out: &str, err: &str) -> String {
    let mut groups = vec![String::new()];
    for line in err.split_inclusive('\n') {
        groups.last_mut().unwrap().push_str(line);
        if line.contains(" Running ") || line.contains(" Doc-tests ") {
            groups.push(String::new());
        }
    }
    let rest = groups.pop().unwrap();
    let mut groups = groups.into_iter();
    let mut text = String::new();
    for line in out.split_inclusive('\n') {
        if line.starts_with("running ") {
            text.extend(groups.next());
        }
        text.push_str(line);
    }
    text + &rest
}

#[cfg(unix)]
#[test]
fn a_test_run_reports_totals_per_binary_with_or_without_a_stderr_merge() {
    let sandbox = Sandbox::new();
    let (out, err) = (fixture("test_workspace.out"), fixture("test_workspace.err"));
    let (out_file, err_file) = (sandbox.file("out", &out), sandbox.file("err", &err));
    let split = sandbox.justless(
        &["run", "--hook", "--", "cargo", "test"],
        (&out_file, &err_file),
        0,
    );
    assert_eq!(split.status.code(), Some(0));
    let shown = text(&split.stdout);
    starts(
        &shown,
        "test result: ok. 21 passed; 0 failed; 0 ignored · 11 test binaries\n  \
         2 passed  unittests src/lib.rs (api) [stdout:2-6]\n  \
         1 passed  tests/cli.rs [stdout:14-17]\n",
    );
    has(
        &shown,
        "  3 ran no tests: unittests src/main.rs, doc-tests api, doc-tests reports\n\
         stderr: 15 cargo status line(s) [stderr:1-15]\n",
    );
    // The report stands in for both streams, and each range replays exactly.
    assert!(
        text(&split.stderr).starts_with("[justless run-"),
        "{split:?}"
    );
    assert_eq!(
        sandbox.log(&split, "stdout", (14, 17)),
        lines(&out, (14, 17))
    );
    assert_eq!(sandbox.log(&split, "stderr", (1, 15)), err);

    let joined = merged(&out, &err);
    let joined_file = sandbox.file("joined", &joined);
    let empty = sandbox.file("empty", "");
    let merge = sandbox.justless(
        &["run", "--hook", "--merge", "--fresh", "--", "cargo", "test"],
        (&joined_file, &empty),
        0,
    );
    let shown = text(&merge.stdout);
    starts(
        &shown,
        "test result: ok. 21 passed; 0 failed; 0 ignored · 11 test binaries\n  \
         2 passed  unittests src/lib.rs (api) [stdout:7-11]\n",
    );
    has(&shown, "stdout: 15 cargo status line(s) [stdout:2-87]\n");
    let all = (1, joined.lines().count());
    assert_eq!(sandbox.log(&merge, "stdout", all), joined);
}

#[cfg(unix)]
#[test]
fn a_failing_test_run_keeps_its_failures_status_and_stderr_lines() {
    let sandbox = Sandbox::new();
    let (out, err) = (fixture("test_failing.out"), fixture("test_failing.err"));
    let joined = sandbox.file("joined", &merged(&out, &err));
    let empty = sandbox.file("empty", "");
    let output = sandbox.justless(
        &["run", "--hook", "--merge", "--", "cargo", "test"],
        (&joined, &empty),
        101,
    );
    assert_eq!(output.status.code(), Some(101));
    let shown = text(&output.stdout);
    starts(
        &shown,
        "1 reported failing test(s)\n1 test(s): total_includes_fee tests/checkout.rs:11:5: \
         assertion `left == right` failed left: 106 right: 105 [stdout:",
    );
    has(
        &shown,
        "test result: FAILED. 0 passed; 1 failed; 0 ignored · 2 test binaries\n",
    );
    has(
        &shown,
        "stdout: 1 other line(s) [stdout:36-36]\n  error: test failed, to rerun pass `--test checkout`\n",
    );
}

#[cfg(unix)]
#[test]
fn a_clean_build_is_one_line_and_anything_else_keeps_its_lines() {
    let sandbox = Sandbox::new();
    let err = fixture("clippy_clean.err");
    let empty = sandbox.file("empty", "");
    let err_file = sandbox.file("err", &err);
    let output = sandbox.justless(
        &["run", "--hook", "--", "cargo", "clippy", "--all-targets"],
        (&empty, &err_file),
        0,
    );
    assert_eq!(
        text(&output.stdout),
        "cargo clippy: 0 findings · 14 cargo status line(s) [stderr:1-14] · \
         Finished `dev` profile [unoptimized + debuginfo] target(s) in 31.71s\n"
    );
    assert_eq!(sandbox.log(&output, "stderr", (1, 14)), err);

    // A failure, a line Cargo did not print and a one-line build stay as they were.
    let finished = lines(&err, (14, 14));
    for (stderr, code) in [
        (err.clone(), 1),
        (format!("{err}build script says hello\n"), 0),
        (finished.clone(), 0),
    ] {
        let err_file = sandbox.file("err", &stderr);
        let output = sandbox.justless(
            &["run", "--hook", "--fresh", "--", "cargo", "build"],
            (&empty, &err_file),
            code,
        );
        assert_eq!(output.status.code(), Some(code));
        assert_eq!(text(&output.stdout), "");
        starts(&text(&output.stderr), &stderr);
    }
}
