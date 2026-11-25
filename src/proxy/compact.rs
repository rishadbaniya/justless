//! Live-zone-only, prefix-cache-safe compaction for newest uncached tool results.
//!
//! Mutating any message inside a cached prompt prefix invalidates the provider's
//! prompt cache (and breaks Anthropic preserved-thinking signatures). By contrast,
//! the newest uncached tool outputs strictly at the tail of the request — after at
//! least one prior assistant / call turn and strictly after any `cache_control`
//! breakpoint — have never been written to the prompt cache:
//! - Anthropic `/v1/messages`: `tool_result` blocks in the final `user` message.
//! - OpenAI `/chat/completions`: trailing contiguous run of `role: "tool"` messages.
//! - OpenAI `/responses`: trailing contiguous run of `type: "function_call_output"` items.
//!
//! Each eligible oversized text block is:
//! 1. Checked against strict safety guards (tail-only live zone, prior turn present,
//!    strictly after any `cache_control`, text-only, `>= min_bytes`, not already
//!    starting with `[justless `).
//! 2. Compacted on its redacted text through Justless's structured compactors
//!    (`jsonview`, `tableview`, `logview`, `stacktrace`, or bounded head+tail).
//! 3. Replaced **only** when the compacted text plus its exact recall footer fits
//!    within `compact_live_budget` and saves at least 30% of the original bytes.
//! 4. Archived verbatim via `crate::observations::archive` *before* replacement so
//!    `justless recall <id>` always reproduces 100% exact original bytes.
//!
//! All bytes outside the replaced string spans are copied byte-for-byte untouched.
//! Any parse, archive, or format error fails open and forwards the original block.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use serde_json::value::RawValue;

use super::ledger::LiveCompacted;
use super::plan::{self, Body, Options};
use crate::history::human_bytes;
use crate::workspace::Workspace;

/// Upper bound on text fed to `logview` / `stacktrace` to keep latency bounded.
const MAX_STRUCTURED_BYTES: usize = 2 * 1024 * 1024;
/// Minimum percentage of UTF-8 bytes that must be saved (strictly smaller by >= 30%).
const MIN_SAVED_PERCENT: usize = 30;
/// Exact length of an observation SHA-256 hex ID.
const OBSERVATION_ID_LEN: usize = 64;

static LOG_ISO_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\[?\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\]?(?:\s+|$)",
    )
    .expect("iso log timestamp pattern")
});
static LOG_SYSLOG_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}\s+",
    )
    .expect("syslog timestamp pattern")
});
static LOG_CLOCK_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[?\d{2}:\d{2}:\d{2}(?:[.,]\d+)?\]?\s+").expect("clock timestamp pattern")
});
static LOG_KLOG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[IWEF]\d{4}\s+\d{2}:\d{2}:\d{2}\.\d+\s+\d+\s+").expect("klog pattern")
});
static LOG_LEVEL_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:\[(?i:trace|trc|debug|dbg|info|inf|notice|warn|warning|wrn|error|err|erro|fatal|fata|crit|critical|severe)\]|(?:TRACE|TRC|DEBUG|DBG|INFO|INF|NOTICE|WARN|WARNING|WRN|ERROR|ERR|ERRO|FATAL|FATA|CRIT|CRITICAL|SEVERE)(?:\s*[:|\-]\s*|\s+))",
    )
    .expect("log level prefix pattern")
});
static LOG_LOGFMT_LEVEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|\s)(?:level|lvl|severity)=[A-Za-z]+").expect("logfmt level pattern")
});
static LOG_K8S_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)\]\s+(.*)$").expect("k8s prefix pattern")
});
static LOG_COMPOSE_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z0-9._-]{1,48})\s*\|\s+(.*)$").expect("compose prefix pattern")
});

/// Result of compacting one request's uncached live zone.
pub struct LiveResult {
    pub body: Vec<u8>,
    pub stats: LiveCompacted,
}

/// Byte-exact replacement of one JSON string literal span inside the request body.
struct Replacement {
    start: usize,
    end: usize,
    encoded_json: String,
    before_bytes: u64,
    after_bytes: u64,
}

/// Attempts to compact eligible oversized `tool_result` text blocks in the final
/// uncached `user` message of an Anthropic Messages request `body`.
///
/// Returns `None` if nothing was compacted or on any parse/safety/archive error
/// (fail-open). All bytes outside the replaced string spans are preserved verbatim.
pub fn compact_live_zone(ws: &Workspace, body: &Body<'_>, options: &Options) -> Option<LiveResult> {
    if !live_enabled(options) {
        return None;
    }
    let raw_messages = body.raw("messages")?;
    let messages: Vec<&RawValue> = serde_json::from_str(raw_messages.get()).ok()?;
    if messages.len() < 2 || !has_role(&messages[..messages.len() - 1], "assistant") {
        return None;
    }

    let last_msg = *messages.last()?;
    let msg_members = plan::parse_members(last_msg.get())?;
    if !member_str_eq(&msg_members, "role", "user")
        || has_non_null_member(&msg_members, "cache_control")
    {
        return None;
    }

    let raw_content = member(&msg_members, "content")?;
    if !raw_content.get().trim_start().starts_with('[') {
        return None;
    }
    let blocks: Vec<&RawValue> = serde_json::from_str(raw_content.get()).ok()?;
    let eligible = after_last_cache_control(&blocks);
    if eligible.is_empty() {
        return None;
    }

    let retrieval_ids = anthropic_retrieval_ids(&messages[..messages.len() - 1]);
    let mut redactor: Option<Option<&'static crate::redact::Redactor>> = None;
    let mut replacements: Vec<Replacement> = Vec::new();

    for raw_block in eligible {
        let Some(block_members) = plan::parse_members(raw_block.get()) else {
            continue;
        };
        if !member_str_eq(&block_members, "type", "tool_result")
            || has_non_null_member(&block_members, "cache_control")
            || is_retrieval_result(&block_members, "tool_use_id", &retrieval_ids)
        {
            continue;
        }
        let Some(content_raw) = member(&block_members, "content") else {
            continue;
        };
        compact_field_raw(
            ws,
            body,
            content_raw,
            &["text"],
            options,
            &mut redactor,
            &mut replacements,
        );
    }

    apply_replacements(body, replacements)
}

