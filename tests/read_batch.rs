#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_justless");

fn native(dir: &Path, script: &str) -> String {
    let output = Command::new("/bin/sh")
        .args(["-c", script])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn hook(dir: &Path, command: &str, response: Option<Value>) -> Value {
    let (mode, event) = if response.is_some() {
        ("after-bash", "PostToolUse")
    } else {
        ("before-command", "PreToolUse")
    };
    let input = json!({"hook_event_name":event,"tool_name":"Bash","cwd":dir,"session_id":"batch-test",
        "tool_input":{"command":command},"tool_response":response});
    let mut child = Command::new(BIN)
        .args(["--root", dir.to_str().unwrap(), "hook-claude", mode])
        .current_dir(dir)
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
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

fn response(text: &str) -> Value {
    json!({"stdout":text,"stderr":"","interrupted":false})
}

fn fixture() -> TempDir {
    let temp = TempDir::new().unwrap();
    let source = (0..200)
        .map(|i| {
            format!("pub fn add_{i:03}(value: u64) -> u64 {{\n    value.wrapping_add({i})\n}}\n")
        })
        .collect::<String>();
    fs::write(temp.path().join("big.rs"), source).unwrap();
    fs::write(temp.path().join("notes"), "alpha\nβeta\n").unwrap();
    fs::write(temp.path().join("part"), "suffix").unwrap();
    native(
        temp.path(),
        "git init -q && git add big.rs && git -c user.email=a@localhost -c user.name=fixture -c commit.gpgsign=false commit -qm fixture",
    );
    fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join("big.rs"))
        .unwrap()
        .write_all(b"// changed\n")
        .unwrap();
    temp
}

fn verified(dir: &Path, list: &str, event: Value, expected: &str) -> (String, Value) {
    assert_eq!(hook(dir, list, None), json!({}));
    let result = hook(dir, list, Some(event));
    let view = result["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
        .as_str()
        .unwrap_or_else(|| panic!("{list} returned {result}"));
    assert_eq!(view.matches("Exact: ").count(), 1);
    assert!(view.len() < expected.len());
    let id = view
        .lines()
        .find(|s| s.starts_with("Exact: "))
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap();
    let archived = fs::read(dir.join(".justless/runs").join(id).join("stdout")).unwrap();
    assert_eq!(archived, expected.as_bytes());
    let replay = Command::new(BIN)
        .args(["log", id])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(replay.status.success());
    assert_eq!(replay.stdout, archived);
    let record = serde_json::from_slice(
        &fs::read(dir.join(".justless/runs").join(id).join("record.json")).unwrap(),
    )
    .unwrap();
    (view.to_owned(), record)
}

#[test]
fn mixed_read_list_retains_each_pipeline_and_one_exact_replay() {
    let temp = fixture();
    let dir = temp.path();
    let commands = [
        "git diff --stat",
        "sed -n '1,170p;222,348p' big.rs",
        "nl -ba notes | sed -n '1,2p' | cut -c 1-13",
        "grep -n 'add_0' big.rs | head -4",
        "cat part",
        "tail -1 notes",
    ];
    let list = commands.join("; ");
    let raw = native(dir, &list);
    let (view, record) = verified(dir, &list, response(&raw), &raw);
    assert!(
        view.contains("function add_") && view.contains("big.rs |"),
        "{view}"
    );
    let mut offset = 0;
    for (index, command) in commands.iter().enumerate() {
        assert!(view.contains(&format!("$ {command} ")));
        let bytes = native(dir, command);
        let piece = &record["pieces"][index];
        assert_eq!(piece["command_index"], index + 1);
        assert_eq!(piece["start_byte"], offset);
        assert_eq!(piece["first_line"], 1 + raw[..offset].matches('\n').count());
        offset += bytes.len();
        assert_eq!(piece["end_byte"], offset);
        assert_eq!(
            piece["last_line"],
            1 + raw[..offset].matches('\n').count() - usize::from(bytes.ends_with('\n'))
        );
    }
    assert_eq!(offset, raw.len());
    assert_eq!(record["raw_bytes"], raw.len());
    assert_eq!(
        record["preview_bytes"].as_u64().unwrap() + record["footer_bytes"].as_u64().unwrap(),
        view.len() as u64
    );
}

#[test]
fn status_and_short_circuit_order_are_preserved() {
    let temp = fixture();
    let dir = temp.path();
    let list = "grep -n missing notes && cat absent || git show HEAD:big.rs; head -1 notes; grep -n absent notes";
    let raw = native(dir, "git show HEAD:big.rs; head -1 notes");
    let mut event = response(raw.trim_end_matches('\n'));
    event["returnCodeInterpretation"] = json!("No matches found");
    let (view, record) = verified(dir, list, event, raw.trim_end_matches('\n'));
    assert!(!view.contains("cat absent"));
    assert!(view.contains("git show HEAD:big.rs"));
    assert_eq!(record["status"], 1);
    assert_eq!(record["pieces"][1]["command_index"], 3);
    assert!(record["pieces"][3]["first_line"].is_null());
}

#[test]
fn persisted_output_keeps_its_native_bytes_and_client_preview_baseline() {
    let temp = fixture();
    let dir = temp.path();
    let list = "cat big.rs; head -1 notes";
    let raw = native(dir, list);
    let stored = dir.join("tool-results/batch.txt");
    fs::create_dir(stored.parent().unwrap()).unwrap();
    fs::write(&stored, &raw).unwrap();
    let preview = &raw[..2000];
    let mut event = response(preview);
    event["persistedOutputPath"] = json!(stored.canonicalize().unwrap());
    event["persistedOutputSize"] = json!(raw.len());
    let (view, record) = verified(dir, list, event, &raw);
    assert!(view.len() < preview.len());
    assert_eq!(record["raw_bytes"], 2000);
}

#[test]
fn unsupported_mismatched_and_small_batches_leave_no_run() {
    let temp = fixture();
    let dir = temp.path();
    let list = "cat big.rs; tail -1 notes";
    let raw = native(dir, list);
    fs::write(dir.join("notes"), "different\n").unwrap();
    for (list, body) in [
        (list, raw.clone()),
        (
            "head -1 notes; tail -1 notes",
            native(dir, "head -1 notes; tail -1 notes"),
        ),
        ("cat big.rs; echo hello", raw.clone()),
        ("cat big.rs; cut -c 1-2 < notes", raw.clone()),
        ("cat big.rs; git diff --output=unexpected", raw.clone()),
        (
            "cat big.rs; grep -n value big.rs --color=always",
            raw.clone(),
        ),
        ("cat big.rs; cat ../outside", raw),
    ] {
        assert_eq!(hook(dir, list, Some(response(&body))), json!({}), "{list}");
    }
    assert!(!dir.join(".justless/runs").exists());
    assert!(!dir.join("unexpected").exists());
}

#[test]
fn filters_and_quoted_operators_apply_before_accounting() {
    let temp = fixture();
    let dir = temp.path();
    fs::write(dir.join("a; b"), "δ beta\n").unwrap();
    // Use real rg where installed. grep supplies the same numbered shape on machines without rg.
    let search = if [
        "/usr/bin/rg",
        "/opt/homebrew/bin/rg",
        "/usr/local/bin/rg",
        "/bin/rg",
    ]
    .iter()
    .any(|p| Path::new(p).is_file())
        && Command::new("rg").arg("--version").output().is_ok()
    {
        "rg"
    } else {
        "grep -E"
    };
    let list = format!(
        "head -1 'a; b'; sed -n '1,4p;3,4p' big.rs | tail -3; {search} -Hn 'add_0|unmatched' big.rs | cut -c 1-72; cat big.rs | tail -n +2 | head -250"
    );
    let raw = native(dir, &list);
    let (view, _) = verified(dir, &list, response(&raw), &raw);
    assert!(
        view.contains("δ beta") && view.contains("function add_"),
        "{view}"
    );
    if search == "rg" {
        assert!(
            view.contains("matches") && view.contains("big.rs ×"),
            "{view}"
        );
    }
}

#[test]
fn diff_uses_global_line_pointers_without_touching_the_real_index() {
    let temp = fixture();
    let dir = temp.path();
    let old = fs::read_to_string(dir.join("big.rs")).unwrap();
    fs::write(
        dir.join("big.rs"),
        old.replace("wrapping_add", "wrapping_sub"),
    )
    .unwrap();
    let list = "head -1 notes; git diff -- big.rs; tail -1 notes";
    let raw = native(dir, list);
    let index = fs::read(dir.join(".git/index")).unwrap();
    let (view, _) = verified(dir, list, response(&raw), &raw);
    assert!(
        view.contains("diff section(s)") && view.contains("[stdout:"),
        "{view}"
    );
    assert_eq!(fs::read(dir.join(".git/index")).unwrap(), index);
}

#[test]
fn recheck_skips_external_git_diff_and_ambiguous_pipe_status() {
    let temp = fixture();
    let dir = temp.path();
    let list = "cat big.rs; git diff";
    native(dir, "git config diff.external 'touch external-ran'");
    let simulated = fs::read_to_string(dir.join("big.rs")).unwrap() + "custom diff\n";
    assert_eq!(hook(dir, list, Some(response(&simulated))), json!({}));
    assert!(!dir.join("external-ran").exists());
    let list = "cat big.rs | head -300 && tail -1 notes";
    assert_eq!(
        hook(dir, list, Some(response(&native(dir, list)))),
        json!({})
    );
    assert!(!dir.join(".justless/runs").exists());
}

#[test]
fn wrappers_are_looked_through_before_compacting() {
    let temp = fixture();
    let dir = temp.path();
    for command in [
        "timeout 5 sed -n '1,150p' big.rs && sed -n '150,300p' big.rs",
        "env -u HOME sed -n '1,150p' big.rs && sed -n '150,300p' big.rs",
        "env FOO=1 sed -n '1,150p' big.rs && sed -n '150,300p' big.rs",
        "timeout 5 sed -n '1,300p' big.rs",
        "env -u HOME sed -n '1,300p' big.rs",
        "nice -n 5 sed -n '1,300p' big.rs",
        "bash -c \"sed -n '1,300p' big.rs\"",
        "cat big.rs",
        "sed -n '1,300p' big.rs | head -200",
    ] {
        let raw = native(dir, command);
        let out = hook(dir, command, Some(response(&raw)));
        let shown = out["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .unwrap_or("");
        assert!(shown.contains("big.rs"), "{command}: {out}");
    }
}

#[test]
fn cd_lists_compact_in_their_directory() {
    let temp = fixture();
    let dir = temp.path();
    fs::create_dir(dir.join("sub")).unwrap();
    fs::copy(dir.join("big.rs"), dir.join("sub/big.rs")).unwrap();
    for args in [
        vec!["add", "sub/big.rs"],
        vec![
            "-c",
            "user.email=a@b",
            "-c",
            "user.name=x",
            "commit",
            "-qm",
            "sub",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    }
    let list = "cd sub && sed -n '1,600p' big.rs";
    let raw = native(dir, list);
    let (view, record) = verified(dir, list, response(&raw), &raw);
    assert!(view.contains("function add_"), "{view}");
    assert_eq!(record["pieces"][0]["command_index"], 1);

    let list = "cd sub; sed -n '1,600p' big.rs";
    let raw = native(dir, list);
    let (view, _) = verified(dir, list, response(&raw), &raw);
    assert!(view.contains("function add_"), "{view}");
}

#[test]
fn a_cd_outside_the_workspace_or_backtracking_stays_raw() {
    let temp = fixture();
    let dir = temp.path();
    let raw = native(dir, "cd / && sed -n '1,3p' etc/hosts");
    assert_eq!(
        hook(dir, "cd / && sed -n '1,3p' etc/hosts", Some(response(&raw))),
        json!({})
    );
    assert_eq!(
        hook(
            dir,
            "cd ../.. && sed -n '1,3p' big.rs",
            Some(response(&raw))
        ),
        json!({})
    );
}

#[test]
fn a_batch_the_recheck_cannot_verify_still_gets_the_ordinary_report() {
    let temp = fixture();
    let dir = temp.path();
    fs::create_dir(dir.join("sub")).unwrap();
    let old = fs::read_to_string(dir.join("big.rs")).unwrap();
    fs::write(
        dir.join("big.rs"),
        old.replace("wrapping_add", "wrapping_sub"),
    )
    .unwrap();
    // The isolated recheck leaves out repository settings, so its diff differs
    // from the one the agent saw.
    native(dir, "git config diff.noprefix true");
    for command in [
        "cd sub && git diff",
        "timeout 30 git diff",
        "git diff | head -400",
    ] {
        let raw = native(dir, command);
        let out = hook(dir, command, Some(response(&raw)));
        let shown = out["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .unwrap_or_else(|| panic!("{command} stayed raw"));
        assert!(shown.len() < raw.len(), "{command}: {shown}");
        assert!(shown.contains("diff section(s)"), "{command}: {shown}");
    }
}
