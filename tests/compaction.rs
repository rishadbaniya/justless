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
