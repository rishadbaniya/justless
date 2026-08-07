use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::process::{Command, Output};

use tempfile::TempDir;

fn fixture() -> TempDir {
    let target = TempDir::new().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/failing");
    for file in [
        "Cargo.toml",
        ".gitignore",
        "src/lib.rs",
        "src/pricing.rs",
        "src/obsolete.rs",
        "tests/checkout.rs",
    ] {
        let path = target.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(source.join(file), path).unwrap();
    }
    target
}

fn mixed_fixture() -> TempDir {
    let target = TempDir::new().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mixed");
    for file in [
        "frontend/orders.ts",
        "frontend/format.ts",
        "backend/orders.py",
        "db/schema.sql",
        "docs/operations.md",
        "backend/settings.custom",
    ] {
        let path = target.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(source.join(file), path).unwrap();
    }
    target
}

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(args)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap()
}

fn success(root: &Path, args: &[&str]) -> String {
    let output = cli(root, args);
    assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    stdout(&output)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn run_id(output: &Output) -> String {
    stderr(output)
        .split_once("[justless ")
        .unwrap()
        .1
        .split_once(']')
        .unwrap()
        .0
        .to_owned()
}

#[test]
fn failing_cargo_test_leads_to_source_and_exact_log_slice() {
    let root = fixture();
    let path = root.path();
    let output = cli(
        path,
        &[
            "run",
            "--max-bytes",
            "512",
            "--",
            "cargo",
            "test",
            "--offline",
        ],
    );
    assert_eq!(output.status.code(), Some(101), "{:?}", output);
    let preview = stdout(&output);
    assert!(preview.len() <= 512, "{preview}");
    assert!(preview.contains("total_includes_fee"), "{preview}");
    assert!(preview.contains("tests/checkout.rs:11"), "{preview}");
    assert!(preview.contains("assertion"), "{preview}");
    assert!(!preview.contains("only-in-the-full-log"), "{preview}");
    let id = run_id(&output);
    let context = success(path, &["context", "--run", &id]);
    assert!(context.contains("tests/checkout.rs:11"), "{context}");
    assert!(
        context.contains("assert_eq!(total(100, 5), 105);"),
        "{context}"
    );
    assert!(
        context.contains("checkout_fixture::pricing::checkout_total"),
        "{context}"
    );
    let code = success(path, &["context", "--symbol", "checkout_total"]);
    assert!(code.contains("src/pricing.rs:1-3"), "{code}");
    assert!(code.contains("apply_fee(subtotal, fee)"), "{code}");
    let full = success(path, &["log", &id, "--stream", "stdout"]);
    assert!(full.len() > 15_000, "{}", full.len());
    let matches = success(path, &["search", &id, "only-in-the-full-log"]);
    let number = matches.split(':').nth(1).unwrap();
    let part = success(
        path,
        &[
            "log", &id, "--stream", "stdout", "--from", number, "--to", number,
        ],
    );
    assert_eq!(part, "billing-trace probe=only-in-the-full-log\n");
    assert_eq!(
        full.lines().nth(number.parse::<usize>().unwrap() - 1),
        Some(part.trim_end())
    );
    let bounded = success(path, &["context", "--run", &id, "--budget", "240"]);
    assert!(bounded.len() <= 240, "{bounded}");
    assert!(bounded.contains("[context budget reached]"), "{bounded}");
    fs::write(path.join("tests/checkout.rs"), "fn new_source() {}\n").unwrap();
    let historical = success(path, &["context", "--run", &id]);
    assert!(
        historical.contains("Source changed or disappeared"),
        "{historical}"
    );
}

#[test]
fn index_tracks_relationships_and_reuses_unchanged_files() {
    let root = fixture();
    let path = root.path();
    assert!(success(path, &["index"]).contains("4 parsed"));
    assert!(success(path, &["index"]).contains("0 parsed · 4 reused"));
    let tests = success(path, &["query", "apply_fee", "--view", "tests"]);
    assert!(tests.contains("related test (2 call hops)"), "{tests}");
    assert!(tests.contains("total_includes_fee"), "{tests}");
    let callers = success(path, &["query", "checkout_total", "--view", "callers"]);
    assert!(
        callers.contains(
            "call from checkout_fixture_test_checkout::total_includes_fee at tests/checkout.rs:11"
        ),
        "{callers}"
    );
    let references = success(path, &["query", "checkout_total", "--view", "references"]);
    assert!(
        references.contains("import from file scope at tests/checkout.rs:1"),
        "{references}"
    );
    fs::write(
        path.join("src/pricing.rs"),
        "pub fn replacement_total() -> u32 { 105 }\n",
    )
    .unwrap();
    fs::remove_file(path.join("src/obsolete.rs")).unwrap();
    let changed = success(path, &["index"]);
    assert!(
        changed.contains("1 parsed · 2 reused · 1 removed"),
        "{changed}"
    );
    assert!(
        success(
            path,
            &["query", "replacement_total", "--view", "definition"]
        )
        .contains("src/pricing.rs:1")
    );
    for stale in ["checkout_total", "apply_fee", "obsolete_helper"] {
        assert!(
            stderr(&cli(path, &["query", stale])).contains("symbol not found"),
            "{stale}"
        );
    }
}

#[test]
fn git_changes_and_codex_setup_work_in_a_repository() {
    let root = fixture();
    let path = root.path();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?} {}", stderr(&output));
    };
    git(&["init", "-q"]);
    fs::write(path.join("AGENTS.md"), "# Existing instructions\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Justless Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
    success(path, &["init-codex"]);
    success(path, &["init-codex"]);
    let agents = fs::read_to_string(path.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("# Existing instructions\n"));
    assert_eq!(agents.matches("<!-- justless:begin -->").count(), 1);
    git(&["check-ignore", ".justless/index.json"]);
    let pricing = path.join("src/pricing.rs");
    let previous = fs::read_to_string(&pricing).unwrap();
    fs::write(&pricing, previous.replace("fee + 1", "fee + 2")).unwrap();
    let changes = success(path, &["context", "--changes"]);
    assert!(changes.contains("Changed src/pricing.rs:6-6"), "{changes}");
    assert!(changes.contains("apply_fee [function]"), "{changes}");
    assert!(changes.contains("total_includes_fee"), "{changes}");
    git(&["add", "src/pricing.rs"]);
    assert!(success(path, &["context", "--changes", "--staged"]).contains("apply_fee"));
    fs::write(&pricing, &previous).unwrap();
    let staged = success(path, &["context", "--changes", "--staged"]);
    assert!(
        staged.contains("working tree differs from staged source"),
        "{staged}"
    );
    let output = cli(path, &["run", "--", "git", "diff", "--", "src/pricing.rs"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let id = run_id(&output);
    assert!(success(path, &["log", &id, "--stream", "stdout"]).contains("fee + 1"));
}

#[test]
fn log_replay_preserves_binary_streams_and_command_status() {
    let root = fixture();
    let path = root.path();
    let command = [
        "sh",
        "-c",
        "printf '\\001\\000hello\\377'; printf 'oops\\n' >&2; exit 23",
    ];
    let output = cli(path, &["run", "--", command[0], command[1], command[2]]);
    assert_eq!(output.status.code(), Some(23));
    let id = run_id(&output);
    let log = cli(path, &["log", &id]);
    assert_eq!(log.stdout, b"\x01\x00hello\xff");
    assert_eq!(log.stderr, b"oops\n");
    let raw = cli(
        path,
        &["run", "--raw", "--", command[0], command[1], command[2]],
    );
    assert_eq!(raw.status.code(), Some(23));
    assert_eq!(raw.stdout, log.stdout);
    assert_eq!(raw.stderr, log.stderr);
    assert!(!stderr(&cli(path, &["log", "../not-a-run"])).is_empty());
    assert!(!stderr(&cli(path, &["code", "../Cargo.toml", "--to", "1"])).is_empty());
    let state = PathBuf::from(path).join(".justless/runs");
    assert_eq!(fs::read_dir(state).unwrap().count(), 1);
}

#[test]
fn compiler_json_errors_link_to_source_and_the_original_message() {
    let root = fixture();
    let path = root.path();
    fs::write(
        path.join("src/pricing.rs"),
        "pub fn checkout_total(subtotal: u32, fee: u32) -> u32 {\n    subtotal + fee + \"wrong\"\n}\n",
    ).unwrap();
    let output = cli(
        path,
        &[
            "run",
            "--max-bytes",
            "512",
            "--",
            "cargo",
            "check",
            "--offline",
            "--message-format=json",
        ],
    );
    assert_eq!(output.status.code(), Some(101), "{}", stderr(&output));
    let preview = stdout(&output);
    assert!(preview.contains("error[E0277]"), "{preview}");
    assert!(preview.contains("src/pricing.rs:2"), "{preview}");
    let id = run_id(&output);
    let source = success(path, &["context", "--run", &id]);
    assert!(source.contains("src/pricing.rs:2"), "{source}");
    assert!(source.contains("subtotal + fee + \"wrong\""), "{source}");
    let full = success(path, &["log", &id, "--stream", "stdout"]);
    assert!(full.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .is_ok_and(|json| json["reason"] == "compiler-message")
    }));
}

