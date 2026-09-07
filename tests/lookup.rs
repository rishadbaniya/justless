use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const FILES: &[&str] = &[
    "src/net/retry.rs",
    "src/net/socket.rs",
    "config/app.toml",
    "docs/operations.md",
    "web/client.ts",
    "scripts/fetch.py",
    "tests/test_fetch.py",
    "package-lock.json",
    "data/events.csv",
];

fn fixture() -> TempDir {
    let target = TempDir::new().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/lookup");
    for file in FILES {
        let path = target.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(source.join(file), path).unwrap();
    }
    target
}

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_justless"))
        .arg("--root")
        .arg(root)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn lookup(root: &Path, args: &[&str]) -> String {
    let mut all = vec!["lookup"];
    all.extend_from_slice(args);
    let output = cli(root, &all);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        text(&output.stderr)
    );
    text(&output.stdout)
}

/// Result header lines: everything that is not a snippet line or footer.
fn headers(output: &str) -> Vec<&str> {
    output
        .lines()
        .take_while(|line| !line.contains(" ranked") && !line.starts_with('['))
        .filter(|line| !line.starts_with("  "))
        .collect()
}

fn files(output: &str) -> Vec<&str> {
    headers(output)
        .iter()
        .map(|header| header.split(':').next().unwrap())
        .collect()
}

fn store(root: &Path) -> PathBuf {
    root.join(".justless/lookup")
}

