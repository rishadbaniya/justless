//! Compact views of ordinary previews: JSON tables, Cargo.lock dependency changes and
//! the generic floor. Each keeps the exact output one `justless log` away.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, git, has, piped, run_id, starts, text, words};

fn workspace(settings: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".justless.toml"), settings).unwrap();
    dir
}

fn justless(root: &Path, args: &[&str]) -> Output {
    let output = Command::new(BIN)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    output
}

/// The stdout of `line`, split at spaces into arguments.
fn stdout(root: &Path, line: &str) -> String {
    text(&justless(root, &words(line)).stdout)
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
        let output = justless(root, &words("run --hook -- cat data.json"));
        let shown = text(&output.stdout);
        starts(&shown, "[JSON array as a table: ");
        assert!(shown.contains(header) && shown.contains(first), "{shown}");
        assert!(shown.len() < json.len() / 2, "{shown}");
        assert_eq!(stdout(root, &format!("log {}", run_id(&output))), json);
        // A typed run that fits could feed a pipe, so it stays verbatim.
        assert_eq!(stdout(root, "run --fresh -- cat data.json"), json);
    }
}

#[cfg(unix)]
#[test]
fn json_reads_stay_exact_when_they_fit_unless_compact_json_is_smaller() {
    let dir = workspace("");
    let root = dir.path();
    // A Secret in one line, whose base64 the floor would cut, and a pretty-printed
    // Deployment that holds no array of objects.
    let base64 = "QmFzZTY0IGRhdGEgdGhhdCBydW5zIGxvbmcK".repeat(8);
    let secret = format!(
        "{{\"apiVersion\":\"v1\",\"data\":{{\"tls.crt\":\"{base64}\"}},\"kind\":\"Secret\"}}\n"
    );
    let deployment = json!({"apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "api", "labels": {"app": "ledger", "tier": "api"}},
        "spec": {"replicas": 3, "selector": {"matchLabels": {"app": "ledger"}},
            "template": {"spec": {"containers": [{"name": "api", "image": "ledger:1.4"}]}}},
        "status": {"readyReplicas": 3, "updatedReplicas": 3}});
    let pretty = format!("{}\n", serde_json::to_string_pretty(&deployment).unwrap());
    fs::write(root.join("secret.json"), &secret).unwrap();
    fs::write(root.join("deploy.json"), &pretty).unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    crate::common::executable(&bin.join("kubectl"), "#!/bin/sh\ncat \"$2.json\"\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let run = |line: &str| {
        let mut command = Command::new(BIN);
        command
            .args(words(line))
            .current_dir(root)
            .env("PATH", &path);
        let output = command.output().unwrap();
        assert!(output.status.success(), "{line}: {output:?}");
        output
    };
    let output = run("run --hook -- kubectl get secret -o json");
    assert_eq!(text(&output.stdout), secret);
    assert_eq!(text(&output.stderr), "");
    let output = run("run --hook -- kubectl get deploy -o json");
    let compact = serde_json::to_string(&deployment).unwrap();
    assert_eq!(
        text(&output.stdout),
        format!("[JSON without its whitespace; exact JSON in the full log]\n{compact}\n")
    );
    let log = run(&format!("log {}", run_id(&output)));
    assert_eq!(text(&log.stdout), pretty);
    // A typed run that fits could feed a pipe, so it stays verbatim.
    let typed = run("run --fresh -- kubectl get deploy -o json");
    assert_eq!(text(&typed.stdout), pretty);
}

#[test]
fn compact_json_too_long_to_cut_gives_way_to_tables() {
    // Quoted keys make the tables larger than the compact JSON, which is one line
    // longer than the budget, so the tables are cut instead.
    let mut members: Vec<String> = (0..70)
        .map(|i| format!("  \"note {i:02}\": \"value {i} for this resource, long enough\""))
        .collect();
    let ports: Vec<String> = (0..6)
        .map(|i| {
            let port = 8000 + i;
            format!("    {{\"name\": \"port-{i}\", \"port\": {port}, \"protocol\": \"TCP\"}}")
        })
        .collect();
    members.push(format!("  \"ports\": [\n{}\n  ]", ports.join(",\n")));
    let json = format!("{{\n{}\n}}\n", members.join(",\n"));
    let dir = workspace("");
    let root = dir.path();
    fs::write(root.join("data.json"), &json).unwrap();
    let shown = stdout(root, "run --hook --max-bytes 2048 -- cat data.json");
    starts(&shown, "[JSON object with arrays of objects as tables: ");
    has(&shown, "\"note 00\": \"value 0");
    has(&shown, " more lines of this view in the full log]\n");
}

