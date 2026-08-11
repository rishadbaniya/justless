use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

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

/// Sends each line to `justless mcp`, closes stdin and returns the parsed
/// responses after checking that stdout holds only JSON-RPC messages.
fn exchange(root: &Path, lines: &[String]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(["mcp", "--root"])
        .arg(root)
        .current_dir(root)
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
    let stdout = text(&output.stdout);
    assert!(stdout.ends_with('\n') || stdout.is_empty(), "{stdout}");
    stdout
        .lines()
        .map(|line| {
            let message: Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("non-JSON stdout line {line:?}: {error}"));
            assert_eq!(message["jsonrpc"], "2.0", "{line}");
            assert!(
                message.get("result").is_some() != message.get("error").is_some(),
                "{line}"
            );
            message
        })
        .collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn request(id: impl Into<Value>, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id.into(), "method": method, "params": params}).to_string()
}

fn call(id: impl Into<Value>, name: &str, arguments: Value) -> String {
    request(
        id,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
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
    response["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            assert_eq!(item["type"], "text");
            item["text"].as_str().unwrap()
        })
        .collect()
}

fn succeeded(response: &Value) -> String {
    assert_eq!(response["result"]["isError"], false, "{response:#}");
    tool_text(response)
}

fn failed(response: &Value) -> String {
    assert_eq!(response["result"]["isError"], true, "{response:#}");
    tool_text(response)
}