#[test]
fn ambiguous_names_and_invalid_edits_cannot_return_stale_definitions() {
    let root = fixture();
    let path = root.path();
    let source = path.join("src/obsolete.rs");
    fs::write(&source, "pub fn apply_fee() {}\n").unwrap();
    let ambiguous = cli(path, &["query", "apply_fee"]);
    assert_ne!(ambiguous.status.code(), Some(0));
    let message = stderr(&ambiguous);
    assert!(message.contains("ambiguous symbol"), "{message}");
    assert!(message.contains("src/obsolete.rs:1"), "{message}");
    assert!(message.contains("src/pricing.rs:5-7"), "{message}");
    assert!(success(path, &["query", "pricing::apply_fee"]).contains("src/pricing.rs:5-7"));
    fs::write(&source, "pub fn apply_fee(\n").unwrap();
    let errors = cli(path, &["index"]);
    assert!(
        stderr(&errors).contains("src/obsolete.rs: line"),
        "{}",
        stderr(&errors)
    );
    assert!(success(path, &["query", "apply_fee"]).contains("src/pricing.rs:5-7"));
}

#[test]
fn mixed_repo_paths_and_exact_text_refresh_after_edits() {
    let repo = mixed_fixture();
    let root = repo.path();
    let index = cli(root, &["index"]);
    assert!(index.status.success(), "{}", stderr(&index));
    assert!(index.stderr.is_empty(), "{}", stderr(&index));
    assert!(
        stdout(&index).contains("6 text files"),
        "{}",
        stdout(&index)
    );
    let route = success(root, &["path", "fetchOrders", "purchase_orders"]);
    assert!(
        route.contains("HTTP route at frontend/orders.ts:4"),
        "{route}"
    );
    assert!(
        route.contains("list_orders [function] at backend/orders.py:7-8"),
        "{route}"
    );
    assert!(
        route.contains("SQL relation at backend/orders.py:8"),
        "{route}"
    );
    assert!(
        route.contains("purchase_orders [table] at db/schema.sql:1-3"),
        "{route}"
    );
    let imports = success(
        root,
        &["query", "frontend/orders.ts", "--view", "dependencies"],
    );
    assert!(
        imports.contains("formatOrder [function] at frontend/format.ts:1-3"),
        "{imports}"
    );
    let hits = success(root, &["find", "-i", "CANARY-CHECKOUT"]);
    assert!(hits.contains("docs/operations.md:3"), "{hits}");
    assert!(hits.contains("backend/settings.custom:1"), "{hits}");
    fs::write(root.join(".justlessignore"), "*.custom\n").unwrap();
    let hits = success(root, &["find", "canary-checkout"]);
    assert!(!hits.contains("settings.custom"), "{hits}");
    fs::write(
        root.join("frontend/orders.ts"),
        "export function replacementOrder() {}\n",
    )
    .unwrap();
    fs::remove_file(root.join("db/schema.sql")).unwrap();
    for symbol in ["fetchOrders", "purchase_orders"] {
        assert!(
            stderr(&cli(root, &["query", symbol])).contains("symbol not found"),
            "{symbol}"
        );
    }
    assert!(
        success(root, &["query", "replacementOrder", "--view", "definition"])
            .contains("frontend/orders.ts:1")
    );
}

