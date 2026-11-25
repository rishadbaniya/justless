use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, has, lacks, starts, text};

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

const RETRIES: &str = "where are retries configured";

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
    Command::new(BIN)
        .arg("--root")
        .arg(root)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

fn lookup(root: &Path, args: &[&str]) -> String {
    let output = cli(root, &[&["lookup"], args].concat());
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
    let manifest = store(root).join("manifest.bin");
    fs::metadata(manifest).unwrap().modified().unwrap()
}

fn write(root: &Path, file: &str, contents: &str) {
    let path = root.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn ranks_symbols_sections_and_config_for_a_question() {
    let root = fixture();
    let output = lookup(root.path(), &[RETRIES]);
    let headers = headers(&output);
    assert_eq!(
        headers[0], "src/net/retry.rs:5-9 struct RetryPolicy",
        "{output}"
    );
    has(
        &output,
        "  5 | /// How many times a failed download is retried and how long to wait between attempts.\n",
    );
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
    has(&output, " ranked · matched retries · no match configured\n");
    starts(output.lines().last().unwrap(), "Next: justless expand ");
    // Lookup is not a read, so it leaves activity alone.
    assert!(!root.path().join(".justless/activity.json").exists());
}

#[test]
fn questions_rank_the_symbol_they_name() {
    let root = fixture();
    let socket = "src/net/socket.rs:9-13 function parse_http_header";
    let client = "web/client.ts:1-7 function parseHttpHeader";
    let reconnect = "src/net/socket.rs:1-7 function reconnect_websocket";
    let test = "tests/test_fetch.py:4-8 test test_download_retries";
    // (question, how many first results to search, headers among them, text shown)
    for (question, first, expected, shown) in [
        // Identifier spellings meet across languages.
        ("parseHttpHeader", 2, &[socket, client][..], ""),
        ("parse_http_header", 2, &[socket, client], ""),
        // A doc comment explains a symbol.
        (
            "how does the websocket reconnect",
            1,
            &[reconnect],
            "  1 | /// Reconnects the websocket after the server closes it.\n",
        ),
        // Tests rank when the question asks for them.
        ("download retry tests", 3, &[test], ""),
        // An identifier matches through its parts.
        ("retryBudget", 0, &[], " ranked · matched retrybudget\n"),
    ] {
        let output = lookup(root.path(), &[question]);
        let headers = headers(&output);
        for header in expected {
            assert!(headers[..first].contains(header), "{question}: {output}");
        }
        assert!(output.contains(shown), "{question}: {output}");
    }
    // A file with CRLF line endings keeps its headers, and snippets lose the `\r`.
    write(
        root.path(),
        "docs/windows.md",
        "# Line endings\r\nCarriage returns stay out of snippets.\r\n",
    );
    let output = lookup(root.path(), &["carriage returns"]);
    starts(&output, "docs/windows.md:1-2 section Line endings\n");
    has(&output, "  2 | Carriage returns stay out of snippets.\n");
    lacks(&output, "\r");
}

fn next_command(output: &str) -> Vec<String> {
    let line = output
        .lines()
        .find_map(|line| line.strip_prefix("Next: justless "))
        .unwrap_or_else(|| panic!("no Next line: {output}"));
    line.split(' ').map(str::to_owned).collect()
}

/// Runs a printed `Next:` command and returns its output.
fn open(root: &Path, command: &[String]) -> String {
    let args = command.iter().map(String::as_str).collect::<Vec<_>>();
    let opened = cli(root, &args);
    assert!(opened.status.success(), "{}", text(&opened.stderr));
    text(&opened.stdout)
}

#[test]
fn the_next_command_opens_the_top_result() {
    let root = fixture();
    let output = lookup(root.path(), &[RETRIES]);
    let command = next_command(&output);
    assert_eq!(command[0], "expand");
    let opened = open(root.path(), &command);
    let snippets: Vec<&str> = output
        .lines()
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(|line| line.split_once(" | ").unwrap().1)
        .collect();
    assert_eq!(snippets.len(), 2, "{output}");
    for snippet in snippets {
        has(&opened, snippet);
    }

    // A short result made of long lines is too large to open whole.
    let mut notes = String::from("# Quorum notes\n");
    for line in 2..=12 {
        notes.push_str(&format!("Line {line} {}\n", "word ".repeat(48)));
    }
    write(root.path(), "docs/quorum.md", &notes);
    let output = lookup(root.path(), &["quorum notes"]);
    starts(&output, "docs/quorum.md:1-12 section Quorum notes\n");
    assert_eq!(
        next_command(&output),
        ["code", "docs/quorum.md", "--from", "1", "--to", "12"]
    );
    let output = lookup(root.path(), &["quorum notes", "--budget", "8000"]);
    assert_eq!(next_command(&output), ["expand", "docs/quorum.md", "L1-12"]);

    // A long result gets a budgeted excerpt of under 40 lines instead.
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
    starts(&output, "docs/runbook.md:1-120 section Runbook\n");
    let command = next_command(&output);
    assert_eq!(
        command,
        ["code", "docs/runbook.md", "--from", "97", "--to", "120"]
    );
    has(&open(root.path(), &command), "Trigger the failover");
}

#[test]
fn output_fits_the_budget_and_limit() {
    let root = fixture();
    let long = (0..40)
        .map(|index| format!("missingword{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    for question in [RETRIES, &format!("retries {long}")] {
        for budget in ["128", "200", "400", "2400"] {
            let size = budget.parse().unwrap();
            let output = lookup(root.path(), &[question, "--budget", budget]);
            assert!(output.len() <= size, "{budget}: {output}");
            // Even the smallest budget shows the top result's header.
            assert!(
                output.starts_with("src/net/retry.rs:"),
                "{budget}: {output}"
            );
            let output = lookup(root.path(), &[question, "--budget", budget, "--json"]);
            assert!(output.len() <= size, "{budget}: {output}");
            serde_json::from_str::<Value>(&output).unwrap();
        }
    }
    let output = lookup(root.path(), &[RETRIES, "--budget", "128"]);
    assert_eq!(
        headers(&output),
        ["src/net/retry.rs:5-9 struct RetryPolicy"],
        "{output}"
    );
    has(&output, "\n[context budget reached]\n");
    let output = lookup(root.path(), &[RETRIES, "--limit", "1"]);
    assert_eq!(headers(&output).len(), 1, "{output}");
    has(&output, "1 of ");
    let output = lookup(
        root.path(),
        &["retry", "--limit", "50", "--budget", "100000"],
    );
    let found = files(&output);
    for file in &found {
        let count = found.iter().filter(|other| *other == file).count();
        assert!(count <= 2, "{file} appears {count} times: {output}");
    }
}

#[test]
fn json_has_the_same_results_and_no_timestamps() {
    let root = fixture();
    let first = lookup(root.path(), &[RETRIES, "--json"]);
    assert_eq!(first, lookup(root.path(), &[RETRIES, "--json"]));
    let value: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(value["query"], RETRIES);
    assert_eq!(value["matched"], json!(["retries"]));
    assert_eq!(value["unmatched"], json!(["configured"]));
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
    let text = lookup(root.path(), &[RETRIES]);
    let headers = headers(&text);
    assert_eq!(headers.len(), results.len());
    for (result, header) in results.iter().zip(&headers) {
        starts(header, result["file"].as_str().unwrap());
    }
    for key in ["age", "time", "timestamp", "completed_ms"] {
        lacks(&first, &format!("\"{key}\""));
    }
    let small = lookup(root.path(), &[RETRIES, "--json", "--budget", "700"]);
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

fn answers(root: &Path) -> Vec<String> {
    [RETRIES, "parse records", "proxy timeout upload"]
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
    let retry = retry.replace("failed download", "failed upload");
    write(root, "src/net/retry.rs", &retry);
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
    has(&incremental[0], "failed upload");
    lacks(&incremental.concat(), "socket.rs");
    has(&incremental[2], "src/net/proxy.rs");
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
fn a_large_first_build_is_announced_once() {
    let root = fixture();
    let root = root.path();
    for index in 0..2_000 {
        let file = format!("notes/n{:02}/{index:04}.txt", index / 100);
        write(root, &file, &format!("note {index}\n"));
    }
    let first = cli(root, &["lookup", RETRIES]);
    assert!(first.status.success());
    assert_eq!(
        text(&first.stderr),
        "Building lookup index for 2009 files…\n"
    );
    write(root, "notes/n00/0000.txt", "changed note\n");
    let second = cli(root, &["lookup", RETRIES]);
    assert_eq!(second.stdout, first.stdout);
    assert!(second.stderr.is_empty(), "{}", text(&second.stderr));
}

#[test]
fn ignored_large_binary_and_linked_files_are_skipped() {
    let root = fixture();
    let root = root.path();
    write(root, ".justlessignore", "docs/\n");
    assert_eq!(
        lookup(root, &["rollback"]),
        "No ranked matches for: rollback. Use justless find TEXT for exact text.\n"
    );

    write(root, ".justless.toml", "max_file_bytes = 300\n");
    lacks(&lookup(root, &["RetryPolicy"]), "src/net/retry.rs");
    write(root, ".justless.toml", "max_file_bytes = 100000\n");
    starts(
        &lookup(root, &["RetryPolicy"]),
        "src/net/retry.rs:5-9 struct RetryPolicy",
    );

    fs::write(
        root.join("src/blob.rs"),
        b"zephyrword \0\x01\x02 zephyrword\n",
    )
    .unwrap();
    starts(
        &lookup(root, &["zephyrword"]),
        "No ranked matches for: zephyrword.",
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
        lacks(
            &lookup(root, &["RetryPolicy", "--limit", "50"]),
            "src/linked.rs",
        );
    }
}

#[test]
fn damaged_and_foreign_stores_are_rebuilt_without_touching_other_files() {
    let root = fixture();
    let root = root.path();
    write(root, "outside.txt", "keep me\n");
    let expected = lookup(root, &[RETRIES]);
    let original = fs::read(store(root).join(&segments(root, "base")[0])).unwrap();
    let header = fs::read(store(root).join("manifest.bin")).unwrap()[..12].to_vec();
    fs::write(store(root).join("notes.txt"), "mine\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("outside.txt"), store(root).join("base-99.seg")).unwrap();
    type Damage = fn(&mut Vec<u8>);
    // (case, whether it damages the base segment rather than the manifest, damage)
    let damages: [(&str, bool, Damage); 4] = [
        ("truncated base", true, |bytes| bytes.truncate(10)),
        // The header's fifth section offset is where the postings start.
        ("zeroed postings", true, |bytes| {
            let postings = u64::from_le_bytes(bytes[52..60].try_into().unwrap()) as usize;
            assert!(postings < bytes.len());
            bytes[postings..].fill(0);
        }),
        // A store from another version or format.
        ("manifest version", false, |bytes| bytes[4] ^= 1),
        ("manifest format", false, |bytes| bytes[8] ^= 1),
    ];
    for (case, in_base, damage) in damages {
        let name = if in_base {
            segments(root, "base").remove(0)
        } else {
            "manifest.bin".into()
        };
        let file = store(root).join(name);
        let mut bytes = fs::read(&file).unwrap();
        damage(&mut bytes);
        fs::write(&file, bytes).unwrap();
        let output = cli(root, &["lookup", RETRIES]);
        assert!(output.status.success(), "{case}: {}", text(&output.stderr));
        assert_eq!(text(&output.stdout), expected, "{case}");
        if case == "zeroed postings" {
            has(
                &text(&output.stderr),
                "lookup index was damaged and has been rebuilt",
            );
        }
        let rebuilt = segments(root, "base");
        assert_eq!(rebuilt.len(), 1, "{case}");
        assert_eq!(
            fs::read(store(root).join(&rebuilt[0])).unwrap(),
            original,
            "{case}"
        );
        let manifest = fs::read(store(root).join("manifest.bin")).unwrap();
        assert_eq!(manifest[..12], header, "{case}");
        // The next lookup reuses the rebuilt store.
        let before = manifest_time(root);
        let output = cli(root, &["lookup", RETRIES]);
        assert_eq!(text(&output.stdout), expected, "{case}");
        assert!(output.stderr.is_empty(), "{case}: {}", text(&output.stderr));
        assert_eq!(manifest_time(root), before, "{case}");
    }
    assert!(store(root).join("notes.txt").exists());
    let outside = fs::read_to_string(root.join("outside.txt")).unwrap();
    assert_eq!(outside, "keep me\n");
    // The link at a store name is removed, not followed.
    assert!(fs::symlink_metadata(store(root).join("base-99.seg")).is_err());
}

#[test]
fn identical_files_share_one_result() {
    let root = fixture();
    let root = root.path();
    let original = lookup(root, &[RETRIES]);
    let docs = fs::read_to_string(root.join("docs/operations.md")).unwrap();
    write(root, "vendor/a/operations.md", &docs);
    write(root, "vendor/b/operations.md", &docs);
    let output = lookup(root, &[RETRIES]);
    let header = "docs/operations.md:9-13 section Retries";
    has(
        &output,
        &format!("\n{header} · same in vendor/a/operations.md +1 more\n"),
    );
    lacks(&output, "vendor/a/operations.md:");
    lacks(&output, "vendor/b/operations.md:");
    // The copies take no places, so the same results are shown.
    let shown: Vec<&str> = headers(&output)
        .iter()
        .map(|header| header.split(" · same in ").next().unwrap())
        .collect();
    assert_eq!(shown, headers(&original));
    let value: Value = serde_json::from_str(&lookup(root, &[RETRIES, "--json"])).unwrap();
    let results = value["results"].as_array().unwrap();
    let docs_result = results
        .iter()
        .find(|result| result["file"] == "docs/operations.md")
        .unwrap();
    assert_eq!(
        docs_result["same"],
        json!(["vendor/a/operations.md", "vendor/b/operations.md"])
    );
    // Copies share the two-per-file limit, so a third chunk of the
    // original cannot come back through a copy.
    let quorum = "# Quorum\n\n## Quorum reads\nReads need a quorum.\n\n## Quorum writes\nWrites need a quorum.\n\n## Quorum repair\nRepair restores a quorum.\n";
    write(root, "docs/quorum.md", quorum);
    write(root, "vendor/c/quorum.md", quorum);
    let output = lookup(root, &["quorum", "--limit", "20"]);
    let found = files(&output);
    let copies = found.iter().filter(|file| file.ends_with("quorum.md"));
    assert_eq!(copies.count(), 2, "{output}");
    assert!(!found.contains(&"vendor/c/quorum.md"), "{output}");
    has(&output, " · same in vendor/c/quorum.md\n");

    // A copy that changes stands on its own again.
    let changed = docs.replace("five times", "seven times");
    write(root, "vendor/b/operations.md", &changed);
    let output = lookup(root, &[RETRIES, "--limit", "20"]);
    has(
        &output,
        &format!("\n{header} · same in vendor/a/operations.md\n"),
    );
    has(&output, "\nvendor/b/operations.md:9-13 section Retries\n");
}

/// Puts links at store names, as a repository that commits `.justless/` could.
#[cfg(unix)]
fn plant(root: &Path, target: &Path, names: &[&str]) {
    fs::create_dir_all(store(root)).unwrap();
    for name in names {
        let link = store(root).join(name);
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(target, link).unwrap();
    }
}

#[cfg(unix)]
fn assert_no_links(root: &Path) {
    for name in store_files(root) {
        let metadata = fs::symlink_metadata(store(root).join(&name)).unwrap();
        assert!(metadata.is_file(), "{name} is not a regular file");
    }
    let lock = fs::symlink_metadata(root.join(".justless/lookup.lock")).unwrap();
    assert!(lock.is_file());
}

#[cfg(unix)]
#[test]
fn planted_links_in_the_store_are_never_followed() {
    let clean = fixture();
    let planted = fixture();
    let outside = TempDir::new().unwrap();
    let victim = outside.path().join("victim.txt");
    fs::write(&victim, "precious\n").unwrap();
    for root in [clean.path(), planted.path()] {
        add_modules(root, 60, "record ");
    }
    let root = planted.path();
    let names = [
        "manifest.bin",
        "manifest.bin.tmp",
        "base-1.seg",
        "base-1.seg.tmp",
        "base-1.seg.postings.tmp",
    ];
    plant(root, &victim, &names);
    let created = outside.path().join("created");
    std::os::unix::fs::symlink(&created, root.join(".justless/lookup.lock")).unwrap();
    assert_eq!(lookup(root, &[RETRIES]), lookup(clean.path(), &[RETRIES]));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "precious\n");
    assert!(!created.exists());
    assert_no_links(root);

    // An incremental refresh writes a delta segment and a new manifest.
    let names = [
        "manifest.bin.tmp",
        "delta-2.seg",
        "delta-2.seg.tmp",
        "delta-2.seg.postings.tmp",
    ];
    plant(root, &victim, &names);
    for root in [clean.path(), planted.path()] {
        let retry = fs::read_to_string(root.join("src/net/retry.rs")).unwrap();
        write(
            root,
            "src/net/retry.rs",
            &retry.replace("download", "upload"),
        );
    }
    assert_eq!(lookup(root, &[RETRIES]), lookup(clean.path(), &[RETRIES]));
    assert_eq!(segments(root, "delta"), ["delta-2.seg"]);
    assert_eq!(fs::read_to_string(&victim).unwrap(), "precious\n");
    assert_no_links(root);
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
#[test]
fn an_unwritable_store_still_answers() {
    let running_as_root = Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| text(&output.stdout).trim() == "0");
    if running_as_root {
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
    starts(
        &text(&output.stdout),
        "src/net/quota.rs:1-2 constant QUOTA_BYTES",
    );
    has(&text(&output.stderr), "lookup index not saved");

    let fresh = fixture();
    let path = fresh.path();
    fs::create_dir(path.join(".justless")).unwrap();
    set_mode(&path.join(".justless"), 0o500);
    let output = cli(path, &["lookup", RETRIES]);
    set_mode(&path.join(".justless"), 0o700);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    starts(
        &text(&output.stdout),
        "src/net/retry.rs:5-9 struct RetryPolicy",
    );
    has(&text(&output.stderr), "lookup index not saved");
    assert!(!store(path).exists());
}

#[test]
fn concurrent_lookups_agree_and_share_one_store() {
    let root = fixture();
    let spawn = || {
        Command::new(BIN)
            .arg("--root")
            .arg(root.path())
            .args(["lookup", RETRIES])
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
    // An unchanged repository writes nothing, whatever the question.
    let before = (manifest_time(root.path()), store_files(root.path()));
    assert_eq!(lookup(root.path(), &[RETRIES]).as_bytes(), first.stdout);
    lookup(root.path(), &["socket"]);
    lookup(root.path(), &["retry", "--json"]);
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
    has(
        &output,
        "src/net/socket.rs:1-7 function reconnect_websocket",
    );
    has(&output, "src/net/untracked.rs");
    lacks(&output, "build/generated.rs");
    lacks(&output, "src/net/linked.rs");
}

#[test]
fn logs_add_ranked_run_output() {
    let root = fixture();
    let path = root.path();
    let script = "echo \"connection reset; retrying upload\"; exit 3";
    let run = cli(path, &["run", "--", "sh", "-c", script]);
    assert_eq!(run.status.code(), Some(3));
    let without = lookup(path, &["upload retrying", "--limit", "20"]);
    let with = lookup(path, &["upload retrying", "--logs", "--limit", "20"]);
    let log = headers(&with)
        .into_iter()
        .find(|header| header.contains(" log "))
        .unwrap_or_else(|| panic!("{with}"));
    starts(log, "run-");
    has(log, " stdout:1-1 log other · exit 3");
    lacks(&with, " ago");
    let sources = |output: &str| {
        headers(output)
            .into_iter()
            .filter(|header| !header.contains(" log "))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    // Log windows come first and take no places from source results.
    assert_eq!(headers(&with)[0], log, "{with}");
    assert_eq!(sources(&with), sources(&without));
    let limited = lookup(path, &["upload retrying", "--logs", "--limit", "2"]);
    assert_eq!(sources(&limited).len(), 2, "{limited}");
    assert_eq!(headers(&limited)[0], log, "{limited}");
    has(&limited, "\nNext: justless log run-");

    let output = lookup(path, &["connection reset", "--logs"]);
    let run = log.split(' ').next().unwrap();
    starts(&output, run);
    has(&output, "  1 | connection reset; retrying upload\n");
    assert_eq!(
        output.lines().last().unwrap(),
        format!("Next: justless log {run} --stream stdout --from 1 --to 1")
    );
    let replay = cli(
        path,
        &["log", run, "--stream", "stdout", "--from", "1", "--to", "1"],
    );
    assert_eq!(text(&replay.stdout), "connection reset; retrying upload\n");
    let json = lookup(path, &["connection reset", "--logs", "--json"]);
    let value: Value = serde_json::from_str(&json).unwrap();
    let top = &value["results"][0];
    assert_eq!(top["source"], "log");
    assert_eq!(top["run"], run);
    assert_eq!(top["stream"], "stdout");
    assert_eq!(top["status"], 3);
    assert!(top.get("file").is_none());
    // Without --logs the log is not ranked.
    starts(&lookup(path, &["connection reset"]), "No ranked matches");
}

#[test]
fn logs_take_three_places_and_forget_deleted_runs() {
    let root = fixture();
    let path = root.path();
    for shard in 0..5 {
        let script = format!("echo \"quota exceeded on shard {shard}\"; exit 1");
        let run = cli(path, &["run", "--", "sh", "-c", &script]);
        assert_eq!(run.status.code(), Some(1));
    }
    let logs = |output: &str| {
        headers(output)
            .into_iter()
            .filter(|header| header.contains(" log "))
            .map(|header| header.split(' ').next().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let output = lookup(path, &["quota exceeded", "--logs"]);
    assert_eq!(logs(&output).len(), 3, "{output}");
    let runs = path.join(".justless/runs");
    let mut ids = fs::read_dir(&runs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("run-"))
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids.len(), 5);
    let kept = ids.pop().unwrap();
    for id in &ids {
        fs::remove_dir_all(runs.join(id)).unwrap();
    }
    let output = lookup(path, &["quota exceeded", "--logs"]);
    assert_eq!(logs(&output), [kept], "{output}");
}

#[test]
fn questions_need_a_searchable_word() {
    let root = fixture();
    for (args, error) in [
        (&["   "][..], "enter a question or keywords"),
        (&["?!"], "enter a question or keywords"),
        (
            &["retry", "--limit", "0"],
            "--limit expects a number of at least 1",
        ),
    ] {
        let output = cli(root.path(), &[&["lookup"], args].concat());
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        has(&text(&output.stderr), error);
    }
    let output = cli(root.path(), &["lookup", "--", "-v flag"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = lookup(root.path(), &["where", "are", "retries", "configured"]);
    assert_eq!(output, lookup(root.path(), &[RETRIES]));
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
    // Ordinary non-relational lookup must never open or build the code graph index.
    assert!(
        !root.path().join(".justless/index.json").exists(),
        "non-relational lookup must not open the code graph index"
    );
}

fn copy_tree(src: &Path, dst: &Path) {
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == ".justless" {
            continue;
        }
        let target = dst.join(name);
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn graph_traversal_traces_cross_language_and_auto_relational_queries() {
    let mixed = TempDir::new().unwrap();
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mixed"),
        mixed.path(),
    );
    let root = mixed.path();

    // Auto-relational natural-language query traces TypeScript -> Python -> SQL.
    let auto = lookup(
        root,
        &["how do frontend orders connect to purchase_orders?"],
    );
    has(&auto, "Graph paths · 1");
    has(
        &auto,
        "fetchOrders [function · TypeScript] frontend/orders.ts:3-7",
    );
    has(
        &auto,
        "→ possible HTTP route at frontend/orders.ts:4 → list_orders [function · Python] backend/orders.py:6-8",
    );
    has(
        &auto,
        "→ possible SQL relation at backend/orders.py:8 → purchase_orders [table · SQL] db/schema.sql:1-3",
    );

    // Shorter relational wording without naming "frontend" also finds the chain.
    let auto_short = lookup(root, &["how does orders connect to purchase_orders?"]);
    has(&auto_short, "Graph paths · ");
    has(&auto_short, "fetchOrders");
    has(&auto_short, "list_orders");
    has(&auto_short, "purchase_orders");

    // Explicit --graph works on non-relational keywords and emits structured JSON.
    let raw_json = lookup(root, &["orders purchase_orders", "--graph", "--json"]);
    let parsed: Value = serde_json::from_str(&raw_json).unwrap();
    let paths = parsed["graph_paths"]
        .as_array()
        .expect("graph_paths array in JSON");
    assert!(!paths.is_empty(), "{parsed:#}");
    assert_eq!(paths[0]["from"]["name"], "fetchOrders");
    assert_eq!(paths[0]["to"]["name"], "purchase_orders");
    assert_eq!(paths[0]["hops"], 2);
    assert_eq!(paths[0]["directed"], true);
    assert_eq!(paths[0]["possible"], true);
    assert_eq!(paths[0]["steps"].as_array().unwrap().len(), 2);

    // Tight budget keeps both text and JSON strictly within the byte ceiling.
    let tight_text = lookup(
        root,
        &[
            "how do frontend orders connect to purchase_orders?",
            "--budget",
            "650",
        ],
    );
    assert!(
        tight_text.len() <= 650,
        "{} bytes\n{tight_text}",
        tight_text.len()
    );
    has(&tight_text, "Graph paths · ");

    let tight_json = lookup(
        root,
        &[
            "how do frontend orders connect to purchase_orders?",
            "--json",
            "--budget",
            "1100",
        ],
    );
    assert!(
        tight_json.len() <= 1100,
        "{} bytes\n{tight_json}",
        tight_json.len()
    );
    let _: Value = serde_json::from_str(&tight_json).unwrap();

    // Non-relational query without --graph emits no graph_paths key in JSON.
    let plain_json: Value =
        serde_json::from_str(&lookup(root, &["purchase_orders", "--json"])).unwrap();
    assert!(
        plain_json.get("graph_paths").is_none(),
        "plain lookup JSON must omit graph_paths: {plain_json:#}"
    );
}

#[test]
fn graph_traversal_labels_reverse_hops_and_respects_hop_bound() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    // Two distinct leaves `alpha_leaf` and `beta_leaf` share a common caller
    // `shared_caller`, so connecting them requires a reverse edge (`←`).
    write(
        root,
        "src/shared.py",
        "from src.alpha import alpha_leaf\nfrom src.beta import beta_leaf\ndef shared_caller():\n    return alpha_leaf() + beta_leaf()\n",
    );
    write(root, "src/alpha.py", "def alpha_leaf():\n    return 1\n");
    write(root, "src/beta.py", "def beta_leaf():\n    return 2\n");
    // A 7-hop chain exceeds the 5-hop traversal bound.
    write(
        root,
        "src/chain.py",
        "def hop_start():\n    return hop_1()\ndef hop_1():\n    return hop_2()\ndef hop_2():\n    return hop_3()\ndef hop_3():\n    return hop_4()\ndef hop_4():\n    return hop_5()\ndef hop_5():\n    return hop_6()\ndef hop_6():\n    return hop_end()\ndef hop_end():\n    return 0\n",
    );

    let reverse = lookup(root, &["how does alpha_leaf connect to beta_leaf?"]);
    has(&reverse, "Graph paths · 1");
    has(&reverse, "via reverse");
    has(&reverse, "← call at src/shared.py:");
    has(&reverse, "shared_caller");

    let rev_json: Value = serde_json::from_str(&lookup(
        root,
        &["how does alpha_leaf connect to beta_leaf?", "--json"],
    ))
    .unwrap();
    assert_eq!(
        rev_json["graph_paths"][0]["directed"], false,
        "{rev_json:#}"
    );
    assert_eq!(
        rev_json["graph_paths"][0]["steps"][0]["direction"], "reverse",
        "{rev_json:#}"
    );

    let too_far = lookup(root, &["hop_start hop_end", "--graph"]);
    has(&too_far, "Graph paths · none within 5 hops");
}
