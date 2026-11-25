use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::tempdir;

fn run(root: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut p = Command::new(env!("CARGO_BIN_EXE_justless"))
        .current_dir(root)
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A command that does not read stdin, such as `recall`, may exit and close
    // the pipe before the input is written.
    if let Err(error) = p.stdin.take().unwrap().write_all(input) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "{error}");
    }
    p.wait_with_output().unwrap()
}

fn ok(out: Output) -> Vec<u8> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// The stdout of a command, given no input, that must succeed.
fn out(root: &Path, args: &[&str]) -> Vec<u8> {
    ok(run(root, args, b""))
}

fn fails(root: &Path, args: &[&str], input: &[u8]) -> bool {
    !run(root, args, input).status.success()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_output(root: &Path, args: &[&str], input: &Value) -> Value {
    serde_json::from_slice(&ok(run(root, args, &serde_json::to_vec(input).unwrap()))).unwrap()
}

fn json(root: &Path, args: &[&str]) -> Value {
    json_output(root, args, &Value::Null)
}

fn outline(root: &Path, name: &str) -> Value {
    json(root, &["read", name, "--json"])
}

fn revision(root: &Path, name: &str) -> String {
    outline(root, name)["revision"].as_str().unwrap().to_owned()
}

/// One field of every row of an outline page.
fn column<'a>(page: &'a Value, key: &str) -> Vec<&'a str> {
    let entries = page["entries"].as_array().unwrap();
    entries
        .iter()
        .map(|entry| entry[key].as_str().unwrap())
        .collect()
}

fn read(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(name)).unwrap()
}

fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

#[track_caller]
fn contains(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} not in:\n{text}");
}

#[test]
fn reads_and_edits_use_parser_extents_even_with_unicode_crlf_or_same_line_declarations() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let cases = [
        (
            "hello.rs",
            "\u{feff}#!/usr/bin/env rust-script\r\n// 🍐\r\nfn one() {} fn two() { let _ = \"é\"; }",
            "two",
            "fn two() { let _ = \"é\"; }",
        ),
        (
            "hello.cpp",
            "// あ\r\nint one() {return 1;} int two() { return 2; }",
            "two",
            "int two() { return 2; }",
        ),
        (
            "hello.ts",
            "export const one = () => 1; export const two = () => 'λ';",
            "two",
            "export const two = () => 'λ';",
        ),
        (
            "hello.py",
            "class Foo:\r\n    @property\r\n    def two(self):\r\n        return '💿'\r\n",
            "Foo::two",
            "@property\r\n    def two(self):\r\n        return '💿'",
        ),
    ];
    for (name, source, symbol, selected) in cases {
        fs::write(root.join(name), source).unwrap();
        assert_eq!(out(root, &["read", name]), source.as_bytes());
        let revision = &revision(root, name);
        let edit = ["edit", name, symbol, "--revision", revision];
        let expanded = out(root, &["expand", name, symbol, "--revision", revision]);
        assert_eq!(expanded, selected.as_bytes(), "{name}");
        assert!(fails(root, &["edit", name, symbol], b"oops"));
        let edited = format!("{selected} /* updated */");
        let dry_run = [&edit[..], &["--dry-run"]].concat();
        let preview = ok(run(root, &dry_run, edited.as_bytes()));
        contains(&String::from_utf8(preview).unwrap(), "/* updated */");
        assert_eq!(read(root, name), source);
        ok(run(root, &edit, edited.as_bytes()));
        assert_eq!(read(root, name), source.replace(selected, &edited));
        assert!(fails(
            root,
            &["edit", name, "L1", "--revision", revision],
            b"oops"
        ));
    }
}

#[test]
fn pagination_and_batch_edits_are_revision_bound() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let text = "use std::fmt;\nmod a { pub fn same() {} }\nmod b { pub fn same() {} }\n";
    fs::write(root.join("lib.rs"), text).unwrap();
    fs::write(root.join("copy.rs"), text).unwrap();
    let page = json(root, &words("read lib.rs --json --match same --limit 1"));
    let cursor = page["next"].as_str().unwrap();
    assert_eq!(page["entries"][0]["name"], "a::same");
    let after = [
        "read", "lib.rs", "--json", "--match", "same", "--after", cursor,
    ];
    let next = json(root, &after);
    assert_eq!(next["entries"][0]["name"], "b::same");
    assert!(next["next"].is_null());
    assert!(fails(root, &["expand", "lib.rs", "same"], b""));
    assert!(fails(root, &["edit", "copy.rs", cursor], b"oops"));
    let inspected = json(root, &words("expand lib.rs a::same --json"));
    assert_eq!(inspected["content"], "pub fn same() {}");
    assert_eq!(inspected["first"], 2);
    let selector = inspected["selector"].as_str().unwrap();
    assert_eq!(
        out(root, &["expand", "lib.rs", selector]),
        b"pub fn same() {}"
    );
    let rev = page["revision"].as_str().unwrap();
    let batch = |data: Value| {
        let edit = ["edit", "lib.rs", "--batch", "--revision", rev];
        run(root, &edit, &serde_json::to_vec(&data).unwrap())
    };
    // Overlapping selectors reject the whole batch.
    let overlapping =
        json!([{"selector":"a","replacement":""},{"selector":"a::same","replacement":""}]);
    assert!(!batch(overlapping).status.success());
    assert_eq!(read(root, "lib.rs"), text);
    ok(batch(
        json!([{"selector":"L1","replacement":"// intro\n","mode":"before"},{"selector":cursor,"replacement":"pub fn fresh() {}"},{"selector":"L3","mode":"after","replacement":"// eof"}]),
    ));
    let fresh = text.replacen("fn same", "fn fresh", 1);
    assert_eq!(read(root, "lib.rs"), format!("// intro\n{fresh}// eof"));
    assert!(fails(root, &["read", "lib.rs", "--after", cursor], b""));
}