#[test]
fn legacy_handshake_lists_and_calls_read_only_tools() {
    let fixture = mixed_fixture();
    let root = fixture.path();
    let lines = vec![
        request(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}),
        ),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        request(2, "ping", json!({})),
        request("list", "tools/list", json!({})),
        call(
            "query",
            "query",
            json!({"symbol": "purchase_orders", "view": "definition"}),
        ),
        call(
            "path",
            "path",
            json!({"from": "fetchOrders", "to": "purchase_orders"}),
        ),
        call(
            "read",
            "read",
            json!({"file": "frontend/orders.ts", "outline": true}),
        ),
        call(
            "expand",
            "expand",
            json!({"file": "frontend/orders.ts", "selector": "fetchOrders"}),
        ),
        call(
            "code",
            "code",
            json!({"file": "db/schema.sql", "from": 1, "to": 2}),
        ),
        call("context", "context", json!({"at": "backend/orders.py:7"})),
        call(
            "pack",
            "pack",
            json!({"question": "What breaks if I rename purchase_orders?", "budget": 800}),
        ),
        call(
            "find",
            "find",
            json!({"text": "purchase_orders", "limit": 5}),
        ),
        call("map", "map", json!({"budget": 1200})),
        call(
            "affected",
            "affected",
            json!({"target": "purchase_orders", "depth": 2}),
        ),
        call(
            "affected-both",
            "affected",
            json!({"target": "purchase_orders", "changes": true}),
        ),
        // Values that look like options stay positional.
        call("flag", "find", json!({"text": "--budget=1"})),
        call("stats", "stats", json!({})),
        call("missing", "query", json!({"symbol": "no_such_symbol"})),
        call("escape", "read", json!({"file": "../outside.txt"})),
        call("absolute", "code", json!({"file": "/etc/hosts", "to": 1})),
        call(
            "unknown-arg",
            "read",
            json!({"file": "db/schema.sql", "lines": 3}),
        ),
        call("range", "find", json!({"text": "x", "limit": 0})),
        call(
            "both",
            "context",
            json!({"at": "db/schema.sql:1", "changes": true}),
        ),
        call("run-id", "log", json!({"run_id": "../../etc"})),
        call("mutating", "run", json!({"command": ["true"]})),
        request(
            "arguments",
            "tools/call",
            json!({"name": "query", "arguments": "x"}),
        ),
        request(3, "resources/subscribe", json!({})),
        "{not json".into(),
        json!([{"jsonrpc": "2.0", "id": 9, "method": "ping"}]).to_string(),
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 2}})
            .to_string(),
    ];
    let responses = exchange(root, &lines);
    let requests = lines
        .iter()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .map(|value| !value.is_object() || value.get("id").is_some())
                .unwrap_or(true)
        })
        .count();
    assert_eq!(responses.len(), requests, "{responses:#?}");

    let initialize = &by_id(&responses, 1)["result"];
    assert_eq!(initialize["protocolVersion"], "2025-06-18");
    assert_eq!(initialize["serverInfo"]["name"], "justless");
    assert!(initialize["capabilities"]["tools"].is_object());
    assert!(
        initialize["instructions"]
            .as_str()
            .unwrap()
            .contains("pack")
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
            "affected", "code", "context", "expand", "find", "history", "log", "map", "pack",
            "path", "query", "read", "recall", "resume", "search", "stats",
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
    let query = tools.iter().find(|tool| tool["name"] == "query").unwrap();
    assert_eq!(query["inputSchema"]["required"], json!(["symbol"]));
    assert!(query["inputSchema"]["properties"]["budget"].is_object());
    let expand = tools.iter().find(|tool| tool["name"] == "expand").unwrap();
    assert!(expand["inputSchema"]["properties"].get("budget").is_none());

    assert!(
        succeeded(by_id(&responses, "query")).contains("db/schema.sql:1-3"),
        "{responses:#?}"
    );
    let route = succeeded(by_id(&responses, "path"));
    assert!(
        route.contains("HTTP route at frontend/orders.ts:4"),
        "{route}"
    );
    let outline = succeeded(by_id(&responses, "read"));
    assert!(outline.starts_with("revision "), "{outline}");
    assert!(outline.contains("fetchOrders"), "{outline}");
    assert!(succeeded(by_id(&responses, "expand")).contains("export async function fetchOrders()"));
    let code = succeeded(by_id(&responses, "code"));
    assert!(code.contains("1 |") && !code.contains("3 |"), "{code}");
    assert!(succeeded(by_id(&responses, "context")).contains("backend/orders.py"));
    assert!(succeeded(by_id(&responses, "pack")).contains("purchase_orders"));
    assert!(succeeded(by_id(&responses, "find")).contains("db/schema.sql:1"));
    let map = succeeded(by_id(&responses, "map"));
    assert!(
        map.contains("  backend/ → db/ · Python → SQL · possible SQL relation 1 · list_orders backend/orders.py:8 → purchase_orders db/schema.sql:1-3"),
        "{map}"
    );
    assert!(map.len() <= 1200, "{map}");
    let affected = succeeded(by_id(&responses, "affected"));
    assert!(
        affected.contains("  fetchOrders [function] frontend/orders.ts:3-7 · possible HTTP route at frontend/orders.ts:4 → list_orders"),
        "{affected}"
    );
    assert!(failed(by_id(&responses, "affected-both")).contains("exactly one of `target`"));
    assert!(succeeded(by_id(&responses, "flag")).contains("No matching indexed text"));
    succeeded(by_id(&responses, "stats"));

    assert!(failed(by_id(&responses, "missing")).contains("symbol not found"));
    assert!(failed(by_id(&responses, "escape")).contains("workspace-relative"));
    assert!(failed(by_id(&responses, "absolute")).contains("workspace-relative"));
    assert!(failed(by_id(&responses, "unknown-arg")).contains("unknown argument `lines`"));
    assert!(failed(by_id(&responses, "range")).contains("from 1 to 1000"));
    assert!(failed(by_id(&responses, "both")).contains("exactly one"));
    assert!(failed(by_id(&responses, "run-id")).contains("run ID"));

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
    let root = fixture.path();
    let lines = vec![
        request("early", "tools/list", json!({})),
        request(
            "discover",
            "server/discover",
            json!({"_meta": modern_meta()}),
        ),
        request("list", "tools/list", json!({"_meta": modern_meta()})),
        request(
            "call",
            "tools/call",
            json!({"name": "query", "arguments": {"symbol": "purchase_orders"}, "_meta": modern_meta()}),
        ),
        request(
            "bad-call",
            "tools/call",
            json!({"name": "code", "arguments": {"file": "db/missing.sql", "to": 2}, "_meta": modern_meta()}),
        ),
        request(
            "old",
            "tools/list",
            json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "1900-01-01", "io.modelcontextprotocol/clientCapabilities": {}}}),
        ),
        request(
            "no-capabilities",
            "tools/list",
            json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}),
        ),
        request("unknown", "prompts/list", json!({"_meta": modern_meta()})),
    ];
    let responses = exchange(root, &lines);
    assert_eq!(responses.len(), lines.len());

    let early = &by_id(&responses, "early")["error"];
    assert_eq!(early["code"], -32602);
    assert!(early["message"].as_str().unwrap().contains("2026-07-28"));

    let discover = &by_id(&responses, "discover")["result"];
    assert_eq!(discover["resultType"], "complete");
    assert_eq!(discover["supportedVersions"][0], "2026-07-28");
    assert!(discover["capabilities"]["tools"].is_object());
    assert!(discover["ttlMs"].is_u64() && discover["cacheScope"] == "public");
    assert_eq!(
        discover["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "justless"
    );

    let list = &by_id(&responses, "list")["result"];
    assert_eq!(list["resultType"], "complete");
    assert_eq!(list["tools"].as_array().unwrap().len(), 16);
    assert!(list["ttlMs"].is_u64());

    let call = by_id(&responses, "call");
    assert_eq!(call["result"]["resultType"], "complete");
    assert!(succeeded(call).contains("db/schema.sql"));
    failed(by_id(&responses, "bad-call"));

    let old = &by_id(&responses, "old")["error"];
    assert_eq!(old["code"], -32022);
    assert_eq!(old["data"]["requested"], "1900-01-01");
    assert!(
        old["data"]["supported"]
            .as_array()
            .unwrap()
            .contains(&json!("2025-11-25"))
    );
    assert_eq!(
        by_id(&responses, "no-capabilities")["error"]["code"],
        -32602
    );
    assert_eq!(by_id(&responses, "unknown")["error"]["code"], -32601);
}

