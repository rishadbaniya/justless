use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail, ensure};
use clap::{Arg, ArgGroup, ArgMatches, Command};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::capture::{Counted, source_digest};
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
    /// range, or numbered matching lines for `--grep`. Returns the bytes printed.
    fn print(&self, text: &[u8]) -> Result<u64> {
        let mut out = Counted::new(io::stdout().lock());
        self.write(&mut out, text)?;
        Ok(out.written)
    }

    fn write(&self, out: &mut impl Write, text: &[u8]) -> Result<()> {
        if self.whole() {
            out.write_all(text)?;
            return Ok(());
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
        // Every match is counted, so a last line can say what the limit left out.
        let mut shown = 0;
        let mut found = 0;
        for (number, line) in lines {
            let line = String::from_utf8_lossy(line);
            let line = line.trim_end_matches(['\r', '\n']);
            let Some(at) = line.find(needle) else {
                continue;
            };
            found += 1;
            if shown == self.limit {
                continue;
            }
            // Long lines are cut around the match, as `justless search` does.
            let start = line.floor_char_boundary(at.saturating_sub(100));
            let end = line.floor_char_boundary((start + 400).min(line.len()));
            writeln!(out, "{number}: {}", &line[start..end])?;
            shown += 1;
        }
        if found == 0 {
            writeln!(out, "No line matches {needle:?}.")?;
        } else if shown < found {
            writeln!(
                out,
                "[{shown} of {found} matching lines shown; --limit {found} shows all]"
            )?;
        }
        Ok(())
    }
}

/// Writes the typed JSON value of an archived result. Returns the bytes printed.
fn print_json(value: &Value) -> Result<u64> {
    let mut out = Counted::new(io::stdout().lock());
    serde_json::to_writer(&mut out, value)?;
    Ok(out.written)
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

/// Prints an archived result by its tool_use ID. Returns the bytes printed.
fn recall_tool_use(ws: &Workspace, id: &str, as_json: bool, selection: &Selection) -> Result<u64> {
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
        return print_json(&value);
    }
    match &value {
        Value::Null => Ok(0),
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

/// Maximum characters for a standalone diagnostic cue in a retired stub.
const MAX_CUE_CHARS: usize = 80;
/// Maximum characters when combining a run/file prefix with a diagnostic cue.
const MAX_COMBINED_CUE_CHARS: usize = 100;
/// Bound line scanning so huge outputs stay fast while still catching tail summaries.
const MAX_HEAD_SCAN_LINES: usize = 1000;
const MAX_TAIL_SCAN_LINES: usize = 200;
/// Maximum bytes examined from a single line before redacting (matches redact's HOLD).
const MAX_CUE_LINE_BYTES: usize = 8 * 1024;
/// Enriched stubs are emitted only when strictly smaller than this share of the
/// original serialized JSON value.
const MAX_ENRICHED_PERCENT: usize = 35;

static ANSI_ESCAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("valid ansi pattern"));
static RUN_FOOTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[justless (run-[A-Za-z0-9]+)(?:\](.*)| · ([^\]]+)\])")
        .expect("valid run footer pattern")
});
static EXIT_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bexit -?[0-9]+\b").expect("valid exit pattern"));
static REVISION_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^revision [0-9a-f]{12,64} · [0-9]+ lines(?: · window L[0-9]+-[0-9]+)?$")
        .expect("valid revision header pattern")
});
static FILE_REVISION_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[^\s]+ \(revision [0-9a-f]{8,64}[^)]*\)$")
        .expect("valid file revision header pattern")
});
static ARCHIVED_READ_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[justless recall [0-9a-f]{64}\] exact original Read result · ([0-9]+ lines)$")
        .expect("valid archived read header pattern")
});
static ARCHIVED_SEARCH_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[justless recall [0-9a-f]{64}\] exact original result$")
        .expect("valid archived search header pattern")
});

fn text_content(value: &Value) -> Option<Cow<'_, str>> {
    text_kind(value)?;
    if let Some(s) = value.as_str() {
        return Some(Cow::Borrowed(s));
    }
    let parts = value.as_array()?;
    if parts.len() == 1 {
        return parts[0]["text"].as_str().map(Cow::Borrowed);
    }
    let joined: String = parts.iter().filter_map(|p| p["text"].as_str()).collect();
    Some(Cow::Owned(joined))
}

fn content_digest(value: &Value) -> Option<String> {
    let text = text_content(value)?;
    Some(source_digest(text.as_bytes()))
}

fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.as_bytes().contains(&0x1b) {
        return Cow::Borrowed(text);
    }
    ANSI_ESCAPE.replace_all(text, "")
}