#[test]
fn text_fallback_handles_empty_files_and_verified_ranges() {
    let d = tempdir().unwrap();
    let root = d.path();
    fs::write(root.join("settings.custom"), "").unwrap();
    let rev = &revision(root, "settings.custom");
    let source = "é\r\n".repeat(80) + "last";
    let edit = ["edit", "settings.custom", "L1", "--revision", rev];
    ok(run(root, &edit, source.as_bytes()));
    let range = out(root, &["expand", "settings.custom", "L80-L81"]);
    assert_eq!(range, "é\r\nlast".as_bytes());
    let outline = out(root, &["read", "settings.custom", "--budget", "128"]);
    assert!(outline.len() <= 128);
    fs::write(root.join("binary"), b"hello\0world").unwrap();
    assert!(fails(root, &["read", "binary"], b""));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("settings.custom"), root.join("alias")).unwrap();
        assert!(fails(
            root,
            &["edit", "alias", "L1", "--revision", rev],
            b"wrong"
        ));
    }
}

#[test]
fn concurrent_verified_edits_allow_one_writer() {
    let project = tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("x.txt"), "before\n").unwrap();
    let rev = &revision(root, "x.txt");
    std::thread::scope(|threads| {
        let handles: Vec<_> = [b"left".as_slice(), b"right"]
            .into_iter()
            .map(|text| {
                let edit = ["edit", "x.txt", "L1", "--revision", rev];
                threads.spawn(move || run(root, &edit, text).status.success())
            })
            .collect();
        let written = handles.into_iter().map(|h| h.join().unwrap());
        assert_eq!(written.filter(|&success| success).count(), 1);
    });
}

#[test]
fn native_reads_preserve_explicit_windows_and_reject_mismatched_content() {
    let d = tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    let subdir = root.join("folder");
    fs::create_dir(&subdir).unwrap();
    let path = subdir.join("sample with space.py");
    let source = format!(
        "def calculate():\n{}    return 42\n",
        "    # detail\n".repeat(700)
    );
    fs::write(&path, &source).unwrap();
    let mut event = json!({"tool_name":"Read","hook_event_name":"PostToolUse","tool_input":{"file_path":path},"tool_response":{"type":"text","file":{"filePath":path,"content":source.trim_end_matches('\n'),"startLine":1,"numLines":702,"totalLines":702}}});
    let post = |event: &Value| json_output(&root, &["hook-claude", "after-read"], event);
    let compact = |event: &Value| {
        let result = post(event);
        let file = &result["hookSpecificOutput"]["updatedToolOutput"]["file"];
        let content = file["content"].as_str().unwrap();
        assert!(!content.contains("# detail"), "{content}");
        file.clone()
    };
    let file = compact(&event);
    contains(file["content"].as_str().unwrap(), "calculate");
    assert_eq!(file["totalLines"], 702);
    event["tool_input"]["limit"] = json!(32);
    assert_eq!(post(&event), json!({}));
    event["tool_input"].as_object_mut().unwrap().remove("limit");
    // `allow_large` asks for the whole file, so it arrives as Claude Code read it.
    // Claude Code also accepts the flag as a string.
    for flag in [json!(true), json!("true")] {
        event["tool_input"]["allow_large"] = flag;
        assert_eq!(post(&event), json!({}));
    }
    for flag in [json!(false), json!("false"), Value::Null] {
        event["tool_input"]["allow_large"] = flag;
        compact(&event);
    }
    let input = event["tool_input"].as_object_mut().unwrap();
    input.remove("allow_large");
    fs::write(&path, source.replace("42", "43")).unwrap();
    assert_eq!(post(&event), json!({}));
    let command = |cmd: &str| json!({"tool_name":"Bash","hook_event_name":"PreToolUse","tool_input":{"command":cmd,"workdir":subdir}});
    let shell = |cmd: &str| json_output(&root, &["hook-codex", "before-command"], &command(cmd));
    let result = shell("cat './sample with space.py'");
    let rewrite = result["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .unwrap();
    let words = shlex::split(rewrite).unwrap();
    assert_eq!(words.last().unwrap(), "folder/sample with space.py");
    assert!(words.contains(&"read".into()));
    for cmd in ["cat a b", "cat ../outside", "cat a | head", "cat -n a"] {
        assert_eq!(shell(cmd), json!({}));
    }
}

fn recall_id(stub: &Value) -> String {
    let text = stub.as_str().or_else(|| stub[0]["text"].as_str()).unwrap();
    text.strip_prefix("[justless recall ")
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap()
        .to_owned()
}

#[test]
fn outbound_requests_restore_exact_values_and_protect_current_context() {
    let d = tempdir().unwrap();
    let root = d.path();
    let data = "a🦀\r\n".repeat(1000);
    let cases = [
        (
            json!({"system":"stay","messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"old","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"text","text":"user text"},{"type":"tool_result","tool_use_id":"old","content":[{"type":"text","text":data},{"type":"text","text":"end"}]}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"err","is_error":true,"content":data},{"type":"tool_result","tool_use_id":"recent","content":data}]}]}),
            "/messages/1/content/1/content",
        ),
        (
            json!({"messages":[{"role":"assistant","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"old","content":data},{"role":"tool","tool_call_id":"recent","content":data}]}),
            "/messages/1/content",
        ),
        (
            json!({"input":[{"type":"function_call","call_id":"old","name":"read"},{"type":"function_call_output","call_id":"old","output":[{"type":"input_text","text":data}]},{"type":"function_call_output","call_id":"recent","output":data}]}),
            "/input/1/output",
        ),
        (
            json!({"messages":[{"info":{"role":"assistant"},"parts":[{"type":"tool","callID":"old","tool":"read","state":{"status":"completed","output":data}},{"type":"tool","callID":"recent","tool":"run","state":{"status":"completed","output":data}}]}]}),
            "/messages/0/parts/0/state/output",
        ),
    ];
    for (original, pointer) in cases {
        let keep = words("mask --keep 1 --model claude-sonnet-4-6");
        let masked = json_output(root, &keep, &original);
        let mut expected = original.clone();
        let stub = masked.pointer(pointer).unwrap();
        let id = recall_id(stub);
        *expected.pointer_mut(pointer).unwrap() = stub.clone();
        assert_eq!(masked, expected);
        let recalled = json(root, &["recall", &id, "--json"]);
        assert_eq!(recalled, *original.pointer(pointer).unwrap());
        assert_eq!(json_output(root, &["mask", "--restore"], &masked), original);
        assert_eq!(json_output(root, &["mask", "--keep", "1"], &masked), masked);
        for flags in ["--keep-tool read", "--pin old", "--trigger-bytes 100000"] {
            let mask = format!("mask --keep 1 {flags}");
            assert_eq!(
                json_output(root, &words(&mask), &original),
                original,
                "{flags}"
            );
        }
    }
}

#[test]
fn multimodal_or_annotated_results_survive_and_corrupt_archives_fail_closed() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let data = "y".repeat(2048);
    let req = json!({"messages":[{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"1","content":[{"type":"text","text":data,"cache_control":{"type":"ephemeral"}}]},
        {"type":"tool_result","tool_use_id":"2","content":[{"type":"image","source":data}]}
    ]}]});
    assert_eq!(json_output(root, &["mask", "--keep", "0"], &req), req);
    let req = json!({"messages":[{"role":"tool","tool_call_id":"1","content":data}]});
    let masked = json_output(root, &["mask", "--keep", "0"], &req);
    let mut archive = fs::read_dir(root.join(".justless/observations")).unwrap();
    let path = archive.next().unwrap().unwrap().path();
    fs::write(path, b"\"corrupt\"").unwrap();
    for (args, input) in [
        (["mask", "--restore"].as_slice(), &masked),
        (&["mask", "--keep", "0"], &req),
    ] {
        let out = run(root, args, &serde_json::to_vec(input).unwrap());
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn markdown_sections_skip_fenced_headings_and_drop_closing_hashes() {
    let d = tempdir().unwrap();
    let root = d.path();
    let section = "# Install\nrun setup\n\n~~~sh\n# fake\n~~~\n## Linux\nread this\n";
    fs::write(root.join("guide.md"), format!("{section}# Use\ncommands")).unwrap();
    assert_eq!(column(&outline(root, "guide.md"), "name").len(), 3);
    assert_eq!(
        out(root, &["expand", "guide.md", "Install"]),
        section.as_bytes()
    );
    let linux = out(root, &["expand", "guide.md", "Linux"]);
    assert_eq!(linux, b"## Linux\nread this\n");

    let text = "# Guide #\n\n## Setup ##\nInstall steps.\n\n## C#\nRun it.\n\n### Setup\nPer-command setup.\n";
    fs::write(root.join("doc.md"), text).unwrap();
    let page = outline(root, "doc.md");
    assert_eq!(column(&page, "name"), ["Guide", "Setup", "C#", "Setup"]);
    assert_eq!(out(root, &["expand", "doc.md", "Guide"]), text.as_bytes());
    let setup = run(root, &["expand", "doc.md", "Setup"], b"");
    assert!(!setup.status.success());
    contains(&stderr(&setup), "ambiguous symbol");
}

#[test]
fn agent_setup_preserves_existing_plugin_and_registers_claude_read_once() {
    let d = tempdir().unwrap();
    let root = d.path();
    let install = words("init --hooks --agent opencode --agent claude");
    out(root, &install);
    let plugin = root.join(".opencode/plugins/justless.js");
    let script = fs::read_to_string(&plugin).unwrap();
    contains(&script, "experimental.chat.messages.transform");
    assert!(out(root, &install).is_empty());
    let settings = fs::read(root.join(".claude/settings.json")).unwrap();
    let settings: Value = serde_json::from_slice(&settings).unwrap();
    let hooks = settings["hooks"]["PostToolUse"].as_array().unwrap();
    let matchers: Vec<Value> = hooks.iter().map(|hook| hook["matcher"].clone()).collect();
    let expected = [
        "^Read$",
        "^(Grep|Glob)$",
        "^Bash$",
        "^(Edit|MultiEdit|Write)$",
    ];
    assert_eq!(matchers, expected);
    fs::write(&plugin, "// user plugin").unwrap();
    assert!(fails(root, &install, b""));
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "// user plugin");
}

