use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};

use anyhow::{Context, Result, ensure};
use clap::{Arg, ArgMatches, Command};
use serde_json::{Value, json};

use crate::capture::source_digest;
use crate::workspace::{Workspace, private_dir, write_json};

pub fn commands() -> Vec<Command<'static>> {
    vec![
        Command::new("mask").about("Replace older tool outputs in an outbound model request with recoverable references")
            .arg(crate::number("keep", "3", "Number of newest tool results to retain"))
            .arg(crate::number("min-bytes", "1024", "Smallest result eligible for retirement"))
            .arg(crate::number("trigger-bytes", "0", "Only retire once eligible tool data reaches this size"))
            .arg(Arg::new("keep-tool").long("keep-tool").value_name("NAME").multiple_occurrences(true))
            .arg(Arg::new("pin").long("pin").value_name("CALL_ID").multiple_occurrences(true))
            .arg(Arg::new("stats").long("stats").help("Write byte counts to stderr"))
            .arg(Arg::new("restore").long("restore").help("Expand all archived references in a request")),
        Command::new("recall").about("Recover an archived tool result")
            .arg(Arg::new("id").required(true)).arg(Arg::new("json").long("json").help("Emit the original typed JSON value")),
    ]
}

const PREFIX: &str = "[justless recall ";

fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn recover(ws: &Workspace, id: &str) -> Result<Value> {
    ensure!(valid_id(id), "invalid observation ID");
    let path = ws.source_path(&format!(".justless/observations/{id}.json"))?;
    let bytes = fs::read(path)?;
    ensure!(source_digest(&bytes) == id, "observation checksum mismatch");
    Ok(serde_json::from_slice(&bytes)?)
}

fn archive(ws: &Workspace, value: &Value) -> Result<String> {
    let id = source_digest(&serde_json::to_vec(value)?);
    ws.prepare_state()?;
    let directory = ws.state.join("observations");
    private_dir(&directory)?;
    let path = directory.join(format!("{id}.json"));
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            recover(ws, &id)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => write_json(&path, value)?,
        Err(error) => return Err(error.into()),
    }
    Ok(id)
}

// JSON pointers refer only to array indices and fixed field names; no user-provided keys.
struct Output {
    path: String,
    call: String,
    tool: String,
    protected: bool,
}

fn outputs(request: &Value) -> Result<Vec<Output>> {
    let (key, messages) = if let Some(messages) = request["messages"].as_array() {
        ("messages", messages)
    } else if let Some(items) = request["input"].as_array() {
        ("input", items)
    } else if request["input"].is_string() {
        return Ok(Vec::new());
    } else {
        anyhow::bail!("expected a request with messages or input");
    };
    let mut names = HashMap::new();
    let mut out = vec![];
    for (i, m) in messages.iter().enumerate() {
        let base = format!("/{key}/{i}");
        let mut add = |path: String, call: &Value, tool: &Value, protected: bool| {
            if let Some(id) = call.as_str() {
                out.push(Output {
                    path,
                    call: id.to_owned(),
                    tool: tool.as_str().unwrap_or("").to_owned(),
                    protected,
                });
            }
        };
        if key == "input" {
            match m["type"].as_str() {
                Some("function_call") => {
                    names.insert(m["call_id"].as_str(), &m["name"]);
                }
                Some("function_call_output") => add(
                    format!("{base}/output"),
                    &m["call_id"],
                    &Value::Null,
                    m["status"] == "incomplete",
                ),
                _ => {}
            }
            continue;
        }
        if m["info"]["role"] == "assistant"
            && let Some(parts) = m["parts"].as_array()
        {
            for (j, p) in parts
                .iter()
                .enumerate()
                .filter(|(_, p)| p["type"] == "tool")
            {
                let state = &p["state"];
                if matches!(state["status"].as_str(), Some("completed" | "error")) {
                    let attached = !matches!(state.get("attachments"), None | Some(Value::Null))
                        && !state["attachments"].as_array().is_some_and(Vec::is_empty);
                    add(
                        format!("{base}/parts/{j}/state/output"),
                        &p["callID"],
                        &p["tool"],
                        attached || state["status"] == "error",
                    );
                }
            }
        }
        if m["role"] == "assistant" {
            for call in m["tool_calls"].as_array().into_iter().flatten() {
                names.insert(call["id"].as_str(), &call["function"]["name"]);
            }
            for block in m["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
            {
                names.insert(block["id"].as_str(), &block["name"]);
            }
        } else if m["role"] == "tool" {
            add(
                format!("{base}/content"),
                &m["tool_call_id"],
                &m["name"],
                false,
            );
        } else if m["role"] == "user"
            && let Some(blocks) = m["content"].as_array()
        {
            for (j, b) in blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| b["type"] == "tool_result")
            {
                add(
                    format!("{base}/content/{j}/content"),
                    &b["tool_use_id"],
                    &Value::Null,
                    b["is_error"] == true,
                );
            }
        }
    }
    for o in &mut out {
        if o.tool.is_empty()
            && let Some(name) = names.get(&Some(o.call.as_str())).and_then(|v| v.as_str())
        {
            o.tool = name.to_owned();
        }
    }
    Ok(out)
}