/// Attempts to compact eligible oversized trailing `role: "tool"` messages in an
/// OpenAI Chat Completions request `body` (`/chat/completions`).
///
/// Only the trailing contiguous run of `role: "tool"` messages strictly after at
/// least one prior `role: "assistant"` message (and after any `cache_control`
/// breakpoint) is eligible. Every earlier byte is preserved verbatim.
pub fn compact_openai_chat_live_zone(
    ws: &Workspace,
    body: &Body<'_>,
    options: &Options,
) -> Option<LiveResult> {
    if !live_enabled(options) {
        return None;
    }
    let raw_messages = body.raw("messages")?;
    if !raw_messages.get().trim_start().starts_with('[') {
        return None;
    }
    let messages: Vec<&RawValue> = serde_json::from_str(raw_messages.get()).ok()?;
    if messages.len() < 2 {
        return None;
    }

    let mut run_start = messages.len();
    while run_start > 0 {
        let Some(members) = plan::parse_members(messages[run_start - 1].get()) else {
            break;
        };
        if !member_str_eq(&members, "role", "tool") {
            break;
        }
        run_start -= 1;
    }
    if run_start == messages.len()
        || run_start == 0
        || !has_role(&messages[..run_start], "assistant")
    {
        return None;
    }

    let eligible = after_last_cache_control(&messages[run_start..]);
    if eligible.is_empty() {
        return None;
    }

    let retrieval_ids = openai_chat_retrieval_ids(&messages[..run_start]);
    let mut redactor: Option<Option<&'static crate::redact::Redactor>> = None;
    let mut replacements: Vec<Replacement> = Vec::new();

    for raw_msg in eligible {
        let Some(msg_members) = plan::parse_members(raw_msg.get()) else {
            continue;
        };
        if has_non_null_member(&msg_members, "cache_control")
            || is_retrieval_result(&msg_members, "tool_call_id", &retrieval_ids)
        {
            continue;
        }
        let Some(content_raw) = member(&msg_members, "content") else {
            continue;
        };
        compact_field_raw(
            ws,
            body,
            content_raw,
            &["text"],
            options,
            &mut redactor,
            &mut replacements,
        );
    }

    apply_replacements(body, replacements)
}

/// Attempts to compact eligible oversized trailing `type: "function_call_output"`
/// items in an OpenAI Responses API request `body` (`/responses`, `input[]`).
///
/// Only the trailing contiguous run of `function_call_output` items strictly after
/// at least one prior `function_call` or `message` item (and after any
/// `cache_control` breakpoint) is eligible. Every earlier byte is preserved verbatim.
pub fn compact_openai_responses_live_zone(
    ws: &Workspace,
    body: &Body<'_>,
    options: &Options,
) -> Option<LiveResult> {
    if !live_enabled(options) {
        return None;
    }
    let raw_input = body.raw("input")?;
    if !raw_input.get().trim_start().starts_with('[') {
        return None;
    }
    let items: Vec<&RawValue> = serde_json::from_str(raw_input.get()).ok()?;
    if items.len() < 2 {
        return None;
    }

    let mut run_start = items.len();
    while run_start > 0 {
        let Some(members) = plan::parse_members(items[run_start - 1].get()) else {
            break;
        };
        if !member_str_eq(&members, "type", "function_call_output") {
            break;
        }
        run_start -= 1;
    }
    if run_start == items.len() || run_start == 0 {
        return None;
    }

    let has_prior_turn = items[..run_start].iter().any(|raw| {
        let Some(members) = plan::parse_members(raw.get()) else {
            return false;
        };
        let kind =
            member(&members, "type").and_then(|t| serde_json::from_str::<String>(t.get()).ok());
        match kind.as_deref() {
            Some("function_call" | "message") => true,
            None => member(&members, "role")
                .and_then(|r| serde_json::from_str::<String>(r.get()).ok())
                .is_some(),
            _ => false,
        }
    });
    if !has_prior_turn {
        return None;
    }

    let eligible = after_last_cache_control(&items[run_start..]);
    if eligible.is_empty() {
        return None;
    }

    let retrieval_ids = openai_responses_retrieval_ids(&items[..run_start]);
    let mut redactor: Option<Option<&'static crate::redact::Redactor>> = None;
    let mut replacements: Vec<Replacement> = Vec::new();

    for raw_item in eligible {
        let Some(item_members) = plan::parse_members(raw_item.get()) else {
            continue;
        };
        if has_non_null_member(&item_members, "cache_control")
            || is_retrieval_result(&item_members, "call_id", &retrieval_ids)
        {
            continue;
        }
        let Some(output_raw) = member(&item_members, "output") else {
            continue;
        };
        compact_field_raw(
            ws,
            body,
            output_raw,
            &["input_text", "text"],
            options,
            &mut redactor,
            &mut replacements,
        );
    }

    apply_replacements(body, replacements)
}

fn member_str(members: &[(String, &RawValue)], key: &str) -> Option<String> {
    serde_json::from_str::<String>(member(members, key)?.get()).ok()
}

fn is_retrieval_result(
    members: &[(String, &RawValue)],
    id_key: &str,
    retrieval_ids: &HashSet<String>,
) -> bool {
    !retrieval_ids.is_empty()
        && member_str(members, id_key).is_some_and(|id| retrieval_ids.contains(&id))
}

/// Collects tool-call IDs from prior Anthropic assistant messages that invoke
/// `justless recall|log|expand|search` (shell or MCP), preventing recall loops.
fn anthropic_retrieval_ids(messages: &[&RawValue]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for raw_msg in messages {
        let Some(msg_members) = plan::parse_members(raw_msg.get()) else {
            continue;
        };
        if !member_str_eq(&msg_members, "role", "assistant") {
            continue;
        }
        let Some(content) = member(&msg_members, "content") else {
            continue;
        };
        let Ok(blocks) = serde_json::from_str::<Vec<&RawValue>>(content.get()) else {
            continue;
        };
        for block in blocks {
            let Some(bm) = plan::parse_members(block.get()) else {
                continue;
            };
            let is_use = member_str(&bm, "type")
                .is_some_and(|t| matches!(t.as_str(), "tool_use" | "mcp_tool_use"));
            if !is_use {
                continue;
            }
            let name = member_str(&bm, "name").unwrap_or_default();
            if is_retrieval_call(&name, member(&bm, "input"))
                && let Some(id) = member_str(&bm, "id")
            {
                ids.insert(id);
            }
        }
    }
    ids
}

/// Collects `tool_calls[].id` values from prior OpenAI Chat assistant messages
/// that invoke `justless recall|log|expand|search`.
fn openai_chat_retrieval_ids(messages: &[&RawValue]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for raw_msg in messages {
        let Some(msg_members) = plan::parse_members(raw_msg.get()) else {
            continue;
        };
        if !member_str_eq(&msg_members, "role", "assistant") {
            continue;
        }
        let Some(calls_raw) = member(&msg_members, "tool_calls") else {
            continue;
        };
        let Ok(calls) = serde_json::from_str::<Vec<&RawValue>>(calls_raw.get()) else {
            continue;
        };
        for call in calls {
            let Some(cm) = plan::parse_members(call.get()) else {
                continue;
            };
            let Some(fn_raw) = member(&cm, "function") else {
                continue;
            };
            let Some(fm) = plan::parse_members(fn_raw.get()) else {
                continue;
            };
            let name = member_str(&fm, "name").unwrap_or_default();
            if is_retrieval_call(&name, member(&fm, "arguments"))
                && let Some(id) = member_str(&cm, "id")
            {
                ids.insert(id);
            }
        }
    }
    ids
}

/// Collects `call_id` values from prior OpenAI Responses `function_call` items
/// that invoke `justless recall|log|expand|search`.
fn openai_responses_retrieval_ids(items: &[&RawValue]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for raw_item in items {
        let Some(im) = plan::parse_members(raw_item.get()) else {
            continue;
        };
        if !member_str_eq(&im, "type", "function_call") {
            continue;
        }
        let name = member_str(&im, "name").unwrap_or_default();
        if is_retrieval_call(&name, member(&im, "arguments"))
            && let Some(id) = member_str(&im, "call_id")
        {
            ids.insert(id);
        }
    }
    ids
}

fn is_retrieval_call(name: &str, args_raw: Option<&RawValue>) -> bool {
    is_retrieval_tool_name(name) || args_have_retrieval_cmd(args_raw)
}