#[test]
fn oversized_files_report_the_size_cap_even_when_it_splits_a_character() {
    let d = tempdir().unwrap();
    let root = d.path();
    fs::write(root.join(".justless.toml"), "max_file_bytes = 8\n").unwrap();
    // The cap's extra byte falls inside the two-byte é.
    fs::write(root.join("big.txt"), "aaaaaaaaé\n").unwrap();
    fs::write(root.join("bad.txt"), b"abc\xff\n").unwrap();
    let big = run(root, &["read", "big.txt", "--outline"], b"");
    assert!(!big.status.success());
    contains(&stderr(&big), "file exceeds max_file_bytes");
    let bad = run(root, &["read", "bad.txt"], b"");
    contains(&stderr(&bad), "expected UTF-8 source");
}

#[test]
fn rust_outline_signatures_skip_docs_and_attributes() {
    let d = tempdir().unwrap();
    let root = d.path();
    let function = "/// Computes the settlement-adjusted fair value, taking the smile-adjusted vol\n/// and the time to expiry into account; see the design notes for the details.\n#[inline]\n#[must_use]\npub fn fair_value(spot: f64, vol: f64) -> f64 {\n    spot * vol\n}";
    let text = format!(
        "{function}\n\n#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]\n#[repr(i8)]\npub enum Side {{ Buy = 1 }}\n"
    );
    fs::write(root.join("lib.rs"), &text).unwrap();
    let page = outline(root, "lib.rs");
    let signatures = column(&page, "signature");
    let fair_value = "pub fn fair_value(spot: f64, vol: f64) -> f64";
    assert_eq!(signatures, [fair_value, "pub enum Side"]);
    assert_eq!(page["entries"][0]["first"], 1);
    // Doc text stays searchable and inside the editable span.
    let found = json(root, &words("read lib.rs --json --match smile-adjusted"));
    assert_eq!(found["entries"][0]["name"], "fair_value");
    let expanded = out(root, &["expand", "lib.rs", "fair_value"]);
    assert_eq!(expanded, function.as_bytes());
}

