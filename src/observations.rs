use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail, ensure};
use clap::{Arg, ArgGroup, ArgMatches, Command};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::capture::source_digest;
use crate::history::{age, human_bytes, now_ms};
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
            .arg(Arg::new("restore").long("restore").help("Expand all archived references in a request"))
            .arg(Arg::new("model").long("model").value_name("ID").help("Model the request is for; overrides its `model` field")),
        Command::new("recall").about("Recover an archived tool result")
            .arg(Arg::new("id").value_name("ID").help("Recall ID from a placeholder, or a tool_use ID the proxy archived"))
            .arg(Arg::new("tool-use").long("tool-use").value_name("TOOL_USE_ID").help("Print a result the proxy archived, by its tool_use ID"))
            .arg(Arg::new("list").long("list").value_name("TEXT").min_values(0).max_values(1).help("List archived tool results whose tool or call matches TEXT"))
            .arg(crate::number("limit", "20", "Maximum rows for --list or matching lines for --grep"))
            .arg(Arg::new("from").long("from").value_name("N").help("First line to print, inclusive"))
            .arg(Arg::new("to").long("to").value_name("N").help("Last line to print, inclusive"))
            .arg(Arg::new("grep").long("grep").value_name("TEXT").help("Print only numbered lines that contain TEXT"))
            .arg(Arg::new("json").long("json").conflicts_with_all(&["list", "from", "to", "grep"]).help("Emit the original typed JSON value"))
            .group(ArgGroup::new("target").required(true).args(&["id", "tool-use", "list"])),
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

/// An archived tool result, indexed by the tool_use ID the request carried it under.
#[derive(Serialize, Deserialize)]
struct ToolUse {
    id: String,
    observation: String,
    tool: String,
    call: String,
    /// The call's compact input JSON, cut near 512 bytes, for `--list` matching.
    #[serde(default)]
    input: String,
    bytes: u64,
    is_error: bool,
    first_seen_ms: u64,
}

/// What `recall` prints of a text result: all of it, a line range, or the
/// numbered lines that contain some text.
struct Selection<'a> {
    from: usize,
    to: usize,
    grep: Option<&'a str>,
    limit: usize,
}

impl<'a> Selection<'a> {
    fn of(args: &'a ArgMatches) -> Result<Self> {
        let line = |name: &str| -> Result<Option<usize>> {
            args.value_of(name)
                .map(|value| {
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|&n| n > 0)
                        .with_context(|| format!("--{name} expects a line number from 1"))
                })
                .transpose()
        };
        let (from, to) = (
            line("from")?.unwrap_or(1),
            line("to")?.unwrap_or(usize::MAX),
        );
        ensure!(from <= to, "--from must not be after --to");
        Ok(Self {
            from,
            to,
            grep: args.value_of("grep"),
            limit: crate::read_num(args, "limit", 1)?,
        })
    }

    fn whole(&self) -> bool {
        self.from == 1 && self.to == usize::MAX && self.grep.is_none()
    }

    /// Prints the selected part of `text`: every byte, the exact bytes of a line
    /// range, or numbered matching lines for `--grep`.
    fn print(&self, text: &[u8]) -> Result<()> {
        let mut out = io::stdout().lock();
        if self.whole() {
            return Ok(out.write_all(text)?);
        }
        let lines = text
            .split_inclusive(|&byte| byte == b'\n')
            .enumerate()
            .map(|(i, line)| (i + 1, line))
            .skip(self.from - 1)
            .take_while(|(number, _)| *number <= self.to);
        let Some(needle) = self.grep else {
            for (_, line) in lines {
                out.write_all(line)?;
            }
            return Ok(());
        };
        let mut shown = 0;
        for (number, line) in lines {
            let line = String::from_utf8_lossy(line);
            let line = line.trim_end_matches(['\r', '\n']);
            let Some(at) = line.find(needle) else {
                continue;
            };
            // Long lines are cut around the match, as `justless search` does.
            let start = line.floor_char_boundary(at.saturating_sub(100));
            let end = line.floor_char_boundary((start + 400).min(line.len()));
            writeln!(out, "{number}: {}", &line[start..end])?;
            shown += 1;
            if shown == self.limit {
                break;
            }
        }
        if shown == 0 {
            writeln!(out, "No line matches {needle:?}.")?;
        }
        Ok(())
    }
}