#[test]
fn previews_collapse_progress_encoded_runs_and_repeats() {
    let dir = workspace("");
    let root = dir.path();
    let frames: String = (0..=100).map(|i| format!("\r{i:>3}%")).collect();
    let blob: String = (0..400).map(|i| ["Qm", "x9", "aZ", "k+"][i % 4]).collect();
    let noisy = format!(
        "fetch{frames}\n{}error: kept\nerror: kept\nkey {blob}\n",
        "waiting\n".repeat(200)
    );
    fs::write(root.join("noisy.txt"), &noisy).unwrap();
    let floored = format!(
        "100% [101 earlier progress frame(s) cut]\nwaiting\n[last line repeated 199 more time(s)]\n\
         error: kept\nerror: kept\nkey {}[… 776 base64 characters cut …]{}\n",
        &blob[..12],
        &blob[788..]
    );
    let output = justless(root, &words("run --hook -- cat noisy.txt"));
    assert_eq!(text(&output.stdout), floored);
    assert_eq!(stdout(root, &format!("log {}", run_id(&output))), noisy);
    // A typed run that fits could feed a pipe, so it stays verbatim, but a clipped
    // one folds its head and tail.
    assert_eq!(stdout(root, "run --fresh -- cat noisy.txt"), noisy);
    let shown = stdout(root, "run --fresh --max-bytes 1024 -- cat noisy.txt");
    starts(
        &shown,
        "100% [101 earlier progress frame(s) cut]\nwaiting\n[last line repeated 199",
    );
    has(&shown, "error: kept\nerror: kept\nkey Qmx9");
}

/// The Claude Code post-run hook's rewrite of a successful Bash result, if any.
fn after_bash(root: &Path, command: &str, stdout: &str) -> Option<String> {
    let input = json!({
        "hook_event_name": "PostToolUse", "tool_name": "Bash", "session_id": "s",
        "cwd": root, "tool_input": {"command": command},
        "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false},
    });
    let mut hook = Command::new(BIN);
    hook.args(["hook-claude", "after-bash"]).current_dir(root);
    let output = piped(&mut hook, input.to_string().as_bytes());
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    let view = reply["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str();
    view.map(str::to_owned)
}

#[cfg(unix)]
#[test]
fn the_post_run_hook_previews_large_output_of_unknown_commands() {
    let dir = workspace("");
    let root = dir.path().canonicalize().unwrap();
    let issues: Vec<String> = (0..200)
        .map(|i| {
            let login = i % 3;
            format!(r#"{{"number":{i},"state":"open","title":"Issue {i}","user":{{"login":"u{login}"}}}}"#)
        })
        .collect();
    let issues = format!("[{}]\n", issues.join(","));
    let progress = format!("{}done\n", "waiting for the build\n".repeat(600));
    let repeated = "waiting for the build\n[last line repeated 599 more time(s)]\ndone\n";
    for (command, stdout, shown) in [
        (
            "gh api repos/acme/widget/issues",
            issues.as_str(),
            Some("[JSON array as a table: "),
        ),
        ("python3 build.py", progress.as_str(), Some(repeated)),
        // So does a pipe, a list, a loop or a script fed by a here-document.
        (
            "python3 build.py | tail -n 700",
            progress.as_str(),
            Some(repeated),
        ),
        (
            "cd tools && python3 build.py; echo done",
            progress.as_str(),
            Some(repeated),
        ),
        (
            "for i in 1 2; do python3 build.py $i; done",
            progress.as_str(),
            Some(repeated),
        ),
        (
            "python3 - <<'PY'\nimport build\nprint(open('cat').read())\nPY",
            progress.as_str(),
            Some(repeated),
        ),
        // Output an agent may edit or copy, and short output, stay whole.
        ("strings build.bin", progress.as_str(), None),
        ("python3 build.py", &progress[..3000], None),
        ("python3 build.py; cat build.log", progress.as_str(), None),
        (
            "for f in a b; do sed -n 1,40p $f; done",
            progress.as_str(),
            None,
        ),
        ("python3 list.py | xargs cat", progress.as_str(), None),
        (
            "while read line; do echo \"$line\"; done < build.log",
            progress.as_str(),
            None,
        ),
        // A command a hook supports keeps its own rules in any shape.
        (
            "for f in src/*; do grep -n waiting $f; done",
            progress.as_str(),
            None,
        ),
        (
            "cargo test 2>&1 | python3 filter.py",
            progress.as_str(),
            None,
        ),
        ("./target/debug/justless stats", progress.as_str(), None),
    ] {
        let rewritten = after_bash(&root, command, stdout);
        let Some(start) = shown else {
            assert_eq!(rewritten, None, "{command}");
            continue;
        };
        let compacted = rewritten.unwrap_or_else(|| panic!("{command} was not compacted"));
        assert!(compacted.starts_with(start), "{command}: {compacted}");
        assert!(compacted.len() < stdout.len() / 2, "{command}: {compacted}");
        let id = compacted.rsplit_once("[justless ").unwrap().1;
        let id = id.split(' ').next().unwrap();
        let log = justless(&root, &["log", id]);
        assert_eq!(text(&log.stdout), stdout);
    }
}

/// A Cargo.lock holding `(name, version, checksum, dependencies)` packages.
fn lockfile(packages: &[(&str, &str, &str, &str)]) -> String {
    let mut text = String::from("version = 4\n");
    for (name, version, checksum, deps) in packages {
        text.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\ndependencies = [{deps}]\n"
        ));
    }
    text
}