fn is_retrieval_tool_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("justless")
        && ["recall", "log", "expand", "search"]
            .iter()
            .any(|op| lower.ends_with(op))
}

fn args_have_retrieval_cmd(args_raw: Option<&RawValue>) -> bool {
    let Some(raw) = args_raw else {
        return false;
    };
    let text = raw.get();
    if !text.contains("justless") && !text.contains("\\u") {
        return false;
    }
    let Ok(val) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    value_has_retrieval_cmd(&val)
}

fn value_has_retrieval_cmd(val: &Value) -> bool {
    match val {
        Value::String(s) => {
            if contains_justless_retrieval_cmd(s) {
                return true;
            }
            let trimmed = s.trim_start();
            (trimmed.starts_with('{') || trimmed.starts_with('['))
                && serde_json::from_str::<Value>(trimmed)
                    .ok()
                    .is_some_and(|inner| value_has_retrieval_cmd(&inner))
        }
        Value::Array(items) => {
            if items.iter().all(Value::is_string) {
                let joined = items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                if contains_justless_retrieval_cmd(&joined) {
                    return true;
                }
            }
            items.iter().any(value_has_retrieval_cmd)
        }
        Value::Object(map) => map.values().any(value_has_retrieval_cmd),
        _ => false,
    }
}

fn contains_justless_retrieval_cmd(cmd: &str) -> bool {
    const OPS: [&str; 4] = ["recall", "log", "expand", "search"];
    if !cmd.contains("justless") {
        return false;
    }
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    for (i, token) in tokens.iter().enumerate() {
        let bin = token.trim_matches(|c| matches!(c, '\'' | '"' | '(' | ')'));
        let name = bin.rsplit(['/', '\\']).next().unwrap_or(bin);
        if name != "justless" && name != "justless.exe" {
            continue;
        }
        let mut j = i + 1;
        while j < tokens.len() {
            let arg = tokens[j].trim_matches(|c| matches!(c, '\'' | '"'));
            if arg == "--root" {
                j += 2;
            } else if arg.starts_with("--root=") {
                j += 1;
            } else {
                if OPS.contains(&arg) {
                    return true;
                }
                break;
            }
        }
    }
    false
}

fn live_enabled(options: &Options) -> bool {
    options.compact_live && options.compact_live_budget >= 256
}

fn member<'a>(members: &[(String, &'a RawValue)], key: &str) -> Option<&'a RawValue> {
    members
        .iter()
        .find_map(|(name, value)| (name == key).then_some(*value))
}

fn member_str_eq(members: &[(String, &RawValue)], key: &str, expected: &str) -> bool {
    member(members, key)
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
        .as_deref()
        == Some(expected)
}

fn has_non_null_member(members: &[(String, &RawValue)], key: &str) -> bool {
    member(members, key).is_some_and(|raw| raw.get().trim() != "null")
}

fn has_role(messages: &[&RawValue], role: &str) -> bool {
    messages.iter().any(|raw| {
        plan::parse_members(raw.get()).is_some_and(|members| member_str_eq(&members, "role", role))
    })
}

/// Returns the trailing slice strictly after the last item containing a non-null
/// `cache_control` breakpoint so cached prefixes are never mutated.
fn after_last_cache_control<'a>(items: &'a [&'a RawValue]) -> &'a [&'a RawValue] {
    let last_breakpoint = items
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, raw)| raw_has_cache_control(raw.get()).then_some(idx));
    &items[last_breakpoint.map_or(0, |idx| idx + 1)..]
}

fn compact_field_raw(
    ws: &Workspace,
    body: &Body<'_>,
    field_raw: &RawValue,
    text_types: &[&str],
    options: &Options,
    redactor: &mut Option<Option<&'static crate::redact::Redactor>>,
    replacements: &mut Vec<Replacement>,
) {
    let trimmed = field_raw.get().trim_start();
    if trimmed.starts_with('"') {
        if let Some(rep) = compact_string_raw(ws, body, field_raw, options, redactor) {
            replacements.push(rep);
        }
    } else if trimmed.starts_with('[') {
        let Ok(inner_blocks) = serde_json::from_str::<Vec<&RawValue>>(field_raw.get()) else {
            return;
        };
        // Only compact plain text parts; images, documents, files, and other
        // block types stay byte-for-byte untouched.
        for inner in inner_blocks {
            let Some(inner_members) = plan::parse_members(inner.get()) else {
                continue;
            };
            if has_non_null_member(&inner_members, "cache_control") {
                continue;
            }
            let is_text = member(&inner_members, "type")
                .and_then(|k| serde_json::from_str::<String>(k.get()).ok())
                .is_some_and(|kind| text_types.contains(&kind.as_str()));
            if !is_text {
                continue;
            }
            let Some(text_raw) = member(&inner_members, "text") else {
                continue;
            };
            if !text_raw.get().trim_start().starts_with('"') {
                continue;
            }
            if let Some(rep) = compact_string_raw(ws, body, text_raw, options, redactor) {
                replacements.push(rep);
            }
        }
    }
}

fn apply_replacements(body: &Body<'_>, mut replacements: Vec<Replacement>) -> Option<LiveResult> {
    if replacements.is_empty() {
        return None;
    }

    replacements.sort_by_key(|r| r.start);
    // Sanity check: spans must be strictly non-overlapping and within body.
    let src = body.text().as_bytes();
    let mut prev_end = 0usize;
    for r in &replacements {
        if r.start < prev_end || r.start > r.end || r.end > src.len() {
            return None;
        }
        prev_end = r.end;
    }

    let mut out = Vec::with_capacity(src.len());
    let mut cursor = 0usize;
    let mut stats = LiveCompacted::default();
    for r in replacements {
        out.extend_from_slice(&src[cursor..r.start]);
        out.extend_from_slice(r.encoded_json.as_bytes());
        cursor = r.end;
        stats.results += 1;
        stats.bytes_before += r.before_bytes;
        stats.bytes_after += r.after_bytes;
    }
    out.extend_from_slice(&src[cursor..]);
    stats.saved_bytes = stats.bytes_before.saturating_sub(stats.bytes_after);
    stats.est_tokens = stats.saved_bytes.div_ceil(4);

    // Final invariant: outbound JSON body must actually be smaller.
    if out.len() >= src.len() {
        return None;
    }
    Some(LiveResult { body: out, stats })
}

/// Checks whether a raw JSON block contains any non-null `cache_control` key
/// (either on the block itself or on any nested object inside it).
fn raw_has_cache_control(raw: &str) -> bool {
    // Fast path: avoid full Value parse when `cache_control` (or unicode escape) is absent.
    if !raw.contains("cache_control") && !raw.contains("\\u") {
        return false;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value_has_cache_control(&value),
        // Fail safe: if we cannot parse the block, treat it as protected.
        Err(_) => true,
    }
}

fn value_has_cache_control(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(k, v)| (k == "cache_control" && !v.is_null()) || value_has_cache_control(v)),
        Value::Array(items) => items.iter().any(value_has_cache_control),
        _ => false,
    }
}