fn clean_single_line(line: &str) -> String {
    let bounded = &line[..line.floor_char_boundary(MAX_CUE_LINE_BYTES.min(line.len()))];
    let plain = strip_ansi(bounded);
    let sanitized: String = plain
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    sanitized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn redact_line(redactor: Option<&crate::redact::Redactor>, line: &str) -> String {
    let clean = clean_single_line(line);
    if clean.is_empty() || (clean.contains("-----BEGIN") && clean.contains("PRIVATE KEY")) {
        return String::new();
    }
    match redactor {
        Some(redactor) => redactor.text(&clean).into_owned(),
        None => clean,
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => {
            let mut out = text[..cut].trim_end().to_owned();
            // Never emit a half-truncated `[redacted:...` marker.
            if let Some(pos) = out.rfind("[redacted:")
                && !out[pos..].contains(']')
            {
                out.truncate(pos);
                out = out.trim_end().to_owned();
            }
            if out.is_empty() {
                String::new()
            } else {
                out.push('…');
                out
            }
        }
        None => text.to_owned(),
    }
}

fn is_error_line(trimmed: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "error:",
        "error[",
        "error TS",
        "Error:",
        "ERROR:",
        "ERROR ",
        "fatal:",
        "Fatal:",
        "FATAL:",
        "FAIL:",
        "FAIL ",
        "FAILED:",
        "FAILED ",
        "--- FAIL:",
        "test result: FAILED",
        "panic:",
        "AssertionError:",
        "TypeError:",
        "ValueError:",
        "KeyError:",
        "RuntimeError:",
        "ReferenceError:",
        "SyntaxError:",
        "RangeError:",
        "AttributeError:",
        "ImportError:",
        "ModuleNotFoundError:",
        "ZeroDivisionError:",
        "IndexError:",
        "FileNotFoundError:",
        "PermissionError:",
        "OSError:",
        "NameError:",
        "TimeoutError:",
        "ConnectionError:",
        "Exception:",
    ];
    if PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix)) {
        return true;
    }
    if trimmed.starts_with("thread '") && trimmed.contains("panicked at") {
        return true;
    }
    if trimmed.starts_with("test ") && trimmed.ends_with(" ... FAILED") {
        return true;
    }
    if trimmed.starts_with("Summary ") && trimmed.contains(" failed") {
        return true;
    }
    if trimmed.starts_with("===") && trimmed.contains(" failed") && trimmed.ends_with("===") {
        return true;
    }
    if (trimmed.starts_with("Tests:") || trimmed.starts_with("Test Files"))
        && trimmed.contains("failed")
    {
        return true;
    }
    if let Some((before, _)) = trimmed.split_once(": error")
        && !before.is_empty()
        && !before.contains(' ')
        && (trimmed.contains(": error:")
            || trimmed.contains(": error[")
            || trimmed.contains(": error TS"))
    {
        return true;
    }
    false
}

fn is_pass_summary(trimmed: &str) -> bool {
    trimmed.starts_with("test result: ok.")
        || (trimmed.starts_with("Summary ") && trimmed.contains(" passed"))
        || (trimmed.starts_with("===") && trimmed.contains(" passed") && trimmed.ends_with("==="))
        || ((trimmed.starts_with("Tests:") || trimmed.starts_with("Test Files"))
            && trimmed.contains("passed"))
}

/// Finds the highest-signal diagnostic or test-summary line in `text`, bounded
/// to head + tail windows so multi-megabyte logs stay fast.
fn extract_diagnostic(text: &str, redactor: Option<&crate::redact::Redactor>) -> Option<String> {
    let mut pass_candidate: Option<&str> = None;
    let mut head_count = 0usize;
    let mut total_lines = 0usize;
    for line in text.lines() {
        total_lines += 1;
        if head_count < MAX_HEAD_SCAN_LINES {
            head_count += 1;
            let trimmed = strip_ansi(line);
            let trimmed = trimmed.trim();
            if trimmed.is_empty() {
                continue;
            }
            if is_error_line(trimmed) {
                let redacted = redact_line(redactor, line);
                if !redacted.is_empty() {
                    return Some(redacted);
                }
            } else if pass_candidate.is_none() && is_pass_summary(trimmed) {
                pass_candidate = Some(line);
            }
        }
    }
    if total_lines > MAX_HEAD_SCAN_LINES {
        let tail_start = total_lines
            .saturating_sub(MAX_TAIL_SCAN_LINES)
            .max(MAX_HEAD_SCAN_LINES);
        for line in text.lines().skip(tail_start) {
            let trimmed = strip_ansi(line);
            let trimmed = trimmed.trim();
            if trimmed.is_empty() {
                continue;
            }
            if is_error_line(trimmed) {
                let redacted = redact_line(redactor, line);
                if !redacted.is_empty() {
                    return Some(redacted);
                }
            } else if pass_candidate.is_none() && is_pass_summary(trimmed) {
                pass_candidate = Some(line);
            }
        }
    }
    pass_candidate
        .map(|line| redact_line(redactor, line))
        .filter(|s| !s.is_empty())
}

