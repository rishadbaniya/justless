//! Secret redaction across every view of a captured run. Sample secrets are built at
//! runtime from split prefixes and hash-derived bodies, so no scanner-flagged literal
//! lives in the repository.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

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

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// No 8-byte piece of `secret` appears in `haystack`.
fn hidden(haystack: &str, secret: &str) -> bool {
    secret
        .as_bytes()
        .windows(8)
        .all(|piece| !haystack.contains(std::str::from_utf8(piece).unwrap()))
}

fn run_id(output: &Output) -> String {
    let stderr = text(&output.stderr);
    let start = stderr
        .rfind("[justless run-")
        .expect("footer names the run")
        + 10;
    stderr[start..].split([']', ' ']).next().unwrap().to_owned()
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

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_justless"))
            .args(args)
            .current_dir(self.root())
            .env("CARGO_TARGET_DIR", self.root().join("target"))
            .env_remove("JUSTLESS_SESSION")
            .output()
            .unwrap()
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

    fn config(&self, settings: &str) {
        fs::write(self.root().join(".justless.toml"), settings).unwrap();
    }

    /// An executable named like a real tool, so Justless picks that tool's report.
    fn tool(&self, name: &str, body: &str) -> String {
        let bin = self.root().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
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
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.path().join("record.json").exists())
            .map(|entry| {
                let record = entry.path().join("record.json");
                (
                    fs::metadata(record).unwrap().modified().unwrap(),
                    entry.file_name().to_string_lossy().into_owned(),
                )
            })
            .collect();
        runs.sort();
        runs.pop().unwrap().1
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
    assert!(
        stdout.starts_with("GITHUB_TOKEN=[redacted:github-token:"),
        "{stdout}"
    );
    assert!(stderr.contains(&format!("db={scheme}://app:[redacted:url-password:")));
    assert!(
        hidden(&stdout, &token) && hidden(&stderr, &password),
        "{stderr}"
    );
    assert!(
        stderr.contains(" · 2 secrets redacted · justless log run-"),
        "{stderr}"
    );

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
    assert_eq!(
        text(&output.stderr),
        format!("[justless {id} · 1 secret redacted]\n")
    );
    assert!(text(&output.stdout).starts_with("token=[redacted:secret:"));

    // A rerun with output this short is shown again, so the count stays.
    let again = project.cli(&["run", "--hook", "--", "sh", "-c", &short]);
    assert!(text(&again.stdout).starts_with("token=[redacted:secret:"));
    assert!(text(&again.stderr).ends_with("· 1 secret redacted]\n"));

    let failing = format!("echo token={secret}; exit 1");
    let output = project.cli(&["run", "--hook", "--", "sh", "-c", &failing]);
    let id = run_id(&output);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        format!("[justless {id} · 1 secret redacted · exit 1]\n")
    );

    let long = format!("seq 1 400; echo token={secret}; exit 2");
    let args = [
        "run",
        "--hook",
        "--max-bytes",
        "512",
        "--",
        "sh",
        "-c",
        &long,
    ];
    let output = project.cli(&args);
    let footer = text(&output.stderr);
    assert!(
        footer.contains(" shown · 1 secret redacted · exit 2]"),
        "{footer}"
    );
    assert!(hidden(&text(&output.stdout), &secret));
    // The same output again is reported as unchanged: nothing was shown, so no count.
    let output = project.cli(&args);
    assert!(text(&output.stdout).starts_with("Output unchanged since run-"));
    let footer = text(&output.stderr);
    assert!(
        footer.ends_with(" · exit 2]\n") && !footer.contains("redacted"),
        "{footer}"
    );

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

    let log = project.ok(&["log", &id, "--stream", "stdout"]);
    let replay = text(&log.stdout);
    assert_eq!(replay.lines().count(), 3);
    assert!(replay.starts_with("start\napi_key=[redacted:secret:") && replay.ends_with("]\nend\n"));
    let line = project.ok(&["log", &id, "--stream", "stdout", "--from", "2", "--to", "2"]);
    assert!(text(&line.stdout).starts_with("api_key=[redacted:secret:"));
    assert_eq!(text(&line.stdout).lines().count(), 1);
    let stderr = project.ok(&["log", &id, "--stream", "stderr"]);
    assert!(text(&stderr.stderr).starts_with("Authorization: Bearer [redacted:authorization:"));
    assert!(hidden(&text(&stderr.stderr), &bearer));

    let exact = project.ok(&["log", &id, "--stream", "stdout", "--unredacted"]);
    assert_eq!(exact.stdout, archived);
    let exact = project.ok(&["log", &id, "--unredacted", "--stream", "stderr"]);
    assert_eq!(exact.stderr, project.archive(&id, "stderr"));

    let stats: Value = serde_json::from_slice(&project.ok(&["stats", "--json"]).stdout).unwrap();
    assert_eq!(stats["redacted_secrets"], 2);
    assert_eq!(stats["unredacted_replays"], 2);
    let family = &stats["commands"][0];
    assert_eq!(family["reopened_runs"], 1);
    assert_eq!(family["full_reads"], 4);
    let report = text(&project.ok(&["stats"]).stdout);
    assert!(
        report.contains("Redacted 2 secret(s) in captured output"),
        "{report}"
    );
    assert!(report.contains("· 2 unredacted replay(s)"), "{report}");
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
    assert_eq!(
        project.ok(&["log", &id, "--stream", "stdout"]).stdout,
        archived
    );
    // A different allowlist rescans the archive, and it is still clean.
    project.config("redact_allow = ['^never$']\n");
    assert_eq!(
        project.ok(&["log", &id, "--stream", "stdout"]).stdout,
        archived
    );
    let search = project.ok(&["search", &id, "policy"]);
    assert!(text(&search.stdout).contains("stdout:2: password policy ok"));
}

