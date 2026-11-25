//! Machine-wide setup: hook kill switches and `justless rewrite`, the project
//! registry, and user-level hooks. Every command runs with `HOME`, the state home
//! and the Claude Code and Codex config homes inside the test's own folder.
//! Command hooks pass every command through on Windows, so these run on Unix.
#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, git, has, lacks, piped, starts, text, words};

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

    fn output(
        &self,
        cwd: &Path,
        args: &[&str],
        input: Option<&Value>,
        env: &[(&str, &str)],
    ) -> Output {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env_remove("JUSTLESS_TEST_AGY_CONFIG_DIR")
            .env_remove("JUSTLESS_TEST_AGY_HOME")
            .env(
                "JUSTLESS_TEST_CLAUDE_MANAGED_DIR",
                self.base.join("managed"),
            )
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("JUSTLESS_DISABLE")
            .envs(env.iter().copied());
        let input = input.map_or(String::new(), Value::to_string);
        piped(&mut command, input.as_bytes())
    }

    fn hook(&self, cwd: &Path, args: &[&str], input: &Value, env: &[(&str, &str)]) -> String {
        let output = self.output(cwd, args, Some(input), env);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.output(cwd, args, None, &[]);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
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
    let command = answer["hookSpecificOutput"]["updatedInput"]["command"].as_str();
    command.map(str::to_owned)
}