fn extract_run_footer(text: &str, redactor: Option<&crate::redact::Redactor>) -> Option<String> {
    let mut non_empty_seen = 0usize;
    let mut matched: Option<(String, Option<String>)> = None;
    for line in text.lines().rev() {
        let plain = strip_ansi(line);
        let trimmed = plain.trim();
        if trimmed.is_empty() {
            continue;
        }
        non_empty_seen += 1;
        if let Some(caps) = RUN_FOOTER.captures(trimmed) {
            let run_id = caps[1].to_owned();
            let detail = caps
                .get(2)
                .or_else(|| caps.get(3))
                .map(|m| m.as_str())
                .unwrap_or("");
            let exit = EXIT_CODE.find(detail).map(|m| m.as_str().to_owned());
            matched = Some((run_id, exit));
            break;
        }
        if non_empty_seen >= 10 {
            break;
        }
    }
    let (run_id, exit) = matched?;
    let prefix = match exit {
        Some(exit) => format!("{run_id} · {exit}"),
        None => run_id,
    };
    if let Some(diag) = extract_diagnostic(text, redactor) {
        let budget = MAX_COMBINED_CUE_CHARS.saturating_sub(prefix.chars().count() + 3);
        if budget >= 20 {
            let short = truncate_chars(&diag, budget);
            if !short.is_empty() {
                return Some(format!("{prefix} · {short}"));
            }
        }
    }
    Some(prefix)
}

fn extract_outline_header(
    text: &str,
    redactor: Option<&crate::redact::Redactor>,
) -> Option<String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let first_raw = lines.next()?;
    let second_raw = lines.next();
    let first_plain = strip_ansi(first_raw);
    let first = first_plain.trim();
    if REVISION_HEADER.is_match(first) || FILE_REVISION_HEADER.is_match(first) {
        let clean = redact_line(redactor, first);
        let bounded = truncate_chars(&clean, MAX_CUE_CHARS);
        return (!bounded.is_empty()).then_some(bounded);
    }
    if let Some(file) = first
        .strip_prefix("=== ")
        .and_then(|s| s.strip_suffix(" ==="))
        .filter(|s| !s.is_empty())
        && let Some(second_raw) = second_raw
    {
        let second_plain = strip_ansi(second_raw);
        let second = second_plain.trim();
        if REVISION_HEADER.is_match(second) {
            let combined = format!("{file} ({second})");
            let clean = redact_line(redactor, &combined);
            let bounded = truncate_chars(&clean, MAX_COMBINED_CUE_CHARS);
            return (!bounded.is_empty()).then_some(bounded);
        }
    }
    if let Some(caps) = ARCHIVED_READ_HEADER.captures(first) {
        return Some(format!("Read outline · {}", &caps[1]));
    }
    if ARCHIVED_SEARCH_HEADER.is_match(first)
        && let Some(second_raw) = second_raw
    {
        let second_plain = strip_ansi(second_raw);
        let second = second_plain.trim();
        if second.starts_with("Grep: ") || second.starts_with("Glob in ") {
            let clean = redact_line(redactor, second);
            let bounded = truncate_chars(&clean, MAX_CUE_CHARS);
            return (!bounded.is_empty()).then_some(bounded);
        }
    }
    if first.starts_with("Justless outline of ") {
        let clean = redact_line(redactor, first);
        let bounded = truncate_chars(&clean, MAX_CUE_CHARS);
        return (!bounded.is_empty()).then_some(bounded);
    }
    None
}

/// Extracts a deterministic, redacted, single-line semantic cue purely from a
/// retired result's text, or `None` for generic plain text.
fn semantic_cue(text: &str, redactor: Option<&crate::redact::Redactor>) -> Option<String> {
    if let Some(cue) = extract_run_footer(text, redactor) {
        return Some(cue);
    }
    if let Some(cue) = extract_outline_header(text, redactor) {
        return Some(cue);
    }
    let diag = extract_diagnostic(text, redactor)?;
    let bounded = truncate_chars(&diag, MAX_CUE_CHARS);
    (!bounded.is_empty()).then_some(bounded)
}

fn wrap_stub(kind: &str, text: String) -> Value {
    if kind.is_empty() {
        Value::String(text)
    } else {
        json!([{"type": kind, "text": text}])
    }
}

fn build_stub(
    original: &Value,
    kind: &str,
    id: &str,
    original_bytes: usize,
    redactor: Option<&crate::redact::Redactor>,
) -> Result<Value> {
    let bare = wrap_stub(kind, format!("{PREFIX}{id}]"));
    let Some(text) = text_content(original) else {
        return Ok(bare);
    };
    let Some(cue) = semantic_cue(&text, redactor) else {
        return Ok(bare);
    };
    let enriched = wrap_stub(kind, format!("{PREFIX}{id} · {cue}]"));
    let enriched_bytes = serde_json::to_vec(&enriched)?.len();
    if enriched_bytes < original_bytes
        && enriched_bytes.saturating_mul(100) < original_bytes.saturating_mul(MAX_ENRICHED_PERCENT)
    {
        debug_assert_eq!(marker(&enriched), Some(id));
        Ok(enriched)
    } else {
        Ok(bare)
    }
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
    if text.contains('\n') || text.contains('\r') {
        return None;
    }
    let inner = text.strip_prefix(PREFIX)?.strip_suffix(']')?;
    if valid_id(inner) {
        return Some(inner);
    }
    let (id, cue) = inner.split_once(" · ")?;
    (!cue.is_empty() && valid_id(id)).then_some(id)
}