fn compact_string_raw(
    ws: &Workspace,
    body: &Body<'_>,
    raw_string: &RawValue,
    options: &Options,
    redactor: &mut Option<Option<&'static crate::redact::Redactor>>,
) -> Option<Replacement> {
    let raw_text: String = serde_json::from_str(raw_string.get()).ok()?;
    let before = raw_text.len();
    if before < options.compact_live_min_bytes {
        return None;
    }
    if already_justless(&raw_text) || raw_text.contains('\0') {
        return None;
    }

    let active_redactor = *redactor.get_or_insert_with(|| crate::redact::for_workspace(ws));
    let safe: Cow<'_, str> = match active_redactor {
        Some(r) => r.text(&raw_text),
        None => Cow::Borrowed(&raw_text),
    };

    // Effective target budget: both <= compact_live_budget AND saves >= 30%.
    let max_for_30pct = before.saturating_mul(100 - MIN_SAVED_PERCENT) / 100;
    let effective_target = options.compact_live_budget.min(max_for_30pct);
    // Exact upper bound on the footer length so the compacted body + footer fits cleanly.
    let max_footer_len = footer_len_upper_bound(before, effective_target);
    let body_budget = effective_target.saturating_sub(max_footer_len);
    if body_budget < 128 {
        return None;
    }

    let compacted_body = compact_payload(safe.as_ref(), body_budget, &ws.root)?;
    let compacted_body = compacted_body.trim_end_matches('\n');
    if compacted_body.is_empty() {
        return None;
    }

    // Verify exact total size & >= 30% reduction BEFORE writing to the archive so
    // non-qualifying blocks never touch disk.
    let expected_total = exact_total_len(compacted_body.len(), before)?;
    if expected_total > options.compact_live_budget || expected_total >= before {
        return None;
    }
    if expected_total.saturating_mul(100) > before.saturating_mul(100 - MIN_SAVED_PERCENT) {
        return None;
    }

    // Archive the exact unredacted original bytes BEFORE replacing.
    // Fail open on any archive error.
    let id = crate::observations::archive(ws, &Value::String(raw_text)).ok()?;
    if id.len() != OBSERVATION_ID_LEN {
        return None;
    }

    let final_text = format!(
        "{compacted_body}\n[justless recall {id} · compacted {expected_total} of {before} bytes]"
    );
    debug_assert_eq!(final_text.len(), expected_total);

    let encoded_json = serde_json::to_string(&final_text).ok()?;
    if encoded_json.len() >= raw_string.get().len() {
        return None;
    }

    let (start, end) = body.span(raw_string);
    Some(Replacement {
        start,
        end,
        encoded_json,
        before_bytes: before as u64,
        after_bytes: final_text.len() as u64,
    })
}

fn already_justless(text: &str) -> bool {
    if text.starts_with("[justless ") || text.trim_start().starts_with("[justless ") {
        return true;
    }
    // Also skip results already carrying a live-zone recall footer.
    text.trim_end()
        .rsplit('\n')
        .next()
        .is_some_and(|last| last.starts_with("[justless recall ") && last.ends_with(']'))
}

/// Exact byte length of `"\n[justless recall <64-hex> · compacted <total> of <before> bytes]"`
/// excluding the decimal digits of `<total>`.
fn footer_fixed_len(before: usize) -> usize {
    "\n[justless recall  · compacted  of  bytes]".len()
        + OBSERVATION_ID_LEN
        + before.to_string().len()
}

fn footer_len_upper_bound(before: usize, target: usize) -> usize {
    footer_fixed_len(before) + target.max(1).to_string().len()
}

/// Solves `total == body_len + footer_fixed_len(before) + digits(total)` exactly.
fn exact_total_len(body_len: usize, before: usize) -> Option<usize> {
    let base = body_len.checked_add(footer_fixed_len(before))?;
    for digits in 1..=20 {
        let candidate = base.checked_add(digits)?;
        if candidate.to_string().len() == digits {
            return Some(candidate);
        }
    }
    None
}

/// Runs Justless's zero-semantic-loss compaction pipeline on `text` within `budget`:
/// 1. NDJSON structured logs → `logview::format` (error-first folding)
/// 2. JSON objects/arrays/JSONL → `jsonview::views` (lossless table/compact, clipped cleanly if over budget)
/// 3. ASCII/Markdown/CLI tables → `tableview::format` (Tier 1 lossless or Tier 2 bounded)
/// 4. Structured text logs → `logview::format`
/// 5. Multi-language stack traces → `stacktrace::collapse` (bounded if still over budget)
/// 6. Bounded line-aware head+tail preview (fallback for plain text / code)
fn compact_payload(text: &str, budget: usize, workspace_root: &Path) -> Option<String> {
    if text.is_empty() || budget < 128 {
        return None;
    }

    // 1. JSON (with NDJSON structured-log detection so error lines inside JSON logs
    //    are surfaced instead of clipped).
    if could_be_json_prefix(text.as_bytes()) {
        if text.len() <= MAX_STRUCTURED_BYTES
            && looks_like_json_logs(text)
            && let Some(folded) = crate::logview::format(text, "", budget)
            && folded.len() <= budget
        {
            return Some(folded);
        }
        if text.len() <= crate::jsonview::LIMIT {
            let views = crate::jsonview::views(text, true);
            if let Some(best) = views.into_iter().next() {
                if best.len() <= budget {
                    return Some(best);
                }
                if let Some(clipped) = bound_json_view(&best, budget) {
                    return Some(clipped);
                }
            }
        }
    }

    // 2. CLI / ASCII / Markdown tables (`psql`, `mysql`, `aws --output table`, etc.).
    if text.len() <= crate::tableview::LIMIT {
        if let Some(table) = crate::tableview::format(text, budget)
            && table.len() <= budget
        {
            return Some(table);
        }
        if text.contains('\x1b') {
            let stripped = crate::capture::ANSI.replace_all(text, "");
            if let Some(table) = crate::tableview::format(&stripped, budget)
                && table.len() <= budget
            {
                return Some(table);
            }
        }
    }

    // 3. Structured container / timestamped / level-prefixed logs.
    if text.len() <= MAX_STRUCTURED_BYTES
        && looks_like_structured_logs(text)
        && let Some(folded) = crate::logview::format(text, "", budget)
        && folded.len() <= budget
    {
        return Some(folded);
    }

    // 4. Multi-language stack traces / tracebacks (Python, Node, Rust, Go, Java).
    if text.len() <= MAX_STRUCTURED_BYTES
        && let Some(collapsed) = crate::stacktrace::collapse(text, Some(workspace_root), None)
    {
        if collapsed.len() <= budget {
            return Some(collapsed);
        }
        if let Some(bounded) = bounded_head_tail(&collapsed, budget) {
            return Some(bounded);
        }
    }

    // 5. Bounded head+tail preview fallback.
    bounded_head_tail(text, budget)
}

/// Bounds a `jsonview` table/compact view to `budget`, preserving the schema/header
/// lines and leading rows along with trailing rows when multiline.
fn bound_json_view(view: &str, budget: usize) -> Option<String> {
    if view.len() <= budget {
        return Some(view.to_owned());
    }
    bounded_head_tail(view, budget)
}

