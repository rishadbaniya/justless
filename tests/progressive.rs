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
    p.stdin.take().unwrap().write_all(input).unwrap();
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

fn json_output(root: &Path, args: &[&str], input: &Value) -> Value {
    serde_json::from_slice(&ok(run(root, args, &serde_json::to_vec(input).unwrap()))).unwrap()
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
        assert_eq!(ok(run(root, &["read", name], b"")), source.as_bytes());
        let outline = json_output(root, &["read", name, "--json"], &Value::Null);
        let revision = outline["revision"].as_str().unwrap();
        assert_eq!(
            ok(run(
                root,
                &["expand", name, symbol, "--revision", revision],
                b""
            )),
            selected.as_bytes(),
            "{name}"
        );
        assert!(!run(root, &["edit", name, symbol], b"oops").status.success());
        let edited = format!("{selected} /* updated */");
        let preview = ok(run(
            root,
            &["edit", name, symbol, "--revision", revision, "--dry-run"],
            edited.as_bytes(),
        ));
        assert!(
            String::from_utf8(preview)
                .unwrap()
                .contains("/* updated */")
        );
        assert_eq!(fs::read_to_string(root.join(name)).unwrap(), source);
        ok(run(
            root,
            &["edit", name, symbol, "--revision", revision],
            edited.as_bytes(),
        ));
        assert_eq!(
            fs::read_to_string(root.join(name)).unwrap(),
            source.replace(selected, &edited)
        );
        assert!(
            !run(root, &["edit", name, "L1", "--revision", revision], b"oops")
                .status
                .success()
        );
    }
}

#[test]
fn pagination_and_batch_edits_are_revision_bound() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let text = "use std::fmt;\nmod a { pub fn same() {} }\nmod b { pub fn same() {} }\n";
    fs::write(root.join("lib.rs"), text).unwrap();
    fs::write(root.join("copy.rs"), text).unwrap();
    let page = json_output(
        root,
        &[
            "read", "lib.rs", "--json", "--match", "same", "--limit", "1",
        ],
        &Value::Null,
    );
    let cursor = page["next"].as_str().unwrap();
    assert_eq!(page["entries"][0]["name"], "a::same");
    let next = json_output(
        root,
        &[
            "read", "lib.rs", "--json", "--match", "same", "--after", cursor,
        ],
        &Value::Null,
    );
    assert_eq!(next["entries"][0]["name"], "b::same");
    assert!(next["next"].is_null());
    assert!(
        !run(root, &["expand", "lib.rs", "same"], b"")
            .status
            .success()
    );
    assert!(
        !run(root, &["edit", "copy.rs", cursor], b"oops")
            .status
            .success()
    );
    let inspected = json_output(
        root,
        &["expand", "lib.rs", "a::same", "--json"],
        &Value::Null,
    );
    assert_eq!(inspected["content"], "pub fn same() {}");
    assert_eq!(inspected["first"], 2);
    assert_eq!(
        ok(run(
            root,
            &["expand", "lib.rs", inspected["selector"].as_str().unwrap()],
            b""
        )),
        b"pub fn same() {}"
    );
    let rev = page["revision"].as_str().unwrap();
    let batch = |data: Value| {
        run(
            root,
            &["edit", "lib.rs", "--batch", "--revision", rev],
            &serde_json::to_vec(&data).unwrap(),
        )
    };
    assert!(
        !batch(json!([{"selector":"a","replacement":""},{"selector":"a::same","replacement":""}]))
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(root.join("lib.rs")).unwrap(), text);
    ok(batch(
        json!([{"selector":"L1","replacement":"// intro\n","mode":"before"},{"selector":cursor,"replacement":"pub fn fresh() {}"},{"selector":"L3","mode":"after","replacement":"// eof"}]),
    ));
    assert_eq!(
        fs::read_to_string(root.join("lib.rs")).unwrap(),
        format!(
            "// intro\n{}// eof",
            text.replacen("fn same", "fn fresh", 1)
        )
    );
    assert!(
        !run(root, &["read", "lib.rs", "--after", cursor], b"")
            .status
            .success()
    );
}