#[cfg(unix)]
#[test]
fn cargo_lock_patches_name_dependency_changes_and_flag_substitutions() {
    let dir = workspace("");
    let root = dir.path();
    git(root, &["init", "-q"]);
    // Every fifth filler package is updated, so the patch has many hunks.
    let lock = |updated: bool, head: &[(&str, &str, &str, &str)]| {
        let names: Vec<String> = (0..40).map(|i| format!("pkg{i}")).collect();
        let mut packages = head.to_vec();
        packages.extend(names.iter().enumerate().map(|(i, name)| {
            let bumped = updated && i % 5 == 0;
            (
                name.as_str(),
                if bumped { "1.0.1" } else { "1.0.0" },
                "00",
                "",
            )
        }));
        lockfile(&packages)
    };
    let old = lock(
        false,
        &[
            ("tokio", "1.40.0", "aa", "\"mio\""),
            ("mio", "1.0.0", "bb", ""),
        ],
    );
    fs::write(root.join("Cargo.lock"), old).unwrap();
    fs::write(root.join("lib.rs"), "fn a() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    let new = lock(
        true,
        &[
            ("tokio", "1.41.0", "cc", "\"mio\", \"signal-hook-registry\""),
            ("mio", "1.0.0", "dd", ""),
            ("signal-hook-registry", "1.4.2", "ee", ""),
        ],
    );
    fs::write(root.join("Cargo.lock"), new).unwrap();
    let summary = "Cargo.lock dependencies: 1 added, 9 updated, 1 with changed dependencies, 1 source or checksum change(s)";
    let flag = "checksum changed on existing version mio 1.0.0: bb -> dd";
    // The working tree has no blob for the new side, so the file is read.
    let output = justless(root, &words("run --hook -- git diff"));
    let shown = text(&output.stdout);
    for expected in [
        summary,
        flag,
        "pkg5 1.0.0 -> 1.0.1, tokio 1.40.0 -> 1.41.0\n",
        "dependencies: tokio +signal-hook-registry\n",
    ] {
        has(&shown, expected);
    }
    let log = stdout(root, &format!("log {}", run_id(&output)));
    assert_eq!(log, git(root, &["diff"]));
    // A commit, and a patch too large to show whole, whose hunk summary describes
    // the lockfile with the flag on top.
    let large: String = (0..300).map(|i| format!("fn f{i}() {{}}\n")).collect();
    fs::write(root.join("lib.rs"), large).unwrap();
    git(root, &["commit", "-qam", "bump"]);
    has(
        &stdout(root, "run --hook -- git show HEAD -- Cargo.lock"),
        summary,
    );
    let diff = "run --hook -- git diff HEAD~1";
    let shown = stdout(root, diff);
    has(&shown, &format!(" diff section(s)\n! Cargo.lock: {flag}\n"));
    has(&shown, summary);
    assert!(!shown.contains("Cargo.lock @@"), "{shown}");
    // A commit too large to show whole lists its header, then the description with
    // the flag on top, and each `[stdout:A-B]` replays exactly those lines.
    let output = justless(root, &words("run --max-bytes 1024 -- git show HEAD"));
    let shown = text(&output.stdout);
    let header = format!("1 commit(s), 9 diff section(s)\n! Cargo.lock: {flag}\ncommit ");
    starts(&shown, &header);
    has(&shown, " bump [stdout:1-5]\n");
    has(&shown, summary);
    assert!(!shown.contains("Cargo.lock @@"), "{shown}");
    let patch: Vec<String> = git(root, &["show", "HEAD"])
        .split_inclusive('\n')
        .map(str::to_owned)
        .collect();
    let id = run_id(&output);
    for tag in shown.split("[stdout:").skip(1) {
        let (from, to) = tag.split_once(']').unwrap().0.split_once('-').unwrap();
        let replay = stdout(root, &format!("log {id} --from {from} --to {to}"));
        let (from, to): (usize, usize) = (from.parse().unwrap(), to.parse().unwrap());
        assert_eq!(replay, patch[from - 1..to].concat());
    }
    // Any other preview still names the flag: a rerun's unchanged notice and a
    // clipped commit whose `--stat` the summary does not read.
    let clipped = "run --hook --max-bytes 1024 -- git show --stat --patch HEAD";
    for (line, start) in [(diff, "Output unchanged"), (clipped, "commit ")] {
        starts(
            &stdout(root, line),
            &format!("! Cargo.lock: {flag}\n{start}"),
        );
    }
}