/// UTF-8-safe, redaction-marker-safe, line-aware bounded head+tail preview fitting
/// strictly within `budget` bytes.
fn bounded_head_tail(text: &str, budget: usize) -> Option<String> {
    if text.len() <= budget {
        return Some(text.to_owned());
    }
    // Worst-case separator size: ~72 bytes.
    const SEP_RESERVE: usize = 80;
    if budget <= SEP_RESERVE + 32 {
        return None;
    }
    let usable = budget - SEP_RESERVE;
    let head_budget = usable * 3 / 5;
    let tail_budget = usable - head_budget;

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.len() >= 4
        && lines[0].len() <= head_budget
        && lines[lines.len() - 1].len() <= tail_budget
    {
        let mut head_end_idx = 0usize;
        let mut head_bytes = 0usize;
        for line in &lines {
            if head_bytes + line.len() > head_budget {
                break;
            }
            head_bytes += line.len();
            head_end_idx += 1;
        }

        let mut tail_start_idx = lines.len();
        let mut tail_bytes = 0usize;
        for idx in (head_end_idx..lines.len()).rev() {
            let line = lines[idx];
            if tail_bytes + line.len() > tail_budget {
                break;
            }
            tail_bytes += line.len();
            tail_start_idx = idx;
        }

        if head_end_idx > 0 && tail_start_idx > head_end_idx && tail_start_idx < lines.len() {
            let omitted_lines = tail_start_idx - head_end_idx;
            let omitted_bytes = text.len().saturating_sub(head_bytes + tail_bytes);
            let head: String = lines[..head_end_idx].concat();
            let tail: String = lines[tail_start_idx..].concat();
            let prefix = if head.ends_with('\n') { "" } else { "\n" };
            let sep = format!(
                "{prefix}[… omitted {omitted_lines} lines ({}) …]\n",
                human_bytes(omitted_bytes as u64)
            );
            let combined = format!("{head}{sep}{tail}");
            if combined.len() <= budget {
                return Some(combined);
            }
        }
    }

    // Character-boundary fallback for single very long lines (e.g. minified JSON/JS).
    let mut head_end = text.floor_char_boundary(head_budget);
    // Avoid splitting inside a `[redacted:...]` marker.
    if let Some(open) = text[..head_end].rfind("[redacted:")
        && !text[open..head_end].contains(']')
    {
        head_end = open;
    }

    let mut tail_start = text.ceil_char_boundary(text.len().saturating_sub(tail_budget));
    if tail_start > 0
        && let Some(open) = text[..tail_start].rfind("[redacted:")
        && !text[open..tail_start].contains(']')
        && let Some(close) = text[tail_start..].find(']')
    {
        tail_start = (tail_start + close + 1).min(text.len());
    }

    if head_end >= tail_start {
        return None;
    }

    let omitted = tail_start - head_end;
    let sep = format!("\n[… omitted {} …]\n", human_bytes(omitted as u64));
    let combined = format!("{}{}{}", &text[..head_end], sep, &text[tail_start..]);
    (combined.len() <= budget).then_some(combined)
}

// ---------------------------------------------------------------------------
// Fast content-type classifiers (aligned with src/exec.rs)

fn could_be_json_prefix(bytes: &[u8]) -> bool {
    let trimmed = bytes.trim_ascii_start();
    let Some((&first, rest)) = trimmed.split_first() else {
        return false;
    };
    let next = rest.trim_ascii_start();
    match first {
        b'{' => matches!(next.first(), Some(b'"' | b'}') | None),
        b'[' => match next.first() {
            Some(b'{' | b'[' | b'"' | b't' | b'f' | b'n' | b']') | None => true,
            Some(b'-' | b'0'..=b'9') => {
                let after_digits = next
                    .iter()
                    .skip(usize::from(next[0] == b'-'))
                    .skip_while(|b| b.is_ascii_digit())
                    .copied()
                    .next();
                !matches!(after_digits, Some(b'-' | b':'))
            }
            Some(_) => false,
        },
        _ => false,
    }
}

fn strip_leading_timestamp(line: &str) -> &str {
    if let Some(m) = LOG_ISO_TS.find(line) {
        &line[m.end()..]
    } else if let Some(m) = LOG_SYSLOG_TS.find(line) {
        &line[m.end()..]
    } else if let Some(m) = LOG_CLOCK_TS.find(line) {
        &line[m.end()..]
    } else {
        line
    }
}

fn has_json_key(line: &str, key: &str) -> bool {
    let pattern = format!("\"{key}\"");
    let mut offset = 0;
    while let Some(pos) = line[offset..].find(&pattern) {
        let after = &line[offset + pos + pattern.len()..];
        if after.trim_start().starts_with(':') {
            return true;
        }
        offset += pos + pattern.len();
    }
    false
}

fn is_json_log_line(line: &str) -> bool {
    let trimmed = line.trim();
    let candidate = if trimmed.starts_with('{') && trimmed.ends_with('}') {
        trimmed
    } else {
        let rest = strip_leading_timestamp(trimmed).trim();
        if rest.starts_with('{') && rest.ends_with('}') {
            rest
        } else {
            return false;
        }
    };
    let has_level = ["level", "lvl", "severity", "levelname", "log.level"]
        .iter()
        .any(|k| has_json_key(candidate, k));
    if has_level {
        return true;
    }
    let has_ts = ["ts", "timestamp", "time", "@timestamp", "datetime"]
        .iter()
        .any(|k| has_json_key(candidate, k));
    let has_msg = ["msg", "message", "event", "error", "err"]
        .iter()
        .any(|k| has_json_key(candidate, k));
    has_ts && has_msg
}

fn looks_like_json_logs(text: &str) -> bool {
    let mut sampled = 0usize;
    let mut json_objects = 0usize;
    let mut json_logs = 0usize;
    for raw in text.lines() {
        let clean = crate::capture::ANSI.replace_all(raw, "");
        let line = clean.trim();
        if line.is_empty() {
            continue;
        }
        sampled += 1;
        let body = strip_leading_timestamp(line).trim();
        if body.starts_with('{') && body.ends_with('}') {
            json_objects += 1;
            if is_json_log_line(line) {
                json_logs += 1;
            }
        }
        if sampled >= 64 {
            break;
        }
    }
    sampled >= 8 && json_objects * 4 >= sampled * 3 && json_logs * 2 >= json_objects
}

fn is_level_word_upper(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "TRACE"
            | "TRC"
            | "DEBUG"
            | "DBG"
            | "INFO"
            | "INF"
            | "LOG"
            | "NOTICE"
            | "WARN"
            | "WARNING"
            | "WRN"
            | "ERROR"
            | "ERR"
            | "ERRO"
            | "FATAL"
            | "FATA"
            | "CRIT"
            | "CRITICAL"
            | "PANIC"
            | "SEVERE"
    )
}