#[test]
fn structural_parsers_cover_common_non_rust_languages() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    for (file, source, name) in [
        (
            "program.js",
            "export function jsThing() { return 1; }\n",
            "jsThing",
        ),
        (
            "view.tsx",
            "export function TsView() { return <div />; }\n",
            "TsView",
        ),
        (
            "main.go",
            "package main\nfunc GoThing() int { return 1 }\n",
            "GoThing",
        ),
        (
            "Demo.java",
            "class JavaThing { int javaMethod() { return 1; } }\n",
            "JavaThing",
        ),
        ("main.c", "int c_thing(void) { return 1; }\n", "c_thing"),
        ("main.cpp", "int cpp_thing() { return 1; }\n", "cpp_thing"),
        (
            "Demo.cs",
            "class CSharpThing { int Run() { return 1; } }\n",
            "CSharpThing",
        ),
        (
            "demo.rb",
            "class RubyThing\n  def run\n    1\n  end\nend\n",
            "RubyThing",
        ),
        (
            "demo.php",
            "<?php function php_thing() { return 1; }\n",
            "php_thing",
        ),
        (
            "demo.sh",
            "bash_thing() { printf 'hello\\n'; }\n",
            "bash_thing",
        ),
    ] {
        fs::write(root.join(file), source).unwrap();
        let output = success(root, &["query", name, "--view", "definition"]);
        assert!(output.contains(&format!("{file}:")), "{output}");
    }
}

fn hook(root: &Path, mode: &str, input: serde_json::Value) -> serde_json::Value {
    agent_hook(root, "hook-codex", mode, input)
}

