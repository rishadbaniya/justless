use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, has, mcp_session, starts, text};

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

/// `justless mcp --root ROOT` answering `lines`.
fn exchange(root: &Path, lines: &[String]) -> Vec<Value> {
    let mut command = Command::new(BIN);
    command.args(["mcp", "--root"]).arg(root).current_dir(root);
    mcp_session(command.env_remove("CLAUDE_PROJECT_DIR"), lines).1
}

/// `justless mcp` without `--root`, in `cwd` with `env`: its stderr and responses.
fn serve_from(cwd: &Path, env: &[(&str, &Path)], lines: &[String]) -> (String, Vec<Value>) {
    let mut command = Command::new(BIN);
    command
        .arg("mcp")
        .current_dir(cwd)
        .env_remove("CLAUDE_PROJECT_DIR");
    mcp_session(command.envs(env.iter().copied()), lines)
}

fn request(id: impl Into<Value>, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id.into(), "method": method, "params": params}).to_string()
}

fn call(id: impl Into<Value>, name: &str, arguments: Value) -> String {
    let params = json!({"name": name, "arguments": arguments});
    request(id, "tools/call", params)
}

fn initialize(version: &str) -> String {
    let client = json!({"name": "t", "version": "1"});
    let params = json!({"protocolVersion": version, "capabilities": {}, "clientInfo": client});
    request(0, "initialize", params)
}

fn legacy_initialize() -> String {
    initialize("2025-06-18")
}

fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

fn by_id(responses: &[Value], id: impl Into<Value>) -> &Value {
    let id = id.into();
    responses
        .iter()
        .find(|response| response["id"] == id)
        .unwrap_or_else(|| panic!("no response for {id}: {responses:#?}"))
}

fn tool_text(response: &Value) -> String {
    let content = response["result"]["content"].as_array().unwrap();
    let texts = content.iter().map(|item| {
        assert_eq!(item["type"], "text");
        item["text"].as_str().unwrap()
    });
    texts.collect()
}

fn succeeded(response: &Value) -> String {
    assert_eq!(response["result"]["isError"], false, "{response:#}");
    tool_text(response)
}

fn failed(response: &Value) -> String {
    assert_eq!(response["result"]["isError"], true, "{response:#}");
    tool_text(response)
}

fn find_tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools.iter().find(|tool| tool["name"] == name).unwrap()
}

