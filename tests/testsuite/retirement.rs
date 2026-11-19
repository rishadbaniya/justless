//! `justless mask` leaves history alone for Claude models that tie thinking to the
//! earlier turns, and keeps retiring results everywhere else.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::tempdir;

use crate::common::{BIN, piped, text};

fn run(root: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut command = Command::new(BIN);
    command
        .current_dir(root)
        .args(["--root", root.to_str().unwrap()]);
    let out = piped(command.args(args), input);
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    out
}

/// Masks `input` with `--stats` and returns the output and the held reason.
fn mask(root: &Path, extra: &[&str], input: &[u8]) -> (Vec<u8>, Value) {
    let mut args = vec!["mask", "--keep", "1", "--stats"];
    args.extend_from_slice(extra);
    let out = run(root, &args, input);
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

/// Where `messages` keeps the old result.
const OLD_RESULT: &str = "/messages/2/content/0/content";

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
fn mask_holds_requests_by_model_and_reasoning_and_retires_the_rest() {
    let dir = tempdir().unwrap();
    // Pretty-printed with unusual spacing: a held request comes back byte for byte.
    let pretty = serde_json::to_string_pretty(&messages(Some("claude-opus-5-5"), true)).unwrap();
    let pretty = format!("{pretty}\n\n").replace("\"role\": ", "\"role\" :  ");
    let (output, held) = mask(dir.path(), &[], pretty.as_bytes());
    assert_eq!(output, pretty.as_bytes());
    assert_eq!(held, "preserved-thinking");
    assert!(!dir.path().join(".justless/observations").exists());

    // A held request comes back unchanged with its reason; otherwise the old result
    // at `pointer` is retired.
    let check = |case: &str, request: &Value, flags: &[&str], held: Option<&str>, pointer: &str| {
        let input = serde_json::to_vec(request).unwrap();
        let (output, reason) = mask(dir.path(), flags, &input);
        match held {
            Some(held) => assert_eq!((output, reason), (input, json!(held)), "{case}"),
            None => {
                assert!(retired(&output, pointer), "{case}");
                assert_eq!(reason, Value::Null, "{case}");
            }
        }
    };
    for (model, held) in [
        ("claude-opus-5-5", "preserved-thinking"),
        ("claude-sonnet-5-5", "preserved-thinking"),
        ("claude-haiku-5-5", "preserved-thinking"),
        ("claude-fable-5-2", "preserved-thinking"),
        ("claude-opus-6", "preserved-thinking"),
        ("sonnet-prod", "unknown-model"),
        ("deepseek-chat", "unknown-model"),
    ] {
        check(model, &messages(Some(model), false), &[], Some(held), "");
    }
    // Provider names are normalized before the check.
    let thinking = Some("preserved-thinking");
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
        let held = if retire { None } else { thinking };
        check(model, &chat(model), &[], held, "/messages/1/content");
    }
    // OpenCode requests name no model, so `--model` has to.
    for (model, held) in [
        (None, Some("model-required")),
        (Some("openai/gpt-5.5"), None),
        (Some("anthropic/claude-opus-5-5"), thinking),
        (Some("anthropic/claude-haiku-4-5"), None),
        (Some("databricks/databricks-claude-opus-5-5"), thinking),
    ] {
        let flags = model.map_or(vec![], |model| vec!["--model", model]);
        let pointer = "/messages/0/parts/0/state/output";
        check(&format!("{model:?}"), &opencode(), &flags, held, pointer);
    }
    // Without a model, a request is held only when it carries reasoning.
    let responses = json!({"input":[
        {"type":"function_call","call_id":"old","name":"read"},
        {"type":"reasoning","id":"rs_1","encrypted_content":"x"},
        {"type":"function_call_output","call_id":"old","output":big()},
        {"type":"function_call_output","call_id":"recent","output":big()}
    ]});
    let mut details = chat("x");
    details.as_object_mut().unwrap().remove("model");
    details["messages"][0]["reasoning_details"] = json!([{"type":"reasoning.encrypted"}]);
    let reasoning = Some("reasoning-present");
    check("thinking", &messages(None, true), &[], reasoning, "");
    check("no thinking", &messages(None, false), &[], None, OLD_RESULT);
    check("responses", &responses, &[], reasoning, "");
    check("reasoning_details", &details, &[], reasoning, "");
    // `--model` overrides the request's model.
    let older = messages(Some("claude-sonnet-4-6"), false);
    let flags = ["--model", "claude-opus-5-5"];
    check("--model", &older, &flags, thinking, "");
}