#[test]
fn plain_recall_prints_the_text_of_retired_text_parts() {
    let d = tempdir().unwrap();
    let root = d.path();
    let lines: String = (1..=80)
        .map(|n| format!("line {n} of the file\n"))
        .collect();
    let request = json!({"messages":[
        {"role":"assistant","tool_calls":[{"id":"old","type":"function","function":{"name":"read","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"old","content":[{"type":"text","text":lines},{"type":"text","text":"end"}]},
        {"role":"tool","tool_call_id":"recent","content":"x"}]});
    let masked = json_output(root, &["mask", "--keep", "1"], &request);
    let id = recall_id(&masked["messages"][1]["content"]);
    let plain = out(root, &["recall", &id]);
    assert_eq!(String::from_utf8(plain).unwrap(), format!("{lines}end"));
    let recalled = json(root, &["recall", &id, "--json"]);
    assert_eq!(recalled, request["messages"][1]["content"]);
}

#[test]
fn rust_syntax_errors_are_reported_instead_of_hiding_symbols() {
    let d = tempdir().unwrap();
    let root = d.path();
    let mut text: String = (1..=100)
        .map(|i| format!("pub fn f{i}() -> u8 {{ {i} }}\n"))
        .collect();
    fs::write(root.join("ok.rs"), &text).unwrap();
    assert!(outline(root, "ok.rs").get("parse_error").is_none());
    text.push_str("pub fn wip( {\n");
    fs::write(root.join("lib.rs"), &text).unwrap();
    let outlined = String::from_utf8(out(root, &["read", "lib.rs", "--outline"])).unwrap();
    let error = "Rust parse failed at line 101: cannot parse string into token stream";
    assert_eq!(outlined.lines().nth(1), Some(error));
    let page = outline(root, "lib.rs");
    let parse_error = page["parse_error"].as_str().unwrap();
    assert!(parse_error.starts_with("Rust parse failed at line 101"));
    let small = out(root, &["read", "lib.rs", "--budget", "128"]);
    assert!(small.len() <= 128, "{}", String::from_utf8_lossy(&small));
    let expand = run(root, &["expand", "lib.rs", "f3"], b"");
    assert!(!expand.status.success());
    let error = stderr(&expand);
    assert!(
        error.contains("line 101") && error.contains("line range"),
        "{error}"
    );
    let line = out(root, &["expand", "lib.rs", "L3"]);
    assert_eq!(line, b"pub fn f3() -> u8 { 3 }\n");
}

#[test]
fn deeply_nested_rust_reports_an_error_instead_of_overflowing_the_stack() {
    let d = tempdir().unwrap();
    let root = d.path();
    fs::write(root.join(".justless.toml"), "").unwrap();
    fs::write(root.join("ok.rs"), "pub fn hello() -> u8 { 1 }\n").unwrap();
    let parens = ("(".repeat(3000), ")".repeat(3000));
    let deep = format!("fn f() -> u8 {{ {}1{} }}\n", parens.0, parens.1);
    let unary = format!("const B: bool = {}true;\n", "!".repeat(3000));
    let chain = format!("const X: u64 = {}1;\n", "1 + ".repeat(20000));
    fs::write(root.join("deep.rs"), &deep).unwrap();
    fs::write(root.join("unary.rs"), &unary).unwrap();
    fs::write(root.join("chain.rs"), &chain).unwrap();
    fs::write(root.join("late.rs"), "fn a() {}\n\nfn b( -> {}\n").unwrap();
    for (name, text) in [("deep.rs", &deep), ("unary.rs", &unary)] {
        let error = "Rust parse failed at line 1: nesting deeper than 1024 levels";
        assert_eq!(outline(root, name)["parse_error"], error);
        // Line ranges never need the parser.
        assert_eq!(out(root, &["expand", name, "L1"]), text.as_bytes());
    }
    assert_eq!(outline(root, "chain.rs")["entries"][0]["name"], "X");
    let page = outline(root, "late.rs");
    let error = page["parse_error"].as_str().unwrap();
    assert!(error.starts_with("Rust parse failed at line 3: "), "{page}");
    let found = String::from_utf8(out(root, &["query", "hello"])).unwrap();
    contains(&found, "hello [function] at ok.rs:1");
}

/// A member's decorators belong to its span, so they are read and edited with it,
/// but its outline row shows only its own signature.
#[test]
fn typescript_member_decorators_stay_in_spans_and_out_of_signatures() {
    let d = tempdir().unwrap();
    let root = d.path();
    let method =
        "@Get(\"/admin\")\n  // admins only\n  @UseGuards(AdminGuard)\n  admin() { return 1; }";
    let text = format!(
        "class Ctl {{\n  /** Lists users. */\n  {method}\n\n  open() {{ return 2; }}\n}}\n"
    );
    // More than the 160 bytes a row shows before its signature is clipped.
    let decorators = "@ApiResponse({ status: 200, description: \"The list of users visible to the caller\" })\n  @ApiResponse({ status: 403, description: \"The caller lacks the users:read permission\" })\n  @Get(\"/users\")\n  ";
    assert!(decorators.len() > 160);
    let list = format!(
        "{decorators}listUsers(page: number, size: number): Promise<User[]> {{ return []; }}"
    );
    let controller = format!("class UsersController {{\n  {list}\n}}\n");
    for extension in ["ts", "tsx"] {
        let name = &format!("ctl.{extension}");
        fs::write(root.join(name), &text).unwrap();
        let found = json(root, &["expand", name, "Ctl::admin", "--json"]);
        assert_eq!(found["content"], method, "{name}");
        assert_eq!(found["first"], 3);
        let revision = found["revision"].as_str().unwrap();
        ok(run(
            root,
            &["edit", name, "Ctl::admin", "--revision", revision],
            b"",
        ));
        // The decorators leave with the method instead of moving to open().
        let rest = "class Ctl {\n  /** Lists users. */\n  \n\n  open() { return 2; }\n}\n";
        assert_eq!(read(root, name), rest);

        let name = &format!("u.{extension}");
        let symbol = "UsersController::listUsers";
        fs::write(root.join(name), &controller).unwrap();
        let row = &outline(root, name)["entries"][1];
        assert_eq!(row["name"], symbol, "{name}");
        let signature = "listUsers(page: number, size: number): Promise<User[]>";
        assert_eq!(row["signature"], signature, "{name}");
        // The line range and the editable span still cover the decorators.
        assert_eq!(row["first"], 2, "{name}");
        let found = json(root, &["expand", name, symbol, "--json"]);
        assert_eq!(found["content"], list.as_str(), "{name}");
        // Decorator text stays searchable.
        let matched = json(root, &["read", name, "--json", "--match", "users:read"]);
        assert_eq!(matched["entries"][0]["name"], symbol, "{name}");
    }
}

#[test]
fn symbol_insertions_land_on_line_boundaries() {
    let d = tempdir().unwrap();
    let root = d.path();
    let insert = |name: &str, text: &str, args: &str, replacement: &str| {
        fs::write(root.join(name), text).unwrap();
        let revision = revision(root, name);
        let mut command = vec!["edit", name];
        command.extend(words(args));
        command.extend(["--revision", revision.as_str()]);
        ok(run(root, &command, replacement.as_bytes()));
        read(root, name)
    };
    assert_eq!(
        insert(
            "a.py",
            "class A:\n    def old(self):\n        return 1\n\n    def last(self):\n        return 2\n",
            "A::old --after",
            "    def new(self):\n        return 0\n",
        ),
        "class A:\n    def old(self):\n        return 1\n    def new(self):\n        return 0\n\n    def last(self):\n        return 2\n"
    );
    let crlf = insert(
        "x.rs",
        "fn a() {}\r\nfn b() {}\r\n",
        "a --after",
        "fn n() {}\r\n",
    );
    assert_eq!(crlf, "fn a() {}\r\nfn n() {}\r\nfn b() {}\r\n");
    assert_eq!(
        insert(
            "ctl.ts",
            "class Ctl {\n  @Get(\"/a\")\n  admin() {}\n}\n",
            "Ctl::admin --before",
            "  fresh() {}\n",
        ),
        "class Ctl {\n  fresh() {}\n  @Get(\"/a\")\n  admin() {}\n}\n"
    );
    // A symbol that shares its line keeps its exact boundary.
    let same = insert(
        "same.rs",
        "fn one() {} fn two() {}\n",
        "two --before",
        "fn mid() {} ",
    );
    assert_eq!(same, "fn one() {} fn mid() {} fn two() {}\n");
    // The last line has no line ending, so the insertion goes at the end.
    let end = insert("end.rs", "fn a() {}", "a --after", "\nfn z() {}");
    assert_eq!(end, "fn a() {}\nfn z() {}");
    // Line ranges already end at a line boundary and keep a blank next line.
    let lines = insert("t.txt", "one\n\nthree\n", "L1 --after", "two\n");
    assert_eq!(lines, "one\ntwo\n\nthree\n");
}

#[test]
fn source_paths_resolve_from_the_current_directory_within_the_workspace() {
    let d = tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    let crate_dir = root.join("crate");
    fs::create_dir(&crate_dir).unwrap();
    // The marker makes the parent the discovered root, as a Git checkout would.
    fs::write(root.join(".justless.toml"), "").unwrap();
    fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(crate_dir.join("Cargo.toml"), "[package]\n").unwrap();
    fs::write(crate_dir.join("only.txt"), "only here\n").unwrap();
    let from_crate = |args: &[&str], input: &[u8]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
            .current_dir(&crate_dir)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    };
    let read_from_crate = |file: &str| ok(from_crate(&["read", file], b""));
    // Without --root, a bare path resolves like a shell path and says so when
    // the root has a different file of that name.
    let shadowed = from_crate(&["read", "Cargo.toml"], b"");
    contains(
        &stderr(&shadowed),
        "use ../Cargo.toml for the workspace root's file",
    );
    assert_eq!(ok(shadowed), b"[package]\n");
    let absolute = crate_dir.join("Cargo.toml");
    for (path, expected) in [
        ("./Cargo.toml", "[package]\n"),
        ("../Cargo.toml", "[workspace]\n"),
        ("only.txt", "only here\n"),
        // A workspace-relative path still works when the current directory lacks it.
        ("crate/only.txt", "only here\n"),
        (absolute.to_str().unwrap(), "[package]\n"),
    ] {
        assert_eq!(read_from_crate(path), expected.as_bytes(), "{path}");
    }
    let outside = from_crate(&["read", "../../outside.txt"], b"");
    assert!(!outside.status.success());
    // With --root, as hooks pass it, a bare path is root-relative first.
    let rooted = ["--root", root.to_str().unwrap(), "read", "Cargo.toml"];
    let hooked = from_crate(&rooted, b"");
    contains(&stderr(&hooked), "use ./Cargo.toml for crate/Cargo.toml");
    assert_eq!(ok(hooked), b"[workspace]\n");
    // Every spelling of one file shares its name and revision.
    let local = from_crate(&["expand", "Cargo.toml", "L1", "--json"], b"");
    let local: Value = serde_json::from_slice(&ok(local)).unwrap();
    let rooted = json(&root, &words("expand crate/Cargo.toml L1 --json"));
    assert_eq!(local["file"], "crate/Cargo.toml");
    assert_eq!(local["revision"], rooted["revision"]);
    let revision = local["revision"].as_str().unwrap();
    let edit = [
        "edit",
        "Cargo.toml",
        "L1",
        "--before",
        "--revision",
        revision,
    ];
    ok(from_crate(&edit, b"# added\n"));
    assert_eq!(read(&crate_dir, "Cargo.toml"), "# added\n[package]\n");
    assert_eq!(read(&root, "Cargo.toml"), "[workspace]\n");
}

#[test]
fn dry_run_patches_name_the_workspace_file_and_apply_cleanly() {
    let d = tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    let copy = tempdir().unwrap();
    let name = "src/a b/é.rs";
    let text = "fn a() {}\nfn b() {}\nfn c() {}\n";
    for base in [root.as_path(), copy.path()] {
        fs::create_dir_all(base.join("src/a b")).unwrap();
        fs::write(base.join(name), text).unwrap();
    }
    let revision = revision(&root, name);
    let edit = ["edit", name, "b", "--revision", &revision];
    let dry_run = [&edit[..], &["--dry-run"]].concat();
    let patch = ok(run(&root, &dry_run, b"fn b2() {}"));
    let header = String::from_utf8_lossy(&patch);
    // Git may quote the non-ASCII name, depending on core.quotePath.
    let lines: Vec<_> = header.lines().collect();
    assert!(lines[0].starts_with("diff --git ") && lines[0].contains("b/src/a b/"));
    assert!(lines[2].starts_with("--- ") && lines[2].contains("a/src/a b/"));
    assert!(lines[3].starts_with("+++ ") && lines[3].contains("b/src/a b/"));
    assert!(!header.contains(root.to_str().unwrap()) && !header.contains(".tmp"));
    assert_eq!(read(&root, name), text);
    let mut apply = Command::new("git")
        .args(["apply", "-"])
        .current_dir(copy.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    apply.stdin.take().unwrap().write_all(&patch).unwrap();
    assert!(apply.wait().unwrap().success());
    ok(run(&root, &edit, b"fn b2() {}"));
    assert_eq!(read(copy.path(), name), read(&root, name));
}

#[cfg(unix)]
#[test]
fn edits_refuse_read_only_files_and_keep_links_and_attributes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let d = tempdir().unwrap();
    let root = d.path();
    let edit = |name: &str| {
        let revision = revision(root, name);
        run(
            root,
            &["edit", name, "L2", "--revision", &revision],
            b"TWO\n",
        )
    };
    fs::write(root.join("ro.txt"), "one\ntwo\n").unwrap();
    fs::set_permissions(root.join("ro.txt"), fs::Permissions::from_mode(0o444)).unwrap();
    // Root may write anyway, as a shell would let it.
    let writable = fs::File::options().write(true).open(root.join("ro.txt"));
    if writable.is_err() {
        contains(&stderr(&edit("ro.txt")), "ro.txt is not writable");
        assert_eq!(read(root, "ro.txt"), "one\ntwo\n");
    }
    fs::write(root.join("linked.txt"), "one\ntwo\n").unwrap();
    fs::hard_link(root.join("linked.txt"), root.join("link.txt")).unwrap();
    let inode = |name: &str| fs::metadata(root.join(name)).unwrap().ino();
    let before = inode("linked.txt");
    ok(edit("linked.txt"));
    assert_eq!(inode("linked.txt"), before);
    assert_eq!(read(root, "link.txt"), "one\nTWO\n");
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        let tagged = root.join("tagged.txt");
        fs::write(&tagged, "one\ntwo\n").unwrap();
        // Some filesystems, such as older tmpfs, have no user attributes.
        let flags = rustix::fs::XattrFlags::empty();
        if rustix::fs::setxattr(&tagged, "user.justless.test", b"kept", flags).is_ok() {
            ok(edit("tagged.txt"));
            let mut value = [0; 8];
            let len = rustix::fs::getxattr(&tagged, "user.justless.test", &mut value[..]).unwrap();
            assert_eq!(&value[..len], b"kept");
            assert_eq!(read(root, "tagged.txt"), "one\nTWO\n");
        }
    }
}

#[test]
fn mask_keeps_lone_surrogate_escapes_and_retires_other_results() {
    let d = tempdir().unwrap();
    let root = d.path();
    let data = "line of output\n".repeat(200);
    // JSON.stringify("x\uD83D") and Python's json.dumps emit unpaired escapes.
    // The escaped backslash before `ud83d` in the second message is not one.
    let request = format!(
        concat!(
            r#"{{"messages":[{{"role":"user","content":"file caf\udc80 and \\ud83d"}},"#,
            r#"{{"role":"assistant","tool_calls":[{{"id":"a","type":"function","function":{{"name":"read","arguments":"{{}}"}}}}]}},"#,
            r#"{{"role":"tool","tool_call_id":"a","content":{}}},"#,
            r#"{{"role":"tool","tool_call_id":"b","content":"{}cut \uD83D"}},"#,
            r#"{{"role":"tool","tool_call_id":"c","content":"pair \ud83e\udd80"}},"#,
            r#"{{"role":"tool","tool_call_id":"d","content":"x"}}]}}"#
        ),
        serde_json::to_string(&data).unwrap(),
        "y".repeat(2000),
    );
    // Without retirement the request comes back byte for byte, except that
    // the paired escape is decoded like any other character.
    let normalized = request.replace(r"\ud83e\udd80", "🦀");
    let same = ok(run(root, &["mask", "--keep", "10"], request.as_bytes()));
    assert_eq!(String::from_utf8(same).unwrap(), normalized);

    let masked = ok(run(root, &["mask", "--keep", "1"], request.as_bytes()));
    let masked_text = String::from_utf8(masked.clone()).unwrap();
    contains(&masked_text, r#""file caf\udc80 and \\ud83d""#);
    contains(&masked_text, r#"cut \uD83D""#);
    let start = masked_text.find("[justless recall ").unwrap();
    let id = &masked_text[start + 17..start + 81];
    assert_eq!(out(root, &["recall", id]), data.as_bytes());
    let restored = ok(run(root, &["mask", "--restore"], &masked));
    assert_eq!(String::from_utf8(restored).unwrap(), normalized);
}

/// `template` with each line that ends in ` detail` repeated 400 times, which makes
/// the file long enough for a native read to become an outline.
fn padded(template: &str) -> String {
    let line = |line: &str| {
        let line = format!("{line}\n");
        if line.ends_with(" detail\n") {
            line.repeat(400)
        } else {
            line
        }
    };
    template.lines().map(line).collect()
}

#[test]
fn script_outlines_show_or_keep_top_level_code_the_rows_leave_out() {
    let d = tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    // These scripts are sized for outlines from 151 lines, below the default.
    fs::write(root.join(".justless.toml"), "read_outline_lines = 150\n").unwrap();
    let steps: String = (1..=21).map(|n| format!("step {n}\n")).collect();
    // `None` keeps the native read. Otherwise the outline lists these lines of code
    // that no row covers, and only a file with none has its `cat` rewritten.
    let files: [(&str, String, Option<&[&str]>); 17] = [
        // Commands between and after a shell script's functions.
        (
            "deploy.sh",
            padded(
                "#!/bin/sh\n# Deploy the service.\nset -eu\n\nrequire() {\n  # detail\n  command -v \"$1\" >/dev/null\n}\n\nrequire cargo\ncargo build --release\n",
            ),
            Some(&["set -eu", "require cargo", "cargo build --release"]),
        ),
        // In shell, a lone quoted word and `require NAME` are commands too.
        (
            "dispatch.sh",
            padded("#!/bin/sh\nbuild() {\n  # detail\n  :\n}\n\n\"$@\"\n"),
            Some(&["\"$@\""]),
        ),
        (
            "setup.sh",
            padded("#!/bin/sh\nrequire() {\n  # detail\n  command -v \"$1\"\n}\n\nrequire cargo\n"),
            Some(&["require cargo"]),
        ),
        // A script that is mostly top-level commands keeps its native read.
        (
            "release.sh",
            padded("#!/bin/sh\nstep() {\n  # detail\n  echo \"$1\"\n}\n") + &steps,
            None,
        ),
        // A Python entry point.
        (
            "tool.py",
            padded(
                "import sys\n\n\ndef main():\n    # detail\n    return 0\n\n\nif __name__ == \"__main__\":\n    sys.exit(main())\n",
            ),
            Some(&["if __name__ == \"__main__\":", "    sys.exit(main())"]),
        ),
        // Docstrings, comments and imports are not code the outline hides.
        (
            "module.py",
            padded(
                "\"\"\"Pricing rules.\n\nLong description.\n\"\"\"\n# Helpers follow.\nimport math\n\n\ndef price():\n    # detail\n    return math.pi\n",
            ),
            Some(&[]),
        ),
        // Neither are `__future__` imports and imports only for type checkers.
        (
            "typed.py",
            padded(
                "from __future__ import annotations\nfrom __future__ import (\n    division,\n)\nfrom typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from collections.abc import Iterator\n\n\ndef main():\n    # detail\n    return 0\n",
            ),
            Some(&[]),
        ),
        // CommonJS and ES module bindings, including ones that span lines, only
        // name other modules.
        (
            "util.js",
            padded(
                "'use strict'\nconst fs = require('fs')\nconst { join } = require('path')\nconst {\n  a,\n  b,\n} = require('./x')\nfunction read(p) {\n  // detail\n  return fs.readFileSync(p, 'utf8')\n}\nexport { read }\nexport * from './other'\nexport default read\nmodule.exports = {\n  read,\n  load: read,\n}\nexports.read = read\n",
            ),
            Some(&[]),
        ),
        // A call, a declaration with a value and an exported object with data are
        // code, so they follow the rows.
        (
            "server.js",
            padded(
                "const app = require('./app')(options)\nlet cache\nfunction handle(req, res) {\n  // detail\n  res.end('ok')\n}\napp.listen(8080)\n",
            ),
            Some(&[
                "const app = require('./app')(options)",
                "let cache",
                "app.listen(8080)",
            ]),
        ),
        (
            "config.js",
            padded(
                "module.exports = {\n  name: 'thing',\n  run() {\n    // detail\n    return 1\n  },\n}\n",
            ),
            Some(&["module.exports = {", "  name: 'thing',"]),
        ),
        // After the first statement, a lone string can be part of one, so it is
        // shown with the rest of the statement.
        (
            "message.js",
            padded(
                "'use strict'\nconst MESSAGE = 'Refusing to delete. ' +\n  'It blocks republishing. ' +\n  'Run with --force.'\nfunction check() {\n  // detail\n  return MESSAGE\n}\n",
            ),
            Some(&[
                "const MESSAGE = 'Refusing to delete. ' +",
                "  'It blocks republishing. ' +",
                "  'Run with --force.'",
            ]),
        ),
        (
            "usage.py",
            padded(
                "import sys\n\n\ndef main():\n    # detail\n    return 0\n\n\nprint(\"\"\"\nusage: tool\n\"\"\")\nsys.exit(main())\n",
            ),
            Some(&["print(\"\"\"", "usage: tool", "\"\"\")", "sys.exit(main())"]),
        ),
        // A function exported whole is a row of its own.
        (
            "factory.js",
            padded(
                "const fs = require('fs')\nmodule.exports = function make(options) {\n  // detail\n  return fs\n}\n",
            ),
            Some(&[]),
        ),
        // PHP tags, `declare` and a lone directive string are not code either.
        (
            "Service.php",
            padded(
                "<?php\n'use strict';\ndeclare(strict_types=1);\n\nnamespace App;\n\nclass Service\n{\n    public function total(): int\n    {\n        // detail\n        return 1;\n    }\n}\n",
            ),
            Some(&[]),
        ),
        // Rust holds only items at the top level, so an `impl` wrapper and its
        // associated constant do not stop the outline.
        (
            "lib.rs",
            padded(
                "pub struct Price(u32);\n\nimpl Price {\n    const LIMIT: u32 = 10;\n\n    pub fn checked(&self) -> bool {\n        // detail\n        self.0 < Self::LIMIT\n    }\n}\n",
            ),
            Some(&[]),
        ),
        // Code shown after the rows is bounded by the context budget too.
        (
            "wide.py",
            padded("def main():\n    # detail\n    return 0\n\n\n")
                + &format!("print('{}')\n", "x".repeat(3000)),
            None,
        ),
        // A small file stays native whatever it holds.
        ("small.sh", "#!/bin/sh\nmake test\n".into(), None),
    ];
    for (name, source, expected) in files {
        let path = root.join(name);
        fs::write(&path, &source).unwrap();
        let lines = source.lines().count();
        let event = json!({"tool_name":"Read","hook_event_name":"PostToolUse","tool_input":{"file_path":path},"tool_response":{"type":"text","file":{"filePath":path,"content":source.trim_end_matches('\n'),"startLine":1,"numLines":lines,"totalLines":lines}}});
        let read = json_output(&root, &["hook-claude", "after-read"], &event);
        match expected {
            None => assert_eq!(read, json!({}), "{name}"),
            Some(code) => {
                let content = read["hookSpecificOutput"]["updatedToolOutput"]["file"]["content"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: {read}"));
                let shown = content
                    .split_once("Code outside these rows:\n")
                    .map_or("", |(_, code)| code);
                let numbered: Vec<String> = code
                    .iter()
                    .map(|line| {
                        let number = source.lines().position(|l| l == *line).unwrap() + 1;
                        format!("L{number}: {line}")
                    })
                    .collect();
                assert_eq!(shown, numbered.join("\n"), "{name}: {content}");
            }
        }
        let cat = json!({"tool_name":"Bash","hook_event_name":"PreToolUse","tool_input":{"command":format!("cat {name}"),"workdir":root}});
        let rewritten = json_output(&root, &["hook-codex", "before-command"], &cat);
        assert_eq!(
            rewritten != json!({}),
            expected.is_some_and(|code| code.is_empty()),
            "{name}: {rewritten}"
        );
    }
}

#[test]
fn outline_folds_contiguous_imports_and_reports_remaining_entries() {
    let repo = tempfile::TempDir::new().unwrap();
    let root = repo.path();
    let mut source = String::from(
        "use std::collections::BTreeMap;\n\
         use std::fs;\n\
         use std::io;\n\
         use std::path::PathBuf;\n\n",
    );
    for index in 0..8 {
        source.push_str(&format!("pub fn func_{index}() -> usize {{ {index} }}\n"));
    }
    fs::write(root.join("lib.rs"), &source).unwrap();

    // Unfiltered outline folds the 4 contiguous imports into one line while
    // keeping exact `#N` IDs for expand/edit and reporting progress when paged.
    let outline =
        String::from_utf8(out(root, &["read", "lib.rs", "--outline", "--limit", "3"])).unwrap();
    assert!(
        outline.contains("#1..#4 L1-4 imports (4 items)\n"),
        "{outline}"
    );
    assert!(
        outline.contains("#5 L6-6 function func_0  pub fn func_0() -> usize\n"),
        "{outline}"
    );
    assert!(
        outline.contains("… 6 more entries (6 of 12)\n"),
        "{outline}"
    );
    let rev = outline
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    assert!(
        outline.contains(&format!("next --after {rev}#6\n")),
        "{outline}"
    );

    // Individual `#N` selectors inside and after the folded block still expand exact bytes.
    let first_import =
        String::from_utf8(out(root, &["expand", "lib.rs", &format!("{rev}#1")])).unwrap();
    assert_eq!(first_import, "use std::collections::BTreeMap;");
    let func0 = String::from_utf8(out(root, &["expand", "lib.rs", &format!("{rev}#5")])).unwrap();
    assert_eq!(func0, "pub fn func_0() -> usize { 0 }");

    // Filtering with `--match` searches individual imports without folding them.
    let matched = String::from_utf8(out(
        root,
        &["read", "lib.rs", "--outline", "--match", "PathBuf"],
    ))
    .unwrap();
    assert!(
        matched.contains("#4 L4-4 import import  use std::path::PathBuf;\n"),
        "{matched}"
    );

    // Multi-term `--match a|b` matches entries containing any term, while
    // space-containing filters stay literal substring matches.
    let multi = String::from_utf8(out(
        root,
        &["read", "lib.rs", "--outline", "--match", "PathBuf|func_7"],
    ))
    .unwrap();
    assert!(
        multi.contains("#4 L4-4 import import  use std::path::PathBuf;\n"),
        "{multi}"
    );
    assert!(
        multi.contains("#12 L13-13 function func_7  pub fn func_7() -> usize\n"),
        "{multi}"
    );
    assert!(!multi.contains("func_0"), "{multi}");

    let literal_pipe = String::from_utf8(out(
        root,
        &["read", "lib.rs", "--outline", "--match", "PathBuf | func_7"],
    ))
    .unwrap();
    assert!(!literal_pipe.contains("PathBuf"), "{literal_pipe}");
    assert!(!literal_pipe.contains("func_7"), "{literal_pipe}");

    // Budget-truncated outlines at normal budgets reserve room for the progress line
    // while strictly honoring `--budget`.
    let budgeted = String::from_utf8(out(
        root,
        &[
            "read",
            "lib.rs",
            "--outline",
            "--limit",
            "40",
            "--budget",
            "320",
        ],
    ))
    .unwrap();
    assert!(budgeted.len() <= 320, "len={}: {budgeted}", budgeted.len());
    assert!(budgeted.contains(" more "), "{budgeted}");
    assert!(budgeted.contains("next --after "), "{budgeted}");

    // `stats` measures outline views, avoids double-counting file bytes across
    // repeated/paged/filtered outlines of the same unchanged file, and subtracts
    // follow-up `expand` bytes from net outline savings.
    let stats_json: Value = serde_json::from_slice(&out(root, &["stats", "--json"])).unwrap();
    let outlines = &stats_json["source_outlines"];
    assert_eq!(outlines["views"], 5, "{outlines}");
    assert_eq!(outlines["whole_bytes"], source.len() as u64, "{outlines}");
    assert_eq!(
        outlines["expanded_bytes"],
        (first_import.len() + func0.len()) as u64,
        "{outlines}"
    );
    let shown = outlines["shown_bytes"].as_u64().unwrap();
    let expanded = outlines["expanded_bytes"].as_u64().unwrap();
    assert_eq!(
        outlines["net_saved_bytes"].as_i64().unwrap(),
        source.len() as i64 - shown as i64 - expanded as i64,
        "{outlines}"
    );
    let stats_text = String::from_utf8(out(root, &["stats"])).unwrap();
    assert!(
        stats_text.contains("Source outlines: 5 view(s) printed "),
        "{stats_text}"
    );
    assert!(stats_text.contains("expanded), net "), "{stats_text}");

    // Even after Activity::reads rotates past its 32-entry window, Outlines::seen
    // still avoids double-counting `whole_bytes` and attributes follow-up `expand`.
    let other_lines: String = (1..=40).map(|i| format!("// line {i}\n")).collect();
    fs::write(root.join("other.rs"), &other_lines).unwrap();
    for line in 1..=35 {
        let range = format!("L{line}-{line}");
        out(root, &["expand", "other.rs", &range]);
    }
    let again = out(root, &["read", "lib.rs", "--outline", "--match", "func_7"]);
    let func1 = out(root, &["expand", "lib.rs", "func_1"]);
    let after_rotation: Value = serde_json::from_slice(&out(root, &["stats", "--json"])).unwrap();
    let outlines = &after_rotation["source_outlines"];
    assert_eq!(outlines["views"], 6, "{outlines}");
    assert_eq!(outlines["whole_bytes"], source.len() as u64, "{outlines}");
    assert_eq!(
        outlines["shown_bytes"],
        shown + again.len() as u64,
        "{outlines}"
    );
    assert_eq!(
        outlines["expanded_bytes"],
        expanded + func1.len() as u64,
        "{outlines}"
    );

    // Follow-up `code` reads on an outlined unchanged file also count toward
    // `expanded_bytes` so net outline savings stay honest across read paths.
    let code_before = outlines["expanded_bytes"].as_u64().unwrap();
    let code_out = out(root, &["code", "lib.rs", "--from", "1", "--to", "5"]);
    let code_text = String::from_utf8(code_out).unwrap();
    // `snippet()` measures the numbered lines it printed (excluding the header).
    let header_len = code_text.find('\n').unwrap() + 1;
    let snippet_bytes = (code_text.len() - header_len) as u64;
    let after_code: Value = serde_json::from_slice(&out(root, &["stats", "--json"])).unwrap();
    assert_eq!(
        after_code["source_outlines"]["expanded_bytes"],
        code_before + snippet_bytes,
        "{after_code}"
    );
}