/// Prints one archived result, by recall ID or tool_use ID. Returns the bytes
/// printed.
fn recall(ws: &Workspace, args: &ArgMatches, selection: &Selection) -> Result<(u64, bool)> {
    let as_json = args.is_present("json");
    if let Some(id) = args.value_of("tool-use") {
        return Ok((recall_tool_use(ws, id, as_json, selection)?, true));
    }
    let id = args.value_of("id").unwrap();
    // A tool_use ID reaches the proxy's archive, so `justless recall ID`
    // works for a recall placeholder and for a cleared tool result alike.
    if !valid_id(id) {
        ensure!(
            valid_tool_use_id(id),
            "invalid ID: expected a 64-character recall ID or a tool_use ID"
        );
        return Ok((recall_tool_use(ws, id, as_json, selection)?, true));
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
        return Ok((selection.print(text.as_bytes())?, false));
    }
    ensure!(
        selection.whole(),
        "--from, --to and --grep need a text result; use --json for this one"
    );
    Ok((print_json(&value)?, false))
}

struct MaskOptions<'a> {
    keep: usize,
    minimum: usize,
    trigger: usize,
    restore: bool,
    pins: Vec<&'a str>,
    keep_tools: Vec<&'a str>,
}

enum Change {
    Restore(Value),
    Retire { kind: String, bytes: usize },
}

fn mask_request(
    ws: &Workspace,
    request: &mut Value,
    surrogates: Option<&Surrogates>,
    opts: &MaskOptions<'_>,
) -> Result<(usize, i128)> {
    let candidates = outputs(request)?;
    let cutoff = candidates.len().saturating_sub(opts.keep);
    let allowed = |c: &Output| {
        !c.protected
            && !opts.pins.iter().any(|&s| s == c.call)
            && !opts.keep_tools.iter().any(|&s| s == c.tool)
    };
    let mut digests: Vec<Option<String>> = Vec::new();
    let mut last_occurrence: HashMap<String, usize> = HashMap::new();
    if !opts.restore {
        digests.reserve(candidates.len());
        for (index, c) in candidates.iter().enumerate() {
            let digest = request
                .pointer(&c.path)
                .filter(|value| marker(value).is_none())
                .and_then(content_digest);
            if let Some(d) = &digest {
                last_occurrence.insert(d.clone(), index);
            }
            digests.push(digest);
        }
    }
    let mut changes: Vec<(&str, Change)> = Vec::new();
    let mut eligible_bytes: usize = 0;
    let mut saved_bytes: i128 = 0;
    for (index, c) in candidates.iter().enumerate() {
        let Some(value) = request.pointer(&c.path) else {
            continue;
        };
        if opts.restore {
            if !c.protected
                && let Some(id) = marker(value)
            {
                changes.push((&c.path, Change::Restore(recover(ws, id)?)));
            }
            continue;
        }
        if !allowed(c) || marker(value).is_some() {
            continue;
        }
        // Earlier identical copies are retired even inside the `keep` window
        // because the exact text remains verbatim at the latest occurrence.
        let is_duplicate = digests[index]
            .as_ref()
            .and_then(|d| last_occurrence.get(d))
            .is_some_and(|&last| index < last);
        if index >= cutoff && !is_duplicate {
            continue;
        }
        // The archive holds valid JSON, so a result with a lone surrogate stays.
        if let Some(surrogates) = surrogates
            && serde_json::to_string(value)?.contains(surrogates.marker)
        {
            continue;
        }
        let Some(kind) = text_kind(value) else {
            continue;
        };
        let bytes = serde_json::to_vec(value)?.len();
        if bytes < opts.minimum {
            continue;
        }
        let bare = wrap_stub(kind, format!("{PREFIX}{}]", "0".repeat(64)));
        let cost = serde_json::to_vec(&bare)?.len();
        if cost >= bytes {
            continue;
        }
        eligible_bytes += bytes;
        changes.push((
            &c.path,
            Change::Retire {
                kind: kind.to_owned(),
                bytes,
            },
        ));
    }
    if !opts.restore && eligible_bytes < opts.trigger {
        changes.clear();
    }
    let count = changes.len();
    let mut redactor: Option<Option<&crate::redact::Redactor>> = None;
    for (path, change) in changes {
        let original = request.pointer_mut(path).context("missing result field")?;
        let next = match change {
            Change::Restore(value) => value,
            Change::Retire { kind, bytes } => {
                let id = archive(ws, original)?;
                let redactor = *redactor.get_or_insert_with(|| crate::redact::for_workspace(ws));
                build_stub(original, &kind, &id, bytes, redactor)?
            }
        };
        saved_bytes +=
            serde_json::to_vec(original)?.len() as i128 - serde_json::to_vec(&next)?.len() as i128;
        *original = next;
    }
    Ok((count, saved_bytes))
}