#[test]
fn search_and_history_match_redacted_text() {
    let project = Project::new();
    let token = github("search");
    let script = format!("echo first; echo TOKEN={token}");
    let id = run_id(&project.ok(&["run", "--", "sh", "-c", &script]));

    let found = project.ok(&["search", &id, &token]);
    assert_eq!(text(&found.stdout), "");
    let found = project.ok(&["search", &id, "redacted:github-token"]);
    assert!(text(&found.stdout).starts_with("stdout:2: TOKEN=[redacted:github-token:"));
    let found = project.ok(&["search", &id, &token, "--unredacted"]);
    assert_eq!(text(&found.stdout), format!("stdout:2: TOKEN={token}\n"));

    let history = project.ok(&["history", &token]);
    assert!(text(&history.stdout).contains("No matching captured runs."));
    let history = project.ok(&["history", "redacted:github-token"]);
    assert!(text(&history.stdout).contains(&format!("{id} other stdout:2:")));
    assert!(
        !project
            .cli(&["history", &token, "--unredacted"])
            .status
            .success()
    );
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
    assert!(shown.contains("[output clipped"), "{shown}");
    assert!(shown.contains("[redacted:"), "{shown}");
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
    assert!(!stdout.contains("[output clipped"));
    assert_eq!(stdout.matches("[redacted:url-password:").count(), 25);
    for password in passwords {
        assert!(!stdout.contains(&format!(":{password}@")));
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
    assert!(shown.contains("auth::uses_token"), "{shown}");
    assert!(hidden(&shown, &secret), "{shown}");
    let record = fs::read_to_string(project.run_dir(&id).join("record.json")).unwrap();
    assert!(record.contains("[redacted:secret:") && hidden(&record, &secret));
    assert!(hidden(&text(&project.ok(&["resume"]).stdout), &secret));

    let mut matches = String::new();
    for i in 0..300 {
        matches.push_str(&format!(
            "src/module{}.rs:{}:    let token = \"{}\";\n",
            i % 7,
            i + 1,
            value(&format!("rg-{i}"))
        ));
    }
    fs::write(project.root().join("matches.txt"), &matches).unwrap();
    let rg = project.tool("rg", "cat matches.txt\n");
    let output = project.ok(&["run", "--", &rg, "-n", "token", "src"]);
    let shown = text(&output.stdout);
    assert!(shown.contains("src/module0.rs"), "{shown}");
    assert!(shown.contains("[redacted:secret:"), "{shown}");
    for i in 0..300 {
        assert!(hidden(&shown, &value(&format!("rg-{i}"))));
    }

    let mut findings = String::new();
    for i in 0..40 {
        findings.push_str(&format!(
            "app/settings.py:{}:1: S105 Possible hardcoded password assigned to: \"password={}\"\n",
            i + 1,
            value(&format!("lint-{i}"))
        ));
    }
    findings.push_str("Found 40 errors.\n");
    fs::write(project.root().join("findings.txt"), &findings).unwrap();
    let ruff = project.tool("ruff", "cat findings.txt\nexit 1\n");
    let output = project.cli(&["run", "--", &ruff, "check", "app"]);
    let shown = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(shown.contains("S105"), "{shown}");
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
    assert!(text(&output.stdout).contains(&token));
    assert!(!text(&output.stderr).contains("redacted"));
    assert!(project.record(&id).get("redaction").is_none());

    project.config("");
    let resume = text(&project.ok(&["resume"]).stdout);
    assert!(resume.contains("first tests/test_api.py::test_auth[[redacted:github-token:"));
    assert!(hidden(&resume, &token), "{resume}");
    let log = text(&project.ok(&["log", &id]).stdout);
    assert!(hidden(&log, &token) && log.contains("[redacted:github-token:"));
    let history = text(&project.ok(&["history", "test_auth"]).stdout);
    assert!(hidden(&history, &token), "{history}");

    // The rerun compares with the old record and names the failure it no longer sees.
    fs::write(project.root().join("second"), "").unwrap();
    let output = project.cli(&["run", "--", &pytest]);
    let shown = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(
        shown.contains("test_auth[[redacted:github-token:"),
        "{shown}"
    );
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
    assert!(!text(&output.stderr).contains("redacted"));

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
        assert!(
            text(&output.stderr).contains(message),
            "{}",
            text(&output.stderr)
        );
        assert!(!project.root().join("ran").exists());

        // Views of earlier runs fall back to the built-in rules.
        for id in [&earlier, &redacted_run] {
            let log = project.ok(&["log", id, "--stream", "stdout"]);
            assert!(text(&log.stdout).starts_with("TOKEN=[redacted:github-token:"));
            assert!(text(&log.stderr).contains("redacting with built-in rules only"));
        }
        let exact = project.ok(&["log", &earlier, "--stream", "stdout", "--unredacted"]);
        assert_eq!(text(&exact.stdout), format!("TOKEN={token}\n"));
        assert!(text(&project.ok(&["resume"]).stdout).contains(&redacted_run));
    }

    project.config("redact = [unclosed\n");
    let log = project.ok(&["log", &earlier, "--stream", "stdout"]);
    assert!(text(&log.stdout).starts_with("TOKEN=[redacted:github-token:"));
    assert!(text(&log.stderr).contains("redacting with built-in rules only"));
    let found = project.ok(&["search", &earlier, "TOKEN"]);
    assert!(text(&found.stdout).contains("[redacted:github-token:"));
    // Without --budget, resume reads its default budget from the same file.
    let resume = project.ok(&["resume", "--budget", "2400"]);
    assert!(text(&resume.stdout).contains(&earlier));
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
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let second = text(
        &project
            .ok(&["run", "--fresh", "--", "sh", "-c", &script])
            .stdout,
    );
    assert_eq!(first, second);
    assert!(first.starts_with("api_key=[redacted:secret:") && first.len() == 31);

    fs::remove_file(&key).unwrap();
    fs::create_dir(&key).unwrap();
    let output = project.ok(&["run", "--fresh", "--", "sh", "-c", &script]);
    assert_eq!(text(&output.stdout), "api_key=[redacted:secret]\n");
    assert!(text(&output.stderr).contains("markers omit their hash"));
}

