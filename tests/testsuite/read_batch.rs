#![cfg(unix)]

use serde_json::{Value, json};
use std::{fs, io::Write, path::Path, process::Command};
use tempfile::TempDir;

use crate::common::{BIN, has, piped, text};

fn native(dir: &Path, script: &str) -> String {
    let output = Command::new("/bin/sh")
        .args(["-c", script])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

fn hook(dir: &Path, command: &str, response: Option<Value>) -> Value {
    hook_in(dir, dir, command, response)
}

/// A hook event whose session reports `cwd`, as Claude Code's main session
/// does after a `cd`.
fn hook_in(dir: &Path, cwd: &Path, command: &str, response: Option<Value>) -> Value {
    let (mode, event) = if response.is_some() {
        ("after-bash", "PostToolUse")
    } else {
        ("before-command", "PreToolUse")
    };
    let input = json!({"hook_event_name":event,"tool_name":"Bash","cwd":cwd,"session_id":"batch-test",
        "tool_input":{"command":command},"tool_response":response});
    let mut command = Command::new(BIN);
    command.args(["--root", dir.to_str().unwrap(), "hook-claude", mode]);
    let out = piped(command.current_dir(dir), input.to_string().as_bytes());
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

fn response(text: &str) -> Value {
    json!({"stdout":text,"stderr":"","interrupted":false})
}

/// The view the post-run hook gives `list` run in `cwd`, if it compacts it.
fn compacted(dir: &Path, cwd: &Path, list: &str) -> Option<String> {
    let out = hook_in(dir, cwd, list, Some(response(&native(dir, list))));
    let view = out["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str();
    view.map(str::to_owned)
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
        &format!("git init -q && git add big.rs && {COMMIT} fixture"),
    );
    fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join("big.rs"))
        .unwrap()
        .write_all(b"// changed\n")
        .unwrap();
    temp
}

const COMMIT: &str =
    "git -c user.email=a@localhost -c user.name=fixture -c commit.gpgsign=false commit -qm";

fn verified(dir: &Path, list: &str, event: Value, expected: &str) -> (String, Value) {
    assert_eq!(hook(dir, list, None), json!({}));
    let result = hook(dir, list, Some(event));
    let view = result["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
        .as_str()
        .unwrap_or_else(|| panic!("{list} returned {result}"));
    assert_eq!(view.matches("Exact: ").count(), 1);
    assert!(view.len() < expected.len());
    let exact = view.lines().find(|s| s.starts_with("Exact: ")).unwrap();
    let id = exact.split_whitespace().last().unwrap();
    let run = dir.join(".justless/runs").join(id);
    let archived = fs::read(run.join("stdout")).unwrap();
    assert_eq!(archived, expected.as_bytes());
    let replay = Command::new(BIN)
        .args(["log", id])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(replay.status.success());
    assert_eq!(replay.stdout, archived);
    let record = serde_json::from_slice(&fs::read(run.join("record.json")).unwrap()).unwrap();
    (view.to_owned(), record)
}

/// The source `resume` lists as read or outlined, newest first.
fn resume_reads(dir: &Path) -> Vec<String> {
    let out = Command::new(BIN)
        .args(["resume", "--budget", "100000"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("Read ") || line.starts_with("Outline "))
        .map(str::to_owned)
        .collect()
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
        has(&view, &format!("$ {command} "));
        let bytes = native(dir, command);
        let piece = &record["pieces"][index];
        assert_eq!(piece["command_index"], index + 1);
        assert_eq!(piece["start_byte"], offset);
        assert_eq!(piece["first_line"], 1 + raw[..offset].matches('\n').count());
        offset += bytes.len();
        assert_eq!(piece["end_byte"], offset);
        let last = 1 + raw[..offset].matches('\n').count() - usize::from(bytes.ends_with('\n'));
        assert_eq!(piece["last_line"], last);
    }
    assert_eq!(offset, raw.len());
    assert_eq!(record["raw_bytes"], raw.len());
    let shown =
        record["preview_bytes"].as_u64().unwrap() + record["footer_bytes"].as_u64().unwrap();
    assert_eq!(shown, view.len() as u64);
    // Each file piece is recorded as the single-file read records it: lines
    // shown in full as read, an outline or a column cut as an outline.
    let mut reads = resume_reads(dir);
    reads.sort();
    assert_eq!(
        reads,
        [
            "Outline big.rs:1-170",
            "Outline big.rs:222-348",
            "Outline notes:1-2",
            "Read notes:2-2",
            "Read part:1-1",
        ]
    );
    fs::write(dir.join("notes"), "alpha\nbeta\n").unwrap();
    let stale = resume_reads(dir);
    let changed = stale.iter().filter(|r| r.ends_with(" [source changed]"));
    assert_eq!(changed.count(), 2, "{stale:?}");
    assert!(stale.contains(&"Read notes:2-2 [source changed]".to_owned()));
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
    has(&view, "git show HEAD:big.rs");
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
fn unsupported_mismatched_and_unverifiable_batches_leave_no_run() {
    let temp = fixture();
    let dir = temp.path();
    let list = "cat big.rs; tail -1 notes";
    let raw = native(dir, list);
    let hosts = native(dir, "cd / && sed -n '1,3p' etc/hosts");
    fs::write(dir.join("notes"), "different\n").unwrap();
    let ambiguous = "cat big.rs | head -300 && tail -1 notes";
    for (list, body) in [
        (list, &raw),
        (
            "head -1 notes; tail -1 notes",
            &native(dir, "head -1 notes; tail -1 notes"),
        ),
        ("cat big.rs; echo hello", &raw),
        ("cat big.rs; cut -c 1-2 < notes", &raw),
        ("cat big.rs; git diff --output=unexpected", &raw),
        ("cat big.rs; grep -n value big.rs --color=always", &raw),
        ("cat big.rs; cat ../outside", &raw),
        // A read outside the workspace that is small, and a `cd` back out of it.
        ("cd / && sed -n '1,3p' etc/hosts", &hosts),
        ("cd ../.. && sed -n '1,3p' big.rs", &hosts),
        // A pipe's status is unknown, so the list's short circuit is too.
        (ambiguous, &native(dir, ambiguous)),
    ] {
        assert_eq!(hook(dir, list, Some(response(body))), json!({}), "{list}");
    }
    // The recheck never runs an external diff program.
    native(dir, "git config diff.external 'touch external-ran'");
    let simulated = fs::read_to_string(dir.join("big.rs")).unwrap() + "custom diff\n";
    let list = "cat big.rs; git diff";
    assert_eq!(hook(dir, list, Some(response(&simulated))), json!({}));
    assert!(!dir.join("external-ran").exists());
    assert!(!dir.join(".justless/runs").exists());
    assert!(resume_reads(dir).is_empty());
    assert!(!dir.join("unexpected").exists());
}

#[test]
fn a_glob_is_read_as_the_files_it_names() {
    let temp = fixture();
    let dir = temp.path();
    fs::create_dir(dir.join("src")).unwrap();
    for name in ["a.rs", "b.rs"] {
        fs::copy(dir.join("big.rs"), dir.join("src").join(name)).unwrap();
    }
    for (list, piece) in [
        (
            "sed -n '1,170p' big.rs; grep -n add_00 src/*.rs",
            "$ grep -n add_00 src/a.rs src/b.rs [stdout:",
        ),
        (
            "wc -l src/*.rs; cat big.rs",
            "$ wc -l src/a.rs src/b.rs [stdout:1-3]",
        ),
        ("cat src/?.rs", "$ cat src/b.rs [stdout:"),
        (
            "cd src && grep -c add_ *.rs; cat a.rs",
            "$ grep -c add_ a.rs b.rs [stdout:1-2]",
        ),
    ] {
        let raw = native(dir, list);
        let (view, _) = verified(dir, list, response(&raw), &raw);
        has(&view, piece);
    }
    // A glob that names no file, and a pattern the recheck does not expand.
    for list in [
        "grep -n add_00 src/*.zz; sed -n '1,170p' big.rs",
        "cat src/[ab].rs",
    ] {
        let raw = native(dir, list);
        assert_eq!(hook(dir, list, Some(response(&raw))), json!({}), "{list}");
    }
}

#[test]
fn filters_and_quoted_operators_apply_before_accounting() {
    let temp = fixture();
    let dir = temp.path();
    fs::write(dir.join("a; b"), "δ beta\n").unwrap();
    // Use real rg where installed. grep supplies the same numbered shape on machines without rg.
    let installed = [
        "/usr/bin/rg",
        "/opt/homebrew/bin/rg",
        "/usr/local/bin/rg",
        "/bin/rg",
    ];
    let search = if installed.iter().any(|p| Path::new(p).is_file())
        && Command::new("rg").arg("--version").output().is_ok()
    {
        "rg"
    } else {
        "grep -E"
    };
    let list = format!(
        "head -1 'a; b'; sed -n '1,4p;3,4p' big.rs | tail -3; {search} -Hn 'add_0|unmatched' big.rs | cut -c 1-72; cat big.rs | tail -n +2 | head -250; cat big.rs | sed -n '300,560p'"
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
        let shown = compacted(dir, dir, command).unwrap_or_default();
        assert!(shown.contains("big.rs"), "{command}: {shown}");
    }
}

#[test]
fn cd_lists_compact_in_their_directory() {
    let temp = fixture();
    let dir = temp.path();
    fs::create_dir(dir.join("sub")).unwrap();
    fs::copy(dir.join("big.rs"), dir.join("sub/big.rs")).unwrap();
    native(dir, &format!("git add sub/big.rs && {COMMIT} sub"));
    // A `cd` before one read is the single-file read; this list has two pieces.
    let list = "cd sub && sed -n '1,600p' big.rs && tail -n 1 big.rs";
    let raw = native(dir, list);
    let (view, record) = verified(dir, list, response(&raw), &raw);
    has(&view, "function add_");
    assert_eq!(record["pieces"][0]["command_index"], 1);

    let list = "cd sub; sed -n '1,600p' big.rs";
    let raw = native(dir, list);
    let (view, _) = verified(dir, list, response(&raw), &raw);
    has(&view, "function add_");

    // Claude Code's main session keeps the `cd`, so it reports the directory
    // the list entered rather than the one it started from.
    for list in [
        "cd sub && sed -n '1,600p' big.rs",
        "cd ./sub && sed -n '1,600p' big.rs",
    ] {
        let shown = compacted(dir, &dir.join("sub"), list);
        let shown = shown.unwrap_or_else(|| panic!("{list} stayed raw"));
        has(&shown, "sub/big.rs lines 1-600");
    }
}

#[test]
fn a_cd_the_shell_may_skip_never_moves_later_reads() {
    let temp = fixture();
    let dir = temp.path();
    fs::create_dir(dir.join("sub")).unwrap();
    fs::copy(dir.join("big.rs"), dir.join("sub/big.rs")).unwrap();
    // Each `cd` is skipped, so `sed` reads the top-level big.rs. Its copy in sub
    // has the same bytes, so only the attribution could go wrong.
    for command in [
        "grep -n alpha notes || cd sub; sed -n '1,600p' big.rs",
        "grep -n absent notes && cd sub; sed -n '1,600p' big.rs",
    ] {
        let shown = compacted(dir, dir, command).unwrap_or_default();
        assert!(!shown.contains("sub/big.rs"), "{command}: {shown}");
    }
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
        let shown = compacted(dir, dir, command);
        let shown = shown.unwrap_or_else(|| panic!("{command} stayed raw"));
        assert!(shown.len() < raw.len(), "{command}: {shown}");
        assert!(shown.contains("diff section(s)"), "{command}: {shown}");
    }
}

#[test]
fn git_show_pieces_outline_the_blob_at_their_revision() {
    let temp = fixture();
    let dir = temp.path();
    // HEAD~1 holds the first big.rs, the index a renamed copy and the disk a third.
    native(
        dir,
        &format!(
            "{COMMIT} second -a && sed 's/add_/staged_/' big.rs > staged && mv staged big.rs \
             && git add big.rs && sed 's/staged_/disk_/' big.rs > disk && mv disk big.rs"
        ),
    );
    let list =
        "git show HEAD~1:big.rs | sed -n '1,400p'; git show :big.rs | head -2; tail -1 notes";
    let raw = native(dir, list);
    let (view, _) = verified(dir, list, response(&raw), &raw);
    has(&view, "HEAD~1:big.rs lines 1-400\n");
    has(&view, "function add_000");
    has(&view, "pub fn staged_000");
    assert!(!view.contains("disk_"), "{view}");
    // A blob shows a revision, not the working file.
    assert_eq!(resume_reads(dir), ["Read notes:2-2"]);
}