fn agent_hook(
    root: &Path,
    handler: &str,
    mode: &str,
    input: serde_json::Value,
) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args([handler, mode])
        .current_dir(root)
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
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn codex_hooks_merge_and_use_the_documented_tool_contract() {
    let repo = mixed_fixture();
    let root = repo.path();
    fs::create_dir(root.join(".codex")).unwrap();
    let config = root.join(".codex/hooks.json");
    fs::write(&config, r#"{"description":"preserve","hooks":{"PreToolUse":[{"matcher":"^Other$","hooks":[{"type":"command","command":"echo independent"}]}]}}"#).unwrap();
    success(root, &["init", "--hooks"]);
    let installed = fs::read_to_string(&config).unwrap();
    success(root, &["init-codex", "--hooks"]);
    assert_eq!(installed, fs::read_to_string(&config).unwrap());
    let parsed: serde_json::Value = serde_json::from_str(&installed).unwrap();
    assert_eq!(parsed["description"], "preserve");
    assert_eq!(parsed["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    assert_eq!(
        parsed["hooks"]["SessionStart"][0]["hooks"][0]["async"],
        true
    );
    assert_eq!(
        parsed["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
        "justless hook-codex before-command"
    );

    let start = hook(
        root,
        "session-start",
        serde_json::json!({"hook_event_name":"SessionStart"}),
    );
    assert!(
        start["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Justless indexed")
    );
    assert!(root.join(".justless/index.json").is_file());
    let call = |command: &str| {
        serde_json::json!({
            "hook_event_name":"PreToolUse", "tool_name":"Bash", "tool_cwd": root,
            "tool_input":{"command":command,"workdir":root}
        })
    };
    let rewritten = hook(
        root,
        "before-command",
        call("rg 'fetch|list_orders' frontend"),
    );
    assert_eq!(
        rewritten["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    assert_eq!(
        rewritten["hookSpecificOutput"]["updatedInput"]["command"],
        "justless run -- rg 'fetch|list_orders' frontend"
    );
    assert_eq!(
        rewritten["hookSpecificOutput"]["updatedInput"]["workdir"],
        root.to_str().unwrap()
    );
    assert!(
        rewritten["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("justless query")
    );
    for command in [
        "cargo test && git push",
        "git push origin main",
        "rg $(echo hello)",
        "rg --json fetch frontend",
        "git status --porcelain=v1",
        "rg fetch > matches.txt",
        "npm test -- --watch",
        "justless run -- rg fetch",
        "rg fetch frontend/*.ts",
    ] {
        assert_eq!(
            hook(root, "before-command", call(command)),
            serde_json::json!({}),
            "{command}"
        );
    }
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(git.status.success(), "{}", stderr(&git));
    let rewritten = hook(root, "before-command", call("git status --short"));
    let command = rewritten["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_justless"));
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(binary.parent().unwrap().to_path_buf())
            .chain(std::env::split_paths(&inherited)),
    )
    .unwrap();
    let output = Command::new("sh")
        .args(["-c", command])
        .env("PATH", path)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("frontend/"), "{}", stdout(&output));
    assert!(
        root.join(".justless/runs")
            .join(run_id(&output))
            .join("stdout")
            .is_file()
    );
}

#[test]
fn install_and_run_claude_and_agy_hooks() {
    use serde_json::{Value, json};
    let repo = mixed_fixture();
    let root = repo.path().canonicalize().unwrap();
    fs::create_dir(root.join(".claude")).unwrap();
    let claude = root.join(".claude/settings.json");
    let agy = root.join(".agents/hooks.json");
    fs::create_dir(root.join(".agents")).unwrap();
    fs::write(
        &claude,
        r#"{"permissions":{"defaultMode":"default"},"hooks":{"SessionStart":[]}}"#,
    )
    .unwrap();
    fs::write(
        &agy,
        r#"{"other":{"enabled":false},"justless":{"PreToolUse":"invalid"}}"#,
    )
    .unwrap();
    let setup = ["init", "--hooks", "--agent", "claude", "--agent", "agy"];
    assert!(!cli(&root, &setup).status.success());
    assert!(!root.join("CLAUDE.md").exists());
    fs::write(&agy, r#"{"other":{"enabled":false}}"#).unwrap();
    fs::write(root.join("CLAUDE.md"), "# Team rules\n").unwrap();
    success(&root, &setup);
    let configs = || {
        [
            fs::read_to_string(&claude).unwrap(),
            fs::read_to_string(&agy).unwrap(),
        ]
    };
    let installed = configs();
    success(&root, &setup);
    assert_eq!(installed, configs());
    assert!(!root.join(".codex").exists());
    assert!(root.join("AGENTS.md").is_file());
    let instructions = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(instructions.starts_with("# Team rules\n"));
    assert_eq!(instructions.matches("<!-- justless:begin -->").count(), 1);
    let [claude, agy] = installed.map(|s| serde_json::from_str::<Value>(&s).unwrap());
    assert_eq!(claude["permissions"]["defaultMode"], "default");
    assert_eq!(
        claude["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "justless hook-claude before-command"
    );
    assert_eq!(agy["other"]["enabled"], false);
    assert_eq!(
        agy["justless"]["SessionStart"][0]["command"],
        "justless hook-agy session-start"
    );
    assert_eq!(agy["justless"]["PreToolUse"][0]["matcher"], "^run_command$");
    let claude_input = json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":"git diff","description":"show changes"}});
    let output = agent_hook(&root, "hook-claude", "before-command", claude_input.clone());
    let hook = &output["hookSpecificOutput"];
    assert_eq!(
        hook["updatedInput"],
        json!({"command":"justless run -- git diff","description":"show changes"})
    );
    assert!(hook.get("permissionDecision").is_none());
    let mut plan = claude_input;
    plan["permission_mode"] = json!("plan");
    assert_eq!(
        agent_hook(&root, "hook-claude", "before-command", plan),
        json!({})
    );
    let elsewhere = TempDir::new().unwrap();
    let agy_input = |command: &str, cwd: &Path| json!({"workspacePaths":[root],"toolCall":{"name":"run_command","args":{"CommandLine":command,"Cwd":cwd,"WaitMsBeforeAsync":10}}});
    let dispatch = |command: &str, cwd: &Path| {
        agent_hook(
            elsewhere.path(),
            "hook-agy",
            "before-command",
            agy_input(command, cwd),
        )
    };
    assert_eq!(
        dispatch("git diff", &root),
        json!({"decision":"allow","overwrite":{"CommandLine":"justless run -- git diff"}})
    );
    assert_eq!(
        dispatch("git diff", elsewhere.path()),
        json!({"decision":"allow"})
    );
    assert_eq!(
        dispatch("git diff | cat", &root),
        json!({"decision":"allow"})
    );
    let mut multiple = agy_input("git diff", &root);
    multiple["workspacePaths"] = json!([elsewhere.path(), root]);
    let result = agent_hook(
        elsewhere.path(),
        "hook-agy",
        "before-command",
        multiple.clone(),
    );
    assert_eq!(
        result["overwrite"]["CommandLine"],
        "justless run -- git diff"
    );
    multiple["toolCall"]["args"]["RunPersistent"] = json!(true);
    assert_eq!(
        agent_hook(&root, "hook-agy", "before-command", multiple),
        json!({"decision":"allow"})
    );
    let startup = agent_hook(
        elsewhere.path(),
        "hook-agy",
        "session-start",
        json!({"workspacePaths":[root]}),
    );
    assert!(
        startup["injectSteps"][0]["ephemeralMessage"]
            .as_str()
            .unwrap()
            .contains("Justless indexed")
    );
    assert!(root.join(".justless/index.json").is_file());
    assert!(!elsewhere.path().join(".justless").exists());
}

#[test]
fn non_rust_diagnostics_and_project_budgets_work() {
    let repo = mixed_fixture();
    let root = repo.path();
    fs::write(
        root.join(".justless.toml"),
        "context_budget = 320\nmax_output_bytes = 512\nmax_file_bytes = 1000\n",
    )
    .unwrap();
    let output = cli(
        root,
        &[
            "run",
            "--",
            "sh",
            "-c",
            r#"printf 'Traceback (most recent call last):\n  File "backend/orders.py", line 8, in list_orders\nRuntimeError: synthetic\n' >&2; exit 7"#,
        ],
    );
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    let id = run_id(&output);
    let compact = success(root, &["context", "--run", &id]);
    assert!(compact.len() <= 320, "{compact}");
    let full_context = success(root, &["context", "--run", &id, "--budget", "2000"]);
    assert!(
        full_context.contains("backend/orders.py:8"),
        "{full_context}"
    );
    assert!(
        full_context.contains("purchase_orders [table]"),
        "{full_context}"
    );
    let docs = root.join("docs/operations.md");
    fs::write(&docs, "old marker").unwrap();
    assert!(success(root, &["find", "old marker"]).contains("docs/operations.md"));
    fs::write(&docs, "new marker").unwrap();
    assert!(success(root, &["find", "new marker"]).contains("docs/operations.md"));
    assert!(!success(root, &["find", "old marker"]).contains("docs/operations.md"));
    fs::write(&docs, "new marker".repeat(120)).unwrap();
    assert!(!success(root, &["find", "new marker"]).contains("docs/operations.md"));
}

#[test]
fn diff_sections_delta_sessions_and_exact_recall() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let git = |args: &[&str]| {
        let result = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", stderr(&result));
        result
    };
    git(&["init", "-q"]);
    for file in ["first.txt", "second.txt"] {
        fs::write(root.join(file), "old\n").unwrap();
        git(&["add", file]);
        fs::write(root.join(file), "a long new line of evidence\n".repeat(90)).unwrap();
    }
    let args = [
        "run",
        "--session",
        "one",
        "--max-bytes",
        "512",
        "--",
        "git",
        "diff",
    ];
    let first = cli(root, &args);
    let id = run_id(&first);
    assert!(stdout(&first).contains("first.txt") && stdout(&first).contains("second.txt"));
    let same = success(root, &args);
    assert!(same.contains(&format!("unchanged since {id}")));
    let another = success(
        root,
        &[
            "run",
            "--session",
            "two",
            "--max-bytes",
            "512",
            "--",
            "git",
            "diff",
        ],
    );
    assert!(another.contains("second.txt") && !another.contains("since"));
    fs::write(root.join("first.txt"), "different hunk\n".repeat(93)).unwrap();
    let delta = cli(root, &args);
    assert!(stdout(&delta).contains("+ first.txt") && !stdout(&delta).contains("second.txt"));
    let changed = run_id(&delta);
    let exact = cli(root, &["log", &changed, "--stream", "stdout"]);
    assert_eq!(exact.stdout, git(&["diff"]).stdout);
    let oldest = success(
        root,
        &["history", "a long new line", "--oldest", "--limit", "1"],
    );
    assert!(oldest.contains(&id) && oldest.contains("stdout:"));
    let stats = success(root, &["stats"]);
    assert!(
        stats.contains("1/4 runs reopened in full") && stats.contains("1 unchanged runs"),
        "{stats}"
    );
    assert!(
        success(
            root,
            &[
                "run",
                "--session",
                "one",
                "--fresh",
                "--max-bytes",
                "512",
                "--",
                "git",
                "diff"
            ]
        )
        .contains("second.txt")
    );
    fs::write(root.join("first.txt"), "old\n").unwrap();
    fs::write(root.join("second.txt"), "old\n").unwrap();
    let removed = success(root, &args);
    assert!(removed.contains("- first.txt") && removed.contains("- second.txt"));
}

#[test]
fn task_packs_and_handoffs_link_live_and_historical_evidence() {
    let mixed = mixed_fixture();
    let two_seeds = success(
        mixed.path(),
        &["pack", "trace fetchOrders to purchase_orders"],
    );
    assert!(
        two_seeds.contains("HTTP route") && two_seeds.contains("SQL relation"),
        "{two_seeds}"
    );
    let pack = success(
        mixed.path(),
        &["pack", "What breaks if I rename purchase_orders?"],
    );
    for term in [
        "schema.sql",
        "list_orders",
        "fetchOrders",
        "possible HTTP",
        "SELECT order_id",
    ] {
        assert!(pack.contains(term), "{pack}");
    }
    assert!(
        success(
            mixed.path(),
            &["pack", "purchase_orders", "--budget", "128"]
        )
        .len()
            <= 128
    );
    assert!(success(mixed.path(), &["pack", "canary-checkout"]).contains("docs/operations.md"));
    let repo = fixture();
    let root = repo.path();
    let args = [
        "run",
        "--max-bytes",
        "512",
        "--",
        "cargo",
        "test",
        "--offline",
    ];
    let failed = cli(root, &args);
    assert_eq!(failed.status.code(), Some(101));
    let id = run_id(&failed);
    let evidence = success(root, &["pack", "diagnose the failing checkout test"]);
    for term in [
        &id,
        "tests/checkout.rs:11",
        "apply_fee",
        "subtotal + fee + 1",
    ] {
        assert!(evidence.contains(term), "{evidence}");
    }
    let slice = ["code", "src/pricing.rs", "--from", "5", "--to", "7"];
    success(root, &slice);
    success(root, &slice);
    let source = root.join("src/pricing.rs");
    let text = fs::read_to_string(&source).unwrap();
    fs::write(source, text.replace("subtotal + fee + 1", "subtotal + fee")).unwrap();
    let handoff = success(root, &["resume"]);
    assert!(
        handoff.contains(&id) && handoff.contains("src/pricing.rs:5-7 [source changed]"),
        "{handoff}"
    );
    let fixed = cli(root, &args);
    assert_eq!(fixed.status.code(), Some(0));
    assert!(
        stdout(&fixed).contains("- total_includes_fee"),
        "{}",
        stdout(&fixed)
    );
    assert!(
        !success(root, &["pack", "why did checkout fail"]).contains("Recorded cargo test failure")
    );
    assert!(success(root, &["history", "assertion", "--oldest", "--limit", "1"]).contains(&id));
    assert!(success(root, &["stats"]).contains("1 repeated source-range reads"));
}

#[test]
fn older_runs_are_searchable_and_hook_statistics_do_not_capture_arguments() {
    let repo = mixed_fixture();
    let root = repo.path();
    let run = root.join(".justless/runs/run-older");
    fs::create_dir_all(&run).unwrap();
    fs::write(run.join("record.json"),r#"{"id":"run-older","status":1,"raw_bytes":8,"preview_bytes":8,"diagnostics":[],"source_digests":{}}"#).unwrap();
    fs::write(run.join("stdout"), "old log\n").unwrap();
    fs::write(run.join("stderr"), "").unwrap();
    assert!(success(root, &["history", "old log"]).contains("run-older legacy stdout:1"));
    assert!(success(root, &["resume"]).contains("run-older · legacy"));
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","session_id":"odd ' input","tool_input":{"command":command}});
    hook(
        root,
        "before-command",
        call("git status --porcelain secret-value"),
    );
    let result = hook(root, "before-command", call("git status"));
    let wrapped = result["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .unwrap();
    assert!(
        wrapped.starts_with("justless run --session ")
            && wrapped.ends_with(" -- git status")
            && !wrapped.contains("odd")
    );
    assert!(success(root, &["stats"]).contains("Hook passed through: git × 1"));
    assert!(
        !fs::read_to_string(root.join(".justless/activity.json"))
            .unwrap()
            .contains("secret-value")
    );
}

#[cfg(unix)]
#[test]
fn pytest_summaries_group_shared_diagnostics_and_preserve_failure_exit() {
    use std::os::unix::fs::PermissionsExt;
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let exe = root.join("pytest");
    fs::write(&exe, "#!/bin/sh\ncat results\nexit 1\n").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    let filler = "progress\n".repeat(100);
    fs::write(root.join("results"), format!("{filler}FAILED orders.py::one - TimeoutError\nFAILED orders.py::two - TimeoutError\nFAILED orders.py::three - ValueError\n3 failed in 0.1s\n")).unwrap();
    let args = ["run", "--max-bytes", "512", "--", exe.to_str().unwrap()];
    let failed = cli(root, &args);
    let preview = stdout(&failed);
    assert_eq!(failed.status.code(), Some(1));
    assert!(
        preview.contains("3 reported failing")
            && preview.contains("2 test(s)")
            && preview.matches("TimeoutError").count() == 1,
        "{preview}"
    );
    fs::write(
        root.join("results"),
        format!("{filler}FAILED orders.py::two - TimeoutError\n1 failed in 0.1s\n"),
    )
    .unwrap();
    let next = cli(root, &args);
    assert!(
        stdout(&next).contains("- orders.py::one") && !stdout(&next).contains("- orders.py::two")
    );
    fs::write(root.join("results"), "unrecognized fatal runner output\n").unwrap();
    let unknown = cli(root, &args);
    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(stdout(&unknown), "unrecognized fatal runner output\n");
}

#[test]
fn history_finds_an_exact_match_across_large_line_windows() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let text = format!(
        "{}split-match{}\nnext\n",
        "x".repeat(256 * 1024 - 3),
        "y".repeat(500_000)
    );
    fs::write(root.join("big.txt"), text).unwrap();
    let run = cli(root, &["run", "--", "cat", "big.txt"]);
    let id = run_id(&run);
    assert!(success(root, &["search", &id, "split-match"]).contains("stdout:1:"));
    assert!(success(root, &["history", "split-match"]).contains("split-match"));
    assert!(success(root, &["history", "next"]).contains("stdout:2:"));
}

#[test]
fn concurrent_runs_publish_complete_archives_and_keep_workdirs_separate() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    let a = root.join("a");
    let b = root.join("b");
    for folder in [&a, &b] {
        fs::create_dir(folder).unwrap();
        fs::write(folder.join("report"), "evidence\n".repeat(80)).unwrap();
    }
    let args = ["run", "--", "cat", "report"];
    let runs = std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..4).map(|_| scope.spawn(|| cli(&a, &args))).collect();
        tasks
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        runs.iter()
            .filter(|r| stdout(r).contains("Output unchanged"))
            .count(),
        3
    );
    for run in &runs {
        assert!(run.status.success());
        assert_eq!(
            success(root, &["log", &run_id(run), "--stream", "stdout"]),
            "evidence\n".repeat(80)
        );
    }
    assert_eq!(success(&b, &args), "evidence\n".repeat(80));
    assert_eq!(success(root, &["history"]).lines().count(), 5);
}

fn fake_program(dir: &Path, name: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(name);
    fs::write(&exe, format!("#!/bin/sh\n{script}")).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    exe
}

#[test]
fn container_and_cluster_tables_list_only_rows_that_need_attention() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    fs::write(
        root.join("ps.txt"),
        concat!(
            "CONTAINER ID   IMAGE         COMMAND   CREATED       STATUS                       PORTS     NAMES\n",
            "a1b2c3d4e5f6   nginx:1.27    \"nginx\"   2 hours ago   Up 2 hours (healthy)         80/tcp    web\n",
            "b2c3d4e5f6a1   postgres:16   \"pg\"      2 hours ago   Up 2 hours (unhealthy)                 db\n",
            "c3d4e5f6a1b2   redis:7       \"redis\"   3 hours ago   Exited (137) 5 minutes ago             cache\n",
            "d4e5f6a1b2c3   busybox       \"sh\"      3 hours ago   Up 3 hours                             worker\n",
        ),
    )
    .unwrap();
    fs::write(
        root.join("pods.txt"),
        concat!(
            "NAMESPACE   NAME          READY   STATUS             RESTARTS   AGE\n",
            "prod        api-abcde     1/1     Running            0          3d\n",
            "prod        api-fghij     0/1     CrashLoopBackOff   12         3d\n",
            "jobs        migrate-xyz   0/1     Completed          0          1d\n",
        ),
    )
    .unwrap();
    let docker = fake_program(root, "docker", "cat ps.txt\n");
    let kubectl = fake_program(root, "kubectl", "cat pods.txt\nexit ${FAIL:-0}\n");

    let ps = success(root, &["run", "--", docker.to_str().unwrap(), "ps", "-a"]);
    assert!(
        ps.starts_with("docker ps: 4 listed, 2 need attention\n")
            && ps.contains("db unhealthy [stdout:3-3]")
            && ps.contains("cache Exited (137) [stdout:4-4]")
            && ps.contains("Other examples: web, worker")
            && !ps.contains("nginx:1.27"),
        "{ps}"
    );
    let again = success(root, &["run", "--", docker.to_str().unwrap(), "ps", "-a"]);
    assert!(again.starts_with("Output unchanged since run-"), "{again}");

    let get = ["run", "--", kubectl.to_str().unwrap(), "get", "pods", "-A"];
    let pods = success(root, &get);
    assert!(
        pods.contains("kubectl get pods: 3 listed, 1 need attention")
            && pods.contains("prod/api-fghij CrashLoopBackOff ready=0/1 restarts=12")
            && !pods.contains("prod/api-abcde Running"),
        "{pods}"
    );

    // Formats Justless does not understand keep the ordinary preview.
    let wide = success(
        root,
        &[
            "run",
            "--",
            kubectl.to_str().unwrap(),
            "get",
            "pods",
            "-o",
            "wide",
        ],
    );
    assert_eq!(wide, fs::read_to_string(root.join("pods.txt")).unwrap());
    let failed = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(get)
        .current_dir(root)
        .env("FAIL", "1")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        stdout(&failed),
        fs::read_to_string(root.join("pods.txt")).unwrap()
    );
}