#[test]
fn text_fallback_handles_empty_files_and_verified_ranges() {
    let d = tempdir().unwrap();
    let root = d.path();
    fs::write(root.join("settings.custom"), "").unwrap();
    let page = json_output(root, &["read", "settings.custom", "--json"], &Value::Null);
    let rev = page["revision"].as_str().unwrap();
    let source = "é\r\n".repeat(80) + "last";
    ok(run(
        root,
        &["edit", "settings.custom", "L1", "--revision", rev],
        source.as_bytes(),
    ));
    assert_eq!(
        ok(run(root, &["expand", "settings.custom", "L80-L81"], b"")),
        "é\r\nlast".as_bytes()
    );
    let outline = ok(run(
        root,
        &["read", "settings.custom", "--budget", "128"],
        b"",
    ));
    assert!(outline.len() <= 128);
    fs::write(root.join("binary"), b"hello\0world").unwrap();
    assert!(!run(root, &["read", "binary"], b"").status.success());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("settings.custom"), root.join("alias")).unwrap();
        assert!(
            !run(root, &["edit", "alias", "L1", "--revision", rev], b"wrong")
                .status
                .success()
        );
    }
}

#[test]
fn concurrent_verified_edits_allow_one_writer() {
    let project = tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("x.txt"), "before\n").unwrap();
    let page = json_output(root, &["read", "x.txt", "--json"], &Value::Null);
    let rev = page["revision"].as_str().unwrap();
    std::thread::scope(|threads| {
        let handles: Vec<_> = [b"left".as_slice(), b"right"]
            .into_iter()
            .map(|text| {
                threads.spawn(move || {
                    run(root, &["edit", "x.txt", "L1", "--revision", rev], text)
                        .status
                        .success()
                })
            })
            .collect();
        assert_eq!(
            handles
                .into_iter()
                .filter_map(|h| h.join().unwrap().then_some(()))
                .count(),
            1
        );
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
        "    # detail\n".repeat(400)
    );
    fs::write(&path, &source).unwrap();
    let mut event = json!({"tool_name":"Read","hook_event_name":"PostToolUse","tool_input":{"file_path":path},"tool_response":{"type":"text","file":{"filePath":path,"content":source.trim_end_matches('\n'),"startLine":1,"numLines":402,"totalLines":402}}});
    let post = |event: &Value| json_output(&root, &["hook-claude", "after-read"], event);
    let result = post(&event);
    let compact = &result["hookSpecificOutput"]["updatedToolOutput"]["file"];
    assert!(compact["content"].as_str().unwrap().contains("calculate"));
    assert!(!compact["content"].as_str().unwrap().contains("# detail"));
    assert_eq!(compact["totalLines"], 402);
    event["tool_input"]["limit"] = json!(32);
    assert_eq!(post(&event), json!({}));
    event["tool_input"].as_object_mut().unwrap().remove("limit");
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
        let masked = json_output(root, &["mask", "--keep", "1"], &original);
        let mut expected = original.clone();
        let stub = masked.pointer(pointer).unwrap();
        let text = stub.as_str().or_else(|| stub[0]["text"].as_str()).unwrap();
        let id = text
            .strip_prefix("[justless recall ")
            .unwrap()
            .strip_suffix(']')
            .unwrap();
        *expected.pointer_mut(pointer).unwrap() = stub.clone();
        assert_eq!(masked, expected);
        assert_eq!(
            json_output(root, &["recall", id, "--json"], &Value::Null),
            *original.pointer(pointer).unwrap()
        );
        assert_eq!(json_output(root, &["mask", "--restore"], &masked), original);
        assert_eq!(json_output(root, &["mask", "--keep", "1"], &masked), masked);
        for flag in [
            ["--keep-tool", "read"],
            ["--pin", "old"],
            ["--trigger-bytes", "100000"],
        ] {
            assert_eq!(
                json_output(root, &["mask", "--keep", "1", flag[0], flag[1]], &original),
                original
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
    let path = fs::read_dir(root.join(".justless/observations"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
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
fn markdown_sections_skip_fenced_headings_and_include_subsections() {
    let d = tempdir().unwrap();
    let root = d.path();
    let section = "# Install\nrun setup\n\n~~~sh\n# fake\n~~~\n## Linux\nread this\n";
    fs::write(root.join("guide.md"), format!("{section}# Use\ncommands")).unwrap();
    let page = json_output(root, &["read", "guide.md", "--json"], &Value::Null);
    assert_eq!(page["entries"].as_array().unwrap().len(), 3);
    assert_eq!(
        ok(run(root, &["expand", "guide.md", "Install"], b"")),
        section.as_bytes()
    );
    assert_eq!(
        ok(run(root, &["expand", "guide.md", "Linux"], b"")),
        b"## Linux\nread this\n"
    );
}

#[test]
fn agent_setup_preserves_existing_plugin_and_registers_claude_read_once() {
    let d = tempdir().unwrap();
    let root = d.path();
    let install = [
        "init", "--hooks", "--agent", "opencode", "--agent", "claude",
    ];
    ok(run(root, &install, b""));
    let plugin = root.join(".opencode/plugins/justless.js");
    let script = fs::read_to_string(&plugin).unwrap();
    assert!(script.contains("experimental.chat.messages.transform"));
    assert!(ok(run(root, &install, b"")).is_empty());
    let settings: Value =
        serde_json::from_slice(&fs::read(root.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(
        settings["hooks"]["PostToolUse"].as_array().unwrap().len(),
        2
    );
    assert_eq!(settings["hooks"]["PostToolUse"][0]["matcher"], "^Read$");
    assert_eq!(
        settings["hooks"]["PostToolUse"][1]["matcher"],
        "^(Grep|Glob)$"
    );
    fs::write(&plugin, "// user plugin").unwrap();
    assert!(!run(root, &install, b"").status.success());
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
    let out = run(root, &["read", "big.txt", "--outline"], b"");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("file exceeds max_file_bytes"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(root, &["read", "bad.txt"], b"");
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected UTF-8 source"));
}

#[test]
fn markdown_heading_names_drop_closing_hashes() {
    let d = tempdir().unwrap();
    let root = d.path();
    let text = "# Guide #\n\n## Setup ##\nInstall steps.\n\n## C#\nRun it.\n\n### Setup\nPer-command setup.\n";
    fs::write(root.join("doc.md"), text).unwrap();
    let page = json_output(root, &["read", "doc.md", "--json"], &Value::Null);
    let names: Vec<_> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Guide", "Setup", "C#", "Setup"]);
    assert_eq!(
        ok(run(root, &["expand", "doc.md", "Guide"], b"")),
        text.as_bytes()
    );
    let out = run(root, &["expand", "doc.md", "Setup"], b"");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ambiguous symbol"));
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
    let page = json_output(root, &["read", "lib.rs", "--json"], &Value::Null);
    let signatures: Vec<_> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["signature"].as_str().unwrap())
        .collect();
    assert_eq!(
        signatures,
        [
            "pub fn fair_value(spot: f64, vol: f64) -> f64",
            "pub enum Side"
        ]
    );
    assert_eq!(page["entries"][0]["first"], 1);
    // Doc text stays searchable and inside the editable span.
    let found = json_output(
        root,
        &["read", "lib.rs", "--json", "--match", "smile-adjusted"],
        &Value::Null,
    );
    assert_eq!(found["entries"][0]["name"], "fair_value");
    assert_eq!(
        ok(run(root, &["expand", "lib.rs", "fair_value"], b"")),
        function.as_bytes()
    );
}

#[test]
fn rust_syntax_errors_are_reported_instead_of_hiding_symbols() {
    let d = tempdir().unwrap();
    let root = d.path();
    let mut text: String = (1..=100)
        .map(|i| format!("pub fn f{i}() -> u8 {{ {i} }}\n"))
        .collect();
    fs::write(root.join("ok.rs"), &text).unwrap();
    let healthy = json_output(root, &["read", "ok.rs", "--json"], &Value::Null);
    assert!(healthy.get("parse_error").is_none());
    text.push_str("pub fn wip( {\n");
    fs::write(root.join("lib.rs"), &text).unwrap();
    let outline = String::from_utf8(ok(run(root, &["read", "lib.rs", "--outline"], b""))).unwrap();
    assert_eq!(
        outline.lines().nth(1),
        Some("Rust parse failed at line 101: cannot parse string into token stream")
    );
    let page = json_output(root, &["read", "lib.rs", "--json"], &Value::Null);
    assert!(
        page["parse_error"]
            .as_str()
            .unwrap()
            .starts_with("Rust parse failed at line 101")
    );
    let small = ok(run(root, &["read", "lib.rs", "--budget", "128"], b""));
    assert!(small.len() <= 128, "{}", String::from_utf8_lossy(&small));
    let out = run(root, &["expand", "lib.rs", "f3"], b"");
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(
        error.contains("line 101") && error.contains("line range"),
        "{error}"
    );
    assert_eq!(
        ok(run(root, &["expand", "lib.rs", "L3"], b"")),
        b"pub fn f3() -> u8 { 3 }\n"
    );
}

#[test]
fn deeply_nested_rust_reports_an_error_instead_of_overflowing_the_stack() {
    let d = tempdir().unwrap();
    let root = d.path();
    fs::write(root.join(".justless.toml"), "").unwrap();
    fs::write(root.join("ok.rs"), "pub fn hello() -> u8 { 1 }\n").unwrap();
    let deep = format!(
        "fn f() -> u8 {{ {}1{} }}\n",
        "(".repeat(3000),
        ")".repeat(3000)
    );
    let unary = format!("const B: bool = {}true;\n", "!".repeat(3000));
    let chain = format!("const X: u64 = {}1;\n", "1 + ".repeat(20000));
    fs::write(root.join("deep.rs"), &deep).unwrap();
    fs::write(root.join("unary.rs"), &unary).unwrap();
    fs::write(root.join("chain.rs"), &chain).unwrap();
    fs::write(root.join("late.rs"), "fn a() {}\n\nfn b( -> {}\n").unwrap();
    for (name, text) in [("deep.rs", &deep), ("unary.rs", &unary)] {
        let page = json_output(root, &["read", name, "--json"], &Value::Null);
        assert_eq!(
            page["parse_error"],
            "Rust parse failed at line 1: nesting deeper than 1024 levels"
        );
        // Line ranges never need the parser.
        assert_eq!(ok(run(root, &["expand", name, "L1"], b"")), text.as_bytes());
    }
    let page = json_output(root, &["read", "chain.rs", "--json"], &Value::Null);
    assert_eq!(page["entries"][0]["name"], "X");
    let page = json_output(root, &["read", "late.rs", "--json"], &Value::Null);
    assert!(
        page["parse_error"]
            .as_str()
            .unwrap()
            .starts_with("Rust parse failed at line 3: "),
        "{page}"
    );
    let found = String::from_utf8(ok(run(root, &["query", "hello"], b""))).unwrap();
    assert!(found.contains("hello [function] at ok.rs:1"), "{found}");
}

#[test]
fn typescript_method_spans_include_their_decorators() {
    let d = tempdir().unwrap();
    let root = d.path();
    let method =
        "@Get(\"/admin\")\n  // admins only\n  @UseGuards(AdminGuard)\n  admin() { return 1; }";
    let text = format!(
        "class Ctl {{\n  /** Lists users. */\n  {method}\n\n  open() {{ return 2; }}\n}}\n"
    );
    for name in ["ctl.ts", "ctl.tsx"] {
        fs::write(root.join(name), &text).unwrap();
        let found = json_output(
            root,
            &["expand", name, "Ctl::admin", "--json"],
            &Value::Null,
        );
        assert_eq!(found["content"], method, "{name}");
        assert_eq!(found["first"], 3);
        let revision = found["revision"].as_str().unwrap();
        ok(run(
            root,
            &["edit", name, "Ctl::admin", "--revision", revision],
            b"",
        ));
        // The decorators leave with the method instead of moving to open().
        assert_eq!(
            fs::read_to_string(root.join(name)).unwrap(),
            "class Ctl {\n  /** Lists users. */\n  \n\n  open() { return 2; }\n}\n"
        );
    }
}

#[test]
fn typescript_outline_signatures_skip_member_decorators() {
    let d = tempdir().unwrap();
    let root = d.path();
    // More than the 160 bytes a row shows before its signature is clipped.
    let decorators = "@ApiResponse({ status: 200, description: \"The list of users visible to the caller\" })\n  @ApiResponse({ status: 403, description: \"The caller lacks the users:read permission\" })\n  @Get(\"/users\")\n  ";
    assert!(decorators.len() > 160);
    let method = format!(
        "{decorators}listUsers(page: number, size: number): Promise<User[]> {{ return []; }}"
    );
    let text = format!("class UsersController {{\n  {method}\n}}\n");
    for name in ["u.ts", "u.tsx"] {
        fs::write(root.join(name), &text).unwrap();
        let page = json_output(root, &["read", name, "--json"], &Value::Null);
        let row = &page["entries"][1];
        assert_eq!(row["name"], "UsersController::listUsers", "{name}");
        assert_eq!(
            row["signature"], "listUsers(page: number, size: number): Promise<User[]>",
            "{name}"
        );
        // The line range and the editable span still cover the decorators.
        assert_eq!(row["first"], 2, "{name}");
        let found = json_output(
            root,
            &["expand", name, "UsersController::listUsers", "--json"],
            &Value::Null,
        );
        assert_eq!(found["content"], method.as_str(), "{name}");
        // Decorator text stays searchable.
        let matched = json_output(
            root,
            &["read", name, "--json", "--match", "users:read"],
            &Value::Null,
        );
        assert_eq!(
            matched["entries"][0]["name"], "UsersController::listUsers",
            "{name}"
        );
    }
}

#[test]
fn symbol_insertions_land_on_line_boundaries() {
    let d = tempdir().unwrap();
    let root = d.path();
    let insert = |name: &str, text: &str, args: &[&str], replacement: &str| {
        fs::write(root.join(name), text).unwrap();
        let page = json_output(root, &["read", name, "--json"], &Value::Null);
        let revision = page["revision"].as_str().unwrap();
        let mut command = vec!["edit", name];
        command.extend_from_slice(args);
        command.extend_from_slice(&["--revision", revision]);
        ok(run(root, &command, replacement.as_bytes()));
        fs::read_to_string(root.join(name)).unwrap()
    };
    assert_eq!(
        insert(
            "a.py",
            "class A:\n    def old(self):\n        return 1\n\n    def last(self):\n        return 2\n",
            &["A::old", "--after"],
            "    def new(self):\n        return 0\n",
        ),
        "class A:\n    def old(self):\n        return 1\n    def new(self):\n        return 0\n\n    def last(self):\n        return 2\n"
    );
    assert_eq!(
        insert(
            "x.rs",
            "fn a() {}\r\nfn b() {}\r\n",
            &["a", "--after"],
            "fn n() {}\r\n"
        ),
        "fn a() {}\r\nfn n() {}\r\nfn b() {}\r\n"
    );
    assert_eq!(
        insert(
            "ctl.ts",
            "class Ctl {\n  @Get(\"/a\")\n  admin() {}\n}\n",
            &["Ctl::admin", "--before"],
            "  fresh() {}\n",
        ),
        "class Ctl {\n  fresh() {}\n  @Get(\"/a\")\n  admin() {}\n}\n"
    );
    // A symbol that shares its line keeps its exact boundary.
    assert_eq!(
        insert(
            "same.rs",
            "fn one() {} fn two() {}\n",
            &["two", "--before"],
            "fn mid() {} "
        ),
        "fn one() {} fn mid() {} fn two() {}\n"
    );
    // The last line has no line ending, so the insertion goes at the end.
    assert_eq!(
        insert("end.rs", "fn a() {}", &["a", "--after"], "\nfn z() {}"),
        "fn a() {}\nfn z() {}"
    );
    // Line ranges already end at a line boundary and keep a blank next line.
    assert_eq!(
        insert("t.txt", "one\n\nthree\n", &["L1", "--after"], "two\n"),
        "one\ntwo\n\nthree\n"
    );
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
    let read = |file: &str| ok(from_crate(&["read", file], b""));
    // Without --root, a bare path resolves like a shell path and says so when
    // the root has a different file of that name.
    let shadowed = from_crate(&["read", "Cargo.toml"], b"");
    assert!(
        String::from_utf8_lossy(&shadowed.stderr)
            .contains("use ../Cargo.toml for the workspace root's file")
    );
    assert_eq!(ok(shadowed), b"[package]\n");
    assert_eq!(read("./Cargo.toml"), b"[package]\n");
    assert_eq!(read("../Cargo.toml"), b"[workspace]\n");
    assert_eq!(read("only.txt"), b"only here\n");
    // A workspace-relative path still works when the current directory lacks it.
    assert_eq!(read("crate/only.txt"), b"only here\n");
    assert_eq!(
        read(crate_dir.join("Cargo.toml").to_str().unwrap()),
        b"[package]\n"
    );
    assert!(
        !from_crate(&["read", "../../outside.txt"], b"")
            .status
            .success()
    );
    // With --root, as hooks pass it, a bare path is root-relative first.
    let hooked = from_crate(
        &["--root", root.to_str().unwrap(), "read", "Cargo.toml"],
        b"",
    );
    assert!(
        String::from_utf8_lossy(&hooked.stderr).contains("use ./Cargo.toml for crate/Cargo.toml")
    );
    assert_eq!(ok(hooked), b"[workspace]\n");
    // Every spelling of one file shares its name and revision.
    let local = from_crate(&["expand", "Cargo.toml", "L1", "--json"], b"");
    let local: Value = serde_json::from_slice(&ok(local)).unwrap();
    let rooted = json_output(
        &root,
        &["expand", "crate/Cargo.toml", "L1", "--json"],
        &Value::Null,
    );
    assert_eq!(local["file"], "crate/Cargo.toml");
    assert_eq!(local["revision"], rooted["revision"]);
    let revision = local["revision"].as_str().unwrap();
    ok(from_crate(
        &[
            "edit",
            "Cargo.toml",
            "L1",
            "--before",
            "--revision",
            revision,
        ],
        b"# added\n",
    ));
    assert_eq!(
        fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap(),
        "# added\n[package]\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("Cargo.toml")).unwrap(),
        "[workspace]\n"
    );
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
    let page = json_output(&root, &["read", name, "--json"], &Value::Null);
    let revision = page["revision"].as_str().unwrap();
    let edit = ["edit", name, "b", "--revision", revision];
    let patch = ok(run(
        &root,
        &[&edit[..], &["--dry-run"]].concat(),
        b"fn b2() {}",
    ));
    let header = String::from_utf8_lossy(&patch);
    // Git may quote the non-ASCII name, depending on core.quotePath.
    let lines: Vec<_> = header.lines().collect();
    assert!(lines[0].starts_with("diff --git ") && lines[0].contains("b/src/a b/"));
    assert!(lines[2].starts_with("--- ") && lines[2].contains("a/src/a b/"));
    assert!(lines[3].starts_with("+++ ") && lines[3].contains("b/src/a b/"));
    assert!(!header.contains(root.to_str().unwrap()) && !header.contains(".tmp"));
    assert_eq!(fs::read_to_string(root.join(name)).unwrap(), text);
    let mut apply = Command::new("git")
        .args(["apply", "-"])
        .current_dir(copy.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    apply.stdin.take().unwrap().write_all(&patch).unwrap();
    assert!(apply.wait().unwrap().success());
    ok(run(&root, &edit, b"fn b2() {}"));
    assert_eq!(
        fs::read(copy.path().join(name)).unwrap(),
        fs::read(root.join(name)).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn edits_refuse_read_only_files_and_keep_links_and_attributes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let d = tempdir().unwrap();
    let root = d.path();
    let edit = |name: &str| {
        let page = json_output(root, &["read", name, "--json"], &Value::Null);
        let revision = page["revision"].as_str().unwrap().to_owned();
        run(
            root,
            &["edit", name, "L2", "--revision", &revision],
            b"TWO\n",
        )
    };
    fs::write(root.join("ro.txt"), "one\ntwo\n").unwrap();
    fs::set_permissions(root.join("ro.txt"), fs::Permissions::from_mode(0o444)).unwrap();
    // Root may write anyway, as a shell would let it.
    if fs::File::options()
        .write(true)
        .open(root.join("ro.txt"))
        .is_err()
    {
        let out = edit("ro.txt");
        assert!(String::from_utf8_lossy(&out.stderr).contains("ro.txt is not writable"));
        assert_eq!(
            fs::read_to_string(root.join("ro.txt")).unwrap(),
            "one\ntwo\n"
        );
    }
    fs::write(root.join("linked.txt"), "one\ntwo\n").unwrap();
    fs::hard_link(root.join("linked.txt"), root.join("link.txt")).unwrap();
    let inode = fs::metadata(root.join("linked.txt")).unwrap().ino();
    ok(edit("linked.txt"));
    assert_eq!(fs::metadata(root.join("linked.txt")).unwrap().ino(), inode);
    assert_eq!(
        fs::read_to_string(root.join("link.txt")).unwrap(),
        "one\nTWO\n"
    );
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
            assert_eq!(fs::read_to_string(&tagged).unwrap(), "one\nTWO\n");
        }
    }
}
