//! Live-zone-only, prefix-cache-safe compaction for newest `tool_result` blocks.
//!
//! Mutating any message inside a cached Anthropic Messages prefix invalidates the
//! prompt cache and breaks preserved-thinking signatures. By contrast, `tool_result`
//! blocks inside the **very last `user` message** — strictly after any prior
//! `assistant` turn and strictly after the last `cache_control` breakpoint — have
//! never been written to the provider's prompt cache and have no subsequent thinking
//! blocks tied to them.
//!
//! Each eligible oversized text block is:
//! 1. Checked against strict safety guards (final `user` turn, prior `assistant`
//!    turn present, strictly after any `cache_control`, text-only, `>= min_bytes`,
//!    not already starting with `[justless `).
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
/// uncached `user` message of `body`.
///
/// Returns `None` if nothing was compacted or on any parse/safety/archive error
/// (fail-open). All bytes outside the replaced string spans are preserved verbatim.
pub fn compact_live_zone(ws: &Workspace, body: &Body<'_>, options: &Options) -> Option<LiveResult> {
    if !options.compact_live || options.compact_live_budget < 256 {
        return None;
    }
    let raw_messages = body.raw("messages")?;
    let messages: Vec<&RawValue> = serde_json::from_str(raw_messages.get()).ok()?;
    if messages.len() < 2 {
        return None;
    }

    // Must be strictly after at least one prior assistant turn.
    let prior_assistant = messages[..messages.len() - 1].iter().any(|raw| {
        plan::parse_members(raw.get())
            .and_then(|members| member(&members, "role"))
            .and_then(|role| serde_json::from_str::<String>(role.get()).ok())
            .is_some_and(|role| role == "assistant")
    });
    if !prior_assistant {
        return None;
    }

    let last_msg = *messages.last()?;
    let msg_members = plan::parse_members(last_msg.get())?;
    let role: String = serde_json::from_str(member(&msg_members, "role")?.get()).ok()?;
    if role != "user" {
        return None;
    }
    // If the final message object itself carries a non-null cache_control, leave it untouched.
    if member(&msg_members, "cache_control").is_some_and(|raw| raw.get().trim() != "null") {
        return None;
    }

    let raw_content = member(&msg_members, "content")?;
    if !raw_content.get().trim_start().starts_with('[') {
        return None;
    }
    let blocks: Vec<&RawValue> = serde_json::from_str(raw_content.get()).ok()?;
    if blocks.is_empty() {
        return None;
    }

    // Find the last block in the final user message that carries a non-null
    // `cache_control` breakpoint anywhere inside it. Every block at or before
    // that index is inside the cached prefix and MUST NOT be mutated.
    let last_cache_breakpoint = blocks
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, raw)| raw_has_cache_control(raw.get()).then_some(idx));

    let start_idx = last_cache_breakpoint.map_or(0, |idx| idx + 1);
    if start_idx >= blocks.len() {
        return None;
    }

    let mut redactor: Option<Option<&crate::redact::Redactor>> = None;
    let mut replacements: Vec<Replacement> = Vec::new();

    for raw_block in &blocks[start_idx..] {
        let Some(block_members) = plan::parse_members(raw_block.get()) else {
            continue;
        };
        let Some(kind_raw) = member(&block_members, "type") else {
            continue;
        };
        if serde_json::from_str::<String>(kind_raw.get())
            .ok()
            .as_deref()
            != Some("tool_result")
        {
            continue;
        }
        // Double-check the tool_result itself has no cache_control.
        if member(&block_members, "cache_control").is_some_and(|r| r.get().trim() != "null") {
            continue;
        }
        let Some(content_raw) = member(&block_members, "content") else {
            continue;
        };
        let trimmed = content_raw.get().trim_start();
        if trimmed.starts_with('"') {
            if let Some(rep) = compact_string_raw(ws, body, content_raw, options, &mut redactor) {
                replacements.push(rep);
            }
        } else if trimmed.starts_with('[') {
            let Ok(inner_blocks) = serde_json::from_str::<Vec<&RawValue>>(content_raw.get()) else {
                continue;
            };
            // Only compact plain text blocks inside `tool_result.content`;
            // images, documents, and any other block types stay byte-for-byte untouched.
            for inner in inner_blocks {
                let Some(inner_members) = plan::parse_members(inner.get()) else {
                    continue;
                };
                if member(&inner_members, "cache_control").is_some_and(|r| r.get().trim() != "null")
                {
                    continue;
                }
                let Some(inner_kind) = member(&inner_members, "type") else {
                    continue;
                };
                if serde_json::from_str::<String>(inner_kind.get())
                    .ok()
                    .as_deref()
                    != Some("text")
                {
                    continue;
                }
                let Some(text_raw) = member(&inner_members, "text") else {
                    continue;
                };
                if !text_raw.get().trim_start().starts_with('"') {
                    continue;
                }
                if let Some(rep) = compact_string_raw(ws, body, text_raw, options, &mut redactor) {
                    replacements.push(rep);
                }
            }
        }
    }

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

    // Final invariant: outbound JSON body must actually be smaller and valid JSON.
    if out.len() >= src.len() {
        return None;
    }
    Some(LiveResult { body: out, stats })
}

fn member<'a>(members: &[(String, &'a RawValue)], key: &str) -> Option<&'a RawValue> {
    members
        .iter()
        .find_map(|(name, value)| (name == key).then_some(*value))
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

    let compacted_body = compact_text(safe.as_ref(), body_budget, &ws.root)?;
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
fn compact_text(text: &str, budget: usize, workspace_root: &Path) -> Option<String> {
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
}