#[test]
fn js_tests_logs_and_project_filters_keep_failure_evidence() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let npm = fake_program(
        root,
        "npm",
        concat!(
            "i=0; while [ $i -lt 40 ]; do echo \"PASS src/m$i.test.js\"; i=$((i+1)); done\n",
            "echo 'FAIL src/cart.test.js'\n",
            "echo '  Expected: 30'\n",
            "echo '  Received: 27'\n",
            "echo 'Tests: 1 failed, 40 passed, 41 total'\n",
            "exit 1\n",
        ),
    );
    let tests = cli(root, &["run", "--", npm.to_str().unwrap(), "test"]);
    let preview = stdout(&tests);
    assert_eq!(tests.status.code(), Some(1));
    assert!(
        preview.starts_with("[4 retained lines, 40 filtered, 0 repeats]\n")
            && preview.contains("FAIL src/cart.test.js\n  Expected: 30\n  Received: 27\n")
            && !preview.contains("PASS "),
        "{preview}"
    );
    let id = run_id(&tests);
    assert_eq!(
        success(
            root,
            &["log", &id, "--stream", "stdout", "--from", "1", "--to", "1"]
        ),
        "PASS src/m0.test.js\n"
    );

    let docker = fake_program(
        root,
        "docker",
        "for i in 1 2 3 4 5; do echo 'GET /health 200'; done\necho 'ERROR db refused'\n",
    );
    let logs = success(
        root,
        &["run", "--", docker.to_str().unwrap(), "logs", "api"],
    );
    assert_eq!(
        logs,
        "[2 retained lines, 0 filtered, 4 repeats]\nGET /health 200\nERROR db refused\n"
    );

    let make = fake_program(
        root,
        "make",
        concat!(
            "for i in 1 2 3; do echo \"make[1]: Entering directory '/w/$i'\"; echo \"cc -c f$i.c\"; done\n",
            "echo \"f3.c:10: error: expected ';'\"\n",
            "exit 2\n",
        ),
    );
    fs::write(
        root.join(".justless.toml"),
        "[[filters]]\ncommand = [\"make\"]\nomit = '^make\\[\\d+\\]: Entering'\n",
    )
    .unwrap();
    let built = cli(root, &["run", "--", make.to_str().unwrap()]);
    assert_eq!(built.status.code(), Some(2));
    assert_eq!(
        stdout(&built),
        "[4 retained lines, 3 filtered, 0 repeats]\ncc -c f1.c\ncc -c f2.c\ncc -c f3.c\nf3.c:10: error: expected ';'\n"
    );

    for (rule, message) in [
        ("command = []\nomit = 'x'", "nonempty argv prefix"),
        (
            "command = ['make']\nomit = '.*'",
            "must not match empty text",
        ),
        (
            "command = ['make']\nomit = '('",
            "invalid filters.omit regex",
        ),
    ] {
        fs::write(
            root.join(".justless.toml"),
            format!("[[filters]]\n{rule}\n"),
        )
        .unwrap();
        let rejected = cli(root, &["run", "--", make.to_str().unwrap()]);
        assert!(
            !rejected.status.success() && stderr(&rejected).contains(message),
            "{rule}: {}",
            stderr(&rejected)
        );
    }
}

