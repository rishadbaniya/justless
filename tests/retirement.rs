//! `justless mask` leaves history alone for Claude models that tie thinking to the
//! earlier turns, and keeps retiring results everywhere else.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::tempdir;

fn run(root: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
        .current_dir(root)
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn ok(out: Output) -> Output {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Masks `input` with `--stats` and returns the output and the held reason.
fn mask(root: &Path, extra: &[&str], input: &[u8]) -> (Vec<u8>, Value) {
    let mut args = vec!["mask", "--keep", "1", "--stats"];
    args.extend_from_slice(extra);
    let out = ok(run(root, &args, input));
    let stats: Value = serde_json::from_slice(&out.stderr).unwrap();
    (out.stdout, stats["held"].clone())
}

fn big() -> String {
    "line of tool output\n".repeat(200)
}

/// A Messages conversation with an old result, then thinking, then a recent result.
fn messages(model: Option<&str>, thinking: bool) -> Value {
    let mut call =
        vec![json!({"type":"tool_use","id":"toolu_new","name":"Bash","input":{"command":"ls"}})];
    if thinking {
        call.insert(
            0,
            json!({"type":"thinking","thinking":"","signature":"EqQBCkgIBxABGAIiQL"}),
        );
    }
    let mut request = json!({"max_tokens":1024,"messages":[
        {"role":"user","content":"look around"},
        {"role":"assistant","content":[{"type":"tool_use","id":"toolu_old","name":"Read","input":{"file_path":"a.rs"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_old","content":big()}]},
        {"role":"assistant","content":call},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_new","content":big()}]}
    ]});
    if let Some(model) = model {
        request["model"] = json!(model);
    }
    request
}

fn chat(model: &str) -> Value {
    json!({"model":model,"messages":[
        {"role":"assistant","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"old","content":big()},
        {"role":"tool","tool_call_id":"recent","content":big()}
    ]})
}

fn opencode() -> Value {
    json!({"messages":[{"info":{"role":"assistant"},"parts":[
        {"type":"tool","callID":"old","tool":"read","state":{"status":"completed","output":big()}},
        {"type":"tool","callID":"recent","tool":"run","state":{"status":"completed","output":big()}}
    ]}]})
}

fn retired(output: &[u8], pointer: &str) -> bool {
    let value: Value = serde_json::from_slice(output).unwrap();
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .is_some_and(|text| text.starts_with("[justless recall "))
}

#[test]
fn preserved_thinking_model_requests_are_unchanged() {
    let dir = tempdir().unwrap();
    // Pretty-printed with unusual spacing: a held request comes back byte for byte.
    let input = format!(
        "{}\n\n",
        serde_json::to_string_pretty(&messages(Some("claude-opus-5-5"), true)).unwrap()
    )
    .replace("\"role\": ", "\"role\" :  ");
    let (output, held) = mask(dir.path(), &[], input.as_bytes());
    assert_eq!(output, input.as_bytes());
    assert_eq!(held, "preserved-thinking");
    assert!(!dir.path().join(".justless/observations").exists());
}

#[test]
fn same_model_without_thinking_is_still_unchanged() {
    let dir = tempdir().unwrap();
    for model in [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-2",
    ] {
        let input = serde_json::to_vec(&messages(Some(model), false)).unwrap();
        let (output, held) = mask(dir.path(), &[], &input);
        assert_eq!(output, input, "{model}");
        assert_eq!(held, "preserved-thinking", "{model}");
    }
}

#[test]
fn older_claude_models_retire_as_before() {
    let dir = tempdir().unwrap();
    for model in ["claude-sonnet-4-6", "claude-opus-5"] {
        let request = messages(Some(model), true);
        let (output, held) = mask(dir.path(), &[], &serde_json::to_vec(&request).unwrap());
        assert_eq!(held, Value::Null, "{model}");
        let masked: Value = serde_json::from_slice(&output).unwrap();
        let stub = masked
            .pointer("/messages/2/content/0/content")
            .and_then(Value::as_str)
            .unwrap();
        let id = stub
            .strip_prefix("[justless recall ")
            .and_then(|s| s.strip_suffix(']'))
            .unwrap();
        let recalled = ok(run(dir.path(), &["recall", id], b""));
        assert_eq!(recalled.stdout, big().as_bytes());
        // The thinking block and the newest result are untouched.
        assert_eq!(masked["messages"][3], request["messages"][3]);
        assert_eq!(masked["messages"][4], request["messages"][4]);
    }
}

#[test]
fn retired_results_stay_identical_as_the_conversation_grows() {
    let dir = tempdir().unwrap();
    let first = messages(Some("claude-opus-4-8"), true);
    let (masked_first, _) = mask(dir.path(), &[], &serde_json::to_vec(&first).unwrap());
    let masked_first: Value = serde_json::from_slice(&masked_first).unwrap();
    let mut second = first.clone();
    let list = second["messages"].as_array_mut().unwrap();
    list.push(json!({"role":"assistant","content":[
        {"type":"thinking","thinking":"","signature":"EqQBCkgIBxABGAIiQM"},
        {"type":"tool_use","id":"toolu_third","name":"Grep","input":{"pattern":"fn"}}
    ]}));
    list.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_third","content":big()}]}));
    let (masked_second, _) = mask(dir.path(), &[], &serde_json::to_vec(&second).unwrap());
    let masked_second: Value = serde_json::from_slice(&masked_second).unwrap();
    let pointer = "/messages/2/content/0/content";
    assert!(retired(
        &serde_json::to_vec(&masked_first).unwrap(),
        pointer
    ));
    assert_eq!(
        serde_json::to_string(masked_first.pointer(pointer).unwrap()).unwrap(),
        serde_json::to_string(masked_second.pointer(pointer).unwrap()).unwrap()
    );
    assert!(retired(
        &serde_json::to_vec(&masked_second).unwrap(),
        "/messages/4/content/0/content"
    ));
}

#[test]
fn unknown_and_alias_models_are_unchanged() {
    let dir = tempdir().unwrap();
    for (model, reason) in [
        ("claude-opus-6", "preserved-thinking"),
        ("sonnet-prod", "unknown-model"),
        ("deepseek-chat", "unknown-model"),
    ] {
        let input = serde_json::to_vec(&messages(Some(model), false)).unwrap();
        let (output, held) = mask(dir.path(), &[], &input);
        assert_eq!(output, input, "{model}");
        assert_eq!(held, reason, "{model}");
    }
}

#[test]
fn provider_names_are_normalized_before_the_check() {
    let dir = tempdir().unwrap();
    for (model, retire) in [
        ("openrouter/anthropic/claude-sonnet-4.5", true),
        ("anthropic/claude-sonnet-5-5", false),
        ("us.anthropic.claude-opus-5-5", false),
        ("us.anthropic.claude-opus-4-6-v1:0", true),
        ("gpt-5.5", true),
        // Gateways that prefix the first-party name with a dash or a colon.
        ("databricks-claude-opus-5-5", false),
        ("databricks/databricks-claude-opus-5-5", false),
        ("anthropic:claude-opus-5-5", false),
        ("databricks-claude-sonnet-4-5", true),
    ] {
        let input = serde_json::to_vec(&chat(model)).unwrap();
        let (output, held) = mask(dir.path(), &[], &input);
        assert_eq!(retired(&output, "/messages/1/content"), retire, "{model}");
        if !retire {
            assert_eq!(output, input, "{model}");
            assert_eq!(held, "preserved-thinking", "{model}");
        }
    }
}

#[test]
fn opencode_requests_require_a_model() {
    let dir = tempdir().unwrap();
    let input = serde_json::to_vec(&opencode()).unwrap();
    let pointer = "/messages/0/parts/0/state/output";
    let (output, held) = mask(dir.path(), &[], &input);
    assert_eq!(
        (output.as_slice(), held),
        (input.as_slice(), json!("model-required"))
    );
    let (output, held) = mask(dir.path(), &["--model", "openai/gpt-5.5"], &input);
    assert!(retired(&output, pointer));
    assert_eq!(held, Value::Null);
    let (output, held) = mask(
        dir.path(),
        &["--model", "anthropic/claude-opus-5-5"],
        &input,
    );
    assert_eq!(
        (output.as_slice(), held),
        (input.as_slice(), json!("preserved-thinking"))
    );
    let (output, _) = mask(
        dir.path(),
        &["--model", "anthropic/claude-haiku-4-5"],
        &input,
    );
    assert!(retired(&output, pointer));
    let (output, held) = mask(
        dir.path(),
        &["--model", "databricks/databricks-claude-opus-5-5"],
        &input,
    );
    assert_eq!(
        (output.as_slice(), held),
        (input.as_slice(), json!("preserved-thinking"))
    );
}

#[test]
fn model_less_requests_are_held_only_with_reasoning() {
    let dir = tempdir().unwrap();
    let input = serde_json::to_vec(&messages(None, true)).unwrap();
    let (output, held) = mask(dir.path(), &[], &input);
    assert_eq!((output, held), (input, json!("reasoning-present")));
    let input = serde_json::to_vec(&messages(None, false)).unwrap();
    let (output, held) = mask(dir.path(), &[], &input);
    assert!(retired(&output, "/messages/2/content/0/content"));
    assert_eq!(held, Value::Null);
    let responses = json!({"input":[
        {"type":"function_call","call_id":"old","name":"read"},
        {"type":"reasoning","id":"rs_1","encrypted_content":"x"},
        {"type":"function_call_output","call_id":"old","output":big()},
        {"type":"function_call_output","call_id":"recent","output":big()}
    ]});
    let input = serde_json::to_vec(&responses).unwrap();
    let (output, held) = mask(dir.path(), &[], &input);
    assert_eq!((output, held), (input, json!("reasoning-present")));
    let mut with_details = chat("x");
    with_details.as_object_mut().unwrap().remove("model");
    with_details["messages"][0]["reasoning_details"] = json!([{"type":"reasoning.encrypted"}]);
    let input = serde_json::to_vec(&with_details).unwrap();
    let (output, held) = mask(dir.path(), &[], &input);
    assert_eq!((output, held), (input, json!("reasoning-present")));
}

#[test]
fn restore_is_never_held_and_model_flag_overrides_the_request() {
    let dir = tempdir().unwrap();
    let request = messages(Some("claude-opus-5-5"), true);
    let input = serde_json::to_vec(&request).unwrap();
    let (output, held) = mask(dir.path(), &["--model", "claude-sonnet-4-6"], &input);
    assert_eq!(held, Value::Null);
    assert!(retired(&output, "/messages/2/content/0/content"));
    // The masked request still names a held model, but --restore always expands.
    let restored = ok(run(dir.path(), &["mask", "--restore"], &output));
    assert_eq!(
        serde_json::from_slice::<Value>(&restored.stdout).unwrap(),
        request
    );
    let older = serde_json::to_vec(&messages(Some("claude-sonnet-4-6"), false)).unwrap();
    let (output, held) = mask(dir.path(), &["--model", "claude-opus-5-5"], &older);
    assert_eq!((output, held), (older, json!("preserved-thinking")));
}

#[cfg(unix)]
#[test]
fn opencode_plugin_passes_the_selected_model() {
    use std::os::unix::fs::PermissionsExt;
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: node is not on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let argv = dir.path().join("argv");
    let fake = bin.join("justless");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat\n",
            argv.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/opencode.js");
    let script = r#"
        const { Justless } = await import(process.env.PLUGIN);
        const hooks = await Justless({ directory: process.env.PROJECT });
        const tool = (id) => ({ type: "tool", callID: id, tool: "read",
          state: { status: "completed", output: "x".repeat(400) } });
        const messages = [
          { info: { role: "assistant", providerID: "openai", modelID: "gpt-5.5" },
            parts: [tool("a"), tool("b"), tool("c"), tool("d")] },
        ];
        if (process.env.WITH_MODEL) {
          messages.push({ info: { role: "user",
            model: { providerID: "anthropic", modelID: "claude-opus-5-5" } }, parts: [] });
        } else {
          delete messages[0].info.providerID;
          delete messages[0].info.modelID;
        }
        await hooks["experimental.chat.messages.transform"]({}, { messages });
    "#;
    let node = |with_model: bool| {
        let mut command = Command::new("node");
        command
            .args(["--input-type=module", "-e", script])
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("PLUGIN", format!("file://{}", plugin.display()))
            .env("PROJECT", dir.path());
        if with_model {
            command.env("WITH_MODEL", "1");
        }
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    node(false);
    assert!(!argv.exists(), "mask ran without a model");
    node(true);
    let args = std::fs::read_to_string(&argv).unwrap();
    assert!(
        args.contains("mask\n--model\nanthropic/claude-opus-5-5\n"),
        "{args}"
    );
}

#[cfg(unix)]
#[test]
fn opencode_plugin_compacts_only_bash_results_that_exited_zero() {
    use std::os::unix::fs::PermissionsExt;
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: node is not on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let calls = dir.path().join("calls");
    // Not on PATH: the plugin must find it through JUSTLESS_BIN.
    let fake = dir.path().join("fake-justless");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncat > /dev/null\nprintf '%s' '{{\"hookSpecificOutput\":{{\"updatedToolOutput\":{{\"stdout\":\"compacted\"}}}}}}'\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/opencode.js");
    // A killed command reports a null exit, and a result without metadata has no
    // known status: both stay raw like a failure.
    let script = r#"
        const { Justless } = await import(process.env.PLUGIN);
        const hooks = await Justless({ directory: process.env.PROJECT });
        const seen = [];
        for (const metadata of [{ exit: 1 }, { exit: null }, {}, undefined, { exit: 0 }]) {
          const output = { output: "x".repeat(5000), metadata };
          await hooks["tool.execute.after"]({ tool: "bash", args: { command: "cat big" } }, output);
          seen.push(output.output.length === 5000 ? "raw" : output.output);
        }
        console.log(seen.join(" "));
    "#;
    let out = Command::new("node")
        .args(["--input-type=module", "-e", script])
        .env("JUSTLESS_BIN", &fake)
        .env("PLUGIN", format!("file://{}", plugin.display()))
        .env("PROJECT", dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "raw raw raw raw compacted"
    );
    let calls = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(calls.lines().count(), 1, "{calls}");
}