#[test]
fn initialize_offers_the_newest_legacy_version_for_unknown_requests() {
    let fixture = mixed_fixture();
    let responses = exchange(
        fixture.path(),
        &[request(
            1,
            "initialize",
            json!({"protocolVersion": "2099-01-01", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
        )],
    );
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-11-25");
}

fn install(root: &Path, clients: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_justless"));
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
    for directory in [".cursor", ".gemini", ".vscode", ".codex"] {
        fs::create_dir(root.join(directory)).unwrap();
    }
    fs::write(
        root.join(".cursor/mcp.json"),
        r#"{"mcpServers":{"other":{"command":"other-server","args":["--port","1"]}},"extra":{"keep":true}}"#,
    )
    .unwrap();
    fs::write(
        root.join(".gemini/settings.json"),
        r#"{"theme":"Default","mcpServers":{"other":{"command":"x"}}}"#,
    )
    .unwrap();
    fs::write(
        root.join(".vscode/mcp.json"),
        r#"{"inputs":[{"id":"token","type":"promptString"}],"servers":{}}"#,
    )
    .unwrap();
    let codex = "# Project settings\nmodel = \"gpt-5\" # pinned\n\n[mcp_servers.other]\ncommand = \"other\"\nargs = [\"serve\"]\n";
    fs::write(root.join(".codex/config.toml"), codex).unwrap();

    let all = ["claude", "cursor", "gemini", "vscode", "codex"];
    let first = install(root, &all);
    assert!(first.status.success(), "{}", text(&first.stderr));
    assert_eq!(text(&first.stdout).matches("added justless").count(), 5);

    let claude = read_json(&root.join(".mcp.json"));
    assert_eq!(
        claude,
        json!({"mcpServers": {"justless": {"command": "justless", "args": ["mcp"]}}})
    );
    let cursor = read_json(&root.join(".cursor/mcp.json"));
    assert_eq!(cursor["extra"], json!({"keep": true}));
    assert_eq!(
        cursor["mcpServers"]["other"]["args"],
        json!(["--port", "1"])
    );
    assert_eq!(cursor["mcpServers"]["justless"]["type"], "stdio");
    assert_eq!(
        cursor["mcpServers"]["justless"]["args"],
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
    assert!(toml.starts_with(codex), "{toml}");
    assert!(
        toml.contains("[mcp_servers.justless]\ncommand = \"justless\"\nargs = [\"mcp\"]\n"),
        "{toml}"
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
    assert_eq!(
        text(&second.stdout)
            .matches("already lists justless")
            .count(),
        5
    );
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
    fs::write(root.join(".vscode/mcp.json"), commented).unwrap();
    let rejected = install(root, &["cursor", "vscode"]);
    assert!(!rejected.status.success());
    assert!(text(&rejected.stderr).contains(".vscode/mcp.json"));
    assert_eq!(
        fs::read_to_string(root.join(".vscode/mcp.json")).unwrap(),
        commented
    );
    assert!(!root.join(".cursor").exists());

    fs::write(root.join(".vscode/mcp.json"), r#"{"servers":[]}"#).unwrap();
    let wrong = install(root, &["vscode"]);
    assert!(!wrong.status.success());
    assert!(text(&wrong.stderr).contains("servers in .vscode/mcp.json must contain an object"));
}

#[test]
fn server_follows_claude_project_dir_without_an_explicit_root() {
    let repo = mixed_fixture();
    let elsewhere = TempDir::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
        .arg("mcp")
        .current_dir(elsewhere.path())
        .env("CLAUDE_PROJECT_DIR", repo.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for line in [
            request(
                1,
                "initialize",
                json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}),
            ),
            call(2, "query", json!({"symbol": "purchase_orders"})),
        ] {
            writeln!(stdin, "{line}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    let responses: Vec<Value> = text(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(succeeded(by_id(&responses, 2)).contains("db/schema.sql::purchase_orders [table]"));
    assert!(!elsewhere.path().join(".justless").exists());
}