#[test]
fn legacy_handshake_lists_and_calls_read_only_tools() {
    let fixture = mixed_fixture();
    // [id, tool, arguments]
    let calls = json!([
        ["query", "query", {"symbol": "purchase_orders", "view": "definition"}],
        ["path", "path", {"from": "fetchOrders", "to": "purchase_orders"}],
        ["read", "read", {"file": "frontend/orders.ts", "outline": true}],
        ["expand", "expand", {"file": "frontend/orders.ts", "selector": "fetchOrders"}],
        ["code", "code", {"file": "db/schema.sql", "from": 1, "to": 2}],
        ["context", "context", {"at": "backend/orders.py:7"}],
        ["pack", "pack", {"question": "What breaks if I rename purchase_orders?", "budget": 800}],
        ["find", "find", {"text": "purchase_orders", "limit": 5}],
        ["lookup", "lookup", {"question": "purchase orders", "limit": 3}],
        ["lookup-empty", "lookup", {"question": "?!"}],
        ["map", "map", {"budget": 1200}],
        ["affected", "affected", {"target": "purchase_orders", "depth": 2}],
        ["affected-both", "affected", {"target": "purchase_orders", "changes": true}],
        // Values that look like options stay positional.
        ["flag", "find", {"text": "--budget=1"}],
        ["stats", "stats", {}],
        ["missing", "query", {"symbol": "no_such_symbol"}],
        ["escape", "read", {"file": "../outside.txt"}],
        ["absolute", "code", {"file": "/etc/hosts", "to": 1}],
        ["unknown-arg", "read", {"file": "db/schema.sql", "lines": 3}],
        ["range", "find", {"text": "x", "limit": 0}],
        ["both", "context", {"at": "db/schema.sql:1", "changes": true}],
        ["base-alone", "context", {"base": "main"}],
        ["base-staged", "context", {"changes": true, "staged": true, "base": "main"}],
        ["base-target", "affected", {"target": "purchase_orders", "base": "main"}],
        // Only `--base` needs a Git repository, so this error shows it was passed.
        ["base", "affected", {"changes": true, "base": "main"}],
        ["run-id", "log", {"run_id": "../../etc"}],
        ["mutating", "run", {"command": ["true"]}]
    ]);
    let mut lines = vec![
        legacy_initialize(),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        request(2, "ping", json!({})),
        request("list", "tools/list", json!({})),
    ];
    for row in calls.as_array().unwrap() {
        lines.push(call(
            row[0].clone(),
            row[1].as_str().unwrap(),
            row[2].clone(),
        ));
    }
    let arguments = json!({"name": "query", "arguments": "x"});
    lines.extend([
        request("arguments", "tools/call", arguments),
        request(3, "resources/subscribe", json!({})),
        "{not json".into(),
        json!([{"jsonrpc": "2.0", "id": 9, "method": "ping"}]).to_string(),
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 2}})
            .to_string(),
    ]);
    let responses = exchange(fixture.path(), &lines);
    let requests = lines
        .iter()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .map(|value| !value.is_object() || value.get("id").is_some())
                .unwrap_or(true)
        })
        .count();
    assert_eq!(responses.len(), requests, "{responses:#?}");

    let initialize = &by_id(&responses, 0)["result"];
    assert_eq!(initialize["protocolVersion"], "2025-06-18");
    assert_eq!(initialize["serverInfo"]["name"], "justless");
    assert!(initialize["capabilities"]["tools"].is_object());
    // Questions go to `lookup` first; `pack` gathers evidence for a task.
    let instructions = initialize["instructions"].as_str().unwrap();
    let lookup = instructions.find("`lookup`").unwrap();
    assert!(
        lookup < instructions.find("`pack`").unwrap(),
        "{instructions}"
    );
    let before = &instructions[..lookup];
    assert!(
        before.ends_with("For a how or where question, start with "),
        "{instructions}"
    );
    assert_eq!(by_id(&responses, 2)["result"], json!({}));

    let tools = by_id(&responses, "list")["result"]["tools"]
        .as_array()
        .unwrap();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names,
        BTreeSet::from([
            "affected", "code", "context", "expand", "find", "history", "log", "lookup", "map",
            "pack", "path", "query", "read", "recall", "resume", "search", "stats",
        ])
    );
    // The tool list rides along with every model request, so keep it compact.
    let size = serde_json::to_string(tools).unwrap().len();
    assert!(size < 8_000, "tool list is {size} bytes");
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool:#}");
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        let description = tool["description"].as_str().unwrap().len();
        assert!((40..=200).contains(&description), "{tool:#}");
    }
    let query = find_tool(tools, "query");
    assert_eq!(query["inputSchema"]["required"], json!(["symbol"]));
    assert!(query["inputSchema"]["properties"]["budget"].is_object());
    let expand = find_tool(tools, "expand");
    assert!(expand["inputSchema"]["properties"].get("budget").is_none());
    let lookup = find_tool(tools, "lookup");
    assert_eq!(lookup["inputSchema"]["required"], json!(["question"]));
    assert!(lookup["inputSchema"]["properties"].get("logs").is_none());

    for (id, shown) in [
        ("query", "db/schema.sql:1-3"),
        ("path", "HTTP route at frontend/orders.ts:4"),
        ("read", "fetchOrders"),
        ("expand", "export async function fetchOrders()"),
        ("code", "1 |"),
        ("context", "backend/orders.py"),
        ("pack", "purchase_orders"),
        ("find", "db/schema.sql:1"),
        ("lookup", "db/schema.sql"),
        ("lookup", "Next: justless "),
        (
            "map",
            "  backend/ → db/ · Python → SQL · possible SQL relation 1 · list_orders backend/orders.py:8 → purchase_orders db/schema.sql:1-3",
        ),
        (
            "affected",
            "  fetchOrders [function] frontend/orders.ts:3-7 · possible HTTP route at frontend/orders.ts:4 → list_orders",
        ),
        ("flag", "No matching indexed text"),
        ("stats", ""),
    ] {
        has(&succeeded(by_id(&responses, id)), shown);
    }
    starts(&succeeded(by_id(&responses, "read")), "revision ");
    assert!(!succeeded(by_id(&responses, "code")).contains("3 |"));
    let map = succeeded(by_id(&responses, "map"));
    assert!(map.len() <= 1200, "{map}");

    for (id, error) in [
        ("lookup-empty", "enter a question or keywords"),
        ("affected-both", "exactly one of `target`"),
        ("missing", "symbol not found"),
        ("escape", "workspace-relative"),
        ("absolute", "workspace-relative"),
        ("unknown-arg", "unknown argument `lines`"),
        ("range", "from 1 to 1000"),
        ("both", "exactly one"),
        ("base-alone", "`base` requires `changes: true`"),
        ("base-target", "`base` requires `changes: true`"),
        ("base-staged", "`staged` or `base`, not both"),
        ("base", "--base needs a Git repository"),
        ("run-id", "run ID"),
    ] {
        has(&failed(by_id(&responses, id)), error);
    }
    assert_eq!(by_id(&responses, "mutating")["error"]["code"], -32602);
    assert_eq!(by_id(&responses, "arguments")["error"]["code"], -32602);
    assert_eq!(by_id(&responses, 3)["error"]["code"], -32601);
    let nulls = responses
        .iter()
        .filter(|response| response["id"].is_null())
        .map(|response| response["error"]["code"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(nulls, [-32700, -32600]);
}

#[test]
fn stateless_requests_declare_their_version() {
    let fixture = mixed_fixture();
    let meta = |extra: Value| {
        let mut params = json!({"_meta": modern_meta()});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        params
    };
    let query = json!({"name": "query", "arguments": {"symbol": "purchase_orders"}});
    let missing = json!({"name": "code", "arguments": {"file": "db/missing.sql", "to": 2}});
    let old = json!({"io.modelcontextprotocol/protocolVersion": "1900-01-01",
        "io.modelcontextprotocol/clientCapabilities": {}});
    let bare = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28"});
    let lines = vec![
        request("early", "tools/list", json!({})),
        request("discover", "server/discover", meta(json!({}))),
        request("list", "tools/list", meta(json!({}))),
        request("call", "tools/call", meta(query)),
        request("bad-call", "tools/call", meta(missing)),
        request("old", "tools/list", json!({"_meta": old})),
        request("no-capabilities", "tools/list", json!({"_meta": bare})),
        request("unknown", "prompts/list", meta(json!({}))),
    ];
    let responses = exchange(fixture.path(), &lines);
    assert_eq!(responses.len(), lines.len());

    let early = &by_id(&responses, "early")["error"];
    assert_eq!(early["code"], -32602);
    has(early["message"].as_str().unwrap(), "2026-07-28");

    let discover = &by_id(&responses, "discover")["result"];
    assert_eq!(discover["resultType"], "complete");
    assert_eq!(discover["supportedVersions"][0], "2026-07-28");
    assert!(discover["capabilities"]["tools"].is_object());
    assert!(discover["ttlMs"].is_u64() && discover["cacheScope"] == "public");
    let server = &discover["_meta"]["io.modelcontextprotocol/serverInfo"];
    assert_eq!(server["name"], "justless");

    let list = &by_id(&responses, "list")["result"];
    assert_eq!(list["resultType"], "complete");
    assert_eq!(list["tools"].as_array().unwrap().len(), 17);
    assert!(list["ttlMs"].is_u64());

    let call = by_id(&responses, "call");
    assert_eq!(call["result"]["resultType"], "complete");
    has(&succeeded(call), "db/schema.sql");
    failed(by_id(&responses, "bad-call"));

    let old = &by_id(&responses, "old")["error"];
    assert_eq!(old["code"], -32022);
    assert_eq!(old["data"]["requested"], "1900-01-01");
    let supported = old["data"]["supported"].as_array().unwrap();
    assert!(supported.contains(&json!("2025-11-25")));
    let no_capabilities = &by_id(&responses, "no-capabilities")["error"];
    assert_eq!(no_capabilities["code"], -32602);
    assert_eq!(by_id(&responses, "unknown")["error"]["code"], -32601);
}

#[test]
fn initialize_offers_the_newest_legacy_version_for_unknown_requests() {
    let fixture = mixed_fixture();
    let responses = exchange(fixture.path(), &[initialize("2099-01-01")]);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-11-25");
}

fn install(root: &Path, clients: &[&str]) -> Output {
    let mut command = Command::new(BIN);
    command.arg("--root").arg(root).args(["mcp", "--install"]);
    command.args(clients).current_dir(root).output().unwrap()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn install_merges_client_config_and_is_idempotent() {
    let project = TempDir::new().unwrap();
    let root = project.path();
    let codex = "# Project settings\nmodel = \"gpt-5\" # pinned\n\n[mcp_servers.other]\ncommand = \"other\"\nargs = [\"serve\"]\n";
    for (file, contents) in [
        (
            ".cursor/mcp.json",
            r#"{"mcpServers":{"other":{"command":"other-server","args":["--port","1"]}},"extra":{"keep":true}}"#,
        ),
        (
            ".gemini/settings.json",
            r#"{"theme":"Default","mcpServers":{"other":{"command":"x"}}}"#,
        ),
        (
            ".vscode/mcp.json",
            r#"{"inputs":[{"id":"token","type":"promptString"}],"servers":{}}"#,
        ),
        (".codex/config.toml", codex),
    ] {
        fs::create_dir(root.join(file).parent().unwrap()).unwrap();
        fs::write(root.join(file), contents).unwrap();
    }

    let all = ["claude", "cursor", "gemini", "vscode", "codex"];
    let first = install(root, &all);
    assert!(first.status.success(), "{}", text(&first.stderr));
    assert_eq!(text(&first.stdout).matches("added justless").count(), 5);

    let claude = read_json(&root.join(".mcp.json"));
    let server = json!({"command": "justless", "args": ["mcp"]});
    assert_eq!(claude, json!({"mcpServers": {"justless": server}}));
    let cursor = read_json(&root.join(".cursor/mcp.json"));
    assert_eq!(cursor["extra"], json!({"keep": true}));
    assert_eq!(
        cursor["mcpServers"]["other"]["args"],
        json!(["--port", "1"])
    );
    let justless = &cursor["mcpServers"]["justless"];
    assert_eq!(justless["type"], "stdio");
    assert_eq!(
        justless["args"],
        json!(["mcp", "--root", "${workspaceFolder}"])
    );
    let gemini = read_json(&root.join(".gemini/settings.json"));
    assert_eq!(gemini["theme"], "Default");
    assert_eq!(gemini["mcpServers"]["other"]["command"], "x");
    assert_eq!(gemini["mcpServers"]["justless"]["command"], "justless");
    let vscode = read_json(&root.join(".vscode/mcp.json"));
    assert_eq!(vscode["inputs"][0]["id"], "token");
    assert_eq!(vscode["servers"]["justless"]["type"], "stdio");
    assert!(vscode.get("mcpServers").is_none());
    let toml = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
    starts(&toml, codex);
    has(
        &toml,
        "[mcp_servers.justless]\ncommand = \"justless\"\nargs = [\"mcp\"]\n",
    );

    let files = [
        ".mcp.json",
        ".cursor/mcp.json",
        ".gemini/settings.json",
        ".vscode/mcp.json",
        ".codex/config.toml",
    ];
    let snapshot = files.map(|file| fs::read(root.join(file)).unwrap());
    let second = install(root, &all);
    assert!(second.status.success(), "{}", text(&second.stderr));
    let already = text(&second.stdout)
        .matches("already lists justless")
        .count();
    assert_eq!(already, 5);
    assert_eq!(
        snapshot,
        files.map(|file| fs::read(root.join(file)).unwrap())
    );
}

#[test]
fn install_keeps_existing_entries_and_rejects_unreadable_config() {
    let project = TempDir::new().unwrap();
    let root = project.path();
    let custom = r#"{"mcpServers":{"justless":{"command":"/opt/justless","args":["mcp","--root","/srv/app"]}}}"#;
    fs::write(root.join(".mcp.json"), custom).unwrap();
    let kept = install(root, &["claude"]);
    assert!(kept.status.success(), "{}", text(&kept.stderr));
    assert_eq!(fs::read_to_string(root.join(".mcp.json")).unwrap(), custom);

    // A commented settings file fails before any other client is written.
    fs::create_dir(root.join(".vscode")).unwrap();
    let commented = "{\n  // team servers\n  \"servers\": {}\n}\n";
    let vscode = root.join(".vscode/mcp.json");
    fs::write(&vscode, commented).unwrap();
    let rejected = install(root, &["cursor", "vscode"]);
    assert!(!rejected.status.success());
    has(&text(&rejected.stderr), ".vscode/mcp.json");
    assert_eq!(fs::read_to_string(&vscode).unwrap(), commented);
    assert!(!root.join(".cursor").exists());

    fs::write(&vscode, r#"{"servers":[]}"#).unwrap();
    let wrong = install(root, &["vscode"]);
    assert!(!wrong.status.success());
    has(
        &text(&wrong.stderr),
        "servers in .vscode/mcp.json must contain an object",
    );
}

#[test]
fn claude_project_dir_sets_the_root_without_an_explicit_one() {
    let repo = mixed_fixture();
    let elsewhere = TempDir::new().unwrap();
    let query = call(2, "query", json!({"symbol": "purchase_orders"}));
    let env = [("CLAUDE_PROJECT_DIR", repo.path())];
    let (_, responses) = serve_from(elsewhere.path(), &env, &[legacy_initialize(), query]);
    has(
        &succeeded(by_id(&responses, 2)),
        "db/schema.sql::purchase_orders [table]",
    );
    assert!(!elsewhere.path().join(".justless").exists());

    // A subdirectory finds the project that encloses it.
    let repo = TempDir::new().unwrap();
    let root = repo.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();
    let src = root.join("src");
    let run = Command::new(BIN)
        .args(["run", "--", "echo", "captured-by-hook"])
        .current_dir(&src)
        .env_remove("CLAUDE_PROJECT_DIR")
        .output()
        .unwrap();
    assert!(run.status.success(), "{}", text(&run.stderr));
    let lines = [
        legacy_initialize(),
        call(1, "history", json!({})),
        call(2, "read", json!({"file": "src/lib.rs"})),
    ];
    let (stderr, responses) = serve_from(&src, &[("CLAUDE_PROJECT_DIR", &src)], &lines);
    has(&stderr, &format!("serving {} over stdio", root.display()));
    has(&succeeded(by_id(&responses, 1)), "run-");
    has(&succeeded(by_id(&responses, 2)), "pub fn hello");
    assert!(!src.join(".justless").exists());
}

#[test]
fn install_writes_to_the_current_project_and_names_the_file() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let (a, b) = (
        a.path().canonicalize().unwrap(),
        b.path().canonicalize().unwrap(),
    );
    for root in [&a, &b] {
        fs::create_dir(root.join(".git")).unwrap();
    }
    let output = Command::new(BIN)
        .args(["mcp", "--install", "cursor"])
        .current_dir(&b)
        .env("CLAUDE_PROJECT_DIR", &a)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    let file = b.join(".cursor/mcp.json");
    assert!(file.is_file());
    assert!(!a.join(".cursor").exists() && !a.join(".justless").exists());
    assert_eq!(
        text(&output.stdout),
        format!("Cursor: added justless to {}\n", file.display())
    );
}

#[cfg(unix)]
#[test]
fn install_names_an_unreadable_config_file() {
    use std::os::unix::fs::PermissionsExt;
    let project = TempDir::new().unwrap();
    let root = &project.path().canonicalize().unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    let config = root.join(".codex/config.toml");
    fs::write(&config, "").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&config).is_ok() {
        // Running as root: permissions do not apply.
        return;
    }
    let output = install(root, &["claude", "codex"]);
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!output.status.success());
    has(&text(&output.stderr), &format!("read {}", config.display()));
    assert!(!root.join(".mcp.json").exists());
}