fn store_files(root: &Path) -> Vec<String> {
    let mut names = fs::read_dir(store(root))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn manifest_time(root: &Path) -> std::time::SystemTime {
    fs::metadata(store(root).join("manifest.bin"))
        .unwrap()
        .modified()
        .unwrap()
}

fn write(root: &Path, file: &str, contents: &str) {
    let path = root.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn ranks_symbols_sections_and_config_for_a_question() {
    let root = fixture();
    let output = lookup(root.path(), &["where are retries configured"]);
    let headers = headers(&output);
    assert_eq!(
        headers[0], "src/net/retry.rs:5-9 struct RetryPolicy",
        "{output}"
    );
    assert!(output.contains(
        "  5 | /// How many times a failed download is retried and how long to wait between attempts.\n"
    ));
    let top = &headers[..5.min(headers.len())];
    assert!(top.contains(&"config/app.toml:5-7 text retry"), "{output}");
    assert!(
        top.contains(&"docs/operations.md:9-13 section Retries"),
        "{output}"
    );
    for data in ["package-lock.json", "data/events.csv"] {
        assert!(!files(&output)[..5].contains(&data), "{output}");
    }
    assert!(!headers[0].starts_with("tests/"), "{output}");
    assert!(
        output.contains(" ranked · matched retries · no match configured\n"),
        "{output}"
    );
    assert!(
        output
            .lines()
            .last()
            .unwrap()
            .starts_with("Next: justless expand "),
        "{output}"
    );
    // Lookup is not a read, so it leaves activity alone.
    assert!(!root.path().join(".justless/activity.json").exists());
}

#[test]
fn identifier_spellings_meet_across_languages() {
    let root = fixture();
    let output = lookup(root.path(), &["parseHttpHeader"]);
    let mut top = headers(&output)[..2].to_vec();
    top.sort();
    assert_eq!(
        top,
        [
            "src/net/socket.rs:9-13 function parse_http_header",
            "web/client.ts:1-7 function parseHttpHeader"
        ],
        "{output}"
    );
    let output = lookup(root.path(), &["parse_http_header"]);
    assert_eq!(files(&output)[..2].len(), 2);
}

#[test]
fn doc_comments_explain_a_symbol() {
    let root = fixture();
    let output = lookup(root.path(), &["how does the websocket reconnect"]);
    assert_eq!(
        headers(&output)[0],
        "src/net/socket.rs:1-7 function reconnect_websocket",
        "{output}"
    );
    assert!(
        output.contains("  1 | /// Reconnects the websocket after the server closes it.\n"),
        "{output}"
    );
}

#[test]
fn tests_rank_when_the_question_asks_for_them() {
    let root = fixture();
    let output = lookup(root.path(), &["download retry tests"]);
    assert!(
        files(&output)[..3].contains(&"tests/test_fetch.py"),
        "{output}"
    );
    assert!(output.contains("tests/test_fetch.py:4-8 test test_download_retries"));
}

fn next_command(output: &str) -> Vec<String> {
    let line = output
        .lines()
        .find_map(|line| line.strip_prefix("Next: justless "))
        .unwrap_or_else(|| panic!("no Next line: {output}"));
    line.split(' ').map(str::to_owned).collect()
}

fn snippet_texts(output: &str) -> Vec<String> {
    output
        .lines()
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(|line| line.split_once(" | ").unwrap().1.to_owned())
        .collect()
}

#[test]
fn the_next_command_opens_the_top_result() {
    let root = fixture();
    let output = lookup(root.path(), &["where are retries configured"]);
    let command = next_command(&output);
    assert_eq!(command[0], "expand");
    let args = command.iter().map(String::as_str).collect::<Vec<_>>();
    let opened = cli(root.path(), &args);
    assert!(opened.status.success(), "{}", text(&opened.stderr));
    let opened = text(&opened.stdout);
    let snippets = snippet_texts(&output);
    assert_eq!(snippets.len(), 2, "{output}");
    for snippet in snippets {
        assert!(opened.contains(&snippet), "{snippet} not in {opened}");
    }

    // A result longer than 80 lines gets a budgeted 40-line excerpt instead.
    let mut runbook = String::from("# Runbook\n\n");
    for line in 3..=120 {
        if line == 100 {
            runbook.push_str("Trigger the failover by promoting the standby database.\n");
        } else {
            runbook.push_str(&format!("Step {line} keeps the service healthy.\n"));
        }
    }
    write(root.path(), "docs/runbook.md", &runbook);
    let output = lookup(root.path(), &["failover standby"]);
    assert!(
        output.starts_with("docs/runbook.md:1-120 section Runbook\n"),
        "{output}"
    );
    let command = next_command(&output);
    assert_eq!(
        command,
        ["code", "docs/runbook.md", "--from", "97", "--to", "120"]
    );
    let from = command[3].parse::<usize>().unwrap();
    let to = command[5].parse::<usize>().unwrap();
    assert!(to - from < 40);
    let args = command.iter().map(String::as_str).collect::<Vec<_>>();
    let opened = cli(root.path(), &args);
    assert!(opened.status.success(), "{}", text(&opened.stderr));
    assert!(text(&opened.stdout).contains("Trigger the failover"));
}

#[test]
fn output_fits_the_budget_and_limit() {
    let root = fixture();
    for budget in ["128", "400", "2400"] {
        let output = lookup(
            root.path(),
            &["where are retries configured", "--budget", budget],
        );
        assert!(
            output.len() <= budget.parse().unwrap(),
            "{budget}: {output}"
        );
    }
    let output = lookup(
        root.path(),
        &["where are retries configured", "--budget", "128"],
    );
    let lines = output.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{output}");
    assert!(lines[0].starts_with("0 of "), "{output}");
    assert_eq!(lines[1], "[context budget reached]");
    let output = lookup(
        root.path(),
        &["where are retries configured", "--limit", "1"],
    );
    assert_eq!(headers(&output).len(), 1, "{output}");
    assert!(output.contains("1 of "), "{output}");
    let output = lookup(
        root.path(),
        &["retry", "--limit", "50", "--budget", "100000"],
    );
    let found = files(&output);
    for file in &found {
        let count = found.iter().filter(|other| *other == file).count();
        assert!(count <= 2, "{file} appears {count} times: {output}");
    }
    let output = cli(root.path(), &["lookup", "retry", "--limit", "0"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn json_has_the_same_results_and_no_timestamps() {
    let root = fixture();
    let first = lookup(root.path(), &["where are retries configured", "--json"]);
    let second = lookup(root.path(), &["where are retries configured", "--json"]);
    assert_eq!(first, second);
    let value: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(value["query"], "where are retries configured");
    assert_eq!(value["matched"], serde_json::json!(["retries"]));
    assert_eq!(value["unmatched"], serde_json::json!(["configured"]));
    assert_eq!(value["omitted"], 0);
    assert_eq!(value["next"], "justless expand src/net/retry.rs L5-9");
    let results = value["results"].as_array().unwrap();
    assert!(results.len() <= 8 && results.len() as u64 <= value["total"].as_u64().unwrap());
    let top = &results[0];
    assert_eq!(top["source"], "code");
    assert_eq!(top["file"], "src/net/retry.rs");
    assert_eq!(
        (top["start"].as_u64(), top["end"].as_u64()),
        (Some(5), Some(9))
    );
    assert_eq!(top["kind"], "struct");
    assert_eq!(top["name"], "RetryPolicy");
    assert!(top["score"].as_f64().unwrap() > 0.0);
    assert_eq!(top["lines"][0]["line"], 5);
    let text = lookup(root.path(), &["where are retries configured"]);
    let headers = headers(&text);
    assert_eq!(headers.len(), results.len());
    for (result, header) in results.iter().zip(&headers) {
        let file = result["file"].as_str().unwrap();
        assert!(header.starts_with(file), "{header} vs {result}");
    }
    for key in ["age", "time", "timestamp", "completed_ms"] {
        assert!(!first.contains(&format!("\"{key}\"")), "{first}");
    }
    let small = lookup(
        root.path(),
        &["where are retries configured", "--json", "--budget", "700"],
    );
    assert!(small.len() <= 700, "{small}");
    let value: Value = serde_json::from_str(&small).unwrap();
    let kept = value["results"].as_array().unwrap().len();
    assert!(kept < results.len());
    assert_eq!(
        value["omitted"].as_u64().unwrap() as usize,
        results.len() - kept
    );
}

/// Small modules that give the base segment enough chunks to keep a delta.
fn add_modules(root: &Path, count: usize, marker: &str) {
    for index in 0..count {
        write(
            root,
            &format!("src/modules/m{index:02}.rs"),
            &format!(
                "/// Parses record {index}.\npub fn parse_{index}(line: &str) -> usize {{\n    line.len() + {index}\n}}\n\n/// Formats record {index}.\npub fn format_{index}(value: usize) -> String {{\n    format!(\"{marker}{{value}}\")\n}}\n"
            ),
        );
    }
}

const QUESTIONS: &[&str] = &[
    "where are retries configured",
    "parse records",
    "proxy timeout upload",
];

fn answers(root: &Path) -> Vec<String> {
    QUESTIONS
        .iter()
        .map(|question| lookup(root, &[question, "--json"]))
        .collect()
}

fn segments(root: &Path, kind: &str) -> Vec<String> {
    store_files(root)
        .into_iter()
        .filter(|name| name.starts_with(kind) && name.ends_with(".seg"))
        .collect()
}

#[test]
fn incremental_refresh_matches_a_rebuild() {
    let root = fixture();
    let root = root.path();
    add_modules(root, 60, "record ");
    lookup(root, &["retry"]);
    assert_eq!(segments(root, "base"), ["base-1.seg"]);

    let retry = fs::read_to_string(root.join("src/net/retry.rs")).unwrap();
    write(
        root,
        "src/net/retry.rs",
        &retry.replace("failed download", "failed upload"),
    );
    fs::remove_file(root.join("src/net/socket.rs")).unwrap();
    write(
        root,
        "src/net/proxy.rs",
        "/// Upload proxy with a timeout.\npub fn proxy_timeout_ms() -> u64 {\n    30_000\n}\n",
    );
    let config = fs::read(root.join("config/app.toml")).unwrap();
    fs::write(root.join("config/app.toml"), config).unwrap();
    let incremental = answers(root);
    assert_eq!(segments(root, "base"), ["base-1.seg"]);
    assert_eq!(segments(root, "delta").len(), 1, "{:?}", store_files(root));
    assert!(
        incremental[0].contains("failed upload"),
        "{}",
        incremental[0]
    );
    assert!(!incremental.concat().contains("socket.rs"));
    assert!(incremental[2].contains("src/net/proxy.rs"));
    fs::remove_dir_all(store(root)).unwrap();
    assert_eq!(answers(root), incremental);

    // Changing more than an eighth of the base compacts it into a new base.
    lookup(root, &["retry"]);
    add_modules(root, 30, "row ");
    let compacted = answers(root);
    assert!(
        segments(root, "delta").is_empty(),
        "{:?}",
        store_files(root)
    );
    assert_eq!(segments(root, "base"), ["base-2.seg"]);
    fs::remove_dir_all(store(root)).unwrap();
    assert_eq!(answers(root), compacted);
}

#[test]
fn an_unchanged_repository_writes_nothing() {
    let root = fixture();
    lookup(root.path(), &["retry"]);
    let before = (manifest_time(root.path()), store_files(root.path()));
    lookup(root.path(), &["socket"]);
    lookup(root.path(), &["retry", "--json"]);
    assert_eq!(
        (manifest_time(root.path()), store_files(root.path())),
        before
    );
}

#[test]
fn ignored_large_binary_and_linked_files_are_skipped() {
    let root = fixture();
    let root = root.path();
    write(root, ".justlessignore", "docs/\n");
    let output = lookup(root, &["rollback"]);
    assert_eq!(
        output,
        "No ranked matches for: rollback. Use justless find TEXT for exact text.\n"
    );

    write(root, ".justless.toml", "max_file_bytes = 300\n");
    let output = lookup(root, &["RetryPolicy"]);
    assert!(!output.contains("src/net/retry.rs"), "{output}");
    write(root, ".justless.toml", "max_file_bytes = 100000\n");
    let output = lookup(root, &["RetryPolicy"]);
    assert!(
        output.starts_with("src/net/retry.rs:5-9 struct RetryPolicy"),
        "{output}"
    );

    fs::write(
        root.join("src/blob.rs"),
        b"zephyrword \0\x01\x02 zephyrword\n",
    )
    .unwrap();
    let output = lookup(root, &["zephyrword"]);
    assert!(
        output.starts_with("No ranked matches for: zephyrword."),
        "{output}"
    );
    let manifest = fs::read(store(root).join("manifest.bin")).unwrap();
    assert!(
        manifest.windows(11).any(|window| window == b"src/blob.rs"),
        "binary files are recorded so they are not read again"
    );
    let before = manifest_time(root);
    lookup(root, &["zephyrword"]);
    assert_eq!(manifest_time(root), before);

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("net/retry.rs", root.join("src/linked.rs")).unwrap();
        let output = lookup(root, &["RetryPolicy", "--limit", "50"]);
        assert!(!output.contains("src/linked.rs"), "{output}");
    }
}

#[test]
fn a_damaged_store_is_rebuilt_without_touching_other_files() {
    let root = fixture();
    let root = root.path();
    let expected = lookup(root, &["where are retries configured"]);
    let base = store(root).join(&segments(root, "base")[0]);
    let bytes = fs::read(&base).unwrap();
    fs::write(&base, &bytes[..10]).unwrap();
    write(root, "outside.txt", "keep me\n");
    fs::write(store(root).join("notes.txt"), "mine\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("outside.txt"), store(root).join("base-99.seg")).unwrap();
    assert_eq!(lookup(root, &["where are retries configured"]), expected);
    assert!(store(root).join("notes.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("outside.txt")).unwrap(),
        "keep me\n"
    );
    #[cfg(unix)]
    assert!(
        fs::symlink_metadata(store(root).join("base-99.seg"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let rebuilt = segments(root, "base")
        .into_iter()
        .filter(|name| name != "base-99.seg")
        .collect::<Vec<_>>();
    assert_eq!(rebuilt.len(), 1);
    let rebuilt = fs::read(store(root).join(&rebuilt[0])).unwrap();
    assert!(rebuilt.len() > 10);
    // A second lookup reuses the rebuilt store.
    let before = manifest_time(root);
    lookup(root, &["retry"]);
    assert_eq!(manifest_time(root), before);
}

#[cfg(unix)]
fn running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| text(&output.stdout).trim() == "0")
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
#[test]
fn an_unwritable_store_still_answers() {
    if running_as_root() {
        return;
    }
    let root = fixture();
    let path = root.path();
    lookup(path, &["retry"]);
    set_mode(&store(path), 0o500);
    write(
        path,
        "src/net/quota.rs",
        "/// Upload quota in bytes.\npub const QUOTA_BYTES: u64 = 1024;\n",
    );
    let output = cli(path, &["lookup", "upload quota"]);
    set_mode(&store(path), 0o700);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(
        text(&output.stdout).starts_with("src/net/quota.rs:1-2 constant QUOTA_BYTES"),
        "{}",
        text(&output.stdout)
    );
    assert!(text(&output.stderr).contains("lookup index not saved"));

    let fresh = fixture();
    let path = fresh.path();
    fs::create_dir(path.join(".justless")).unwrap();
    set_mode(&path.join(".justless"), 0o500);
    let output = cli(path, &["lookup", "where are retries configured"]);
    set_mode(&path.join(".justless"), 0o700);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(text(&output.stdout).starts_with("src/net/retry.rs:5-9 struct RetryPolicy"));
    assert!(text(&output.stderr).contains("lookup index not saved"));
    assert!(!store(path).exists());
}

#[test]
fn concurrent_lookups_agree_and_share_one_store() {
    let root = fixture();
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_justless"))
            .arg("--root")
            .arg(root.path())
            .args(["lookup", "where are retries configured"])
            .current_dir(root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (first, second) = (spawn(), spawn());
    let (first, second) = (
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    );
    assert!(first.status.success() && second.status.success());
    assert_eq!(first.stdout, second.stdout);
    let before = (manifest_time(root.path()), store_files(root.path()));
    let third = lookup(root.path(), &["where are retries configured"]);
    assert_eq!(third.as_bytes(), first.stdout);
    assert_eq!(
        (manifest_time(root.path()), store_files(root.path())),
        before
    );
}

#[test]
fn git_checkouts_use_tracked_and_untracked_files() {
    let root = fixture();
    let path = root.path();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
    };
    git(&["init", "-q"]);
    write(path, ".gitignore", "build/\n");
    write(
        path,
        "build/generated.rs",
        "pub fn reconnect_websocket_generated() {}\n",
    );
    // Git lists a tracked symbolic link, which lookup must not follow.
    #[cfg(unix)]
    std::os::unix::fs::symlink("socket.rs", path.join("src/net/linked.rs")).unwrap();
    git(&["add", "src", "config"]);
    write(
        path,
        "src/net/untracked.rs",
        "/// Reconnect budget.\npub const RECONNECT_BUDGET: u32 = 3;\n",
    );
    let output = lookup(path, &["reconnect", "--limit", "20"]);
    assert!(
        output.contains("src/net/socket.rs:1-7 function reconnect_websocket"),
        "{output}"
    );
    assert!(output.contains("src/net/untracked.rs"), "{output}");
    assert!(!output.contains("build/generated.rs"), "{output}");
    assert!(!output.contains("src/net/linked.rs"), "{output}");
}

#[test]
fn logs_add_ranked_run_output() {
    let root = fixture();
    let path = root.path();
    let run = cli(
        path,
        &[
            "run",
            "--",
            "sh",
            "-c",
            "echo \"connection reset; retrying upload\"; exit 3",
        ],
    );
    assert_eq!(run.status.code(), Some(3));
    let without = lookup(path, &["upload retrying", "--limit", "20"]);
    let with = lookup(path, &["upload retrying", "--logs", "--limit", "20"]);
    let log = headers(&with)
        .into_iter()
        .find(|header| header.contains(" log "))
        .unwrap_or_else(|| panic!("{with}"));
    assert!(log.starts_with("run-"), "{log}");
    assert!(log.contains(" stdout:1-1 log other · exit 3"), "{log}");
    assert!(!with.contains(" ago"), "{with}");
    let sources = |output: &str| {
        headers(output)
            .into_iter()
            .filter(|header| !header.contains(" log "))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let kept = sources(&with);
    let original = sources(&without);
    let common = original
        .iter()
        .filter(|header| kept.contains(header))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        kept.iter()
            .filter(|header| common.contains(header))
            .cloned()
            .collect::<Vec<_>>(),
        common
    );

    let output = lookup(path, &["connection reset", "--logs"]);
    let run = log.split(' ').next().unwrap();
    assert!(output.starts_with(run), "{output}");
    assert!(
        output.contains("  1 | connection reset; retrying upload\n"),
        "{output}"
    );
    let next = output.lines().last().unwrap();
    assert_eq!(
        next,
        format!("Next: justless log {run} --stream stdout --from 1 --to 1")
    );
    let replay = cli(
        path,
        &["log", run, "--stream", "stdout", "--from", "1", "--to", "1"],
    );
    assert_eq!(text(&replay.stdout), "connection reset; retrying upload\n");
    let value: Value =
        serde_json::from_str(&lookup(path, &["connection reset", "--logs", "--json"])).unwrap();
    let top = &value["results"][0];
    assert_eq!(top["source"], "log");
    assert_eq!(top["run"], run);
    assert_eq!(top["stream"], "stdout");
    assert_eq!(top["status"], 3);
    assert!(top.get("file").is_none());
    // Without --logs the log is not ranked.
    assert!(lookup(path, &["connection reset"]).starts_with("No ranked matches"));
}

#[test]
fn questions_need_a_searchable_word() {
    let root = fixture();
    for question in ["   ", "?!"] {
        let output = cli(root.path(), &["lookup", question]);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            text(&output.stderr).contains("enter a question or keywords"),
            "{}",
            text(&output.stderr)
        );
    }
    let output = cli(root.path(), &["lookup", "--", "-v flag"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = lookup(root.path(), &["where", "are", "retries", "configured"]);
    assert_eq!(
        output,
        lookup(root.path(), &["where are retries configured"])
    );
}

#[test]
fn the_reference_shows_real_output() {
    let reference =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/reference.md"))
            .unwrap();
    let command = "$ justless lookup \"where are retries configured?\"\n";
    let start = reference
        .find(command)
        .expect("example in docs/reference.md")
        + command.len();
    let documented = &reference[start..start + reference[start..].find("```").unwrap()];
    let root = fixture();
    assert_eq!(
        lookup(root.path(), &["where are retries configured?"]),
        documented
    );
}

#[test]
fn crlf_lines_and_deleted_files_keep_their_headers() {
    let root = fixture();
    let path = root.path();
    write(
        path,
        "docs/windows.md",
        "# Line endings\r\nCarriage returns stay out of snippets.\r\n",
    );
    let output = lookup(path, &["carriage returns"]);
    assert!(
        output.starts_with("docs/windows.md:1-2 section Line endings\n"),
        "{output}"
    );
    assert!(
        output.contains("  2 | Carriage returns stay out of snippets.\n"),
        "{output}"
    );
    assert!(!output.contains('\r'));
}

#[test]
fn a_store_from_another_version_is_rebuilt() {
    let root = fixture();
    let path = root.path();
    let expected = lookup(path, &["where are retries configured"]);
    let manifest = store(path).join("manifest.bin");
    let original = fs::read(&manifest).unwrap();
    for at in [4, 8] {
        let mut changed = original.clone();
        changed[at] ^= 1;
        fs::write(&manifest, &changed).unwrap();
        assert_eq!(lookup(path, &["where are retries configured"]), expected);
        assert_eq!(fs::read(&manifest).unwrap()[..12], original[..12]);
    }
}

#[test]
fn an_identifier_matches_through_its_parts() {
    let root = fixture();
    let output = lookup(root.path(), &["retryBudget"]);
    let counts = output
        .lines()
        .find(|line| line.contains(" ranked"))
        .unwrap();
    assert!(
        counts.ends_with(" ranked · matched retrybudget"),
        "{output}"
    );
    assert!(!counts.contains("no match"), "{output}");
}