#[test]
fn hooks_route_display_commands_and_leave_machine_formats_alone() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":command}});
    for command in [
        "docker ps -a",
        "docker container ls --filter status=exited",
        "kubectl get pods -A",
        "kubectl -n prod get deploy",
        "kubectl logs api-1 --tail 50",
        "docker logs --since 10m api",
        "npm test",
        "pnpm run test",
        "vitest run",
        "node --test",
    ] {
        let output = agent_hook(root, "hook-claude", "before-command", call(command));
        assert_eq!(
            output["hookSpecificOutput"]["updatedInput"]["command"],
            format!("justless run -- {command}"),
            "{command}"
        );
    }
    for command in [
        "docker ps --format json",
        "docker ps -q",
        "kubectl get svc",
        "kubectl get pods -o yaml",
        "kubectl logs -f api-1",
        "npm test -- --watch",
        "vitest",
        "jest --json",
        "docker ps | grep db",
    ] {
        assert_eq!(
            agent_hook(root, "hook-claude", "before-command", call(command)),
            serde_json::json!({}),
            "{command}"
        );
    }
}

#[test]
fn compound_commands_wrap_each_supported_segment_in_place() {
    let repo = TempDir::new().unwrap();
    let root = repo.path().canonicalize().unwrap();
    fs::create_dir(root.join("web")).unwrap();
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":command}});
    let rewrite = |agent: &str, command: &str| {
        agent_hook(&root, agent, "before-command", call(command))["hookSpecificOutput"]
            ["updatedInput"]["command"]
            .as_str()
            .map(str::to_owned)
    };
    for (command, expected) in [
        (
            "git status; git diff",
            "justless run -- git status; justless run -- git diff",
        ),
        ("cd web && npm test", "cd web && justless run -- npm test"),
        (
            "git log --oneline -5  &&  git diff --stat",
            "justless run -- git log --oneline -5  &&  justless run -- git diff --stat",
        ),
    ] {
        for agent in ["hook-claude", "hook-codex"] {
            assert_eq!(
                rewrite(agent, command).as_deref(),
                Some(expected),
                "{agent} {command}"
            );
        }
    }
    // Claude Code checks permissions for each rewritten segment. Codex auto-approves
    // a rewrite, so it only rewrites chains made entirely of supported commands.
    for (command, expected) in [
        (
            "cargo test && git push",
            "justless run -- cargo test && git push",
        ),
        ("cargo test || true", "justless run -- cargo test || true"),
        (
            "echo \"a && b\" && git status",
            "echo \"a && b\" && justless run -- git status",
        ),
    ] {
        assert_eq!(rewrite("hook-claude", command).as_deref(), Some(expected));
        assert_eq!(rewrite("hook-codex", command), None, "{command}");
    }
    for command in [
        "cd ../elsewhere && cargo test",
        "cd /tmp && cargo test",
        "cargo test 2>&1 | tail -20",
        "cargo test &",
        "cargo test && $(touch x)",
        "cargo test &&",
        "; cargo test",
        "(cd web && npm test)",
        "cargo test # && rm",
        "git status\ngit diff",
        "git push && echo done",
    ] {
        assert_eq!(rewrite("hook-claude", command), None, "{command:?}");
    }
}