#[test]
fn batches_are_answered_only_after_negotiating_2025_03_26() {
    let fixture = mixed_fixture();
    let root = fixture.path();
    let batch = json!([
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "ping"},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/list"},
        1,
    ])
    .to_string();
    let notifications =
        json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}]).to_string();
    let lines = [
        initialize("2025-03-26"),
        batch.clone(),
        notifications,
        "[]".into(),
    ];
    let lines = exchange(root, &lines);
    assert_eq!(lines.len(), 3, "{lines:#?}");
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-03-26");
    let replies = lines[1].as_array().unwrap();
    assert_eq!(replies.len(), 3, "{replies:#?}");
    assert_eq!(replies[0], json!({"jsonrpc": "2.0", "id": 2, "result": {}}));
    assert_eq!(replies[1]["id"], 3);
    assert!(replies[1]["result"]["tools"].as_array().unwrap().len() > 10);
    assert_eq!(replies[2]["error"]["code"], -32600);
    assert_eq!(lines[2]["error"]["code"], -32600);
    assert_eq!(lines[2]["id"], Value::Null);

    // Later revisions removed batching, so an array is still one invalid message.
    let lines = exchange(root, &[legacy_initialize(), batch]);
    assert_eq!(lines[1]["error"]["code"], -32600, "{lines:#?}");
    assert_eq!(lines[1]["id"], Value::Null);
}