#[test]
fn older_claude_models_retire_as_before() {
    let dir = tempdir().unwrap();
    for model in ["claude-sonnet-4-6", "claude-opus-5"] {
        let request = messages(Some(model), true);
        let (output, held) = mask(dir.path(), &[], &serde_json::to_vec(&request).unwrap());
        assert_eq!(held, Value::Null, "{model}");
        let masked: Value = serde_json::from_slice(&output).unwrap();
        let stub = masked.pointer(OLD_RESULT).and_then(Value::as_str).unwrap();
        let id = stub
            .strip_prefix("[justless recall ")
            .and_then(|s| s.strip_suffix(']'))
            .unwrap();
        let recalled = run(dir.path(), &["recall", id], b"");
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
    let mut second = first.clone();
    let list = second["messages"].as_array_mut().unwrap();
    list.push(json!({"role":"assistant","content":[
        {"type":"thinking","thinking":"","signature":"EqQBCkgIBxABGAIiQM"},
        {"type":"tool_use","id":"toolu_third","name":"Grep","input":{"pattern":"fn"}}
    ]}));
    list.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_third","content":big()}]}));
    let (masked_second, _) = mask(dir.path(), &[], &serde_json::to_vec(&second).unwrap());
    assert!(retired(&masked_first, OLD_RESULT));
    assert!(retired(&masked_second, "/messages/4/content/0/content"));
    let stub = |masked: &[u8]| {
        let value: Value = serde_json::from_slice(masked).unwrap();
        serde_json::to_string(value.pointer(OLD_RESULT).unwrap()).unwrap()
    };
    assert_eq!(stub(&masked_first), stub(&masked_second));
}

#[test]
fn restore_is_never_held_and_model_flag_overrides_the_request() {
    let dir = tempdir().unwrap();
    let request = messages(Some("claude-opus-5-5"), true);
    let input = serde_json::to_vec(&request).unwrap();
    let (output, held) = mask(dir.path(), &["--model", "claude-sonnet-4-6"], &input);
    assert_eq!(held, Value::Null);
    assert!(retired(&output, OLD_RESULT));
    // The masked request still names a held model, but --restore always expands.
    let restored = run(dir.path(), &["mask", "--restore"], &output);
    assert_eq!(
        serde_json::from_slice::<Value>(&restored.stdout).unwrap(),
        request
    );
}

/// Runs `script` as an ES module under Node with `env`, or `None` without Node.
#[cfg(unix)]
fn node(script: &str, env: &[(&str, String)]) -> Option<String> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: node is not on PATH");
        return None;
    }
    let mut command = Command::new("node");
    command.args(["--input-type=module", "-e", script]);
    let out = command
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    Some(text(&out.stdout))
}

#[cfg(unix)]
fn plugin_url(path: &Path) -> (&'static str, String) {
    ("PLUGIN", format!("file://{}", path.display()))
}

#[cfg(unix)]
fn opencode_plugin() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/opencode.js")
}

#[cfg(unix)]
#[test]
fn opencode_plugin_passes_the_selected_model() {
    let dir = tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let argv = dir.path().join("argv");
    let fake = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat\n",
        argv.display()
    );
    crate::common::executable(&bin.join("justless"), &fake);
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
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut env = vec![
        ("PATH", path),
        plugin_url(&opencode_plugin()),
        ("PROJECT", dir.path().display().to_string()),
    ];
    if node(script, &env).is_none() {
        return;
    }
    assert!(!argv.exists(), "mask ran without a model");
    env.push(("WITH_MODEL", "1".to_owned()));
    node(script, &env);
    let args = std::fs::read_to_string(&argv).unwrap();
    assert!(
        args.contains("mask\n--model\nanthropic/claude-opus-5-5\n"),
        "{args}"
    );
}

#[cfg(unix)]
#[test]
fn opencode_plugin_compacts_only_bash_results_that_exited_zero() {
    let dir = tempdir().unwrap();
    let calls = dir.path().join("calls");
    // Not on PATH: the plugin must find it through JUSTLESS_BIN.
    let fake = dir.path().join("fake-justless");
    let reply = r#"{"hookSpecificOutput":{"updatedToolOutput":{"stdout":"compacted"}}}"#;
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncat > /dev/null\nprintf '%s' '{reply}'\n",
        calls.display()
    );
    crate::common::executable(&fake, &script);
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
    let env = [
        ("JUSTLESS_BIN", fake.display().to_string()),
        plugin_url(&opencode_plugin()),
        ("PROJECT", dir.path().display().to_string()),
    ];
    let Some(out) = node(script, &env) else {
        return;
    };
    assert_eq!(out.trim(), "raw raw raw raw compacted");
    let calls = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(calls.lines().count(), 1, "{calls}");
}

#[cfg(unix)]
#[test]
fn opencode_tool_replays_logs_only_redacted() {
    let dir = tempdir().unwrap();
    // A stand-in for OpenCode's plugin package, next to a copy of the plugin.
    let package = dir.path().join("node_modules/@opencode-ai/plugin");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"@opencode-ai/plugin","type":"module","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        package.join("index.js"),
        "const any = { optional: () => any };\nexport function tool(definition) { return definition; }\ntool.schema = { enum: () => any, array: () => any, string: () => any };\n",
    )
    .unwrap();
    let plugin = dir.path().join("plugin.mjs");
    std::fs::copy(opencode_plugin(), &plugin).unwrap();
    let fake = dir.path().join("fake-justless");
    crate::common::executable(&fake, "#!/bin/sh\nprintf '%s ' \"$@\"\n");
    let script = r#"
        const { Justless } = await import(process.env.PLUGIN);
        const { justless } = (await Justless({ directory: "/project" })).tool;
        const seen = [];
        for (const args of [["run-a"], ["run-a", "--unredacted"], ["run-a", "--unredacted=1"]]) {
          try { seen.push((await justless.execute({ command: "log", args })).trim()); }
          catch { seen.push("refused"); }
        }
        console.log(seen.join("|"));
    "#;
    let env = [
        ("JUSTLESS_BIN", fake.display().to_string()),
        plugin_url(&plugin),
    ];
    let Some(out) = node(script, &env) else {
        return;
    };
    assert_eq!(out.trim(), "--root /project log run-a|refused|refused");
}