fn lint_fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/lint")
        .join(name);
    fs::read_to_string(path).unwrap()
}

#[test]
fn linter_findings_group_by_rule_with_locations() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    // clippy.txt is real `cargo clippy` output. The other fixtures follow each
    // tool's documented default format.
    for (program, args, fixture, stream, expected) in [
        (
            "cargo",
            &["clippy"][..],
            "clippy.txt",
            "2",
            &[
                "cargo clippy: 11 finding(s), 0 error(s), 11 warning(s) · 4 rule(s) · 1 file(s)",
                "warning clippy::needless_return ×5  unneeded `return` statement",
                "  src/lib.rs:1:49, 2:50, 3:50, 3:72 +1 more [stderr:15-27]",
                "warning unused_variables ×2  unused variable: `unused`",
            ][..],
        ),
        (
            "mypy",
            &["app.py"],
            "mypy.txt",
            "1",
            &[
                "error return-value ×2  Incompatible return value type (got \"int\", expected \"str\")",
                "  app.py:7, 14 [stdout:1-1]",
                "error attr-defined ×1",
            ],
        ),
        (
            "ruff",
            &["check", "."],
            "ruff.txt",
            "1",
            &[
                "warning F401 ×3",
                "  app.py:1:8, 2:8, 3:8",
                "warning F841 ×2",
            ],
        ),
        (
            "ruff",
            &["check", "."],
            "ruff_full.txt",
            "1",
            &[
                "warning F401 ×2  `os` imported but unused",
                "  app.py:1:8, 2:8 [stdout:1-7]",
            ],
        ),
        (
            "tsc",
            &["--noEmit"],
            "tsc.txt",
            "1",
            &[
                "error TS2322 ×3",
                "  src/a.ts:1:7, 2:7, src/b.ts:3:33 [stdout:1-1]",
            ],
        ),
        (
            "shellcheck",
            &["s.sh"],
            "shellcheck.txt",
            "1",
            &["warning SC2086 ×2", "  s.sh:2, 3", "error SC2045 ×1"],
        ),
        (
            "rubocop",
            &[],
            "rubocop.txt",
            "1",
            &[
                "warning Style/FrozenStringLiteralComment ×2",
                "  app.rb:1:1, lib.rb:1:1 [stdout:6-6]",
            ],
        ),
    ] {
        fs::write(root.join("output.txt"), lint_fixture(fixture)).unwrap();
        let redirect = if stream == "2" { " >&2" } else { "" };
        let exe = fake_program(
            root,
            program,
            &format!("cat output.txt{redirect}\nexit 1\n"),
        );
        let mut command = vec!["run", "--fresh", "--", exe.to_str().unwrap()];
        command.extend(args);
        let output = cli(root, &command);
        assert_eq!(output.status.code(), Some(1), "{fixture}");
        let preview = stdout(&output);
        for line in expected {
            assert!(
                preview.contains(line),
                "{fixture}: missing {line:?}\n{preview}"
            );
        }
        assert!(
            stderr(&output).trim_start().starts_with("[justless run-"),
            "{fixture}"
        );
    }

    // A repeated ESLint report collapses to one line per rule and one outcome line.
    fs::write(
        root.join("output.txt"),
        lint_fixture("eslint.txt").repeat(4),
    )
    .unwrap();
    let eslint = fake_program(root, "eslint", "cat output.txt\nexit 1\n");
    let report = stdout(&cli(root, &["run", "--", eslint.to_str().unwrap(), "."]));
    assert!(
        report.starts_with("eslint: 20 finding(s), 16 error(s), 4 warning(s) · 3 rule(s) · 2 file(s)\n")
            && report.contains("error no-undef ×12  'foo' is not defined\n  /work/web/b.js:3:15, 4:25, /work/web/c.js:1:1 [stdout:4-4]")
            && report.matches("✖ 5 problems").count() == 1,
        "{report}"
    );

    // A rerun reports which rules appeared, changed or are no longer reported.
    fs::write(
        root.join("output.txt"),
        "/work/web/b.js\n  3:15  error  'foo' is not defined  no-undef\n  9:1  error  'x' is assigned a value but never used  no-unused-vars\n".repeat(3),
    )
    .unwrap();
    let rerun = stdout(&cli(root, &["run", "--", eslint.to_str().unwrap(), "."]));
    assert!(
        rerun.contains("Compared with run-")
            && rerun.contains("~ no-undef ×12 → ×3")
            && rerun.contains("~ no-unused-vars ×4 → ×3")
            && rerun.contains("- eqeqeq ×4 (no longer reported)"),
        "{rerun}"
    );

    // Output with no recognized finding keeps the ordinary preview.
    fs::write(root.join("output.txt"), "All checks passed!\n").unwrap();
    let ruff = fake_program(root, "ruff", "cat output.txt\n");
    assert_eq!(
        success(root, &["run", "--", ruff.to_str().unwrap(), "check"]),
        "All checks passed!\n"
    );
}