/// Captures a shell script with `justless run` and returns its run ID.
fn captured(root: &Path, script: &str) -> String {
    let mut command = Command::new(BIN);
    command
        .args(["--root"])
        .arg(root)
        .args(["run", "--", "sh", "-c", script]);
    crate::common::run_id(&command.current_dir(root).output().unwrap())
}

/// 20,000 ten-byte rows: far more than one 32 KiB page.
const ROWS: &str = "awk 'BEGIN{for(i=1;i<=20000;i++) printf \"row %05d\\n\", i}'";

/// The texts of a successful log call's content.
fn page(responses: &[Value], id: i64) -> Vec<String> {
    let response = by_id(responses, id);
    assert_eq!(response["result"]["isError"], false, "{response:#}");
    let content = response["result"]["content"].as_array().unwrap();
    let texts = content.iter().map(|item| item["text"].as_str().unwrap());
    texts.map(str::to_owned).collect()
}

#[test]
fn log_replays_are_paged_on_line_boundaries() {
    let fixture = mixed_fixture();
    let root = fixture.path();
    let rows = captured(root, &format!("{ROWS}; echo warning >&2"));
    let long = captured(
        root,
        "awk 'BEGIN{s=\"\"; for(i=0;i<40000;i++) s=s \"x\"; print s; print \"after\"}'",
    );
    let lines = exchange(
        root,
        &[
            legacy_initialize(),
            call(1, "log", json!({"run_id": rows})),
            call(
                2,
                "log",
                json!({"run_id": rows, "stream": "stdout", "from": 3277}),
            ),
            call(
                3,
                "log",
                json!({"run_id": rows, "from": 1, "to": 100_000_000}),
            ),
            call(4, "log", json!({"run_id": rows, "stream": "stderr"})),
            call(5, "log", json!({"run_id": long})),
            call(6, "log", json!({"run_id": rows, "from": 19_999})),
        ],
    );
    // 3,276 ten-byte rows fit in 32 KiB.
    let first = page(&lines, 1);
    assert_eq!(first.len(), 2);
    starts(&first[0], "row 00001\n");
    assert!(
        first[0].ends_with(
            "row 03276\n[justless: stdout page ends at line 3276 after 32768 bytes. Call log with stream=\"stdout\", from=3277 for more.]"
        ),
        "{}",
        &first[0][first[0].len() - 200..]
    );
    assert_eq!(
        first[1],
        "[justless: stderr not shown, the page is full. Call log with stream=\"stderr\", from=1.]"
    );
    let second = page(&lines, 2);
    starts(&second[0], "row 03277\n");
    has(&second[0], "from=6553 for more");
    has(&page(&lines, 3)[0], "from=3277, to=100000000 for more");
    assert_eq!(page(&lines, 4), ["warning\n"]);
    let cut = page(&lines, 5);
    assert_eq!(cut.len(), 1);
    starts(&cut[0], &"x".repeat(32_768));
    // The shell the agent runs it in may start anywhere, so the command names the root.
    let server_root = root.canonicalize().unwrap();
    assert!(
        cut[0].contains(&format!(
            "line 1 is longer than 32768 bytes and was cut. Run `justless --root {} log {long} --stream stdout --from 1 --to 1` in a shell",
            server_root.display()
        )) && cut[0].contains("from=2 to continue"),
        "{}",
        &cut[0][32_768..]
    );
    // A replay that fits is returned exactly. stderr has no line 19,999.
    assert_eq!(page(&lines, 6), ["row 19999\nrow 20000\n"]);
}