pub(crate) fn valid_tool_use_id(id: &str) -> bool {
    (1..=200).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn tool_use_path(ws: &Workspace, id: &str) -> std::path::PathBuf {
    // Hashing the ID keeps entries apart on case-insensitive file systems.
    ws.state
        .join("tool-uses")
        .join(format!("{}.json", source_digest(id.as_bytes())))
}

/// Archives one tool result under its tool_use ID. The first copy wins, so a result
/// the client later trims stays recoverable. Returns whether a new entry was made.
/// The call an archived result answered.
pub(crate) struct ToolCall<'a> {
    pub id: &'a str,
    pub tool: &'a str,
    /// A one-line summary, such as a path, a command or a pattern.
    pub summary: &'a str,
    pub input: &'a Value,
}

/// A call's compact input JSON, cut at a character boundary near 512 bytes.
fn input_text(input: &Value) -> String {
    if input.is_null() {
        return String::new();
    }
    let text = serde_json::to_string(input).unwrap_or_default();
    text[..text.floor_char_boundary(512)].to_owned()
}

pub(crate) fn archive_tool_use(
    ws: &Workspace,
    call: &ToolCall,
    content: &Value,
    is_error: bool,
) -> Result<bool> {
    let id = call.id;
    ensure!(valid_tool_use_id(id), "invalid tool_use ID");
    let path = tool_use_path(ws, id);
    if fs::symlink_metadata(&path).is_ok() {
        return Ok(false);
    }
    ws.prepare_state()?;
    let directory = ws.state.join("tool-uses");
    private_dir(&directory)?;
    let entry = ToolUse {
        id: id.to_owned(),
        observation: archive(ws, content)?,
        tool: call.tool.to_owned(),
        call: call.summary.to_owned(),
        input: input_text(call.input),
        bytes: serde_json::to_vec(content)?.len() as u64,
        is_error,
        first_seen_ms: now_ms(),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    serde_json::to_writer(&mut temporary, &entry)?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => Ok(true),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.error.into()),
    }
}

fn tool_uses(ws: &Workspace) -> Result<Vec<ToolUse>> {
    let mut entries = Vec::new();
    let listing = match fs::read_dir(ws.state.join("tool-uses")) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => return Err(error.into()),
    };
    for file in listing {
        let path = file?.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Ok(entry) = serde_json::from_slice::<ToolUse>(&fs::read(&path)?)
        {
            entries.push(entry);
        }
    }
    entries.sort_by(|a, b| b.first_seen_ms.cmp(&a.first_seen_ms).then(a.id.cmp(&b.id)));
    Ok(entries)
}

fn recall_tool_use(ws: &Workspace, id: &str, as_json: bool, selection: &Selection) -> Result<()> {
    ensure!(valid_tool_use_id(id), "invalid tool_use ID");
    let entry: ToolUse = match fs::read(tool_use_path(ws, id)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("read tool result index")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!(
                "no archived tool result for {id}; try `{} recall --list TEXT`",
                ws.justless()
            )
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(entry.id == id, "tool result index mismatch for {id}");
    let value = recover(ws, &entry.observation)?;
    if as_json {
        return Ok(serde_json::to_writer(io::stdout().lock(), &value)?);
    }
    match &value {
        Value::Null => Ok(()),
        Value::String(text) => selection.print(text.as_bytes()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks.iter().map(block_text).collect();
            selection.print(parts.join("\n").as_bytes())
        }
        other => selection.print(&serde_json::to_vec(other)?),
    }
}

/// A content block as text: text blocks verbatim, anything else as a note that
/// points to the exact JSON.
fn block_text(block: &Value) -> String {
    if block["type"] == "text"
        && let Some(text) = block["text"].as_str()
    {
        return text.to_owned();
    }
    let source = &block["source"];
    match (source["media_type"].as_str(), source["data"].as_str()) {
        (Some(media), Some(data)) => format!(
            "[{media} · {} base64 · use --json]",
            human_bytes(data.len() as u64)
        ),
        _ => format!(
            "[{} · use --json]",
            block["type"].as_str().unwrap_or("block")
        ),
    }
}

fn list_tool_uses(ws: &Workspace, text: Option<&str>, limit: usize) -> Result<()> {
    let entries = tool_uses(ws)?;
    if entries.is_empty() {
        let justless = ws.justless();
        println!(
            "No archived tool results yet. `{justless} wrap claude` and `{justless} proxy` archive them."
        );
        return Ok(());
    }
    let needle = text.map(str::to_ascii_lowercase);
    let matching: Vec<&ToolUse> = entries
        .iter()
        .filter(|entry| {
            needle.as_deref().is_none_or(|needle| {
                [&entry.tool, &entry.call, &entry.input]
                    .iter()
                    .any(|field| field.to_ascii_lowercase().contains(needle))
            })
        })
        .collect();
    for entry in matching.iter().take(limit) {
        println!(
            "{}  {}  {}  {}  {}{}",
            entry.id,
            if entry.tool.is_empty() {
                "?"
            } else {
                &entry.tool
            },
            entry.call,
            human_bytes(entry.bytes),
            age(entry.first_seen_ms),
            if entry.is_error { "  error" } else { "" }
        );
    }
    let summary = match text {
        Some(text) => format!(
            "{} of {} archived tool results match {text:?}",
            matching.len(),
            entries.len()
        ),
        None => format!("{} archived tool results", entries.len()),
    };
    let shown = matching.len().min(limit);
    let newest = if shown < matching.len() {
        format!(", newest {shown} shown")
    } else {
        String::new()
    };
    println!(
        "{summary}{newest}. Print one with `{} recall --tool-use ID`.",
        ws.justless()
    );
    Ok(())
}

/// Claude models whose thinking blocks are not tied to earlier turns, so rewriting
/// an old tool result leaves the rest of the conversation valid.
static UNBOUND_THINKING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^claude-(3-.*|(opus|sonnet|haiku)-4(-.*)?|(opus|sonnet|fable|mythos)-5(-\d{8})?|mythos-preview.*)$",
    )
    .expect("valid model pattern")
});
static VERSION_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-v\d+:\d+$").expect("valid suffix pattern"));

