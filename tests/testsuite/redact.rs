//! Secret redaction across every view of a captured run. Sample secrets are built at
//! runtime from split prefixes and hash-derived bodies, so no scanner-flagged literal
//! lives in the repository.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::common::{BIN, has, lacks, mcp_session, piped, run_id, starts, text, words};

fn base62(seed: &str, length: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut output = String::new();
    let mut block = Sha256::digest(seed.as_bytes());
    while output.len() < length {
        for byte in block {
            if output.len() < length {
                output.push(ALPHABET[usize::from(byte) % 62] as char);
            }
        }
        block = Sha256::digest(block);
    }
    output
}

fn github(seed: &str) -> String {
    format!("{}{}{}", "gh", "p_", base62(seed, 36))
}

/// A value the generic `secret` rule redacts after a keyword.
fn value(seed: &str) -> String {
    format!("{}7q", base62(seed, 22))
}

/// No 8-byte piece of `secret` appears in `haystack`.
fn hidden(haystack: &str, secret: &str) -> bool {
    secret
        .as_bytes()
        .windows(8)
        .all(|piece| !haystack.contains(std::str::from_utf8(piece).unwrap()))
}

struct Project {
    dir: TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".justless.toml"), "").unwrap();
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(self.root())
            .env("CARGO_TARGET_DIR", self.root().join("target"))
            .env_remove("JUSTLESS_SESSION");
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        output
    }

    /// The stdout of `line`, split at spaces into arguments.
    fn stdout(&self, line: &str) -> String {
        text(&self.ok(&words(line)).stdout)
    }

    fn config(&self, settings: &str) {
        fs::write(self.root().join(".justless.toml"), settings).unwrap();
    }

    /// An executable named like a real tool, so Justless picks that tool's report.
    fn tool(&self, name: &str, body: &str) -> String {
        let bin = self.root().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join(name);
        crate::common::executable(&path, &format!("#!/bin/sh\n{body}"));
        path.to_string_lossy().into_owned()
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.root().join(".justless/runs").join(id)
    }

    fn record(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.run_dir(id).join("record.json")).unwrap()).unwrap()
    }

    fn archive(&self, id: &str, stream: &str) -> Vec<u8> {
        fs::read(self.run_dir(id).join(stream)).unwrap()
    }

    fn latest_run(&self) -> String {
        let mut runs: Vec<_> = fs::read_dir(self.root().join(".justless/runs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.join("record.json").exists())
            .map(|path| {
                let modified = fs::metadata(path.join("record.json")).unwrap().modified();
                (modified.unwrap(), path.file_name().unwrap().to_owned())
            })
            .collect();
        runs.sort();
        runs.pop().unwrap().1.to_string_lossy().into_owned()
    }
}

#[test]
fn run_previews_redact_both_streams_and_keep_the_archive() {
    let project = Project::new();
    let token = github("run");
    let password = base62("run-password", 12);
    let scheme = "postgres";
    let script = format!(
        "echo GITHUB_TOKEN={token}; echo \"db={scheme}://app:{password}@db/app\" >&2; exit 3"
    );
    let output = project.cli(&["run", "--", "sh", "-c", &script]);
    assert_eq!(output.status.code(), Some(3));
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    starts(&stdout, "GITHUB_TOKEN=[redacted:github-token:");
    has(
        &stderr,
        &format!("db={scheme}://app:[redacted:url-password:"),
    );
    assert!(
        hidden(&stdout, &token) && hidden(&stderr, &password),
        "{stderr}"
    );
    has(&stderr, " · 2 secrets redacted · justless log run-");

    let id = run_id(&output);
    let record = fs::read_to_string(project.run_dir(&id).join("record.json")).unwrap();
    assert!(hidden(&record, &token) && hidden(&record, &password));
    assert_eq!(project.record(&id)["redaction"]["count"], 2);
    assert_eq!(
        project.archive(&id, "stdout"),
        format!("GITHUB_TOKEN={token}\n").as_bytes()
    );
}

#[test]
fn hook_footers_count_secrets_only_when_output_was_shown() {
    let project = Project::new();
    let secret = value("hook");
    let short = format!("echo token={secret}");
    let output = project.cli(&["run", "--hook", "--", "sh", "-c", &short]);
    let id = run_id(&output);
    let footer = format!("[justless {id} · 1 secret redacted]\n");
    assert_eq!(text(&output.stderr), footer);
    starts(&text(&output.stdout), "token=[redacted:secret:");

    // A rerun with output this short is shown again, so the count stays.
    let again = project.cli(&["run", "--hook", "--", "sh", "-c", &short]);
    starts(&text(&again.stdout), "token=[redacted:secret:");
    assert!(text(&again.stderr).ends_with("· 1 secret redacted]\n"));

    let failing = format!("echo token={secret}; exit 1");
    let output = project.cli(&["run", "--hook", "--", "sh", "-c", &failing]);
    let id = run_id(&output);
    assert_eq!(output.status.code(), Some(1));
    let footer = format!("[justless {id} · 1 secret redacted · exit 1]\n");
    assert_eq!(text(&output.stderr), footer);

    let long = format!("seq 1 400; echo token={secret}; exit 2");
    let args = [
        &words("run --hook --max-bytes 512 -- sh -c")[..],
        &[long.as_str()],
    ]
    .concat();
    let output = project.cli(&args);
    has(
        &text(&output.stderr),
        " shown · 1 secret redacted · exit 2]",
    );
    assert!(hidden(&text(&output.stdout), &secret));
    // The same output again is reported as unchanged: nothing was shown, so no count.
    let output = project.cli(&args);
    starts(&text(&output.stdout), "Output unchanged since run-");
    let footer = text(&output.stderr);
    assert!(footer.ends_with(" · exit 2]\n"), "{footer}");
    lacks(&footer, "redacted");

    let clean = project.cli(&["run", "--hook", "--", "echo", "hello"]);
    assert_eq!(text(&clean.stdout), "hello\n");
    assert_eq!(text(&clean.stderr), "");
}

#[test]
fn log_redacts_by_default_and_unredacted_replays_the_archive() {
    let project = Project::new();
    let secret = value("log");
    let bearer = value("log-bearer");
    let script = format!(
        "echo start; echo api_key={secret}; echo end; echo \"Authorization: Bearer {bearer}\" >&2"
    );
    let id = run_id(&project.ok(&["run", "--", "sh", "-c", &script]));
    let archived = project.archive(&id, "stdout");

    let replay = project.stdout(&format!("log {id} --stream stdout"));
    assert_eq!(replay.lines().count(), 3);
    starts(&replay, "start\napi_key=[redacted:secret:");
    assert!(replay.ends_with("]\nend\n"), "{replay}");
    let line = project.stdout(&format!("log {id} --stream stdout --from 2 --to 2"));
    starts(&line, "api_key=[redacted:secret:");
    assert_eq!(line.lines().count(), 1);
    let stderr = text(&project.ok(&["log", &id, "--stream", "stderr"]).stderr);
    starts(&stderr, "Authorization: Bearer [redacted:authorization:");
    assert!(hidden(&stderr, &bearer));

    let exact = project.ok(&["log", &id, "--stream", "stdout", "--unredacted"]);
    assert_eq!(exact.stdout, archived);
    let exact = project.ok(&["log", &id, "--unredacted", "--stream", "stderr"]);
    assert_eq!(exact.stderr, project.archive(&id, "stderr"));

    let stats: Value = serde_json::from_str(&project.stdout("stats --json")).unwrap();
    assert_eq!(stats["redacted_secrets"], 2);
    assert_eq!(stats["unredacted_replays"], 2);
    let family = &stats["commands"][0];
    assert_eq!(family["reopened_runs"], 1);
    assert_eq!(family["full_reads"], 4);
    let report = project.stdout("stats");
    has(&report, "Redacted 2 secret(s) in captured output");
    has(&report, "· 2 unredacted replay(s)");
}

#[test]
fn clean_runs_replay_byte_for_byte() {
    let project = Project::new();
    let script = r"printf 'token count: 5\r\n\033[31mpassword policy\033[0m ok\n\377\376 bytes\nsk- no newline'";
    let output = project.ok(&["run", "--", "sh", "-c", script]);
    let id = run_id(&output);
    assert_eq!(project.record(&id)["redaction"]["count"], 0);
    let archived = project.archive(&id, "stdout");
    assert_eq!(output.stdout, archived);
    let log = || project.ok(&["log", &id, "--stream", "stdout"]).stdout;
    assert_eq!(log(), archived);
    // A different allowlist rescans the archive, and it is still clean.
    project.config("redact_allow = ['^never$']\n");
    assert_eq!(log(), archived);
    let search = project.stdout(&format!("search {id} policy"));
    has(&search, "stdout:2: password policy ok");
}

#[test]
fn search_history_and_lookup_match_redacted_text() {
    let project = Project::new();
    let token = github("search");
    let script = format!("echo first; echo TOKEN={token}");
    let id = run_id(&project.ok(&["run", "--", "sh", "-c", &script]));

    assert_eq!(project.stdout(&format!("search {id} {token}")), "");
    let found = project.stdout(&format!("search {id} redacted:github-token"));
    starts(&found, "stdout:2: TOKEN=[redacted:github-token:");
    let found = project.stdout(&format!("search {id} {token} --unredacted"));
    assert_eq!(found, format!("stdout:2: TOKEN={token}\n"));

    has(
        &project.stdout(&format!("history {token}")),
        "No matching captured runs.",
    );
    let history = project.stdout("history redacted:github-token");
    has(&history, &format!("{id} other stdout:2:"));
    assert!(
        !project
            .cli(&["history", &token, "--unredacted"])
            .status
            .success()
    );

    // `lookup --logs` ranks and quotes captured output, so its log results are
    // redacted too.
    let secret = value("lookup");
    let script = format!("echo 'upload failed for bucket assets'; echo api_key={secret}");
    project.ok(&["run", "--", "sh", "-c", &script]);
    let shown = text(
        &project
            .ok(&["lookup", "upload failed api_key", "--logs"])
            .stdout,
    );
    has(&shown, "[redacted:secret:");
    assert!(hidden(&shown, &secret), "{shown}");
}

#[test]
fn clipped_previews_never_show_part_of_a_secret() {
    let project = Project::new();
    let secrets: Vec<String> = (0..5).map(|i| value(&format!("clip-{i}"))).collect();
    // The first line puts its secret across the head excerpt's limit; the others
    // land in the failure, note and tail sections.
    let script = format!(
        "printf '%0140d token={}\\n' 0; seq 1 200; echo 'error: login failed with password={}';\
         seq 201 300; echo 'warning: api_key={} is deprecated'; seq 301 600;\
         printf '%0300d secret={}\\n' 0; echo 'done token={}'",
        secrets[0], secrets[1], secrets[2], secrets[3], secrets[4]
    );
    let output = project.cli(&["run", "--max-bytes", "1024", "--", "sh", "-c", &script]);
    let shown = format!("{}{}", text(&output.stdout), text(&output.stderr));
    has(&shown, "[output clipped");
    has(&shown, "[redacted:");
    for secret in &secrets {
        assert!(hidden(&shown, secret), "{secret} in {shown}");
    }
}

#[test]
fn lines_grown_by_markers_are_shown_whole() {
    let project = Project::new();
    let scheme = "redis";
    let mut line = String::new();
    let mut passwords = Vec::new();
    for i in 0..25 {
        let password = base62(&format!("grow-{i}"), 4);
        line.push_str(&format!("{scheme}://u:{password}@h "));
        passwords.push(password);
    }
    line.push_str("END");
    assert!(line.len() < 500);
    let output = project.ok(&["run", "--max-bytes", "512", "--", "echo", &line]);
    let stdout = text(&output.stdout);
    assert!(stdout.ends_with("@h END\n"), "{stdout}");
    lacks(&stdout, "[output clipped");
    assert_eq!(stdout.matches("[redacted:url-password:").count(), 25);
    for password in passwords {
        lacks(&stdout, &format!(":{password}@"));
    }
}

#[test]
fn test_search_and_lint_reports_quote_redacted_lines() {
    let project = Project::new();
    let secret = value("report");
    let cargo = project.tool(
        "cargo",
        &format!(
            "cat <<'EOF'\nrunning 1 test\ntest auth::uses_token ... FAILED\n\nfailures:\n\n\
             ---- auth::uses_token stdout ----\n\n\
             thread 'auth::uses_token' panicked at src/lib.rs:3:5:\n\
             assertion `left == right` failed\n  left: \"token={secret}\"\n right: \"x\"\n\n\
             failures:\n    auth::uses_token\n\n\
             test result: FAILED. 0 passed; 1 failed; 0 ignored\nEOF\nexit 101\n"
        ),
    );
    let output = project.cli(&["run", "--", &cargo, "test"]);
    let id = run_id(&output);
    assert_eq!(output.status.code(), Some(101));
    let shown = format!("{}{}", text(&output.stdout), text(&output.stderr));
    has(&shown, "auth::uses_token");
    assert!(hidden(&shown, &secret), "{shown}");
    let record = fs::read_to_string(project.run_dir(&id).join("record.json")).unwrap();
    assert!(record.contains("[redacted:secret:") && hidden(&record, &secret));
    assert!(hidden(&project.stdout("resume"), &secret));

    let matches: String = (0..300)
        .map(|i| {
            let secret = value(&format!("rg-{i}"));
            format!(
                "src/module{}.rs:{}:    let token = \"{secret}\";\n",
                i % 7,
                i + 1
            )
        })
        .collect();
    fs::write(project.root().join("matches.txt"), &matches).unwrap();
    let rg = project.tool("rg", "cat matches.txt\n");
    let shown = text(&project.ok(&["run", "--", &rg, "-n", "token", "src"]).stdout);
    has(&shown, "src/module0.rs");
    has(&shown, "[redacted:secret:");
    for i in 0..300 {
        assert!(hidden(&shown, &value(&format!("rg-{i}"))));
    }

    let mut findings: String = (0..40)
        .map(|i| {
            let secret = value(&format!("lint-{i}"));
            format!(
                "app/settings.py:{}:1: S105 Possible hardcoded password assigned to: \"password={secret}\"\n",
                i + 1
            )
        })
        .collect();
    findings.push_str("Found 40 errors.\n");
    fs::write(project.root().join("findings.txt"), &findings).unwrap();
    let ruff = project.tool("ruff", "cat findings.txt\nexit 1\n");
    let output = project.cli(&["run", "--", &ruff, "check", "app"]);
    let shown = format!("{}{}", text(&output.stdout), text(&output.stderr));
    has(&shown, "S105");
    for i in 0..40 {
        assert!(hidden(&shown, &value(&format!("lint-{i}"))), "{shown}");
    }
}

#[test]
fn records_from_before_redaction_are_redacted_when_shown() {
    let project = Project::new();
    let token = github("old-record");
    let pytest = project.tool(
        "pytest",
        &format!(
            "if [ -f second ]; then echo 'FAILED tests/test_api.py::test_other - assert 1 == 2'; \
             else echo 'FAILED tests/test_api.py::test_auth[{token}] - assert 401 == 200'; fi\n\
             echo '1 failed in 0.10s'\nexit 1\n"
        ),
    );
    project.config("redact = false\n");
    let output = project.cli(&["run", "--", &pytest]);
    let id = run_id(&output);
    has(&text(&output.stdout), &token);
    lacks(&text(&output.stderr), "redacted");
    assert!(project.record(&id).get("redaction").is_none());

    project.config("");
    let resume = project.stdout("resume");
    has(
        &resume,
        "first tests/test_api.py::test_auth[[redacted:github-token:",
    );
    assert!(hidden(&resume, &token), "{resume}");
    let log = project.stdout(&format!("log {id}"));
    assert!(hidden(&log, &token) && log.contains("[redacted:github-token:"));
    let history = project.stdout("history test_auth");
    assert!(hidden(&history, &token), "{history}");

    // The rerun compares with the old record and names the failure it no longer sees.
    fs::write(project.root().join("second"), "").unwrap();
    let output = project.cli(&["run", "--", &pytest]);
    let shown = format!("{}{}", text(&output.stdout), text(&output.stderr));
    has(&shown, "test_auth[[redacted:github-token:");
    assert!(hidden(&shown, &token), "{shown}");
}

#[test]
fn configuration_turns_redaction_off_allows_values_and_rejects_bad_patterns() {
    let project = Project::new();
    let token = github("config");
    let script = format!("echo TOKEN={token}");

    project.config("redact = false\n");
    let output = project.ok(&["run", "--", "sh", "-c", &script]);
    let earlier = run_id(&output);
    assert_eq!(text(&output.stdout), format!("TOKEN={token}\n"));
    lacks(&text(&output.stderr), "redacted");

    project.config(&format!("redact_allow = ['^{}']\n", &token[..12]));
    let output = project.ok(&["run", "--fresh", "--", "sh", "-c", &script]);
    assert_eq!(text(&output.stdout), format!("TOKEN={token}\n"));
    assert_eq!(project.record(&run_id(&output))["redaction"]["count"], 0);

    project.config("");
    let output = project.ok(&["run", "--fresh", "--", "sh", "-c", &script]);
    let redacted_run = run_id(&output);

    for (settings, message) in [
        (
            "redact_allow = ['[']\n",
            "redact_allow: invalid pattern '['",
        ),
        (
            "redact_allow = ['x*']\n",
            "redact_allow: pattern 'x*' matches empty text",
        ),
    ] {
        project.config(settings);
        let output = project.cli(&["run", "--", "sh", "-c", "touch ran"]);
        assert_eq!(output.status.code(), Some(2));
        has(&text(&output.stderr), message);
        assert!(!project.root().join("ran").exists());

        // Views of earlier runs fall back to the built-in rules.
        for id in [&earlier, &redacted_run] {
            let log = project.ok(&["log", id, "--stream", "stdout"]);
            starts(&text(&log.stdout), "TOKEN=[redacted:github-token:");
            has(&text(&log.stderr), "redacting with built-in rules only");
        }
        let exact = project.stdout(&format!("log {earlier} --stream stdout --unredacted"));
        assert_eq!(exact, format!("TOKEN={token}\n"));
        has(&project.stdout("resume"), &redacted_run);
    }

    project.config("redact = [unclosed\n");
    let log = project.ok(&["log", &earlier, "--stream", "stdout"]);
    starts(&text(&log.stdout), "TOKEN=[redacted:github-token:");
    has(&text(&log.stderr), "redacting with built-in rules only");
    let found = project.stdout(&format!("search {earlier} TOKEN"));
    has(&found, "[redacted:github-token:");
    // Without --budget, resume reads its default budget from the same file.
    has(&project.stdout("resume --budget 2400"), &earlier);
    assert_eq!(project.cli(&["run", "--", "true"]).status.code(), Some(2));
}

#[test]
fn marker_key_is_created_on_first_detection() {
    let project = Project::new();
    let key = project.root().join(".justless/redact.key");
    project.ok(&["run", "--", "echo", "token count: 5"]);
    assert!(!key.exists());

    let script = format!("echo api_key={}", value("key"));
    let first = text(&project.ok(&["run", "--", "sh", "-c", &script]).stdout);
    assert_eq!(fs::read(&key).unwrap().len(), 32);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let rerun = ["run", "--fresh", "--", "sh", "-c", &script];
    assert_eq!(first, text(&project.ok(&rerun).stdout));
    assert!(first.starts_with("api_key=[redacted:secret:") && first.len() == 31);

    fs::remove_file(&key).unwrap();
    fs::create_dir(&key).unwrap();
    let output = project.ok(&rerun);
    assert_eq!(text(&output.stdout), "api_key=[redacted:secret]\n");
    has(&text(&output.stderr), "markers omit their hash");
}

#[test]
fn mcp_results_are_redacted_and_cannot_be_unredacted() {
    let project = Project::new();
    let token = github("mcp");
    let script = format!("echo TOKEN={token}");
    let id = run_id(&project.ok(&["run", "--", "sh", "-c", &script]));
    let call = |key: &str, name: &str, arguments: Value| {
        json!({"jsonrpc": "2.0", "id": key, "method": "tools/call",
               "params": {"name": name, "arguments": arguments}})
        .to_string()
    };
    let lines = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}}})
        .to_string(),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        json!({"jsonrpc": "2.0", "id": "list", "method": "tools/list", "params": {}}).to_string(),
        call("log", "log", json!({"run_id": id})),
        call(
            "search",
            "search",
            json!({"run_id": id, "text": "redacted:github"}),
        ),
        call("value", "search", json!({"run_id": id, "text": token})),
        call("history", "history", json!({"text": "TOKEN="})),
        call("raw", "log", json!({"run_id": id, "unredacted": true})),
    ];
    let mut server = Command::new(BIN);
    server.args(["mcp", "--root"]).arg(project.root());
    let (_, responses) = mcp_session(server.current_dir(project.root()), &lines);
    let by_id = |key: &str| {
        let found = responses.iter().find(|response| response["id"] == key);
        found.unwrap()
    };
    let content = |key: &str| {
        let response = by_id(key);
        assert_eq!(response["result"]["isError"], false, "{response:#}");
        let text = &response["result"]["content"][0]["text"];
        text.as_str().unwrap().to_owned()
    };
    starts(&content("log"), "TOKEN=[redacted:github-token:");
    has(
        &content("search"),
        "stdout:1: TOKEN=[redacted:github-token:",
    );
    lacks(&content("value"), "TOKEN");
    assert!(hidden(&content("history"), &token));
    let raw = by_id("raw");
    assert!(raw.get("error").is_some() || raw["result"]["isError"] == true);
    assert!(hidden(&raw.to_string(), &token));

    let tools = by_id("list")["result"]["tools"].as_array().unwrap();
    for tool in tools {
        let properties = &tool["inputSchema"]["properties"];
        assert!(properties.get("unredacted").is_none(), "{tool:#}");
    }
    let log = tools.iter().find(|tool| tool["name"] == "log").unwrap();
    has(log["description"].as_str().unwrap(), "secrets redacted");
}