#[test]
fn kill_switches_leave_commands_alone_and_rewrite_explains_the_hook() {
    let lab = Lab::new();
    let settings = "exclude_commands = [\"cargo bench\", \"^git (log|show)\"]\n";
    let root = lab.project("work", settings);
    let codex = |command: &str| {
        let input = bash(&root, command, None);
        lab.hook(&root, &["hook-codex", "before-command"], &input, &[])
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
    let input = bash(
        &root,
        "cargo bench && cargo test",
        Some("bypassPermissions"),
    );
    let claude = lab.hook(&root, &["hook-claude", "before-command"], &input, &[]);
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
    for (prefix, redirect) in [
        ("", " 2>&1"),
        ("cd src && ", ""),
        ("RUST_LOG=1 ", " 2>&1"),
        ("timeout 60 ", ""),
    ] {
        let test = format!("{prefix}cargo test{redirect} | tail -n 400");
        let bench = format!("{prefix}cargo bench{redirect} | tail -n 400");
        assert_ne!(after(&test).trim(), "{}", "{test}");
        assert_eq!(after(&bench).trim(), "{}", "{bench}");
    }

    // `JUSTLESS_DISABLE` answers every hook with its client's no-op.
    let off = [("JUSTLESS_DISABLE", "1")];
    let input = bash(&root, "cargo test", Some("bypassPermissions"));
    let ask =
        |args: &str, input: &Value, env: &[(&str, &str)]| lab.hook(&root, &words(args), input, env);
    assert_eq!(ask("hook-claude before-command", &input, &off).trim(), "{}");
    let compact = json!({"hook_event_name": "PreCompact"});
    assert_eq!(ask("hook-claude before-compact", &compact, &off), "");
    assert_eq!(
        ask("hook-agy before-command", &json!({}), &off).trim(),
        r#"{"decision":"allow"}"#
    );
    assert_eq!(
        ask("hook-cursor before-command", &json!({}), &off).trim(),
        "{}"
    );
    let on = [("JUSTLESS_DISABLE", "0")];
    has(
        &ask("hook-claude before-command", &input, &on),
        "justless run",
    );

    // `rewrite` prints the hook's own rewrite and records nothing.
    fs::create_dir_all(root.join(".justless")).unwrap();
    let rewrite = |args: &[&str]| lab.run(&root, &[&["rewrite"], args].concat());
    let report = rewrite(&["--agent", "codex", "cargo test"]);
    starts(
        &report,
        "Codex hook: rewritten\n  justless run --hook -- cargo test\n",
    );
    let report = rewrite(&["--agent", "codex", "cargo bench && cargo test"]);
    has(
        &report,
        "only when every command in it is supported and none is excluded",
    );
    has(&report, "Excluded: `cargo bench` matches `cargo bench`");
    let report = rewrite(&["--agent", "agy", "cargo test\ncargo bench"]);
    has(
        &report,
        "Antigravity CLI hook: rewritten\n  justless run --hook -- cargo test\ncargo bench\n",
    );
    has(&report, "Excluded: `cargo bench` matches `cargo bench`");
    has(&report, "asks Antigravity CLI to confirm the rewrite");
    let report = rewrite(&["git show HEAD"]);
    has(&report, "matches `^git (log|show)` in exclude_commands");
    let report = rewrite(&["ls", "--agent", "codex"]);
    has(&report, "no command in it is one the hook routes");
    // Claude Code's verdict: no rule allows `cargo test` in default mode.
    let report = rewrite(&["cargo test"]);
    has(&report, "unchanged\nWhy: Claude Code's permission rules");
    has(&report, "Permission: no rule allows it in default mode");
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::write(
        root.join(".claude/settings.json"),
        r#"{"permissions":{"allow":["Bash(cargo test:*)"]}}"#,
    )
    .unwrap();
    let report = rewrite(&["cargo test", "--mode", "acceptEdits"]);
    has(&report, "rewritten\n  justless run --hook -- cargo test\n");
    has(
        &report,
        "allow rules cover every command in it in acceptEdits mode, so the hook allows the rewrite",
    );
    assert!(!root.join(".justless/activity.json").exists());

    // An invalid expression is a settings error, not a silent pass.
    fs::write(root.join(".justless.toml"), "exclude_commands = [\"^(\"]\n").unwrap();
    let output = lab.output(&root, &["run", "--", "true"], None, &[]);
    has(&text(&output.stderr), "invalid exclude_commands regex");
}

#[test]
fn discover_reports_excluded_commands_in_their_own_group() {
    let lab = Lab::new();
    let settings = "exclude_commands = [\"cargo build\", \"^git log\"]\n";
    let root = lab.project("work", settings);
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
    let mut claude = Vec::new();
    let mut codex = vec![json!({"type": "session_meta",
        "timestamp": "2099-01-01T00:00:00.000Z",
        "payload": {"id": "t", "session_id": "t", "cwd": root, "source": "cli"}})];
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
    let rollout = rollouts.join("rollout-2026-11-19T10-00-00-t.jsonl");
    fs::write(rollout, lines(codex)).unwrap();

    let report = lab.run(&root, &words("discover --json --plan"));
    let report: Value = serde_json::from_str(&report).unwrap();
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
    let (claude, codex) = (&report["groups"], &report["codex"]["groups"]);
    for (groups, route, expected) in [
        (claude, "excluded", &["cargo build ×4", "git log ×1"][..]),
        (
            claude,
            "supported_not_routed",
            &["cargo build ×1", "cargo test ×1"],
        ),
        (claude, "passed_through", &["cargo test ×1"]),
        (codex, "excluded", &["cargo build ×5", "git log ×1"]),
        (codex, "passed_through", &[]),
        (codex, "supported_not_routed", &["cargo test ×2"]),
    ] {
        assert_eq!(labels(groups, route), expected, "{route}");
    }
    let plan = report["plan"].as_array().unwrap();
    assert!(plan.iter().all(|row| row["label"] != "git log"), "{plan:?}");
    let text = lab.run(&root, &["discover"]);
    has(
        &text,
        "Supported, excluded by exclude_commands: 5 command(s)",
    );
    lacks(&text, "xxxx");
}

/// The run ID a `justless run` names on stderr.
fn run_id(lab: &Lab, cwd: &Path, script: &str) -> String {
    let output = lab.output(cwd, &["run", "--", "sh", "-c", script], None, &[]);
    assert!(output.status.success());
    crate::common::run_id(&output)
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
    let main_src: String = (0..30)
        .map(|i| format!("pub fn main_fn_{i}() -> usize {{ {i} }}\n"))
        .collect();
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/lib.rs"), &main_src).unwrap();
    let scratch_src: String = (0..30)
        .map(|i| format!("pub fn scratch_fn_{i}() -> usize {{ {i} }}\n"))
        .collect();
    fs::create_dir_all(scratch.join("src")).unwrap();
    fs::write(scratch.join("src/lib.rs"), &scratch_src).unwrap();
    lab.run(&main, &["read", "src/lib.rs", "--outline"]);
    lab.run(&main, &["expand", "src/lib.rs", "main_fn_0"]);
    lab.run(&scratch, &["read", "src/lib.rs", "--outline"]);
    run_id(&lab, &main, "seq 1 3000");
    run_id(&lab, &tree, "seq 1 3000");
    let elsewhere = run_id(&lab, &scratch, "seq 1 3000; echo needle");

    let file = lab.home.join(".local/state/justless/projects.json");
    let text = fs::read_to_string(&file).unwrap();
    lacks(&text, "seq");
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
    let stats = |line: &str| -> Value {
        let output = lab.output(&main, &words(line), None, &temp);
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let all = stats("stats --all --json");
    let counts = |all: &Value| (all["projects"].clone(), all["runs"].clone());
    assert_eq!(counts(&all), (json!(2), json!(2)), "{all}");
    assert_eq!(all["temporary_projects_left_out"], 1);
    assert_eq!(all["repositories"][0]["repository"], json!(main));
    assert_eq!(all["repositories"][0]["roots"].as_array().unwrap().len(), 2);
    assert_eq!(all["source_outlines"]["views"], 1, "{all}");
    assert_eq!(
        all["source_outlines"]["whole_bytes"],
        main_src.len() as u64,
        "{all}"
    );
    assert!(
        all["source_outlines"]["expanded_bytes"].as_u64().unwrap() > 0,
        "{all}"
    );
    let all = stats("stats --all --include-temp --json");
    assert_eq!(counts(&all), (json!(3), json!(3)), "{all}");
    assert_eq!(all["repositories"].as_array().unwrap().len(), 2);
    assert_eq!(all["source_outlines"]["views"], 2, "{all}");
    assert_eq!(
        all["source_outlines"]["whole_bytes"],
        (main_src.len() + scratch_src.len()) as u64,
        "{all}"
    );
    let output = lab.output(&main, &["stats", "--all"], None, &temp);
    let text = String::from_utf8_lossy(&output.stdout);
    has(&text, "main (2 checkouts)  2 run(s)");
    starts(&text, "In the last 7 day(s), kept ");
    has(&text, "Source outlines: 1 view(s) printed ");
    has(&text, "Left out 1 project(s) in temporary folders");
    // `--days` counts the runs that finished in the window.
    let old = run_id(&lab, &main, "seq 1 3000");
    let record = main.join(".justless/runs").join(&old).join("record.json");
    let mut run: Value = serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    run["completed_ms"] = json!(1_000);
    fs::write(&record, run.to_string()).unwrap();
    let all = stats("stats --all --days 30 --json");
    assert_eq!(
        (&all["days"], &all["runs"]),
        (&json!(30), &json!(2)),
        "{all}"
    );

    // A run made in another project is found through the registry.
    let replay = lab.run(&main, &["log", &elsewhere, "--from", "2", "--to", "3"]);
    assert_eq!(replay, "2\n3\n");
    has(&lab.run(&main, &["search", &elsewhere, "needle"]), "needle");
    let context = lab.run(&tree, &["context", "--run", &elsewhere]);
    starts(&context, &format!("Run {elsewhere}"));
    let unknown = lab.output(&main, &["log", "run-unknown1"], None, &[]);
    has(
        &String::from_utf8_lossy(&unknown.stderr),
        "unknown run ID: run-unknown1",
    );

    // Doctor fails here, in a project with no setup, and still lists the registry.
    let doctor = lab.output(&main, &["doctor"], None, &[]).stdout;
    has(
        &String::from_utf8_lossy(&doctor),
        "Project registry: 3 projects at ",
    );

    // `--purge` drops the project, and the registry is rebuilt as projects are used.
    lab.run(&scratch, &words("init --remove --purge"));
    lacks(&fs::read_to_string(&file).unwrap(), "scratch");
    fs::remove_file(&file).unwrap();
    run_id(&lab, &main, "true");
    has(&fs::read_to_string(&file).unwrap(), "main");
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
    lab.run(&own, &words("init --hooks --agent claude"));

    // Without --agent, setup takes the clients whose config directory exists.
    let setup = lab.run(&lab.base, &["init", "--global"]);
    assert!(setup.contains("Updated ") && !codex.exists(), "{setup}");
    let settings: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
    assert_eq!(settings["model"], "opus");
    let command = &settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"];
    assert_eq!(command, "justless hook-claude before-command --global");
    let again = lab.run(&lab.base, &["init", "--global"]);
    has(&again, "already has the global");
    lab.run(&lab.base, &words("init --global --agent codex"));
    assert!(codex.is_file());
    let refused = words("init --global --agent cursor");
    assert!(!lab.output(&lab.base, &refused, None, &[]).status.success());

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
    let answer = ask("hook-claude", "before-command", &input, &started);
    assert_eq!(answer.trim(), "{}");
    let off = [("JUSTLESS_DISABLE", "1")];
    let answer = ask("hook-claude", "before-command", &input, &off);
    assert_eq!(answer.trim(), "{}");
    let compact = json!({"hook_event_name": "PreCompact", "cwd": lab.home});
    assert_eq!(ask("hook-claude", "before-compact", &compact, &[]), "");

    let doctor = |cwd: &Path| {
        let stdout = lab.output(cwd, &["doctor"], None, &[]).stdout;
        String::from_utf8(stdout).unwrap()
    };
    let report = doctor(&plain);
    has(&report, "Global hooks for Claude Code: 8 handlers in ");
    has(&report, "The global Codex hooks serve this project");
    has(&doctor(&own), "own Claude Code hooks take precedence");
    has(&doctor(&opted), "sets global_hooks = false");
    // The global hooks count as each client's hooks where they serve the project.
    // They write no instructions, and plain `init` adds those without hooks.
    has(&report, "✓ Hooks: the global Claude Code hooks in ");
    has(&report, "✗ Instructions: CLAUDE.md does not exist");
    has(&report, "\n  justless init --agent claude --agent codex\n");
    lacks(&report, "not set up in this project");
    lacks(&report, "Hooks: none");
    has(&doctor(&opted), "not set up in this project");
    // A folder outside any project gets the outside report, as the hooks skip it.
    let loose = doctor(&lab.base.join("loose"));
    has(&loose, "has no Justless setup");
    lacks(&loose, "serve this project");
    lab.run(&plain, &words("init --agent claude --agent codex"));
    let report = doctor(&plain);
    has(
        &report,
        "Instructions: CLAUDE.md has the current Justless block",
    );
    has(&report, "✓ Hooks: the global Codex hooks in ");
    lacks(&report, "Instructions: AGENTS.md has no");
    assert!(!plain.join(".claude").exists());
    let json = lab.output(&plain, &["doctor", "--json"], None, &[]).stdout;
    let json: Value = serde_json::from_slice(&json).unwrap();
    let agents = json["agents"].as_array().unwrap();
    assert_eq!(agents[0]["hooks"], true, "{json}");
    assert_eq!(agents[0]["global_hook_file"], json!(claude));

    // A file nobody edited since is restored byte for byte, and the directory
    // setup created for Codex goes with its file.
    let dry = lab.run(&lab.base, &words("init --remove --global --dry-run"));
    has(&dry, "settings.json: remove 8 hooks");
    assert!(codex.is_file());
    lab.run(&lab.base, &words("init --remove --global"));
    assert_eq!(fs::read_to_string(&claude).unwrap(), original);
    assert!(!codex.parent().unwrap().exists());

    // In an edited file only the handlers with --global go. Another Justless
    // handler there runs in every project too, which doctor reports.
    lab.run(&lab.base, &words("init --global --agent claude"));
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
    let mine = json!({"matcher": "^Bash$", "hooks": [
        {"type": "command", "command": "justless hook-claude before-command"},
        {"type": "command", "command": "my-audit"},
    ]});
    let hooks = settings["hooks"]["PreToolUse"].as_array_mut().unwrap();
    hooks.push(mine);
    fs::write(&claude, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
    has(
        &doctor(&plain),
        "runs 1 Justless hook without --global as well as the global ones",
    );
    lab.run(&lab.base, &words("init --remove --global"));
    let left: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
    assert_eq!(left["model"], "opus");
    let hooks = left["hooks"].as_object().unwrap();
    assert_eq!(hooks.keys().collect::<Vec<_>>(), ["PreToolUse"], "{left}");
    assert_eq!(hooks["PreToolUse"].as_array().unwrap().len(), 1, "{left}");
    lacks(&left.to_string(), "--global");
}

#[test]
fn global_agy_hooks_serve_each_project_once_and_remove_cleanly() {
    let lab = Lab::new();
    let agy = lab.home.join(".gemini/config/hooks.json");
    fs::create_dir_all(agy.parent().unwrap()).unwrap();
    let original = "{\n  \"other\": {\"enabled\": false}\n}\n";
    fs::write(&agy, original).unwrap();
    let plain = lab.project("plain", "");
    let opted = lab.project("opted", "global_hooks = false\n");
    let own = lab.project("own", "");
    lab.run(&own, &words("init --hooks --agent agy"));

    let setup = lab.run(&lab.base, &words("init --global --agent agy"));
    has(&setup, "Updated ");
    let hooks: Value = serde_json::from_str(&fs::read_to_string(&agy).unwrap()).unwrap();
    assert_eq!(hooks["other"]["enabled"], false);
    assert_eq!(
        hooks["justless"]["PreToolUse"][0]["hooks"][0]["command"],
        "justless hook-agy before-command --global"
    );
    assert_eq!(
        hooks["justless"]["PreToolUse"][1]["matcher"],
        "^(read_file|view_file)$"
    );
    assert_eq!(
        hooks["justless"]["PreToolUse"][1]["hooks"][0]["command"],
        "justless hook-agy before-read --global"
    );
    assert_eq!(
        hooks["justless"]["PreToolUse"][2]["matcher"],
        "^(edit_file|write_file|write_to_file|replace_file_content|multi_replace_file_content)$"
    );
    assert_eq!(
        hooks["justless"]["PreToolUse"][2]["hooks"][0]["command"],
        "justless hook-agy before-edit --global"
    );
    assert_eq!(
        hooks["justless"]["PostToolUse"][0]["matcher"],
        "^(read_file|view_file)$"
    );
    assert_eq!(
        hooks["justless"]["PostToolUse"][0]["hooks"][0]["command"],
        "justless hook-agy after-read --global"
    );
    assert_eq!(
        hooks["justless"]["PostToolUse"][1]["matcher"],
        "^(edit_file|write_file|write_to_file|replace_file_content|multi_replace_file_content)$"
    );
    assert_eq!(
        hooks["justless"]["PostToolUse"][1]["hooks"][0]["command"],
        "justless hook-agy after-edit --global"
    );
    assert_eq!(
        hooks["justless"]["PostInvocation"][0]["command"],
        "justless hook-agy post-invocation --global"
    );
    assert_eq!(
        hooks["justless"]["SessionStart"][0]["command"],
        "justless hook-agy session-start --global"
    );
    has(
        &lab.run(&lab.base, &words("init --global --agent agy")),
        "already has the global Antigravity CLI hooks",
    );

    let ask = |mode: &str, input: &Value| {
        let out = lab.hook(&lab.base, &["hook-agy", mode, "--global"], input, &[]);
        serde_json::from_str::<Value>(&out).unwrap()
    };
    let before = |root: &Path, cwd: &Path| {
        let input = json!({
            "cwd": cwd,
            "workspacePaths": [root],
            "toolCall": {"name": "run_command", "args": {"CommandLine": "cargo test"}},
        });
        ask("before-command", &input)
    };
    let rewritten = json!({"decision": "allow", "overwrite": {"CommandLine": "justless run --hook -- cargo test"}});
    let allow = json!({"decision": "allow"});
    assert_eq!(before(&plain, &plain), rewritten);
    assert_eq!(before(&plain, &plain.join("src")), rewritten);
    assert_eq!(before(&opted, &opted), allow);
    assert_eq!(before(&own, &own), allow);
    assert_eq!(before(&lab.home, &lab.home), allow);
    let loose = lab.base.join("loose");
    fs::create_dir_all(&loose).unwrap();
    assert_eq!(before(&loose, &loose), allow);
    assert_eq!(before(agy.parent().unwrap(), agy.parent().unwrap()), allow);

    // SessionStart indexes a served project and skips opted-out and local-hook projects.
    let start = |root: &Path| {
        ask(
            "session-start",
            &json!({"cwd": root, "workspacePaths": [root]}),
        )
    };
    assert_eq!(start(&opted), json!({}));
    assert!(!opted.join(".justless").exists());
    assert_eq!(start(&plain), json!({}));
    assert!(plain.join(".justless/index.json").is_file());
    run_id(&lab, &plain, "true");
    assert_eq!(
        start(&plain)["injectSteps"][0]["systemMessage"]["systemMessage"],
        "`justless resume` lists earlier runs and reads."
    );

    // PostToolUse `after-read --global` records reads in served projects and skips opted-out/local-hook ones.
    for root in [&plain, &opted, &own] {
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    }
    let after_read = |root: &Path| {
        ask(
            "after-read",
            &json!({
                "cwd": root,
                "workspacePaths": [root],
                "toolCall": {
                    "name": "read_file",
                    "args": {"Path": root.join("src/lib.rs"), "StartLine": 1, "EndLine": 1}
                }
            }),
        )
    };
    assert_eq!(after_read(&plain), json!({}));
    assert_eq!(after_read(&opted), json!({}));
    assert_eq!(after_read(&own), json!({}));
    has(&lab.run(&plain, &["resume"]), "Read src/lib.rs:1-1");
    assert!(!opted.join(".justless").exists());
    lacks(&lab.run(&own, &["resume"]), "Read src/lib.rs:1-1");

    let doctor =
        |cwd: &Path| String::from_utf8(lab.output(cwd, &["doctor"], None, &[]).stdout).unwrap();
    let report = doctor(&plain);
    has(&report, "Global hooks for Antigravity CLI: 7 handlers in ");
    has(
        &report,
        "The global Antigravity CLI hooks serve this project",
    );
    has(&report, "✓ Hooks: the global Antigravity CLI hooks in ");
    has(&doctor(&own), "own Antigravity CLI hooks take precedence");
    has(&doctor(&opted), "sets global_hooks = false");

    // Global `before-edit` -> `after-edit` -> `post-invocation` bridge serves `plain`
    // and skips opted-out and local-hook projects.
    let target = plain.join("src/lib.rs");
    let edit_event = |root: &Path, file: &Path| {
        json!({
            "conversationId": "conv-global-1",
            "cwd": root,
            "workspacePaths": [root],
            "toolCall": {
                "id": "call-global-1",
                "name": "edit_file",
                "args": {"Path": file}
            }
        })
    };
    assert_eq!(
        ask("before-edit", &edit_event(&plain, &target)),
        json!({"decision": "allow"})
    );
    fs::write(&target, "pub fn answer( -> u32 {\n").unwrap();
    assert_eq!(ask("after-edit", &edit_event(&plain, &target)), json!({}));
    let post = ask(
        "post-invocation",
        &json!({"conversationId": "conv-global-1", "cwd": &plain, "workspacePaths": [&plain]}),
    );
    let injected = post["injectSteps"][0]["systemMessage"]["systemMessage"]
        .as_str()
        .unwrap_or("");
    has(injected, "Justless parse check: after this edit_file");
    assert_eq!(
        ask(
            "post-invocation",
            &json!({"conversationId": "conv-global-1", "cwd": &plain, "workspacePaths": [&plain]})
        ),
        json!({})
    );
    assert_eq!(
        ask(
            "before-edit",
            &edit_event(&opted, &opted.join("src/lib.rs"))
        ),
        json!({"decision": "allow"})
    );
    assert!(!opted.join(".justless").exists());

    // Unedited file restores byte for byte.
    lab.run(&lab.base, &words("init --remove --global --agent agy"));
    assert_eq!(fs::read_to_string(&agy).unwrap(), original);

    // Edited file removes only the --global handlers, keeping plain and custom ones.
    lab.run(&lab.base, &words("init --global --agent agy"));
    let mut edited: Value = serde_json::from_str(&fs::read_to_string(&agy).unwrap()).unwrap();
    edited["justless"]["PreToolUse"]
        .as_array_mut()
        .unwrap()
        .push(json!({"matcher": "^run_command$", "hooks": [
            {"type": "command", "command": "justless hook-agy before-command"},
            {"type": "command", "command": "my-guard"},
        ]}));
    fs::write(&agy, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
    has(
        &doctor(&plain),
        "runs 1 Justless hook without --global as well as the global ones",
    );
    lab.run(&lab.base, &words("init --remove --global --agent agy"));
    let left: Value = serde_json::from_str(&fs::read_to_string(&agy).unwrap()).unwrap();
    assert_eq!(left["other"]["enabled"], false);
    assert_eq!(
        left["justless"]["PreToolUse"].as_array().unwrap().len(),
        1,
        "{left}"
    );
    lacks(&left.to_string(), "--global");
}

#[test]
fn discover_and_stats_read_antigravity_cli_transcripts() {
    let lab = Lab::new();
    let root = lab.project("work", "exclude_commands = [\"cargo build\"]\n");
    let agy = lab.home.join(".gemini/antigravity-cli");
    let session = "11111111-2222-3333-4444-555555555555";
    let logs = agy
        .join("brain")
        .join(session)
        .join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        agy.join("history.jsonl"),
        format!(
            "{}\n",
            json!({"conversationId": session, "workspace": root, "timestamp": 4_000_000_000_000u64})
        ),
    )
    .unwrap();
    let rewritten = concat!(
        "Created At: 2099-01-01T00:00:00Z\n",
        "Completed At: 2099-01-01T00:00:01Z\n",
        "A pre-tool hook changed the arguments of this tool call before it ran. Changed: CommandLine.\n\n",
        "Created At: 2099-01-01T00:00:00Z\n",
        "Completed At: 2099-01-01T00:00:01Z\n",
        "Command exited with code 0.\n\n",
        "Command output:\n",
        "short preview\n",
    );
    let raw_output = |body: &str| {
        format!(
            "Created At: 2099-01-01T00:00:00Z\nCompleted At: 2099-01-01T00:00:01Z\nCommand exited with code 0.\n\nCommand output:\n{body}"
        )
    };
    let secret_body = "SECRET_PAYLOAD_NEVER_PRINTED\n".repeat(40);
    let bg_started = concat!(
        "Created At: 2099-01-01T00:00:02Z\n",
        "Tool is running as a background task with task id: conv/task-7\n",
        "Task logs are available at: file:///tmp/task-7.log\n",
    );
    let bg_done = format!(
        "<SYSTEM_MESSAGE>\n\
         [Message] timestamp=2099-01-01T00:00:05Z sender=conv/task-7 content=Task id \"conv/task-7\" finished with result:\n\
         A pre-tool hook changed the arguments of this tool call before it ran. Changed: CommandLine.\n\n\
         Command exited with code 0.\n\n\
         Command output:\n\
         {secret_body}\
         Log: file:///tmp/task-7.log\n\
         </SYSTEM_MESSAGE>\n"
    );
    let transcript = [
        json!({
            "type": "PLANNER_RESPONSE",
            "created_at": "2099-01-01T00:00:00Z",
            "tool_calls": [
                {"name": "read_file", "args": {"Path": "/tmp/a"}},
                {"name": "run_command", "args": {"CommandLine": "git diff"}},
                {"name": "run_command", "args": {"CommandLine": "cargo test"}},
                {"name": "run_command", "args": {"CommandLine": "cargo build"}},
                {"name": "run_command", "args": {"CommandLine": "python3 custom.py"}},
                {"name": "run_command", "args": {"CommandLine": "cargo clippy --all-targets -- -D warnings"}}
            ]
        }),
        json!({"type": "GENERIC", "created_at": "2099-01-01T00:00:01Z", "content": "file text"}),
        json!({"type": "GENERIC", "created_at": "2099-01-01T00:00:01Z", "content": rewritten}),
        json!({"type": "GENERIC", "created_at": "2099-01-01T00:00:01Z", "content": raw_output(&secret_body)}),
        json!({"type": "GENERIC", "created_at": "2099-01-01T00:00:01Z", "content": raw_output(&secret_body)}),
        json!({"type": "GENERIC", "created_at": "2099-01-01T00:00:01Z", "content": raw_output(&secret_body)}),
        json!({"type": "GENERIC", "created_at": "2099-01-01T00:00:02Z", "content": bg_started}),
        json!({"type": "SYSTEM_MESSAGE", "created_at": "2099-01-01T00:00:05Z", "content": bg_done}),
    ];
    let text: String = transcript.iter().map(|r| format!("{r}\n")).collect();
    fs::write(logs.join("transcript_full.jsonl"), text).unwrap();

    let report: Value =
        serde_json::from_str(&lab.run(&root, &words("discover --json --plan"))).unwrap();
    let agy_json = &report["agy"];
    assert_eq!(agy_json["sessions"], 1, "{agy_json}");
    assert_eq!(agy_json["commands"], 5, "{agy_json}");
    let route_commands = |route: &str| {
        agy_json["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["route"] == route)
            .map_or(0, |g| g["commands"].as_u64().unwrap())
    };
    assert_eq!(route_commands("through_justless"), 2, "{agy_json}");
    assert_eq!(route_commands("supported_not_routed"), 1, "{agy_json}");
    assert_eq!(route_commands("excluded"), 1, "{agy_json}");
    assert_eq!(route_commands("not_supported"), 1, "{agy_json}");

    let plain = lab.run(&root, &words("discover --plan"));
    has(&plain, "Antigravity CLI ran 5 shell command(s) here");
    has(&plain, "Unrouted Antigravity CLI output by command family");
    has(&plain, "cargo test ×1");
    lacks(&plain, "SECRET_PAYLOAD_NEVER_PRINTED");
    lacks(&plain, "custom.py");

    let stats_text = lab.run(&root, &["stats"]);
    has(
        &stats_text,
        "Antigravity CLI shell commands here: 5 returning ",
    );
    has(&stats_text, "2 of them (");
    let stats_json: Value = serde_json::from_str(&lab.run(&root, &words("stats --json"))).unwrap();
    assert_eq!(stats_json["agy"]["commands"], 5, "{stats_json}");
    assert_eq!(
        stats_json["agy"]["through_justless_commands"], 2,
        "{stats_json}"
    );
}