/// A provider-qualified model name in first-party form, such as
/// `us.anthropic.claude-opus-4-6-v1:0` or `databricks-claude-opus-5-5` as
/// `claude-opus-4-6` or `claude-opus-5-5`.
fn normalize_model(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    let mut name = lower.rsplit('/').next().unwrap_or_default();
    // Gateways put their own prefix before the first-party name, joined by a
    // dot, a dash or a colon.
    if let Some(start) = name.rfind("claude-") {
        name = &name[start..];
    }
    for region in ["us.", "eu.", "apac.", "global."] {
        if let Some(rest) = name.strip_prefix(region) {
            name = rest;
            break;
        }
    }
    name = name.strip_prefix("anthropic.").unwrap_or(name);
    name = name.split('@').next().unwrap_or_default();
    name = name.strip_suffix("[1m]").unwrap_or(name);
    VERSION_SUFFIX.replace(name, "").replace('.', "-")
}

/// The reason to leave a request unchanged, or `None` to retire results.
///
/// Rewriting an earlier tool result invalidates every later thinking block on models
/// that tie thinking to the conversation, so Messages requests are rewritten only for
/// an allowlist of other Claude models. The rule depends only on the model, so a
/// retired result never changes back within a conversation.
fn held(request: &Value, model: Option<&str>) -> Option<&'static str> {
    let messages = request["messages"].as_array();
    let opencode = messages.is_some_and(|list| {
        list.iter()
            .any(|m| m.get("info").is_some() && m.get("parts").is_some())
    });
    let chat = messages.is_some_and(|list| {
        list.iter()
            .any(|m| m["role"] == "tool" || m.get("tool_calls").is_some())
    });
    let anthropic = messages.is_some() && !opencode && !chat;
    let Some(model) = model.or_else(|| request["model"].as_str()) else {
        return if opencode {
            Some("model-required")
        } else if reasoning_present(request) {
            Some("reasoning-present")
        } else {
            None
        };
    };
    let name = normalize_model(model);
    // Any name that mentions Claude is held unless it is known to be unbound.
    let claude = name.contains("claude");
    if (!anthropic && !claude) || UNBOUND_THINKING.is_match(&name) {
        None
    } else if claude {
        Some("preserved-thinking")
    } else {
        Some("unknown-model")
    }
}