#[test]
fn a_log_page_counts_the_bytes_it_returned_not_the_whole_replay() {
    let fixture = mixed_fixture();
    let root = fixture.path();
    let rows = captured(root, ROWS);
    let lines = exchange(
        root,
        &[
            legacy_initialize(),
            call(1, "log", json!({"run_id": rows})),
            call(2, "log", json!({"run_id": rows, "from": 3277, "to": 3286})),
        ],
    );
    let returned = |id| {
        page(&lines, id)
            .iter()
            .map(|text| text.len() as u64)
            .sum::<u64>()
    };
    let (page, range) = (returned(1), returned(2));
    assert!((32_768..33_000).contains(&page), "{page}");
    assert_eq!(range, 100);
    let output = Command::new(BIN)
        .args(["--root"])
        .arg(root)
        .args(["stats", "--json"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), "");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["raw_bytes"], 200_000);
    assert_eq!(report["replayed_bytes"], page + range);
    assert_eq!(report["estimated_replay_bytes"], 0);
    assert_eq!(report["unmeasured_retrievals"], 0);
    assert_eq!(report["commands"][0]["reopened_runs"], 1);
    assert_eq!(report["commands"][0]["targeted_lookups"], 1);
    let preview = report["preview_bytes"].as_i64().unwrap();
    let saved = 200_000 - preview - (page + range) as i64;
    assert_eq!(report["net_saved_bytes"], saved);
}

