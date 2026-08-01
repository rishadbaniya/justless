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
    let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(["hook-codex", mode])
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