pub fn run(ws: &Workspace, name: &str, args: &ArgMatches) -> Result<()> {
    if name == "recall" {
        let selection = Selection::of(args)?;
        if args.is_present("list") {
            ensure!(
                selection.whole(),
                "--from, --to and --grep select lines of one result, not --list rows"
            );
            return list_tool_uses(ws, args.value_of("list"), selection.limit);
        }
        // What a recall prints comes back into context, so `stats` and the status
        // line report it beside the savings it reverses.
        let (printed, proxy) = recall(ws, args, &selection)?;
        crate::history::recalled_archive(ws, printed, proxy);
        return Ok(());
    }
    let mut input = Vec::new();
    io::stdin().lock().read_to_end(&mut input)?;
    let (mut request, surrogates) = decode_request(&input)?;
    let restore = args.is_present("restore");
    if !restore && let Some(reason) = held(&request, args.value_of("model")) {
        if args.is_present("stats") {
            eprintln!("{}", json!({"changed":0,"bytes_saved":0,"held":reason}));
        }
        // A held request is returned exactly as read.
        return Ok(io::stdout().lock().write_all(&input)?);
    }
    let opts = MaskOptions {
        keep: crate::read_num(args, "keep", 0)?,
        minimum: crate::read_num(args, "min-bytes", 0)?,
        trigger: crate::read_num(args, "trigger-bytes", 0)?,
        restore,
        pins: args.values_of("pin").into_iter().flatten().collect(),
        keep_tools: args.values_of("keep-tool").into_iter().flatten().collect(),
    };
    let (count, saved_bytes) = mask_request(ws, &mut request, surrogates.as_ref(), &opts)?;
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

    #[test]
    fn grep_counts_the_matching_lines_its_limit_leaves_out() {
        let text = (1..=30)
            .map(|i| format!("order-42 progress {i}\n"))
            .collect::<String>()
            + "order-42 FAILED\n";
        let grep = |limit: usize| {
            let selection = Selection {
                from: 1,
                to: usize::MAX,
                grep: Some("order-42"),
                limit,
            };
            let mut out = Vec::new();
            selection.write(&mut out, text.as_bytes()).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(
            grep(2),
            "1: order-42 progress 1\n2: order-42 progress 2\n\
             [2 of 31 matching lines shown; --limit 31 shows all]\n"
        );
        let all = grep(31);
        assert_eq!(all.lines().count(), 31, "{all}");
        assert!(all.ends_with("31: order-42 FAILED\n"), "{all}");
    }

    fn test_workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let ws = Workspace {
            state: root.join(".justless"),
            cwd: root.clone(),
            origin: root.clone(),
            root,
        };
        (dir, ws)
    }

    fn default_opts(keep: usize) -> MaskOptions<'static> {
        MaskOptions {
            keep,
            minimum: 1024,
            trigger: 0,
            restore: false,
            pins: Vec::new(),
            keep_tools: Vec::new(),
        }
    }

    fn tool_messages(results: &[(&str, &str, Value, bool)]) -> Value {
        let mut messages = vec![json!({"role":"user","content":"investigate"})];
        for (call_id, tool, content, is_error) in results {
            messages.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":call_id,"name":tool,"input":{}}
            ]}));
            let mut result = json!({"type":"tool_result","tool_use_id":call_id,"content":content});
            if *is_error {
                result["is_error"] = json!(true);
            }
            messages.push(json!({"role":"user","content":[result]}));
        }
        json!({"model":"claude-sonnet-4-6","messages":messages})
    }

    #[test]
    fn retires_earlier_duplicate_results_even_inside_keep_window_with_exact_recall() {
        let (_dir, ws) = test_workspace();
        let dup = "repeated build output line\n".repeat(100);
        let unique = "unique diagnostic output\n".repeat(100);
        let original = tool_messages(&[
            ("c0", "bash", json!(dup), false),
            ("c1", "read", json!(unique), false),
            ("c2", "bash", json!([{"type":"text","text":dup}]), false),
            ("c3", "bash", json!(dup), false),
        ]);
        // keep=5 covers all 4 results, yet earlier identical copies c0 and c2
        // are retired immediately because c3 holds the exact text verbatim.
        let mut masked = original.clone();
        let (changed, saved) = mask_request(&ws, &mut masked, None, &default_opts(5)).unwrap();
        assert_eq!(changed, 2);
        assert!(saved > 4000, "saved={saved}");

        let s0 = &masked["messages"][2]["content"][0]["content"];
        let s1 = &masked["messages"][4]["content"][0]["content"];
        let s2 = &masked["messages"][6]["content"][0]["content"];
        let s3 = &masked["messages"][8]["content"][0]["content"];
        let id0 = marker(s0).expect("c0 retired");
        let id2 = marker(s2).expect("c2 retired");
        assert_eq!(s1, &json!(unique));
        assert_eq!(s3, &json!(dup));
        assert_eq!(recover(&ws, id0).unwrap(), json!(dup));
        assert_eq!(
            recover(&ws, id2).unwrap(),
            json!([{"type":"text","text":dup}])
        );

        // Idempotent when re-masked, and lossless under --restore.
        let mut remasked = masked.clone();
        let (again, _) = mask_request(&ws, &mut remasked, None, &default_opts(5)).unwrap();
        assert_eq!(again, 0);
        assert_eq!(remasked, masked);

        let mut restored = masked.clone();
        let mut restore_opts = default_opts(5);
        restore_opts.restore = true;
        mask_request(&ws, &mut restored, None, &restore_opts).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn duplicate_early_retirement_respects_protected_pin_keep_tool_and_trigger() {
        let (_dir, ws) = test_workspace();
        let dup = "same output line\n".repeat(120);
        let req = tool_messages(&[
            ("err_call", "bash", json!(dup), true),
            ("pinned_call", "bash", json!(dup), false),
            ("kept_tool_call", "read", json!(dup), false),
            ("latest_call", "bash", json!(dup), false),
        ]);
        let mut masked = req.clone();
        let opts = MaskOptions {
            keep: 10,
            minimum: 1024,
            trigger: 0,
            restore: false,
            pins: vec!["pinned_call"],
            keep_tools: vec!["read"],
        };
        let (changed, _) = mask_request(&ws, &mut masked, None, &opts).unwrap();
        assert_eq!(changed, 0);
        assert_eq!(masked, req);

        // Trigger threshold still gates duplicate retirement.
        let mut high_trigger = req.clone();
        let opts = MaskOptions {
            keep: 10,
            minimum: 1024,
            trigger: 100_000,
            restore: false,
            pins: Vec::new(),
            keep_tools: Vec::new(),
        };
        let (changed, _) = mask_request(&ws, &mut high_trigger, None, &opts).unwrap();
        assert_eq!(changed, 0);
        assert_eq!(high_trigger, req);
    }

    #[test]
    fn retired_stubs_stay_prefix_cache_identical_as_conversation_grows() {
        let (_dir, ws) = test_workspace();
        let run_fail = format!(
            "{}error[E0425]: cannot find value `tax` in this scope\n\
             [justless run-Ab12 · 1.8 KB of 9.4 KB shown · exit 101]\n",
            "   Compiling demo v0.1.0\n".repeat(80)
        );
        let outline = format!(
            "revision 77501e2869cd09afbec6895c · 977 lines\n{}",
            "#1 L1-10 function foo  fn foo()\n".repeat(60)
        );
        let other = "normal output line\n".repeat(100);

        // Turn 1: run_fail appears twice inside keep=3 -> index 0 retires early.
        let mut turn1 = tool_messages(&[
            ("c0", "bash", json!(run_fail), false),
            ("c1", "read", json!(outline), false),
            ("c2", "bash", json!(run_fail), false),
        ]);
        mask_request(&ws, &mut turn1, None, &default_opts(3)).unwrap();
        let stub0_t1 = turn1["messages"][2]["content"][0]["content"].clone();
        assert!(marker(&stub0_t1).is_some());

        // Turn 2: append c3 (other). Now c0 is also past cutoff; its stub MUST stay identical.
        let mut turn2 = tool_messages(&[
            ("c0", "bash", json!(run_fail), false),
            ("c1", "read", json!(outline), false),
            ("c2", "bash", json!(run_fail), false),
            ("c3", "bash", json!(other), false),
        ]);
        mask_request(&ws, &mut turn2, None, &default_opts(3)).unwrap();
        assert_eq!(turn2["messages"][2]["content"][0]["content"], stub0_t1);
        // Entire prefix from Turn 1 is unchanged in Turn 2!
        assert_eq!(
            turn2["messages"].as_array().unwrap()[..7],
            turn1["messages"].as_array().unwrap()[..7]
        );

        // Turn 3: append c4 -> c1 (outline) now retires too; c0 stays identical.
        let mut turn3 = tool_messages(&[
            ("c0", "bash", json!(run_fail), false),
            ("c1", "read", json!(outline), false),
            ("c2", "bash", json!(run_fail), false),
            ("c3", "bash", json!(other), false),
            ("c4", "bash", json!(other), false),
        ]);
        mask_request(&ws, &mut turn3, None, &default_opts(3)).unwrap();
        let stub1_t3 = turn3["messages"][4]["content"][0]["content"].clone();
        assert_eq!(turn3["messages"][2]["content"][0]["content"], stub0_t1);
        assert!(marker(&stub1_t3).is_some());

        // Turn 4: append c5 -> c0 and c1 both remain byte-for-byte identical.
        let mut turn4 = tool_messages(&[
            ("c0", "bash", json!(run_fail), false),
            ("c1", "read", json!(outline), false),
            ("c2", "bash", json!(run_fail), false),
            ("c3", "bash", json!(other), false),
            ("c4", "bash", json!(other), false),
            ("c5", "bash", json!("extra\n".repeat(300)), false),
        ]);
        mask_request(&ws, &mut turn4, None, &default_opts(3)).unwrap();
        assert_eq!(turn4["messages"][2]["content"][0]["content"], stub0_t1);
        assert_eq!(turn4["messages"][4]["content"][0]["content"], stub1_t3);
        // c2 had the same run_fail content as c0, so when c2 retires it gets the exact same stub.
        assert_eq!(turn4["messages"][6]["content"][0]["content"], stub0_t1);
    }

    #[test]
    fn semantic_cues_preserve_key_context_and_respect_bounds_and_redaction() {
        let (_dir, ws) = test_workspace();
        let run_output = format!(
            "{}\x1b[1;31merror[E0308]\x1b[0m: mismatched types in checkout\n\
             [justless run-Zx99 · 2.0 KB of 14.0 KB shown · exit 101]\n",
            "   Compiling app v0.1.0\n".repeat(80)
        );
        let outline_output = format!(
            "revision 77501e2869cd09afbec6895c · 977 lines\n{}",
            "#1 L1-10 function helper  fn helper()\n".repeat(60)
        );
        let file_header_output = format!(
            "src/pricing.rs (revision 77501e2869cd09af, 420 lines)\n{}",
            "pub fn rate() -> u64 { 1 }\n".repeat(80)
        );
        let failure_output = format!(
            "{}test checkout::applies_discount ... FAILED\n\
             test result: FAILED. 12 passed; 1 failed; 0 ignored\n",
            "running 13 tests\n".repeat(100)
        );
        let plain_output = "plain log line without structure\n".repeat(100);
        // Secret spanning past the 80-char truncation point must be redacted BEFORE truncation.
        // Built at runtime so `real_build_and_source_output_have_nothing_to_redact` sees no literal secrets.
        let secret = format!(
            "{}_{}{}",
            "ghp", "7kQ9mN2pR5tV8wX1yZ4", "bC6dF0hJ3lM9nP2qS4rT6"
        );
        let secret_error = format!(
            "{}error: failed to authenticate with upstream registry using token {secret} during push\n",
            "info: uploading package\n".repeat(100)
        );
        let short_secret_error = format!(
            "{}error: rejected token {secret}\n",
            "info: calling api\n".repeat(100)
        );
        let pem_head = format!("-----{} RSA {}-----", "BEGIN", "PRIVATE KEY");
        let pem_tail = format!("-----{} RSA {}-----", "END", "PRIVATE KEY");
        let pem_error = format!(
            "{}error: {pem_head}MIIEpAIBAAKCAQEA0123456789abcdef0123456789abcdef{pem_tail}\n",
            "info: loading tls\n".repeat(100)
        );

        let req = tool_messages(&[
            ("r_run", "bash", json!(run_output), false),
            ("r_out", "read", json!(outline_output), false),
            ("r_hdr", "read", json!(file_header_output), false),
            ("r_fail", "bash", json!(failure_output), false),
            ("r_plain", "bash", json!(plain_output), false),
            ("r_sec", "bash", json!(secret_error), false),
            ("r_sec_short", "bash", json!(short_secret_error), false),
            ("r_pem", "bash", json!(pem_error), false),
        ]);
        let mut masked = req.clone();
        mask_request(&ws, &mut masked, None, &default_opts(0)).unwrap();

        let stub_at = |idx: usize| {
            masked["messages"][2 + idx * 2]["content"][0]["content"]
                .as_str()
                .unwrap()
                .to_owned()
        };

        let s_run = stub_at(0);
        assert!(s_run.starts_with(PREFIX) && s_run.ends_with(']'), "{s_run}");
        assert!(s_run.contains("run-Zx99 · exit 101"), "{s_run}");
        assert!(s_run.contains("error[E0308]: mismatched types"), "{s_run}");
        assert!(!s_run.contains('\x1b'), "{s_run}");
        assert!(marker(&json!(s_run)).is_some());

        let s_out = stub_at(1);
        assert!(
            s_out.contains(" · revision 77501e2869cd09afbec6895c · 977 lines]"),
            "{s_out}"
        );
        assert!(marker(&json!(s_out)).is_some());

        let s_hdr = stub_at(2);
        assert!(
            s_hdr.contains(" · src/pricing.rs (revision 77501e2869cd09af, 420 lines)]"),
            "{s_hdr}"
        );
        assert!(marker(&json!(s_hdr)).is_some());

        let s_fail = stub_at(3);
        assert!(
            s_fail.contains(" · test checkout::applies_discount ... FAILED]"),
            "{s_fail}"
        );
        assert!(marker(&json!(s_fail)).is_some());

        // Plain output gets the classic bare stub with no extra cue bytes.
        let s_plain = stub_at(4);
        assert_eq!(s_plain.len(), PREFIX.len() + 64 + 1, "{s_plain}");
        assert!(valid_id(
            s_plain
                .strip_prefix(PREFIX)
                .unwrap()
                .strip_suffix(']')
                .unwrap()
        ));

        // Secret-bearing errors never leak the raw secret into the retired stub,
        // even when the secret straddles the 80-char truncation boundary.
        let s_sec = stub_at(5);
        assert!(!s_sec.contains("ghp_"), "leaked secret: {s_sec}");
        assert!(!s_sec.contains("7kQ9mN"), "leaked secret body: {s_sec}");
        assert!(!s_sec.contains("[redacted:github-token…"), "{s_sec}");
        assert!(s_sec.contains("error: failed to authenticate"), "{s_sec}");
        assert!(marker(&json!(s_sec)).is_some());

        let s_sec_short = stub_at(6);
        assert!(
            !s_sec_short.contains("ghp_"),
            "leaked secret: {s_sec_short}"
        );
        assert!(
            s_sec_short.contains("[redacted:github-token"),
            "expected redaction marker: {s_sec_short}"
        );
        assert!(marker(&json!(s_sec_short)).is_some());

        // PEM private key lines are dropped rather than surfaced in cues.
        let s_pem = stub_at(7);
        assert!(!s_pem.contains("PRIVATE KEY"), "{s_pem}");
        assert!(!s_pem.contains("MIIEpA"), "{s_pem}");
        assert_eq!(s_pem.len(), PREFIX.len() + 64 + 1, "{s_pem}");

        // Every enriched stub stays strictly under 35% of the original bytes and restores exactly.
        let mut restored = masked.clone();
        let mut restore_opts = default_opts(0);
        restore_opts.restore = true;
        mask_request(&ws, &mut restored, None, &restore_opts).unwrap();
        assert_eq!(restored, req);

        // Size guard: on a small result where the enriched stub would exceed 35% of
        // original bytes, fall back cleanly to the bare stub.
        let small_err = format!(
            "error: short failure message with details that make a cue too wide for 35 percent\n{}",
            "x".repeat(220)
        );
        let id = archive(&ws, &json!(small_err)).unwrap();
        let orig_bytes = serde_json::to_vec(&json!(small_err)).unwrap().len();
        let stub = build_stub(
            &json!(small_err),
            "",
            &id,
            orig_bytes,
            crate::redact::for_workspace(&ws),
        )
        .unwrap();
        assert_eq!(stub, json!(format!("{PREFIX}{id}]")));
    }

    #[test]
    fn semantic_cues_cover_multifile_outlines_grouped_search_and_tail_summaries() {
        let (_dir, ws) = test_workspace();
        let multi_outline = format!(
            "=== Cargo.toml ===\nrevision 0123456789abcdef01234567 · 42 lines\n{}",
            "#1 L1-5 table package\n".repeat(80)
        );
        let id_hex = "b".repeat(64);
        let outside_read = format!(
            "{PREFIX}{id_hex}] exact original Read result · 298 lines\n{}",
            "#1 L1-20 function main  fn main()\n".repeat(80)
        );
        let grouped_grep = format!(
            "{PREFIX}{id_hex}] exact original result\nGrep: 150 matches in 31 files\n{}",
            "src/big.rs ×150\n".repeat(120)
        );
        // Error appearing only at the tail of a 1500-line log is still caught.
        let long_log = format!(
            "{}=== 2 failed, 148 passed in 3.10s ===\n",
            "ok step completed cleanly\n".repeat(1500)
        );
        let req = tool_messages(&[
            ("m1", "read", json!(multi_outline), false),
            ("m2", "read", json!(outside_read), false),
            ("m3", "grep", json!(grouped_grep), false),
            ("m4", "bash", json!(long_log), false),
        ]);
        let mut masked = req.clone();
        mask_request(&ws, &mut masked, None, &default_opts(0)).unwrap();
        let stub_at = |idx: usize| {
            masked["messages"][2 + idx * 2]["content"][0]["content"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert!(
            stub_at(0).contains(" · Cargo.toml (revision 0123456789abcdef01234567 · 42 lines)]"),
            "{}",
            stub_at(0)
        );
        assert!(
            stub_at(1).contains(" · Read outline · 298 lines]"),
            "{}",
            stub_at(1)
        );
        assert!(
            stub_at(2).contains(" · Grep: 150 matches in 31 files]"),
            "{}",
            stub_at(2)
        );
        assert!(
            stub_at(3).contains(" · === 2 failed, 148 passed in 3.10s ===]"),
            "{}",
            stub_at(3)
        );
        // Round-trip restores exact originals, including hook-archived headers!
        let mut restored = masked.clone();
        let mut restore_opts = default_opts(0);
        restore_opts.restore = true;
        mask_request(&ws, &mut restored, None, &restore_opts).unwrap();
        assert_eq!(restored, req);
    }

    #[test]
    fn marker_distinguishes_stubs_from_archived_read_and_search_headers() {
        let id = "a".repeat(64);
        assert_eq!(marker(&json!(format!("{PREFIX}{id}]"))), Some(id.as_str()));
        assert_eq!(
            marker(&json!(format!(
                "{PREFIX}{id} · run-ab12 · exit 1 · error[E01]]"
            ))),
            Some(id.as_str())
        );
        // Hook-archived Read and Grep outputs start with `[justless recall ID] exact ...`
        // and must NOT be confused with retired stubs.
        assert_eq!(
            marker(&json!(format!(
                "{PREFIX}{id}] exact original Read result · 298 lines\n#1 L1-5 fn a()"
            ))),
            None
        );
        assert_eq!(
            marker(&json!(format!(
                "{PREFIX}{id}] exact original result\nGrep: 12 matches in 3 files"
            ))),
            None
        );
        assert_eq!(marker(&json!(format!("{PREFIX}{id} · ]"))), None);
        assert_eq!(
            marker(&json!(format!("{PREFIX}{id} · line1\nline2]"))),
            None
        );
    }
}