#[test]
fn hooks_route_linters_build_tools_and_package_runners() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":command}});
    for command in [
        "cargo clippy --all-targets -- -D warnings",
        "npx eslint src",
        "npx -y tsc --noEmit",
        "pnpm exec vue-tsc --noEmit",
        "uv run ruff check .",
        "poetry run mypy app",
        "python -m mypy app",
        "bundle exec rubocop",
        "golangci-lint run ./...",
        "shellcheck -f gcc install.sh",
        "go vet ./...",
        "uv run pytest -q",
        "make",
        "./gradlew build",
        "mvn -q test",
        "dotnet build",
        "swift test",
        "cmake --build build",
    ] {
        let output = agent_hook(root, "hook-claude", "before-command", call(command));
        assert_eq!(
            output["hookSpecificOutput"]["updatedInput"]["command"],
            format!("justless run -- {command}"),
            "{command}"
        );
    }
    for command in [
        "eslint --format json src",
        "ruff check --output-format=json .",
        "npx tsc --watch",
        "mypy --pretty app",
        "shellcheck -f json install.sh",
        "npx",
        "uv run",
        "cmake -S . -B build",
    ] {
        assert_eq!(
            agent_hook(root, "hook-claude", "before-command", call(command)),
            serde_json::json!({}),
            "{command}"
        );
    }
}
