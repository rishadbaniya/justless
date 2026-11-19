//! Reports that must count what happened: `test --compare` with Cargo's default
//! target directory, MCP `log` pages of a run made in another project, the token
//! estimate of `stats --all`, and the options `stats --all` accepts. Every command
//! runs with `HOME`, the state home and the Claude Code and Codex config homes
//! inside the test's own folder.
#![cfg(unix)]
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Lab {
    _temp: TempDir,
    base: PathBuf,
    home: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        Lab {
            _temp: temp,
            base,
            home,
        }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_justless"));
        // Cargo and rustup look for their homes under HOME unless told where they are.
        let real = std::env::var_os("HOME").map(PathBuf::from);
        for (name, default) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
            let path = std::env::var_os(name)
                .map(PathBuf::from)
                .or_else(|| real.as_ref().map(|home| home.join(default)));
            if let Some(path) = path {
                command.env(name, path);
            }
        }
        command
            .args(args)
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("JUSTLESS_DISABLE");
        command
    }

    fn output(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn json(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.output(cwd, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Runs `script` under `justless run` and returns the run ID it names.
    fn run_id(&self, cwd: &Path, script: &str) -> String {
        let output = self.output(cwd, &["run", "--", "sh", "-c", script]);
        assert!(output.status.success(), "{}", text(&output.stderr));
        let stderr = text(&output.stderr);
        let start = stderr.find("[justless run-").unwrap() + "[justless ".len();
        stderr[start..].split([' ', ']']).next().unwrap().to_owned()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(["-c", "user.name=Lab", "-c", "user.email=lab@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        text(&output.stderr)
    );
}

/// Paths into the working tree's own `target/` name the same file as the base
/// build's paths into its target directory, as they do with `CARGO_TARGET_DIR`.
#[test]
fn compare_matches_paths_in_the_default_cargo_target_directory() {
    let lab = Lab::new();
    let root = lab.dir("cmpt");
    let temp = lab.dir("tmp");
    git(&root, &["init", "-q"]);
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"cmpt\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join(".gitignore"), "target\nCargo.lock\n").unwrap();
    fs::create_dir_all(root.join("src/bin")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn value() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("src/bin/cmpt.rs"), "fn main() {}\n").unwrap();
    fs::write(
        root.join("tests/tmpdir.rs"),
        r#"#[test]
fn tmp_dir() { panic!("scratch at {}", env!("CARGO_TARGET_TMPDIR")); }
#[test]
fn bin_path() { panic!("bin at {}", env!("CARGO_BIN_EXE_cmpt")); }
"#,
    )
    .unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    git(&root, &["branch", "-M", "main"]);
    git(&root, &["checkout", "-qb", "feature"]);
    fs::write(
        root.join("src/lib.rs"),
        "// Changed on the branch.\npub fn value() -> i32 { 1 }\n",
    )
    .unwrap();
    git(&root, &["commit", "-qam", "branch"]);

    let args = [
        "test",
        "--compare",
        "main",
        "--",
        "cargo",
        "test",
        "--offline",
        "--no-fail-fast",
        "--test",
        "tmpdir",
    ];
    let output = lab
        .command(&root, &args)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env("TMPDIR", &temp)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let report = format!("{}\n{}", text(&output.stdout), text(&output.stderr));
    assert_eq!(output.status.code(), Some(101), "{report}");
    assert!(root.join("target/debug").is_dir(), "{report}");
    assert!(
        report.contains("Already failing at the base, same message (2): bin_path, tmp_dir"),
        "{report}"
    );
    assert!(!report.contains("Changed failures"), "{report}");
}

/// Sends each line to `justless mcp` in `root` and returns the parsed responses.
fn exchange(lab: &Lab, root: &Path, lines: &[Value]) -> Vec<Value> {
    let mut child = lab
        .command(root, &["mcp", "--root"])
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for line in lines {
            writeln!(stdin, "{line}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    text(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// A `log` page the MCP server delivers counts against the run it replays,
/// in the project that holds it.
#[test]
fn mcp_log_of_another_projects_run_counts_against_that_project() {
    let lab = Lab::new();
    let server = lab.dir("server");
    let owner = lab.dir("owner");
    let id = lab.run_id(&owner, "seq 1 3000");
    let before = lab.json(&owner, &["stats", "--json"]);
    assert_eq!(before["replayed_bytes"], 0, "{before}");

    let responses = exchange(
        &lab,
        &server,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "lab", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "log", "arguments": {"run_id": id}}}),
        ],
    );
    let result = &responses
        .iter()
        .find(|response| response["id"] == 2)
        .unwrap()["result"];
    assert_eq!(result["isError"], false, "{result}");
    let delivered: usize = result["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|content| content["text"].as_str().unwrap().len())
        .sum();
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("2999\n3000\n"),
        "{result}"
    );

    let after = lab.json(&owner, &["stats", "--json"]);
    assert_eq!(after["replayed_bytes"], json!(delivered), "{after}");
    assert_eq!(
        after["net_saved_bytes"].as_i64().unwrap(),
        before["net_saved_bytes"].as_i64().unwrap() - delivered as i64,
        "{after}"
    );
    assert!(!server.join(".justless/runs").join(&id).exists());
}

/// `stats --all` estimates tokens per run by content type, as `stats` does, and
/// says so.
#[test]
fn stats_all_estimates_tokens_like_project_stats() {
    let lab = Lab::new();
    let project = lab.dir("project");
    // Source lines, which classify as code: 2.57 bytes per token, not 2.21.
    lab.run_id(&project, "seq 1 3000 | sed 's/.*/let value_& = &;/'");
    let one = lab.json(&project, &["stats", "--json"]);
    let all = lab.json(&project, &["stats", "--all", "--include-temp", "--json"]);
    assert_eq!(all["runs"], 1, "{all}");
    assert_eq!(all["saved_bytes"], one["saved_bytes"], "{all}\n{one}");
    for key in [
        "estimated_tokens_saved",
        "estimated_tokens_net_saved",
        "token_estimate",
    ] {
        assert!(!one[key].is_null(), "{key}: {one}");
        assert_eq!(all[key], one[key], "{key}: {all}\n{one}");
    }
    assert_eq!(
        all["estimate_by_content"]["code"],
        one["estimate_by_content"]["code"]
    );

    let output = lab.output(&project, &["stats", "--all", "--include-temp"]);
    let report = text(&output.stdout);
    let (saved, net) = (
        &one["estimated_tokens_saved"],
        &one["estimated_tokens_net_saved"],
    );
    assert!(
        report.contains(&format!(
            "(≈{saved} tokens at first preview, ≈{net} net, estimated at 2.57 bytes per token for code)"
        )),
        "{report}"
    );
    assert!(!report.contains("÷ 4"), "{report}");
}

/// `--sessions` reads one workspace's transcripts, so `--all` refuses it rather
/// than leaving it out.
#[test]
fn stats_all_refuses_sessions() {
    let lab = Lab::new();
    let project = lab.dir("project");
    let output = lab.output(&project, &["stats", "--all", "--sessions"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stdout));
    let error = text(&output.stderr);
    assert!(error.contains("cannot be used with"), "{error}");
}