#[test]
fn find_tool_narrows_by_path_and_glob_and_pages_with_a_cursor() {
    let fixture = mixed_fixture();
    let root = fixture.path();
    for i in 0..4 {
        fs::write(
            root.join(format!("db/extra_{i}.sql")),
            "-- purchase_orders\n",
        )
        .unwrap();
    }
    let session = |calls: Value| {
        let mut lines = vec![
            legacy_initialize(),
            request("list", "tools/list", json!({})),
        ];
        for row in calls.as_array().unwrap() {
            lines.push(call(row[0].clone(), "find", row[1].clone()));
        }
        exchange(root, &lines)
    };
    // [id, find arguments]
    let responses = session(json!([
        ["scoped", {"text": "purchase_orders", "path": ["db", "backend"], "glob": ["!extra_*"]}],
        ["one", {"text": "purchase_orders", "path": "db", "limit": 2}],
        ["missing", {"text": "purchase_orders", "path": ["nowhere"]}],
        ["outside", {"text": "purchase_orders", "path": ["../elsewhere"]}],
        ["not-list", {"text": "purchase_orders", "glob": 3}]
    ]));
    let tools = by_id(&responses, "list")["result"]["tools"]
        .as_array()
        .unwrap();
    let find = find_tool(tools, "find");
    let properties = &find["inputSchema"]["properties"];
    assert_eq!(properties["path"]["type"], "array", "{find:#}");
    assert_eq!(properties["glob"]["items"]["type"], "string", "{find:#}");
    assert_eq!(properties["after"]["type"], "string", "{find:#}");
    assert_eq!(find["inputSchema"]["required"], json!(["text"]));

    let scoped = succeeded(by_id(&responses, "scoped"));
    starts(&scoped, "[scope: db/, backend/ · glob !extra_* · 3 of ");
    has(&scoped, "backend/orders.py:8: ");
    has(&scoped, "db/schema.sql:1: ");
    assert!(!scoped.contains("db/extra_"), "{scoped}");
    let one = succeeded(by_id(&responses, "one"));
    has(
        &one,
        "[--limit 2 reached after 2 of 5 matches in 5 files]\n",
    );
    let cursor = one
        .split_whitespace()
        .find_map(|word| word.strip_prefix("--after="))
        .unwrap()
        .to_owned();
    has(
        &failed(by_id(&responses, "missing")),
        "--path nowhere: no such file",
    );
    has(
        &failed(by_id(&responses, "outside")),
        "`path` must be a workspace-relative",
    );
    has(&failed(by_id(&responses, "not-list")), "`glob` must be");

    let next = json!({"text": "purchase_orders", "path": ["db"], "limit": 2, "after": cursor});
    let responses = session(json!([["next", next]]));
    let next = succeeded(by_id(&responses, "next"));
    starts(&next, "[scope: db/ · 5 of ");
    assert_eq!(next.matches(".sql:1: ").count(), 2, "{next}");
    has(
        &next,
        "[--limit 2 reached after 4 of 5 matches in 5 files]\n",
    );
}