#[test]
fn source_views_and_recall_stay_exact() {
    let project = Project::new();
    let secret = value("source");
    let source = format!("API_KEY = \"{secret}\"\nDEBUG = True\n");
    fs::write(project.root().join("settings.py"), &source).unwrap();

    assert_eq!(project.stdout("read settings.py"), source);
    has(&project.stdout("code settings.py --to 2"), &secret);
    let outline: Value = serde_json::from_str(&project.stdout("read settings.py --json")).unwrap();
    let revision = outline["revision"].as_str().unwrap();
    let line = project.stdout(&format!("expand settings.py L1 --revision {revision}"));
    assert_eq!(line, format!("API_KEY = \"{secret}\"\n"));

    // An edit built from the expanded bytes round-trips exactly.
    let replacement = line.replace("API_KEY", "SERVICE_KEY");
    let mut edit = project.command(&["edit", "settings.py", "L1", "--revision", revision]);
    let edited = piped(&mut edit, replacement.as_bytes());
    assert!(edited.status.success(), "{}", text(&edited.stderr));
    assert_eq!(
        fs::read_to_string(project.root().join("settings.py")).unwrap(),
        source.replace("API_KEY", "SERVICE_KEY")
    );

    // An archived tool result comes back exactly as the client sent it.
    let result = format!("cat .env\nTOKEN={}\n", github("recall")).repeat(80);
    let request = json!({"messages": [
        {"role": "assistant", "content": [{"type": "tool_use", "id": "old", "name": "bash", "input": {}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "old", "content": result}]},
    ]});
    let mut mask = project.command(&["mask", "--keep", "0"]);
    let masked = piped(&mut mask, request.to_string().as_bytes());
    let masked: Value = serde_json::from_slice(&masked.stdout).unwrap();
    let stub = masked["messages"][1]["content"][0]["content"]
        .as_str()
        .unwrap();
    let id = stub
        .strip_prefix("[justless recall ")
        .and_then(|s| s.strip_suffix(']'))
        .unwrap();
    let recalled = project.stdout(&format!("recall {id} --json"));
    assert_eq!(
        serde_json::from_str::<Value>(&recalled).unwrap(),
        json!(result)
    );
}

