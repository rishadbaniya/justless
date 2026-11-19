//! Machine-wide setup: hook kill switches and `justless rewrite`, the project
//! registry, and user-level hooks. Every command runs with `HOME`, the state home
//! and the Claude Code and Codex config homes inside the test's own folder.
//! Command hooks pass every command through on Windows, so these run on Unix.
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

    /// A Cargo project under the lab, with optional `.justless.toml` settings.
    fn project(&self, name: &str, settings: &str) -> PathBuf {
        let root = self.base.join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        if !settings.is_empty() {
            fs::write(root.join(".justless.toml"), settings).unwrap();
        }
        root
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_justless"));
        command
            .args(args)
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env(
                "JUSTLESS_TEST_CLAUDE_MANAGED_DIR",
                self.base.join("managed"),
            )
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("JUSTLESS_DISABLE");
        command
    }

    fn output(
        &self,
        cwd: &Path,
        args: &[&str],
        input: Option<&Value>,
        env: &[(&str, &str)],
    ) -> Output {
        let mut child = self
            .command(cwd, args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        if let Some(input) = input {
            stdin.write_all(input.to_string().as_bytes()).unwrap();
        }
        drop(stdin);
        child.wait_with_output().unwrap()
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.output(cwd, args, None, &[]);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn hook(&self, cwd: &Path, args: &[&str], input: &Value, env: &[(&str, &str)]) -> String {
        let output = self.output(cwd, args, Some(input), env);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

fn bash(cwd: &Path, command: &str, mode: Option<&str>) -> Value {
    let mut input = json!({
        "hook_event_name": "PreToolUse", "tool_name": "Bash", "session_id": "",
        "cwd": cwd, "tool_input": {"command": command},
    });
    if let Some(mode) = mode {
        input["permission_mode"] = json!(mode);
    }
    input
}

fn routed(answer: &str) -> Option<String> {
    let answer: Value = serde_json::from_str(answer).unwrap();
    answer["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .map(str::to_owned)
}

#[test]
fn kill_switches_leave_commands_alone_and_rewrite_explains_the_hook() {
    let lab = Lab::new();
    let root = lab.project(
        "work",
        "exclude_commands = [\"cargo bench\", \"^git (log|show)\"]\n",
    );
    let codex = |command: &str| {
        lab.hook(
            &root,
            &["hook-codex", "before-command"],
            &bash(&root, command, None),
            &[],
        )
    };
    assert_eq!(
        routed(&codex("cargo test")).as_deref(),
        Some("justless run --hook -- cargo test")
    );
    for command in [
        "cargo bench",
        "RUST_LOG=1 cargo bench --all",
        "git log -3",
        "cargo bench | tail",
    ] {
        assert_eq!(codex(command).trim(), "{}", "{command}");
    }
    // Codex runs a rewritten list without asking, so one excluded command keeps the
    // whole list as written. Claude Code checks the rewrite, so the rest is routed.
    assert_eq!(codex("cargo bench && cargo test").trim(), "{}");
    let claude = lab.hook(
        &root,
        &["hook-claude", "before-command"],
        &bash(
            &root,
            "cargo bench && cargo test",
            Some("bypassPermissions"),
        ),
        &[],
    );
    assert_eq!(
        routed(&claude).as_deref(),
        Some("cargo bench && justless run --hook -- cargo test")
    );

    // The post-run hook leaves an excluded command's output alone.
    let long: String = (0..400)
        .map(|line| format!("test case_{line} ... ok\n"))
        .collect();
    let after = |command: &str| {
        let input = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "session_id": "s", "cwd": root,
            "tool_input": {"command": command},
            "tool_response": {"stdout": long, "stderr": "", "interrupted": false, "isImage": false},
        });
        lab.hook(&root, &["hook-claude", "after-bash"], &input, &[])
    };
    for (bench, test) in [
        (
            "cargo bench 2>&1 | tail -n 400",
            "cargo test 2>&1 | tail -n 400",
        ),
        (
            "cd src && cargo bench | tail -n 400",
            "cd src && cargo test | tail -n 400",
        ),
        (
            "RUST_LOG=1 cargo bench 2>&1 | tail -n 400",
            "RUST_LOG=1 cargo test 2>&1 | tail -n 400",
        ),
        (
            "timeout 60 cargo bench | tail -n 400",
            "timeout 60 cargo test | tail -n 400",
        ),
    ] {
        assert_ne!(after(test).trim(), "{}", "{test}");
        assert_eq!(after(bench).trim(), "{}", "{bench}");
    }

    // `JUSTLESS_DISABLE` answers every hook with its client's no-op.
    let off = [("JUSTLESS_DISABLE", "1")];
    let input = bash(&root, "cargo test", Some("bypassPermissions"));
    assert_eq!(
        lab.hook(&root, &["hook-claude", "before-command"], &input, &off)
            .trim(),
        "{}"
    );
    assert_eq!(
        lab.hook(
            &root,
            &["hook-claude", "before-compact"],
            &json!({"hook_event_name": "PreCompact"}),
            &off
        ),
        ""
    );
    assert_eq!(
        lab.hook(&root, &["hook-agy", "before-command"], &json!({}), &off)
            .trim(),
        r#"{"decision":"allow"}"#
    );
    assert_eq!(
        lab.hook(&root, &["hook-cursor", "before-command"], &json!({}), &off)
            .trim(),
        "{}"
    );
    assert!(
        lab.hook(
            &root,
            &["hook-claude", "before-command"],
            &input,
            &[("JUSTLESS_DISABLE", "0")]
        )
        .contains("justless run")
    );

    // `rewrite` prints the hook's own rewrite and records nothing.
    fs::create_dir_all(root.join(".justless")).unwrap();
    let report = lab.run(&root, &["rewrite", "--agent", "codex", "cargo test"]);
    assert!(
        report.starts_with("Codex hook: rewritten\n  justless run --hook -- cargo test\n"),
        "{report}"
    );
    let report = lab.run(
        &root,
        &["rewrite", "--agent", "codex", "cargo bench && cargo test"],
    );
    assert!(
        report.contains("only when every command in it is supported and none is excluded"),
        "{report}"
    );
    assert!(
        report.contains("Excluded: `cargo bench` matches `cargo bench`"),
        "{report}"
    );
    let report = lab.run(&root, &["rewrite", "git show HEAD"]);
    assert!(
        report.contains("matches `^git (log|show)` in exclude_commands"),
        "{report}"
    );
    let report = lab.run(&root, &["rewrite", "ls", "--agent", "codex"]);
    assert!(
        report.contains("no command in it is one the hook routes"),
        "{report}"
    );
    // Claude Code's verdict: no rule allows `cargo test` in default mode.
    let report = lab.run(&root, &["rewrite", "cargo test"]);
    assert!(
        report.contains("unchanged\nWhy: Claude Code's permission rules"),
        "{report}"
    );
    assert!(
        report.contains("Permission: no rule allows it in default mode"),
        "{report}"
    );
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::write(
        root.join(".claude/settings.json"),
        r#"{"permissions":{"allow":["Bash(cargo test:*)"]}}"#,
    )
    .unwrap();
    let report = lab.run(&root, &["rewrite", "cargo test", "--mode", "acceptEdits"]);
    assert!(
        report.contains("rewritten\n  justless run --hook -- cargo test\n"),
        "{report}"
    );
    assert!(report.contains("allow rules cover every command in it in acceptEdits mode, so the hook allows the rewrite"), "{report}");
    assert!(!root.join(".justless/activity.json").exists());

    // An invalid expression is a settings error, not a silent pass.
    fs::write(root.join(".justless.toml"), "exclude_commands = [\"^(\"]\n").unwrap();
    let output = lab.output(&root, &["run", "--", "true"], None, &[]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("invalid exclude_commands regex"), "{error}");
}

#[test]
fn discover_reports_excluded_commands_in_their_own_group() {
    let lab = Lab::new();
    let root = lab.project(
        "work",
        "exclude_commands = [\"cargo build\", \"^git log\"]\n",
    );
    let commands = [
        "cargo build",
        "git log -3",
        "cargo build | tail",
        "RUST_LOG=1 timeout 60 cargo build 2>&1 | tail",
        "cd src && cargo build | tail",
        "cargo test | tail",
        "cargo build && cargo test",
        "cargo test",
    ];
    // Claude Code transcripts, and a Codex rollout with the same commands.
    let slug: String = root
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcripts = lab.home.join(".claude/projects").join(slug);
    let rollouts = lab.home.join(".codex/sessions/2026/11/19");
    fs::create_dir_all(&transcripts).unwrap();
    fs::create_dir_all(&rollouts).unwrap();
    let (mut claude, mut codex) = (
        Vec::new(),
        vec![json!({"type": "session_meta",
        "timestamp": "2099-01-01T00:00:00.000Z",
        "payload": {"id": "t", "session_id": "t", "cwd": root, "source": "cli"}})],
    );
    for (id, command) in commands.iter().enumerate() {
        let (id, output) = (format!("c{id}"), "x".repeat(1000));
        claude.push(
            json!({"cwd": root, "timestamp": "2999-01-01T00:00:00Z", "message": {"content": [
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}]}}),
        );
        claude.push(
            json!({"cwd": root, "timestamp": "2999-01-01T00:00:01Z", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "content": output}]}}),
        );
        codex.push(
            json!({"timestamp": "2099-01-01T00:00:00.000Z", "type": "event_msg",
            "payload": {"type": "item_completed", "item": {"type": "CommandExecution", "id": id,
            "command": ["/bin/zsh", "-lc", command], "cwd": format!("file://{}", root.display()),
            "exit_code": 0, "aggregated_output": output, "source": "unified_exec_startup"}}}),
        );
    }
    let lines = |records: Vec<Value>| records.iter().map(|r| format!("{r}\n")).collect::<String>();
    fs::write(transcripts.join("s.jsonl"), lines(claude)).unwrap();
    fs::write(
        rollouts.join("rollout-2026-11-19T10-00-00-t.jsonl"),
        lines(codex),
    )
    .unwrap();

    let report: Value =
        serde_json::from_str(&lab.run(&root, &["discover", "--json", "--plan"])).unwrap();
    let labels = |groups: &Value, route: &str| -> Vec<String> {
        let groups = groups.as_array().unwrap();
        let group = groups.iter().find(|group| group["route"] == route);
        let labels = group.map_or(&[][..], |group| group["labels"].as_array().unwrap());
        labels
            .iter()
            .map(|l| format!("{} ×{}", l["label"].as_str().unwrap(), l["commands"]))
            .collect()
    };
    // Claude Code checks a rewrite, so the rest of a list with an excluded command
    // is still routed; Codex keeps such a list as written. Claude Code's post-run
    // hook leaves an excluded pipe alone; Codex has none.
    assert_eq!(
        labels(&report["groups"], "excluded"),
        ["cargo build ×4", "git log ×1"]
    );
    assert_eq!(
        labels(&report["groups"], "supported_not_routed"),
        ["cargo build ×1", "cargo test ×1"]
    );
    assert_eq!(
        labels(&report["groups"], "passed_through"),
        ["cargo test ×1"]
    );
    assert_eq!(
        labels(&report["codex"]["groups"], "excluded"),
        ["cargo build ×2", "git log ×1"]
    );
    assert_eq!(
        labels(&report["codex"]["groups"], "passed_through"),
        ["cargo build ×3", "cargo test ×1"]
    );
    assert_eq!(
        labels(&report["codex"]["groups"], "supported_not_routed"),
        ["cargo test ×1"]
    );
    let plan = report["plan"].as_array().unwrap();
    assert!(plan.iter().all(|row| row["label"] != "git log"), "{plan:?}");
    let text = lab.run(&root, &["discover"]);
    assert!(
        text.contains("Supported, excluded by exclude_commands: 5 command(s)")
            && !text.contains("xxxx"),
        "{text}"
    );
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=Lab", "-c", "user.email=lab@example.com"])
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// The run ID a `justless run` names on stderr.
fn run_id(lab: &Lab, cwd: &Path, script: &str) -> String {
    let output = lab.output(cwd, &["run", "--", "sh", "-c", script], None, &[]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let start = stderr.find("[justless run-").unwrap() + "[justless ".len();
    stderr[start..].split([' ', ']']).next().unwrap().to_owned()
}

#[test]
fn the_registry_totals_projects_by_repository_and_finds_their_runs() {
    let lab = Lab::new();
    let main = lab.project("main", "");
    git(&main, &["init", "-q"]);
    git(&main, &["add", "Cargo.toml"]);
    git(&main, &["commit", "-qm", "start"]);
    git(&main, &["worktree", "add", "-q", "../tree"]);
    let tree = lab.base.join("tree");
    let scratch = lab.project("tmp/scratch", "");
    run_id(&lab, &main, "seq 1 3000");
    run_id(&lab, &tree, "seq 1 3000");
    let elsewhere = run_id(&lab, &scratch, "seq 1 3000; echo needle");

    let file = lab.home.join(".local/state/justless/projects.json");
    let text = fs::read_to_string(&file).unwrap();
    assert!(!text.contains("seq"), "{text}");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let registry: Value = serde_json::from_str(&text).unwrap();
    let projects = registry["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 3, "{text}");
    let repository = |root: &Path| {
        let entry = projects.iter().find(|entry| entry["root"] == json!(root));
        entry.unwrap()["repository"].clone()
    };
    assert_eq!(repository(&main), json!(main.join(".git")));
    assert_eq!(repository(&tree), repository(&main));
    assert_eq!(repository(&scratch), Value::Null);

    let temp = lab.base.join("tmp");
    let temp = [("JUSTLESS_TEST_TEMP_DIRS", temp.to_str().unwrap())];
    let stats = |args: &[&str]| -> Value {
        let output = lab.output(&main, args, None, &temp);
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let all = stats(&["stats", "--all", "--json"]);
    assert_eq!(
        (&all["projects"], &all["runs"]),
        (&json!(2), &json!(2)),
        "{all}"
    );
    assert_eq!(all["temporary_projects_left_out"], 1);
    assert_eq!(all["repositories"][0]["repository"], json!(main));
    assert_eq!(all["repositories"][0]["roots"].as_array().unwrap().len(), 2);
    let all = stats(&["stats", "--all", "--include-temp", "--json"]);
    assert_eq!(
        (&all["projects"], &all["runs"]),
        (&json!(3), &json!(3)),
        "{all}"
    );
    assert_eq!(all["repositories"].as_array().unwrap().len(), 2);
    let output = lab.output(&main, &["stats", "--all"], None, &temp);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("main (2 checkouts)  2 run(s)"), "{text}");
    assert!(text.starts_with("In the last 7 day(s), kept "), "{text}");
    // `--days` counts the runs that finished in the window.
    let old = run_id(&lab, &main, "seq 1 3000");
    let record = main.join(".justless/runs").join(&old).join("record.json");
    let mut run: Value = serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    run["completed_ms"] = json!(1_000);
    fs::write(&record, run.to_string()).unwrap();
    let all = stats(&["stats", "--all", "--days", "30", "--json"]);
    assert_eq!(
        (&all["days"], &all["runs"]),
        (&json!(30), &json!(2)),
        "{all}"
    );
    assert!(
        text.contains("Left out 1 project(s) in temporary folders"),
        "{text}"
    );

    // A run made in another project is found through the registry.
    let replay = lab.run(&main, &["log", &elsewhere, "--from", "2", "--to", "3"]);
    assert_eq!(replay, "2\n3\n");
    assert!(
        lab.run(&main, &["search", &elsewhere, "needle"])
            .contains("needle")
    );
    let context = lab.run(&tree, &["context", "--run", &elsewhere]);
    assert!(
        context.starts_with(&format!("Run {elsewhere}")),
        "{context}"
    );
    let unknown = lab.output(&main, &["log", "run-unknown1"], None, &[]);
    let error = String::from_utf8_lossy(&unknown.stderr);
    assert!(error.contains("unknown run ID: run-unknown1"), "{error}");

    // Doctor fails here, in a project with no setup, and still lists the registry.
    let doctor = lab.output(&main, &["doctor"], None, &[]).stdout;
    let doctor = String::from_utf8_lossy(&doctor);
    assert!(
        doctor.contains("Project registry: 3 projects at "),
        "{doctor}"
    );

    // `--purge` drops the project, and the registry is rebuilt as projects are used.
    lab.run(&scratch, &["init", "--remove", "--purge"]);
    assert!(!fs::read_to_string(&file).unwrap().contains("scratch"));
    fs::remove_file(&file).unwrap();
    run_id(&lab, &main, "true");
    assert!(fs::read_to_string(&file).unwrap().contains("main"));
}

#[test]
fn global_hooks_serve_each_project_once_and_remove_cleanly() {
    let lab = Lab::new();
    let claude = lab.home.join(".claude/settings.json");
    let codex = lab.home.join(".codex/hooks.json");
    fs::create_dir_all(claude.parent().unwrap()).unwrap();
    let original = "{\n  \"model\": \"opus\",\n  \"env\": {\"A\": \"1\"}\n}\n";
    fs::write(&claude, original).unwrap();
    let plain = lab.project("plain", "");
    let opted = lab.project("opted", "global_hooks = false\n");
    let own = lab.project("own", "");
    lab.run(&own, &["init", "--hooks", "--agent", "claude"]);

    // Without --agent, setup takes the clients whose config directory exists.
    let setup = lab.run(&lab.base, &["init", "--global"]);
    assert!(setup.contains("Updated ") && !codex.exists(), "{setup}");
    let settings: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
    assert_eq!(settings["model"], "opus");
    let command = &settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"];
    assert_eq!(command, "justless hook-claude before-command --global");
    assert!(
        lab.run(&lab.base, &["init", "--global"])
            .contains("already has the global")
    );
    lab.run(&lab.base, &["init", "--global", "--agent", "codex"]);
    assert!(codex.is_file());
    let refused = lab.output(
        &lab.base,
        &["init", "--global", "--agent", "cursor"],
        None,
        &[],
    );
    assert!(!refused.status.success());

    // Each call finds its project from `cwd`, wherever the hook runs.
    let ask = |agent: &str, mode: &str, input: &Value, env: &[(&str, &str)]| {
        lab.hook(&lab.base, &[agent, mode, "--global"], input, env)
    };
    let before = |agent: &str, cwd: &Path| {
        let input = bash(cwd, "cargo test", Some("bypassPermissions"));
        routed(&ask(agent, "before-command", &input, &[]))
    };
    let wrapped = Some("justless run --hook -- cargo test".to_owned());
    assert_eq!(before("hook-claude", &plain), wrapped);
    assert_eq!(before("hook-codex", &plain.join("src")), wrapped);
    assert_eq!(before("hook-claude", &opted), None);
    // The project's own Claude Code hooks take precedence; it has none for Codex.
    assert_eq!(before("hook-claude", &own), None);
    assert_eq!(before("hook-codex", &own), wrapped);
    assert_eq!(before("hook-claude", &lab.home), None);
    fs::create_dir_all(lab.base.join("loose")).unwrap();
    assert_eq!(before("hook-claude", &lab.base.join("loose")), None);
    // Claude Code loads project hooks from the directory it started in.
    let input = bash(&plain, "cargo test", Some("bypassPermissions"));
    let started = [("CLAUDE_PROJECT_DIR", own.to_str().unwrap())];
    assert_eq!(
        ask("hook-claude", "before-command", &input, &started).trim(),
        "{}"
    );
    let off = [("JUSTLESS_DISABLE", "1")];
    assert_eq!(
        ask("hook-claude", "before-command", &input, &off).trim(),
        "{}"
    );
    let compact = json!({"hook_event_name": "PreCompact", "cwd": lab.home});
    assert_eq!(ask("hook-claude", "before-compact", &compact, &[]), "");

    let doctor = |cwd: &Path| {
        let stdout = lab.output(cwd, &["doctor"], None, &[]).stdout;
        String::from_utf8(stdout).unwrap()
    };
    let report = doctor(&plain);
    assert!(
        report.contains("Global hooks for Claude Code: 8 handlers in "),
        "{report}"
    );
    assert!(
        report.contains("The global Codex hooks serve this project"),
        "{report}"
    );
    assert!(doctor(&own).contains("own Claude Code hooks take precedence"));
    assert!(doctor(&opted).contains("sets global_hooks = false"));
    // The global hooks count as each client's hooks where they serve the project.
    // They write no instructions, and plain `init` adds those without hooks.
    assert!(
        report.contains("✓ Hooks: the global Claude Code hooks in ")
            && report.contains("✗ Instructions: CLAUDE.md does not exist")
            && report.contains("\n  justless init --agent claude --agent codex\n")
            && !report.contains("not set up in this project")
            && !report.contains("Hooks: none"),
        "{report}"
    );
    assert!(doctor(&opted).contains("not set up in this project"));
    // A folder outside any project gets the outside report, as the hooks skip it.
    let loose = doctor(&lab.base.join("loose"));
    assert!(
        loose.contains("has no Justless setup") && !loose.contains("serve this project"),
        "{loose}"
    );
    lab.run(&plain, &["init", "--agent", "claude", "--agent", "codex"]);
    let report = doctor(&plain);
    assert!(
        report.contains("Instructions: CLAUDE.md has the current Justless block")
            && report.contains("✓ Hooks: the global Codex hooks in ")
            && !report.contains("Instructions: AGENTS.md has no")
            && !plain.join(".claude").exists(),
        "{report}"
    );
    let json = lab.output(&plain, &["doctor", "--json"], None, &[]).stdout;
    let json: Value = serde_json::from_slice(&json).unwrap();
    let agents = json["agents"].as_array().unwrap();
    assert_eq!(agents[0]["hooks"], true, "{json}");
    assert_eq!(agents[0]["global_hook_file"], json!(claude));

    // A file nobody edited since is restored byte for byte, and the directory
    // setup created for Codex goes with its file.
    let dry = lab.run(&lab.base, &["init", "--remove", "--global", "--dry-run"]);
    assert!(dry.contains("settings.json: remove 8 hooks"), "{dry}");
    assert!(codex.is_file());
    lab.run(&lab.base, &["init", "--remove", "--global"]);
    assert_eq!(fs::read_to_string(&claude).unwrap(), original);
    assert!(!codex.parent().unwrap().exists());

    // In an edited file only the handlers with --global go. Another Justless
    // handler there runs in every project too, which doctor reports.
    lab.run(&lab.base, &["init", "--global", "--agent", "claude"]);
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
    let mine = json!({"matcher": "^Bash$", "hooks": [
        {"type": "command", "command": "justless hook-claude before-command"},
        {"type": "command", "command": "my-audit"},
    ]});
    settings["hooks"]["PreToolUse"]
        .as_array_mut()
        .unwrap()
        .push(mine);
    fs::write(&claude, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
    assert!(
        doctor(&plain).contains("runs 1 Justless hook without --global as well as the global ones")
    );
    lab.run(&lab.base, &["init", "--remove", "--global"]);
    let left: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
    assert_eq!(left["model"], "opus");
    let hooks = left["hooks"].as_object().unwrap();
    assert_eq!(hooks.keys().collect::<Vec<_>>(), ["PreToolUse"], "{left}");
    assert_eq!(hooks["PreToolUse"].as_array().unwrap().len(), 1, "{left}");
    assert!(!left.to_string().contains("--global"));
}