fn text_kind(value: &Value) -> Option<&str> {
    if value.is_string() {
        return Some("");
    }
    let parts = value.as_array()?;
    let kind = parts.first()?["type"].as_str()?;
    if !matches!(kind, "text" | "input_text" | "output_text") {
        return None;
    }
    parts
        .iter()
        .all(|p| {
            p["type"] == kind
                && p["text"].is_string()
                && p.as_object().is_some_and(|o| o.len() == 2)
        })
        .then_some(kind)
}

fn marker(value: &Value) -> Option<&str> {
    let text = if let Some(s) = value.as_str() {
        s
    } else {
        let parts = value.as_array()?;
        if parts.len() != 1 || text_kind(value).is_none() {
            return None;
        }
        parts[0]["text"].as_str()?
    };
    text.strip_prefix(PREFIX)?
        .strip_suffix(']')
        .filter(|id| valid_id(id))
}

pub fn run(ws: &Workspace, name: &str, args: &ArgMatches) -> Result<()> {
    if name == "recall" {
        let value = recover(ws, args.value_of("id").unwrap())?;
        if !args.is_present("json")
            && let Some(text) = value.as_str()
        {
            return Ok(io::stdout().lock().write_all(text.as_bytes())?);
        }
        return Ok(serde_json::to_writer(io::stdout().lock(), &value)?);
    }
    let mut request: Value =
        serde_json::from_reader(io::stdin().lock()).context("decode request")?;
    let candidates = outputs(&request)?;
    let keep = crate::read_num(args, "keep", 0)?;
    let minimum = crate::read_num(args, "min-bytes", 0)?;
    let trigger = crate::read_num(args, "trigger-bytes", 0)?;
    let cutoff = candidates.len().saturating_sub(keep);
    let eligible = |index: usize, c: &Output| {
        index < cutoff
            && !c.protected
            && !args
                .values_of("pin")
                .into_iter()
                .flatten()
                .any(|s| s == c.call)
            && !args
                .values_of("keep-tool")
                .into_iter()
                .flatten()
                .any(|s| s == c.tool)
    };
    let mut changes = Vec::new();
    let mut eligible_bytes: usize = 0;
    let mut saved_bytes: i128 = 0;
    for (index, c) in candidates.iter().enumerate() {
        let Some(value) = request.pointer(&c.path) else {
            continue;
        };
        if args.is_present("restore") {
            if !c.protected
                && let Some(id) = marker(value)
            {
                changes.push((&c.path, recover(ws, id)?));
            }
            continue;
        }
        if !eligible(index, c) || marker(value).is_some() {
            continue;
        }
        let Some(kind) = text_kind(value) else {
            continue;
        };
        let bytes = serde_json::to_vec(value)?.len();
        if bytes < minimum {
            continue;
        }
        let placeholder = format!("{PREFIX}{}]", "0".repeat(64));
        let replacement = if kind.is_empty() {
            json!(placeholder)
        } else {
            json!([{"type":kind,"text":placeholder}])
        };
        let cost = serde_json::to_vec(&replacement)?.len();
        if cost >= bytes {
            continue;
        }
        eligible_bytes += bytes;
        changes.push((&c.path, replacement));
    }
    if !args.is_present("restore") && eligible_bytes < trigger {
        changes.clear();
    }
    let count = changes.len();
    for (path, mut value) in changes {
        let original = request.pointer_mut(path).context("missing result field")?;
        if !args.is_present("restore") {
            let id = archive(ws, original)?;
            let stub = json!(format!("{PREFIX}{id}]"));
            if value.is_string() {
                value = stub;
            } else {
                value[0]["text"] = stub;
            }
        }
        saved_bytes +=
            serde_json::to_vec(original)?.len() as i128 - serde_json::to_vec(&value)?.len() as i128;
        *original = value;
    }
    if args.is_present("stats") {
        eprintln!("{}", json!({"changed":count,"bytes_saved":saved_bytes}));
    }
    serde_json::to_writer(io::stdout().lock(), &request)?;
    Ok(())
}