/// Whether a model-less request carries reasoning tied to its history.
fn reasoning_present(request: &Value) -> bool {
    let messages = request["messages"].as_array().into_iter().flatten();
    let input = request["input"].as_array().into_iter().flatten();
    messages.chain(input).any(|item| {
        item["type"] == "reasoning"
            || item.get("reasoning_details").is_some()
            || item.get("thinking_blocks").is_some()
            || item["content"].as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    matches!(
                        block["type"].as_str(),
                        Some("thinking" | "redacted_thinking")
                    )
                })
            })
    })
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
        let selection = Selection::of(args)?;
        let as_json = args.is_present("json");
        if let Some(id) = args.value_of("tool-use") {
            return recall_tool_use(ws, id, as_json, &selection);
        }
        if args.is_present("list") {
            ensure!(
                selection.whole(),
                "--from, --to and --grep select lines of one result, not --list rows"
            );
            return list_tool_uses(ws, args.value_of("list"), selection.limit);
        }
        let id = args.value_of("id").unwrap();
        // A tool_use ID reaches the proxy's archive, so `justless recall ID`
        // works for a recall placeholder and for a cleared tool result alike.
        if !valid_id(id) {
            ensure!(
                valid_tool_use_id(id),
                "invalid ID: expected a 64-character recall ID or a tool_use ID"
            );
            return recall_tool_use(ws, id, as_json, &selection);
        }
        let value = recover(ws, id)?;
        // Plain recall prints text: a string, or the parts of an all-text array
        // joined without a separator. `--json` prints the typed value.
        if !as_json && text_kind(&value).is_some() {
            let text = match value.as_array() {
                Some(parts) => parts
                    .iter()
                    .map(|part| part["text"].as_str().unwrap_or_default())
                    .collect::<String>(),
                None => value.as_str().unwrap_or_default().to_owned(),
            };
            return selection.print(text.as_bytes());
        }
        ensure!(
            selection.whole(),
            "--from, --to and --grep need a text result; use --json for this one"
        );
        return Ok(serde_json::to_writer(io::stdout().lock(), &value)?);
    }
    let mut input = Vec::new();
    io::stdin().lock().read_to_end(&mut input)?;
    let (mut request, surrogates) = decode_request(&input)?;
    if !args.is_present("restore")
        && let Some(reason) = held(&request, args.value_of("model"))
    {
        if args.is_present("stats") {
            eprintln!("{}", json!({"changed":0,"bytes_saved":0,"held":reason}));
        }
        // A held request is returned exactly as read.
        return Ok(io::stdout().lock().write_all(&input)?);
    }
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
        eprintln!(
            "{}",
            json!({"changed":count,"bytes_saved":saved_bytes,"held":null})
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_provider_model_names() {
        for (raw, expected) in [
            (
                "openrouter/anthropic/claude-sonnet-4.5",
                "claude-sonnet-4-5",
            ),
            ("us.anthropic.claude-opus-5-5", "claude-opus-5-5"),
            ("anthropic.claude-opus-4-6-v1:0", "claude-opus-4-6"),
            ("global.anthropic.claude-haiku-5-5", "claude-haiku-5-5"),
            ("claude-opus-4-5@20251101", "claude-opus-4-5"),
            ("Claude-Opus-4-6[1m]", "claude-opus-4-6"),
            ("databricks-claude-opus-5-5", "claude-opus-5-5"),
            (
                "databricks/databricks-claude-sonnet-4-5",
                "claude-sonnet-4-5",
            ),
            ("anthropic:claude-opus-5-5", "claude-opus-5-5"),
            ("vertex_ai/claude-opus-4-5@20251101", "claude-opus-4-5"),
            ("gpt-5.5", "gpt-5-5"),
        ] {
            assert_eq!(normalize_model(raw), expected, "{raw}");
        }
        for unbound in [
            "claude-3-5-sonnet-20241022",
            "claude-opus-4-8",
            "claude-haiku-4-5",
            "claude-sonnet-4",
            "claude-opus-5",
            "claude-sonnet-5-20260115",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
        ] {
            assert!(UNBOUND_THINKING.is_match(unbound), "{unbound}");
        }
        for bound in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-2",
            "claude-opus-6",
        ] {
            assert!(!UNBOUND_THINKING.is_match(bound), "{bound}");
        }
    }

    #[test]
    fn tool_use_ids_are_plain_tokens() {
        assert!(valid_tool_use_id("toolu_01HxV9mTq3K8c2YdL5aZ7wPe"));
        assert!(valid_tool_use_id("call-1_A"));
        for bad in ["", "../x", "a/b", "a b", &"x".repeat(201)] {
            assert!(!valid_tool_use_id(bad), "{bad}");
        }
    }
}