fn mcp(root: &Path, lines: &[Value]) -> Vec<Value> {
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
    text(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn mcp_results_are_redacted_and_cannot_be_unredacted() {
    let project = Project::new();
    let token = github("mcp");
    let script = format!("echo TOKEN={token}");
    let id = run_id(&project.ok(&["run", "--", "sh", "-c", &script]));
    let call = |id: &str, name: &str, arguments: Value| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": name, "arguments": arguments}})
    };
    let responses = mcp(
        project.root(),
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": "list", "method": "tools/list", "params": {}}),
            call("log", "log", json!({"run_id": id})),
            call(
                "search",
                "search",
                json!({"run_id": id, "text": "redacted:github"}),
            ),
            call("value", "search", json!({"run_id": id, "text": token})),
            call("history", "history", json!({"text": "TOKEN="})),
            call("raw", "log", json!({"run_id": id, "unredacted": true})),
        ],
    );
    let by_id = |key: &str| {
        responses
            .iter()
            .find(|response| response["id"] == key)
            .unwrap()
    };
    let content = |key: &str| {
        let response = by_id(key);
        assert_eq!(response["result"]["isError"], false, "{response:#}");
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(content("log").starts_with("TOKEN=[redacted:github-token:"));
    assert!(content("search").contains("stdout:1: TOKEN=[redacted:github-token:"));
    assert!(!content("value").contains("TOKEN"));
    assert!(hidden(&content("history"), &token));
    let raw = by_id("raw");
    assert!(raw.get("error").is_some() || raw["result"]["isError"] == true);
    assert!(hidden(&raw.to_string(), &token));

    let tools = by_id("list")["result"]["tools"].as_array().unwrap();
    for tool in tools {
        assert!(
            tool["inputSchema"]["properties"]
                .get("unredacted")
                .is_none()
        );
    }
    let log = tools.iter().find(|tool| tool["name"] == "log").unwrap();
    assert!(
        log["description"]
            .as_str()
            .unwrap()
            .contains("secrets redacted")
    );
    let size = serde_json::to_string(tools).unwrap().len();
    assert!(size < 8_000, "tool list is {size} bytes");
}

#[test]
fn source_views_and_recall_stay_exact() {
    let project = Project::new();
    let secret = value("source");
    let source = format!("API_KEY = \"{secret}\"\nDEBUG = True\n");
    fs::write(project.root().join("settings.py"), &source).unwrap();

    assert_eq!(text(&project.ok(&["read", "settings.py"]).stdout), source);
    let code = text(&project.ok(&["code", "settings.py", "--to", "2"]).stdout);
    assert!(code.contains(&secret), "{code}");
    let outline: Value =
        serde_json::from_slice(&project.ok(&["read", "settings.py", "--json"]).stdout).unwrap();
    let revision = outline["revision"].as_str().unwrap();
    let line = project.ok(&["expand", "settings.py", "L1", "--revision", revision]);
    assert_eq!(text(&line.stdout), format!("API_KEY = \"{secret}\"\n"));

    // An edit built from the expanded bytes round-trips exactly.
    let replacement = text(&line.stdout).replace("API_KEY", "SERVICE_KEY");
    let mut edit = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(["edit", "settings.py", "L1", "--revision", revision])
        .current_dir(project.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    edit.stdin
        .take()
        .unwrap()
        .write_all(replacement.as_bytes())
        .unwrap();
    let edited = edit.wait_with_output().unwrap();
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
    let mut mask = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(["mask", "--keep", "0"])
        .current_dir(project.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    mask.stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let masked: Value = serde_json::from_slice(&mask.wait_with_output().unwrap().stdout).unwrap();
    let stub = masked["messages"][1]["content"][0]["content"]
        .as_str()
        .unwrap();
    let id = stub
        .strip_prefix("[justless recall ")
        .and_then(|s| s.strip_suffix(']'))
        .unwrap();
    let recalled: Value =
        serde_json::from_slice(&project.ok(&["recall", id, "--json"]).stdout).unwrap();
    assert_eq!(recalled, json!(result));
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
        let mut args = vec!["run", "--fresh", "--"];
        args.extend(&command);
        project.config("");
        let output = project.cli(&args);
        let id = project.latest_run();
        assert_eq!(project.record(&id)["redaction"]["count"], 0, "{command:?}");
        assert!(!text(&output.stderr).contains("redacted · justless log"));
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
    let reference = project.ok(&["run", "--", "cat", "id_rsa"]);
    let expected = key_marker(&text(&reference.stdout)).to_owned();
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
        let mut args = vec!["run", "--fresh", "--max-bytes", "65536", "--"];
        args.extend(command);
        let output = project.cli(&args);
        let shown = text(&output.stdout);
        let id = project.latest_run();
        let record = project.record(&id);
        assert_eq!(record["redaction"]["count"], 1, "{command:?}: {shown}");
        assert!(body_hidden(&shown, &body), "{command:?}: {shown}");
        // The prefix stays on each line and is not part of the hashed key.
        assert_eq!(key_marker(&shown), expected, "{command:?}: {shown}");
        let archive = text(&project.archive(&id, "stdout"));
        assert_eq!(shown.lines().count(), archive.lines().count());
        let log = text(&project.ok(&["log", &id]).stdout);
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
        let mut text: String = encoded
            .as_bytes()
            .chunks(width)
            .map(|line| format!("{}\n", std::str::from_utf8(line).unwrap()))
            .collect();
        text.push_str("done\n");
        text
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
        let id = project.latest_run();
        let record = project.record(&id);
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
    let markers: Vec<String> = ["key64.b64", "key76.b64"]
        .iter()
        .map(|name| key_marker(&text(&project.ok(&["run", "--", "cat", name]).stdout)).to_owned())
        .collect();
    assert_eq!(markers[0], markers[1]);
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
    let output = project.ok(&["run", "--max-bytes", "65536", "--", "cat", "probe.txt"]);
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
    let id = run_id(&output);
    let shown = text(&output.stdout);
    assert!(!shown.contains("[output clipped"), "{shown}");
    assert!(body_hidden(&shown, &body));
    let log = project.ok(&["log", &id, "--stream", "stdout"]);
    assert_eq!(output.stdout, log.stdout);

    let args = [
        "run", "--hook", "--fresh", "--", "grep", "-n", "", "dump.txt",
    ];
    let output = project.ok(&args);
    let id = run_id(&output);
    assert_eq!(
        text(&output.stderr),
        format!("[justless {id} · 1 secret redacted]\n")
    );
    assert_eq!(text(&output.stdout).lines().count(), dump.lines().count());
}