#[test]
fn real_build_and_source_output_have_nothing_to_redact() {
    let project = Project::new();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/failing");
    for file in [
        "Cargo.toml",
        "src/lib.rs",
        "src/pricing.rs",
        "src/obsolete.rs",
        "tests/checkout.rs",
    ] {
        let path = project.root().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(fixture.join(file), path).unwrap();
    }
    let mut commands = vec![
        vec!["cargo", "build", "-v"],
        vec!["cargo", "test"],
        vec!["cargo", "build", "--message-format", "json"],
    ];
    let clippy = Command::new("cargo")
        .args(["clippy", "--version"])
        .output()
        .is_ok_and(|output| output.status.success());
    if clippy {
        commands.push(vec!["cargo", "clippy", "--all-targets"]);
    }
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<PathBuf> = ["Cargo.lock", "README.md", "docs/reference.md"]
        .iter()
        .map(|file| checkout.join(file))
        .collect();
    for entry in fs::read_dir(checkout.join("src")).unwrap() {
        files.push(entry.unwrap().path());
    }
    for file in &files {
        commands.push(vec!["cat", file.to_str().unwrap()]);
    }
    for command in commands {
        let args = [&["run", "--fresh", "--"][..], &command[..]].concat();
        project.config("");
        let output = project.cli(&args);
        let id = project.latest_run();
        assert_eq!(project.record(&id)["redaction"]["count"], 0, "{command:?}");
        lacks(&text(&output.stderr), "redacted · justless log");
        if command[0] == "cat" {
            // Same preview as with redaction off.
            project.config("redact = false\n");
            assert_eq!(project.cli(&args).stdout, output.stdout, "{command:?}");
        }
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let value = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                output.push(ALPHABET[(value >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                output.push('=');
            }
        }
    }
    output
}

/// A private key file whose body is wrapped at `width` columns, and its body.
fn private_key(seed: &str, width: usize) -> (String, String) {
    let body = base64(base62(seed, 1200).as_bytes());
    let mut file = format!("{}BEGIN RSA PRIVATE KEY-----\n", "-----");
    for line in body.as_bytes().chunks(width) {
        file.push_str(std::str::from_utf8(line).unwrap());
        file.push('\n');
    }
    file.push_str(&format!("{}END RSA PRIVATE KEY-----\n", "-----"));
    (file, body)
}

/// No 40-character piece of `body` appears in `haystack`.
fn body_hidden(haystack: &str, body: &str) -> bool {
    body.as_bytes()
        .chunks(40)
        .filter(|piece| piece.len() == 40)
        .all(|piece| !haystack.contains(std::str::from_utf8(piece).unwrap()))
}

fn key_marker(shown: &str) -> &str {
    let start = shown.find("[redacted:private-key:").expect("a key marker");
    &shown[start..start + shown[start..].find(']').unwrap() + 1]
}

#[test]
fn private_keys_keep_one_marker_whatever_prefixes_their_lines() {
    let project = Project::new();
    let (key, body) = private_key("prefixed", 64);
    fs::write(project.root().join("id_rsa"), &key).unwrap();
    let colored: String = key
        .lines()
        .map(|line| format!("\x1b[32m+\x1b[m\x1b[32m{line}\x1b[m\n"))
        .collect();
    fs::write(project.root().join("colored.diff"), colored).unwrap();
    let expected = key_marker(&project.stdout("run -- cat id_rsa")).to_owned();
    let commands: [&[&str]; 8] = [
        &["grep", "-n", "-A40", "BEGIN", "id_rsa"],
        &["grep", "-H", "-n", "", "id_rsa"],
        &["grep", "-r", "-n", "--include=id_rsa", "", "."],
        &["cat", "-n", "id_rsa"],
        &["diff", "-u", "id_rsa", "/dev/null"],
        &["sed", "s/^/+/", "id_rsa"],
        &["awk", r#"{print "2026-11-18T10:00:00.5Z " $0}"#, "id_rsa"],
        &["cat", "colored.diff"],
    ];
    for command in commands {
        let args = [&words("run --fresh --max-bytes 65536 --")[..], command].concat();
        let shown = text(&project.cli(&args).stdout);
        let id = project.latest_run();
        let record = project.record(&id);
        assert_eq!(record["redaction"]["count"], 1, "{command:?}: {shown}");
        assert!(body_hidden(&shown, &body), "{command:?}: {shown}");
        // The prefix stays on each line and is not part of the hashed key.
        assert_eq!(key_marker(&shown), expected, "{command:?}: {shown}");
        let archive = text(&project.archive(&id, "stdout"));
        assert_eq!(shown.lines().count(), archive.lines().count());
        let log = project.stdout(&format!("log {id}"));
        assert!(body_hidden(&log, &body), "{command:?}: {log}");
    }
}

#[test]
fn unwrapped_wide_and_base64_wrapped_keys_are_hidden() {
    let project = Project::new();
    let (wide, body) = private_key("wide", 132);
    let (unwrapped, _) = private_key("wide", 4096);
    assert_eq!(unwrapped.lines().count(), 3);
    let (key, _) = private_key("encoded", 64);
    let encoded = base64(key.as_bytes());
    let wrap = |width: usize| -> String {
        let lines = encoded.as_bytes().chunks(width);
        let text: String = lines
            .map(|line| format!("{}\n", std::str::from_utf8(line).unwrap()))
            .collect();
        text + "done\n"
    };
    for (name, content) in [
        ("wide.pem", wide),
        ("unwrapped.pem", unwrapped),
        ("key64.b64", wrap(64)),
        ("key76.b64", wrap(76)),
    ] {
        fs::write(project.root().join(name), &content).unwrap();
        let output = project.cli(&["run", "--max-bytes", "65536", "--", "cat", name]);
        let shown = text(&output.stdout);
        let record = project.record(&project.latest_run());
        assert_eq!(record["redaction"]["count"], 1, "{name}: {shown}");
        assert_eq!(shown.lines().count(), content.lines().count(), "{name}");
        if name.ends_with(".b64") {
            assert!(body_hidden(&shown, &encoded), "{name}: {shown}");
            assert!(shown.ends_with("\ndone\n"), "{name}: {shown}");
        } else {
            assert!(body_hidden(&shown, &body), "{name}: {shown}");
        }
    }
    // The wrapping width does not change the marker.
    let marker = |name: &str| key_marker(&project.stdout(&format!("run -- cat {name}"))).to_owned();
    assert_eq!(marker("key64.b64"), marker("key76.b64"));
}

#[test]
fn values_keep_their_symbols_while_paths_digests_and_code_stay_visible() {
    let project = Project::new();
    let symbols = format!("{}$Zt8@Wq3(n)Lm5?x", base62("symbols", 10));
    let django = format!("r8u&t3)x+5k@q!d9z(w2y#n7v$b4m%{}", base62("django", 6));
    let slash = format!("/x+{}", base62("slash", 37));
    let pass = format!("{}{}2024", "MyPass", "word");
    let phrase = base62("phrase", 32);
    let positives = [
        (format!("DB_PASSWORD={symbols}"), &symbols),
        (format!("password: \"{symbols}\""), &symbols),
        (format!("api_key='{symbols}'"), &symbols),
        (format!("DJANGO_SECRET_KEY={django}"), &django),
        (format!("SECRET_KEY = 'django-insecure-{django}'"), &django),
        (format!("AWS_SECRET_ACCESS_KEY={slash}"), &slash),
        (format!("DB_PASSWORD={pass}"), &pass),
        (format!("POLY_PASSPHRASE={phrase}"), &phrase),
        (format!("--passphrase {phrase}"), &phrase),
        // A search over secret files prints each file's name before its content.
        (format!("secrets/db_password:{phrase}"), &phrase),
        (format!("password.txt:{phrase}"), &phrase),
    ];
    let digest = "4f9c2b1a3d5e7f9a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a";
    let ci = format!(".{}hub/tokens/ci-v2.json", "git");
    let negatives = [
        "secrets_dir = config/secrets/prod-2024".to_owned(),
        format!("--token-file {ci}"),
        "password_file=deploy/k8s/db-password.v3.yaml".to_owned(),
        "TOKENIZER_PATH=models/llama-3.1-8b/tokenizer.model".to_owned(),
        "token_path = target/debug/build/token-abc123/out".to_owned(),
        "CREDENTIALS=/Users/Alice/MyProject/.secrets/creds-2024.json".to_owned(),
        format!("tokenizer.json: sha256={digest}"),
        format!(r#"{{"files":{{"src/to_tokens.rs":"{digest}","token.css":"{digest}"}}}}"#),
        format!("Copying from docker://ubuntu:22.04@sha256:{digest}"),
        "api_passphrase: Option<SecretString>,".to_owned(),
        r#"passphrase: """#.to_owned(),
        "    let token = hmac_sha256(key, data);".to_owned(),
        "    secret_key = hashlib.sha256(x2)".to_owned(),
        r#"    println!("token={token_value2}");"#.to_owned(),
        "    let enum_token = input.parse::<Token![enum]>()?;".to_owned(),
        "    let secret_key = buf.read_i32::<BigEndian>()?;".to_owned(),
        "    fn from_secret_key(secret_key: &secp256k1::SecretKey) -> Self {".to_owned(),
        "    let password = url.password().and_then(|pass| {".to_owned(),
        r#"  "token": "0xA604060890923Ff400e8c6f5290461A83AEDACec""#.to_owned(),
        "    let der_private_key: EcPrivateKeyRfc5915Der = key.as_der()?;".to_owned(),
        // Search output names a file before each line.
        "quinn/src/token.rs:107:pub(crate) struct IncomingToken {".to_owned(),
        "crypto/err/trust_token.errordata:1:TRUST_TOKEN,111,BAD_VALIDITY_CHECK".to_owned(),
        // JavaScript.
        r"    const tokens = path.match(/(?::[^\/]+)|(?:\/\*$)|./g) || [];".to_owned(),
        "      token: `${config.baseUrl}/realms/${config.realm}/protocol/token`,".to_owned(),
        r#"const TOKEN_STRINGS = "[A-Za-z0-9._~+/-]+=*";"#.to_owned(),
    ];
    let mut probe = String::new();
    for line in positives.iter().map(|(line, _)| line).chain(&negatives) {
        probe.push_str(line);
        probe.push('\n');
    }
    fs::write(project.root().join("probe.txt"), &probe).unwrap();
    let output = project.ok(&words("run --max-bytes 65536 -- cat probe.txt"));
    let shown = text(&output.stdout);
    let lines: Vec<&str> = shown.lines().collect();
    for (index, (line, value)) in positives.iter().enumerate() {
        let redacted = lines[index];
        assert!(hidden(redacted, value), "{line} -> {redacted}");
        // Nothing of the value is left after the marker.
        let tail = &redacted[redacted.rfind("[redacted:secret:").expect(line)..];
        assert!(
            tail.ends_with(']') || tail.ends_with("]\"") || tail.ends_with("]'"),
            "{line} -> {redacted}"
        );
    }
    for (index, line) in negatives.iter().enumerate() {
        assert_eq!(lines[positives.len() + index], line);
    }
    let id = run_id(&output);
    assert_eq!(project.record(&id)["redaction"]["count"], positives.len());
}

#[test]
fn output_that_fits_once_redacted_is_shown_whole() {
    let project = Project::new();
    let (key, body) = private_key("fits", 64);
    let mut dump = String::new();
    for i in 0..30 {
        dump.push_str(&format!("manifest line {i} with some padding text here\n"));
    }
    dump.push_str(&key);
    for i in 0..30 {
        dump.push_str(&format!("after line {i} with some more padding text\n"));
    }
    assert!(dump.len() > 4096);
    fs::write(project.root().join("dump.txt"), &dump).unwrap();
    let output = project.ok(&["run", "--", "cat", "dump.txt"]);
    let shown = text(&output.stdout);
    lacks(&shown, "[output clipped");
    assert!(body_hidden(&shown, &body));
    let log = project.ok(&["log", &run_id(&output), "--stream", "stdout"]);
    assert_eq!(output.stdout, log.stdout);

    let output = project.ok(&[
        "run", "--hook", "--fresh", "--", "grep", "-n", "", "dump.txt",
    ]);
    let id = run_id(&output);
    let footer = format!("[justless {id} · 1 secret redacted]\n");
    assert_eq!(text(&output.stderr), footer);
    assert_eq!(text(&output.stdout).lines().count(), dump.lines().count());
}

/// `exec` shapes a long stdout from the archive itself, as a line outline or ranked
/// by `--intent`, and the shaped preview is redacted like any other.
#[test]
fn exec_previews_redact_outlined_and_ranked_stdout() {
    let project = Project::new();
    let secret = value("exec");
    let script = format!(
        "echo api_key={secret}; i=0; while [ $i -lt 400 ]; do echo \"row $i of the export\"; i=$((i + 1)); done; echo api_key={secret}"
    );
    for intent in [&[][..], &["--intent", "api_key"]] {
        let args = [&["exec", "--lang", "sh"], intent, &[script.as_str()]].concat();
        let output = project.cli(&args);
        assert!(output.status.success(), "{}", text(&output.stderr));
        let shown = text(&output.stdout);
        assert!(
            shown.contains("api_key=[redacted:secret:"),
            "{intent:?}: {shown}"
        );
        assert!(hidden(&shown, &secret), "{intent:?}: {shown}");
        let id = run_id(&output);
        let footer = text(&output.stderr);
        assert!(footer.contains("secrets redacted"), "{intent:?}: {footer}");
        let exact = project.ok(&["log", &id, "--stream", "stdout", "--unredacted"]);
        assert_eq!(exact.stdout, project.archive(&id, "stdout"));
        has(&text(&exact.stdout), &secret);
    }
}

/// When the record cannot be saved, Justless prints both streams in place of a
/// preview, and they are redacted like the preview would have been.
#[cfg(unix)]
#[test]
fn the_full_replay_after_a_failed_save_is_redacted() {
    let project = Project::new();
    let secret = value("unsaved");
    let script =
        format!("echo api_key={secret}; rm -rf .justless; echo \"token={secret}\" >&2; exit 3");
    let output = project.cli(&["run", "--hook", "--", "sh", "-c", &script]);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    let (shown, errors) = (text(&output.stdout), text(&output.stderr));
    has(&errors, "restoring full output");
    starts(&shown, "api_key=[redacted:secret");
    assert!(
        hidden(&shown, &secret) && hidden(&errors, &secret),
        "{shown}{errors}"
    );
}

/// A background process can add output to a run after its scan. Such a run is never
/// replayed as clean, so its late secrets are redacted too.
#[cfg(unix)]
#[test]
fn late_output_of_a_drained_run_is_redacted() {
    let project = Project::new();
    let secret = value("late");
    let script = format!("(sleep 0.3; echo api_key={secret}) & echo started");
    let output = project.ok(&["run", "--", "sh", "-c", &script]);
    let id = run_id(&output);
    let record = project.record(&id);
    assert_eq!(record["drained"], true, "{record}");
    assert_eq!(record["redaction"]["count"], 0, "{record}");
    let stream = project.run_dir(&id).join("stdout");
    let start = std::time::Instant::now();
    while !fs::read_to_string(&stream).is_ok_and(|text| text.contains(&secret)) {
        assert!(start.elapsed().as_secs() < 10, "the background write");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let log = project.stdout(&format!("log {id} --stream stdout"));
    starts(&log, "started\napi_key=[redacted:secret:");
    let search = project.stdout(&format!("search {id} api_key"));
    assert!(hidden(&search, &secret), "{search}");
    let exact = project.stdout(&format!("log {id} --stream stdout --unredacted"));
    has(&exact, &secret);
}

/// `exec` with `cat "$1"` over `data`, so the script prints exactly that file.
fn exec_cat(project: &Project, data: &str, extra: &[&str]) -> String {
    fs::write(project.root().join("data.txt"), data).unwrap();
    let args = [
        &words("exec --lang sh --file data.txt")[..],
        extra,
        &["cat \"$1\""],
    ]
    .concat();
    let output = project.cli(&args);
    assert!(output.status.success(), "{}", text(&output.stderr));
    text(&output.stdout)
}

/// The `exec` outline cuts a line too long for its half of the budget. It cuts the
/// redacted stream, so a cut never leaves part of a token that no longer matches.
#[test]
fn exec_outline_cuts_never_split_a_secret() {
    let project = Project::new();
    let token = github("outline-cut");
    let filler = "filler line of the export\n".repeat(200);
    let mut cuts = 0;
    for pad in (140..=260).step_by(3) {
        let (head, tail) = ("x".repeat(pad), "y".repeat(pad));
        let data = format!("{head} {token} end\n{filler}{token} {tail}\n");
        let shown = exec_cat(&project, &data, &["--max-bytes", "512"]);
        cuts += usize::from(shown.contains("not shown in full"));
        assert!(hidden(&shown, &token), "{pad}: {shown}");
    }
    assert!(cuts > 0, "no line was cut");
}

/// `--intent` shows windows of lines longer than 240 bytes. A window edge never
/// leaves part of a token.
#[test]
fn exec_intent_windows_never_split_a_secret() {
    let project = Project::new();
    let token = github("intent-window");
    let data: String = (0..60)
        .map(|gap| {
            let (a, b, c) = ("a".repeat(120), "b".repeat(gap * 5), "c".repeat(300));
            format!("{a} needle {b} {token} {c}\n")
        })
        .collect();
    let shown = exec_cat(&project, &data, &words("--intent needle --max-bytes 8000"));
    starts(&shown, "[focus]");
    assert!(hidden(&shown, &token), "{shown}");
}

/// A private key whose `BEGIN` line falls in the outline's head and whose `END` line
/// is left out still shows only its marker.
#[test]
fn exec_outline_keeps_a_split_private_key_hidden() {
    let project = Project::new();
    let (key, body) = private_key("outline-key", 64);
    let data = format!("{key}{}", "filler line of the export\n".repeat(300));
    let shown = exec_cat(&project, &data, &["--max-bytes", "1024"]);
    has(&shown, "left out");
    has(&shown, "[redacted:private-key");
    assert!(body_hidden(&shown, &body), "{shown}");
    for line in body.as_bytes().chunks(64).filter(|line| line.len() == 64) {
        lacks(&shown, std::str::from_utf8(line).unwrap());
    }
}