fn looks_like_structured_logs(text: &str) -> bool {
    let mut non_empty = 0usize;
    let mut sampled = 0usize;
    let mut structured = 0usize;
    for raw in text.lines() {
        let clean = crate::capture::ANSI.replace_all(raw, "");
        let line = clean.trim();
        if line.is_empty() {
            continue;
        }
        non_empty += 1;
        if sampled < 64 {
            sampled += 1;
            let body = if let Some(caps) = LOG_K8S_PREFIX.captures(line) {
                caps.get(2).map_or(line, |m| m.as_str()).trim_start()
            } else if let Some(caps) = LOG_COMPOSE_PREFIX.captures(line) {
                let svc = caps.get(1).map_or("", |m| m.as_str());
                if svc.bytes().any(|b| b.is_ascii_alphabetic()) && !is_level_word_upper(svc) {
                    caps.get(2).map_or(line, |m| m.as_str()).trim_start()
                } else {
                    line
                }
            } else {
                line
            };
            let is_structured = is_json_log_line(body)
                || LOG_KLOG.is_match(body)
                || LOG_ISO_TS.is_match(body)
                || LOG_SYSLOG_TS.is_match(body)
                || LOG_CLOCK_TS.is_match(body)
                || LOG_LEVEL_PREFIX.is_match(body)
                || LOG_LOGFMT_LEVEL.is_match(body);
            if is_structured {
                structured += 1;
            }
        }
        if non_empty >= 8 && sampled >= 64 {
            break;
        }
    }
    non_empty >= 8 && structured >= 4 && structured * 5 >= sampled * 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn workspace() -> (TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace {
            root: dir.path().to_owned(),
            cwd: dir.path().to_owned(),
            state: dir.path().join(".justless"),
            origin: dir.path().to_owned(),
        };
        (dir, ws)
    }

    fn live_options() -> Options {
        Options {
            compact_live: true,
            ..Options::default()
        }
    }

    fn large_json_array() -> String {
        let items: Vec<Value> = (0..180)
            .map(|i| {
                json!({
                    "id": i,
                    "service": "billing-api",
                    "region": "us-east-1",
                    "environment": "production",
                    "status": if i == 90 { "degraded" } else { "healthy" },
                    "latency_ms": 12 + (i % 50),
                    "version": "2.14.0"
                })
            })
            .collect();
        serde_json::to_string_pretty(&items).unwrap()
    }

    fn large_structured_log() -> String {
        let mut out = String::new();
        for i in 0..220 {
            if i == 110 {
                out.push_str(
                    "api-1 | 2026-11-25T10:01:50Z ERROR database connection refused host=pg-primary:5432 err=ECONNREFUSED\n",
                );
            } else {
                out.push_str(&format!(
                    "api-1 | 2026-11-25T10:{:02}:{:02}Z INFO GET /v1/healthz status=200 request_id=req-{i:05} latency={}ms\n",
                    (i / 60) % 60,
                    i % 60,
                    3 + (i % 7)
                ));
            }
        }
        out
    }

    fn large_ascii_table() -> String {
        let mut out = String::from(
            "+----+----------------+-------------+-----------+----------+----------+\n\
             | id | service        | region      | env       | status   | replicas |\n\
             +----+----------------+-------------+-----------+----------+----------+\n",
        );
        for i in 0..160 {
            out.push_str(&format!(
                "| {:<2} | worker-{:<7} | us-east-1   | prod      | running  | 3        |\n",
                i, i
            ));
        }
        out.push_str("+----+----------------+-------------+-----------+----------+----------+\n");
        out
    }

    fn large_python_traceback(root: &Path) -> String {
        let mut out = String::from("Traceback (most recent call last):\n");
        out.push_str(&format!(
            "  File \"{}/src/app.py\", line 42, in handle_request\n    return dispatch(payload)\n",
            root.display()
        ));
        for i in 0..95 {
            out.push_str(&format!(
                "  File \"/usr/lib/python3.12/site-packages/starlette/middleware/layer_{i:02}.py\", line {}, in __call__\n    await self.app(scope, receive, send)\n",
                100 + i
            ));
        }
        out.push_str(&format!(
            "  File \"{}/src/billing.py\", line 118, in charge_invoice\n    raise ValueError('negative invoice total')\n",
            root.display()
        ));
        out.push_str("ValueError: negative invoice total\n");
        out
    }

    #[test]
    fn preserves_prefix_and_thinking_blocks_byte_for_byte() {
        let (_dir, ws) = workspace();
        let payload = large_json_array();
        assert!(payload.len() > 20_000);
        let request = json!({
            "model": "claude-opus-5-5",
            "max_tokens": 4096,
            "system": [{"type": "text", "text": "You are a coding agent.", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Inspect the cluster.", "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "Checking service health first.", "signature": "sig_exact_001"},
                    {"type": "tool_use", "id": "toolu_01", "name": "Bash", "input": {"command": "curl /health"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01", "content": large_structured_log(), "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "Now listing instances.", "signature": "sig_exact_002"},
                    {"type": "tool_use", "id": "toolu_02", "name": "Bash", "input": {"command": "curl /instances"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_02", "content": payload}
                ]}
            ]
        });
        let raw_bytes = serde_json::to_string_pretty(&request)
            .unwrap()
            .replace("\"role\": ", "\"role\" :   ");
        let parsed = Body::parse(raw_bytes.as_bytes()).unwrap();
        let result = compact_live_zone(&ws, &parsed, &live_options()).expect("should compact");

        // Locate the exact start of the final user message in the original bytes.
        let orig_msgs: Vec<&RawValue> =
            serde_json::from_str(parsed.raw("messages").unwrap().get()).unwrap();
        let last_start =
            orig_msgs.last().unwrap().get().as_ptr() as usize - raw_bytes.as_ptr() as usize;

        // Every byte before the final user message — system, tools, cache_control
        // breakpoints, thinking blocks + signatures, older large tool_result — is
        // 100% byte-for-byte identical.
        assert_eq!(
            &result.body[..last_start],
            &raw_bytes.as_bytes()[..last_start]
        );

        let out_json: Value = serde_json::from_slice(&result.body).unwrap();
        assert_eq!(out_json["messages"][0], request["messages"][0]);
        assert_eq!(out_json["messages"][1], request["messages"][1]);
        assert_eq!(out_json["messages"][2], request["messages"][2]);
        assert_eq!(out_json["messages"][3], request["messages"][3]);
        assert_eq!(result.stats.results, 1);
        assert!(result.stats.bytes_after <= 4096);
        assert!(result.stats.saved_bytes * 100 / result.stats.bytes_before >= 70);
    }

    #[test]
    fn compacts_json_logs_tables_and_tracebacks_over_70_percent() {
        let (_dir, ws) = workspace();
        let cases = [
            ("json", large_json_array()),
            ("log", large_structured_log()),
            ("table", large_ascii_table()),
            ("traceback", large_python_traceback(&ws.root)),
        ];
        for (kind, payload) in cases {
            assert!(
                payload.len() >= 8192,
                "{kind} fixture too small: {}",
                payload.len()
            );
            let req = json!({
                "model": "claude-opus-5-5",
                "messages": [
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": payload}]}
                ]
            });
            let bytes = serde_json::to_vec(&req).unwrap();
            let parsed = Body::parse(&bytes).unwrap();
            let result = compact_live_zone(&ws, &parsed, &live_options())
                .unwrap_or_else(|| panic!("{kind} should compact"));
            let pct = result.stats.saved_bytes * 100 / result.stats.bytes_before;
            assert!(
                pct >= 70,
                "{kind} saved only {pct}% ({} -> {})",
                result.stats.bytes_before,
                result.stats.bytes_after
            );
            assert!(result.stats.bytes_after <= 4096, "{kind} exceeded 4096 B");
            let out: Value = serde_json::from_slice(&result.body).unwrap();
            let compacted = out["messages"][1]["content"][0]["content"]
                .as_str()
                .unwrap();
            assert!(
                compacted.contains("[justless recall ")
                    && compacted.ends_with(&format!(" of {} bytes]", payload.len())),
                "{kind}: {compacted}"
            );
            if kind == "log" {
                assert!(
                    compacted.contains("ECONNREFUSED"),
                    "log must keep error: {compacted}"
                );
            }
            if kind == "traceback" {
                assert!(
                    compacted.contains("charge_invoice")
                        && compacted.contains("ValueError: negative invoice total"),
                    "traceback must keep workspace frame & exception: {compacted}"
                );
            }
        }
    }

    #[test]
    fn skips_small_already_compacted_cache_controlled_and_non_text_blocks() {
        let (_dir, ws) = workspace();
        let big = large_json_array();
        let already = format!("[justless recall {} · cached]\n{}", "a".repeat(64), big);

        // 1. Small payload (< 8192 B)
        let small_req = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "x".repeat(4000)}]}
            ]
        });
        assert!(
            compact_live_zone(
                &ws,
                &Body::parse(&serde_json::to_vec(&small_req).unwrap()).unwrap(),
                &live_options()
            )
            .is_none()
        );

        // 2. Already starting with `[justless `
        let already_req = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": already}]}
            ]
        });
        assert!(
            compact_live_zone(
                &ws,
                &Body::parse(&serde_json::to_vec(&already_req).unwrap()).unwrap(),
                &live_options()
            )
            .is_none()
        );

        // 3. `cache_control` directly on `tool_result`
        let cc_req = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": big, "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        });
        assert!(
            compact_live_zone(
                &ws,
                &Body::parse(&serde_json::to_vec(&cc_req).unwrap()).unwrap(),
                &live_options()
            )
            .is_none()
        );

        // 4. `cache_control` on a LATER block in the same final user message protects earlier blocks too!
        let trailing_cc_req = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": big},
                    {"type": "text", "text": "checkpoint", "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        });
        assert!(
            compact_live_zone(
                &ws,
                &Body::parse(&serde_json::to_vec(&trailing_cc_req).unwrap()).unwrap(),
                &live_options()
            )
            .is_none()
        );

        // 5. Image / document blocks stay untouched even inside an array tool_result,
        //    while an oversized text sibling in the same array IS compacted.
        let image_data = "iVBORw0KGgo".repeat(1200);
        let mixed_req = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Screenshot", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": image_data}},
                        {"type": "text", "text": large_structured_log()}
                    ]}
                ]}
            ]
        });
        let mixed_bytes = serde_json::to_vec(&mixed_req).unwrap();
        let res = compact_live_zone(&ws, &Body::parse(&mixed_bytes).unwrap(), &live_options())
            .expect("text sibling should compact");
        let out: Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(
            out["messages"][1]["content"][0]["content"][0],
            mixed_req["messages"][1]["content"][0]["content"][0]
        );
        assert!(
            out["messages"][1]["content"][0]["content"][1]["text"]
                .as_str()
                .unwrap()
                .contains("[justless recall ")
        );
    }

    #[test]
    fn compacts_only_blocks_strictly_after_last_cache_control_in_final_turn() {
        let (_dir, ws) = workspace();
        let first_big = large_json_array();
        let second_big = large_structured_log();
        let req = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}},
                    {"type": "tool_use", "id": "toolu_2", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": first_big, "cache_control": {"type": "ephemeral"}},
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": second_big}
                ]}
            ]
        });
        let bytes = serde_json::to_vec(&req).unwrap();
        let res = compact_live_zone(&ws, &Body::parse(&bytes).unwrap(), &live_options())
            .expect("second uncached block should compact");
        let out: Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(
            out["messages"][1]["content"][0],
            req["messages"][1]["content"][0]
        );
        assert_eq!(res.stats.results, 1);
        assert!(
            out["messages"][1]["content"][1]["content"]
                .as_str()
                .unwrap()
                .contains("[justless recall ")
        );
    }

    #[test]
    fn exact_total_len_always_matches_formatted_footer() {
        for body_len in [100usize, 895, 896, 897, 898, 999, 1000, 3900] {
            for before in [8192usize, 10_000, 99_999, 1_234_567] {
                let total = exact_total_len(body_len, before).unwrap();
                let id = "a".repeat(OBSERVATION_ID_LEN);
                let footer =
                    format!("\n[justless recall {id} · compacted {total} of {before} bytes]");
                assert_eq!(body_len + footer.len(), total);
            }
        }
    }

    #[test]
    fn ndjson_logs_surface_middle_errors_and_parallel_results_compact_independently() {
        let (_dir, ws) = workspace();
        let mut ndjson = String::new();
        for i in 0..200 {
            if i == 100 {
                ndjson.push_str(
                    "{\"ts\":\"2026-11-25T10:01:40Z\",\"level\":\"ERROR\",\"msg\":\"payment gateway timeout\",\"order_id\":\"ord-999\"}\n",
                );
            } else {
                ndjson.push_str(&format!(
                    "{{\"ts\":\"2026-11-25T10:{:02}:{:02}Z\",\"level\":\"INFO\",\"msg\":\"request ok\",\"req\":\"r-{i:04}\"}}\n",
                    (i / 60) % 60,
                    i % 60
                ));
            }
        }
        // Unstructured UTF-8 prose with multibyte emojis to exercise bounded head+tail fallback.
        let mut prose = String::new();
        for i in 0..320 {
            prose.push_str(&format!(
                "Paragraph {i:03}: café 🦀 diagnostic note with unique sequence item-{i:04}.\n"
            ));
        }

        let req = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_ndjson", "name": "Bash", "input": {}},
                    {"type": "tool_use", "id": "toolu_small", "name": "Bash", "input": {}},
                    {"type": "tool_use", "id": "toolu_prose", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_ndjson", "content": ndjson},
                    {"type": "tool_result", "tool_use_id": "toolu_small", "content": "ok"},
                    {"type": "tool_result", "tool_use_id": "toolu_prose", "content": prose}
                ]}
            ]
        });
        let bytes = serde_json::to_vec(&req).unwrap();
        let result = compact_live_zone(&ws, &Body::parse(&bytes).unwrap(), &live_options())
            .expect("both oversized parallel results should compact");
        assert_eq!(result.stats.results, 2);
        let out: Value = serde_json::from_slice(&result.body).unwrap();
        let ndjson_out = out["messages"][1]["content"][0]["content"]
            .as_str()
            .unwrap();
        let small_out = out["messages"][1]["content"][1]["content"]
            .as_str()
            .unwrap();
        let prose_out = out["messages"][1]["content"][2]["content"]
            .as_str()
            .unwrap();

        assert!(
            ndjson_out.contains("payment gateway timeout"),
            "{ndjson_out}"
        );
        assert!(ndjson_out.len() <= 4096);
        assert_eq!(small_out, "ok");
        assert!(prose_out.contains("Paragraph 000:"));
        assert!(prose_out.contains("Paragraph 319:"));
        assert!(prose_out.contains("[justless recall "));
        assert!(prose_out.len() <= 4096);
    }

    #[test]
    fn fails_open_when_no_prior_assistant_or_archive_unwritable() {
        let (_dir, ws) = workspace();
        let big = large_json_array();
        // Single user message with no prior assistant turn -> must not mutate.
        let no_assistant = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": big}]}
            ]
        });
        let bytes = serde_json::to_vec(&no_assistant).unwrap();
        assert!(compact_live_zone(&ws, &Body::parse(&bytes).unwrap(), &live_options()).is_none());

        // Blocking `.justless/observations` with a regular file forces archive failure -> fail open!
        std::fs::create_dir_all(&ws.state).unwrap();
        std::fs::write(ws.state.join("observations"), b"not-a-directory").unwrap();
        let req = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": big}]}
            ]
        });
        let bytes = serde_json::to_vec(&req).unwrap();
        assert!(compact_live_zone(&ws, &Body::parse(&bytes).unwrap(), &live_options()).is_none());
    }

    #[test]
    fn openai_chat_and_responses_compact_trailing_live_zone_only() {
        let (_dir, ws) = workspace();
        let big_json = large_json_array();
        let big_log = large_structured_log();
        let big_table = {
            let mut t = large_ascii_table();
            for i in 160..320 {
                t.push_str(&format!(
                    "| {:<3} | worker-{:<7} | us-east-1   | prod      | running  | 3        |\n",
                    i, i
                ));
            }
            t
        };
        let big_tb = large_python_traceback(&ws.root);

        // 1. OpenAI Chat Completions: trailing contiguous `role: "tool"` run after assistant.
        let chat_req = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are a coding agent."},
                {"role": "user", "content": "Inspect services."},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "call_old", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "call_old", "content": big_log},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_json", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                    {"id": "call_log", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_json", "content": big_json},
                {"role": "tool", "tool_call_id": "call_log", "content": big_log}
            ]
        });
        let chat_raw = serde_json::to_string_pretty(&chat_req)
            .unwrap()
            .replace("\"role\": ", "\"role\" :   ");
        let chat_parsed = Body::parse(chat_raw.as_bytes()).unwrap();
        let chat_res = compact_openai_chat_live_zone(&ws, &chat_parsed, &live_options())
            .expect("chat trailing tools should compact");
        let chat_prefix_end = chat_raw.find("\"call_json\"").unwrap();
        assert_eq!(
            &chat_res.body[..chat_prefix_end],
            &chat_raw.as_bytes()[..chat_prefix_end]
        );
        assert_eq!(chat_res.stats.results, 2);
        let chat_pct = chat_res.stats.saved_bytes * 100 / chat_res.stats.bytes_before;
        assert!(chat_pct > 85, "chat saved only {chat_pct}%");
        let chat_out: Value = serde_json::from_slice(&chat_res.body).unwrap();
        assert_eq!(chat_out["messages"][3], chat_req["messages"][3]);
        assert!(
            chat_out["messages"][6]["content"]
                .as_str()
                .unwrap()
                .contains("ECONNREFUSED")
        );

        // Chat safety guards: no prior assistant, or ends with user message -> untouched.
        for skipped in [
            json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}, {"role": "tool", "tool_call_id": "c1", "content": big_json}]}),
            json!({"model": "gpt-4o", "messages": [{"role": "assistant", "content": ""}, {"role": "tool", "tool_call_id": "c1", "content": big_json}, {"role": "user", "content": "next"}]}),
        ] {
            let raw = serde_json::to_vec(&skipped).unwrap();
            assert!(
                compact_openai_chat_live_zone(&ws, &Body::parse(&raw).unwrap(), &live_options())
                    .is_none()
            );
        }

        // 2. OpenAI Responses API: trailing contiguous `function_call_output` run in `input[]`.
        let resp_req = json!({
            "model": "gpt-5.4",
            "instructions": "You are a coding agent.",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Debug billing."}]},
                {"type": "reasoning", "id": "rs_01", "encrypted_content": "opaque_encrypted_blob"},
                {"type": "function_call", "call_id": "call_old", "name": "shell", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_old", "output": big_table},
                {"type": "function_call", "call_id": "call_tbl", "name": "shell", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_tb", "name": "shell", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_tbl", "output": big_table},
                {"type": "function_call_output", "call_id": "call_tb", "output": big_tb}
            ]
        });
        let resp_raw = serde_json::to_string_pretty(&resp_req)
            .unwrap()
            .replace("\"type\": ", "\"type\" :   ");
        let resp_parsed = Body::parse(resp_raw.as_bytes()).unwrap();
        let resp_res = compact_openai_responses_live_zone(&ws, &resp_parsed, &live_options())
            .expect("responses trailing outputs should compact");
        let resp_prefix_end = resp_raw.rfind("\"call_tbl\"").unwrap();
        assert_eq!(
            &resp_res.body[..resp_prefix_end],
            &resp_raw.as_bytes()[..resp_prefix_end]
        );
        assert_eq!(resp_res.stats.results, 2);
        let resp_pct = resp_res.stats.saved_bytes * 100 / resp_res.stats.bytes_before;
        assert!(resp_pct > 80, "responses saved only {resp_pct}%");
        let resp_out: Value = serde_json::from_slice(&resp_res.body).unwrap();
        assert_eq!(resp_out["input"][3], resp_req["input"][3]);
        assert!(
            resp_out["input"][7]["output"]
                .as_str()
                .unwrap()
                .contains("ValueError: negative invoice total")
        );

        // Responses safety guards: no prior function_call/message, or ends with message -> untouched.
        for skipped in [
            json!({"model": "gpt-5.4", "input": [{"type": "function_call_output", "call_id": "c1", "output": big_tb}]}),
            json!({"model": "gpt-5.4", "input": [{"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"}, {"type": "function_call_output", "call_id": "c1", "output": big_tb}, {"type": "message", "role": "user", "content": "ok"}]}),
        ] {
            let raw = serde_json::to_vec(&skipped).unwrap();
            assert!(
                compact_openai_responses_live_zone(
                    &ws,
                    &Body::parse(&raw).unwrap(),
                    &live_options()
                )
                .is_none()
            );
        }
    }

    #[test]
    fn skips_justless_recall_and_retrieval_calls_to_prevent_loops() {
        let (_dir, ws) = workspace();
        let big = large_json_array();
        // Anthropic shell `justless recall` + MCP `mcp__justless__recall` stay untouched.
        let anth = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t_sh", "name": "Bash", "input": {"command": "justless recall deadbeef"}},
                    {"type": "tool_use", "id": "t_mcp", "name": "mcp__justless__recall", "input": {"id": "deadbeef"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t_sh", "content": big},
                    {"type": "tool_result", "tool_use_id": "t_mcp", "content": big}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&anth).unwrap();
        assert!(compact_live_zone(&ws, &Body::parse(&raw).unwrap(), &live_options()).is_none());

        // OpenAI Chat `justless expand` stays untouched.
        let chat = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c_rec", "type": "function", "function": {"name": "bash", "arguments": "{\"command\":\"justless expand src/lib.rs rev#1\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "c_rec", "content": big}
            ]
        });
        let raw = serde_json::to_vec(&chat).unwrap();
        assert!(
            compact_openai_chat_live_zone(&ws, &Body::parse(&raw).unwrap(), &live_options())
                .is_none()
        );

        // OpenAI Responses `justless log` stays untouched.
        let resp = json!({
            "model": "gpt-5.4",
            "input": [
                {"type": "function_call", "call_id": "r_log", "name": "shell", "arguments": "{\"cmd\":[\"justless\",\"log\",\"run_1\",\"--from\",\"1\"]}" },
                {"type": "function_call_output", "call_id": "r_log", "output": big}
            ]
        });
        let raw = serde_json::to_vec(&resp).unwrap();
        assert!(
            compact_openai_responses_live_zone(&ws, &Body::parse(&raw).unwrap(), &live_options())
                .is_none()
        );
    }
}