#[test]
fn mcp_install_agy_updates_global_mcp_config_and_doctor_checks_it() {
    let repo = mixed_fixture();
    let root = repo.path();
    let agy_dir = TempDir::new().unwrap();
    let config = agy_dir.path().join("mcp_config.json");
    fs::write(
        &config,
        r#"{"mcpServers":{"other":{"command":"other-mcp"}}}"#,
    )
    .unwrap();

    let run = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .current_dir(root)
            .env("JUSTLESS_TEST_AGY_CONFIG_DIR", agy_dir.path())
            .output()
            .unwrap()
    };
    let out = run(&["mcp", "--install", "agy"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    has(&text(&out.stdout), "Antigravity CLI: added justless to ");
    assert!(!root.join(".justless").exists());

    let installed: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(installed["mcpServers"]["other"]["command"], "other-mcp");
    assert_eq!(
        installed["mcpServers"]["justless"],
        json!({"command": "justless", "args": ["mcp"]})
    );

    let again = run(&["mcp", "--install", "agy"]);
    assert!(again.status.success(), "{}", text(&again.stderr));
    has(&text(&again.stdout), "already lists justless");

    let doctor = run(&["doctor"]);
    has(&text(&doctor.stdout), "Antigravity CLI:");
    has(&text(&doctor.stdout), "mcp_config.json");
}
