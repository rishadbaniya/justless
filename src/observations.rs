use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};

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

pub(crate) fn archive(ws: &Workspace, value: &Value) -> Result<String> {
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
        // Plain recall prints text: a string, or the parts of an all-text array
        // joined without a separator. `--json` prints the typed value.
        if !args.is_present("json") && text_kind(&value).is_some() {
            let mut out = io::stdout().lock();
            match value.as_array() {
                Some(parts) => {
                    for part in parts {
                        out.write_all(part["text"].as_str().unwrap_or_default().as_bytes())?;
                    }
                }
                None => out.write_all(value.as_str().unwrap_or_default().as_bytes())?,
            }
            return Ok(());
        }
        return Ok(serde_json::to_writer(io::stdout().lock(), &value)?);
    }
    let mut input = Vec::new();
    io::stdin().lock().read_to_end(&mut input)?;
    let (mut request, surrogates) = decode_request(&input)?;
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
        // The archive holds valid JSON, so a result with a lone surrogate stays.
        if let Some(surrogates) = &surrogates
            && serde_json::to_string(value)?.contains(surrogates.marker)
        {
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
    let output = encode_request(&request, surrogates.as_ref())?;
    io::stdout().lock().write_all(&output)?;
    Ok(())
}

/// Unpaired UTF-16 surrogate escapes, such as `\ud83d` from a JavaScript or
/// Python string cut inside a character, replaced while the request is parsed.
struct Surrogates {
    /// A private-use character absent from the request. Each escape becomes
    /// this character followed by the escape's four hex digits.
    marker: char,
    count: usize,
}

/// Parses a request. serde_json rejects unpaired surrogate escapes, so each is
/// replaced with a marker that `encode_request` turns back into the escape.
fn decode_request(input: &[u8]) -> Result<(Value, Option<Surrogates>)> {
    let hex = |at: usize| {
        input
            .get(at..at + 4)
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u16::from_str_radix(digits, 16).ok())
    };
    let escape = |at: usize| {
        (input.get(at..at + 2) == Some(b"\\u"))
            .then(|| hex(at + 2))
            .flatten()
    };
    let mut lone = Vec::new();
    let mut i = 0;
    // A backslash appears only inside strings and always starts an escape.
    while i < input.len() {
        if input[i] != b'\\' {
            i += 1;
            continue;
        }
        match escape(i) {
            Some(0xD800..=0xDBFF) if matches!(escape(i + 6), Some(0xDC00..=0xDFFF)) => i += 12,
            Some(0xD800..=0xDFFF) => {
                lone.push(i);
                i += 6;
            }
            Some(_) => i += 6,
            None => i += 2,
        }
    }
    if lone.is_empty() {
        return Ok((
            serde_json::from_slice(input).context("decode request")?,
            None,
        ));
    }
    let lower = input.to_ascii_lowercase();
    let marker = ('\u{E000}'..='\u{F8FF}')
        .rev()
        .find(|c| {
            let escaped = format!("\\u{:04x}", *c as u32);
            !contains(input, c.encode_utf8(&mut [0; 4]).as_bytes())
                && !contains(&lower, escaped.as_bytes())
        })
        .context("decode request: no private-use character is free to mark surrogates")?;
    let mut text = Vec::with_capacity(input.len() + lone.len() * 6);
    let mut start = 0;
    for &at in &lone {
        text.extend_from_slice(&input[start..at]);
        text.extend_from_slice(format!("\\u{:04x}", marker as u32).as_bytes());
        text.extend_from_slice(&input[at + 2..at + 6]);
        start = at + 6;
    }
    text.extend_from_slice(&input[start..]);
    let request = serde_json::from_slice(&text).context("decode request")?;
    let count = lone.len();
    Ok((request, Some(Surrogates { marker, count })))
}

/// Serializes a request, restoring each surrogate escape `decode_request` marked.
fn encode_request(request: &Value, surrogates: Option<&Surrogates>) -> Result<Vec<u8>> {
    let output = serde_json::to_vec(request)?;
    let Some(surrogates) = surrogates else {
        return Ok(output);
    };
    let mut buffer = [0; 4];
    let marker = surrogates.marker.encode_utf8(&mut buffer).as_bytes();
    let mut restored = Vec::with_capacity(output.len());
    let mut count = 0;
    let mut i = 0;
    while i < output.len() {
        let digits = output[i..]
            .strip_prefix(marker)
            .and_then(|rest| rest.get(..4))
            .filter(|digits| digits.iter().all(u8::is_ascii_hexdigit));
        if let Some(digits) = digits {
            restored.extend_from_slice(b"\\u");
            restored.extend_from_slice(digits);
            count += 1;
            i += marker.len() + 4;
        } else {
            restored.push(output[i]);
            i += 1;
        }
    }
    ensure!(
        count == surrogates.count,
        "restored results contain U+{:04X}, which marks surrogate escapes in this request",
        surrogates.marker as u32
    );
    Ok(restored)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
