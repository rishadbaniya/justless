//! Structured container, Kubernetes, JSON/NDJSON, and timestamped/syslog log
//! template folding.
//!
//! Collapses repetitive routine log chatter across interleaved services and
//! non-consecutive lines while preserving 100% of distinct errors, warnings,
//! panics, exceptions, and attached stack traces. Every entry carries exact
//! `[stream:line]` or `[stream:A-B]` archive pointers for lossless recall.
//!
//! Only replaces the ordinary preview when the log is long enough and the
//! folded summary fits within `max_bytes` and saves meaningful space.
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

/// Minimum total lines before log folding is attempted.
const MIN_LINES: usize = 8;
/// Refuse inputs with absurdly long single lines (matches `capture::SCAN_LINE_LIMIT`).
const MAX_LINE_BYTES: usize = 256 * 1024;
/// Maximum continuation/stack-trace lines kept per error block (head + tail).
const MAX_FRAMES_HEAD: usize = 12;
const MAX_FRAMES_TAIL: usize = 6;
/// Maximum service names listed explicitly in the summary header.
const MAX_HEADER_SERVICES: usize = 5;
/// Maximum display bytes for a single routine pattern template.
const MAX_TEMPLATE_BYTES: usize = 200;
/// Maximum display bytes for a single error/warning header line.
const MAX_ERROR_LINE_BYTES: usize = 420;
/// Maximum display bytes for a single attached stack-trace frame line.
const MAX_FRAME_LINE_BYTES: usize = 240;
/// Maximum display bytes for a single recent-tail line.
const MAX_TAIL_LINE_BYTES: usize = 180;
/// Maximum recent tail lines rendered when budget allows.
const MAX_TAIL_LINES: usize = 3;
/// Cap distinct routine patterns tracked in memory on pathological logs.
const MAX_TRACKED_PATTERNS: usize = 8_192;

static REDACTED_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[redacted(?:[:\s][^\]]*)?\]").unwrap());

static K8S_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[(?:pod|deployment|statefulset|daemonset|job)/([^\]]+)\]\s?(.*)$").unwrap()
});

static KLOG_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^([IWEF])(\d{4})\s+(\d{2}:\d{2}:\d{2}(?:\.\d+)?)\s+(?:0x[0-9a-fA-F]+|\d+)\s+([^\]]+)\]\s?(.*)$",
    )
    .unwrap()
});

// Consume a single separator space after the leading timestamp so indented
// stack-trace lines in `docker logs -t` / `kubectl logs --timestamps` keep
// their indentation intact.
static LEADING_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:\[?(\d{4}[-/]\d{2}[-/]\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?(?:\s+(?:UTC|GMT))?|(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}|\d{2}/(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)/\d{4}:\d{2}:\d{2}:\d{2}(?:\s+[+-]\d{4})?|\d{2}:\d{2}:\d{2}(?:[.,]\d+)?)\]?\s)",
    )
    .unwrap()
});

static SYSLOG_UNIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z0-9][A-Za-z0-9._-]{0,63})\s+([A-Za-z0-9][A-Za-z0-9_./@-]{0,63})(?:\[(\d+)\])?:\s(.*)$")
        .unwrap()
});

static EXPLICIT_LEVEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?i)(?:\[\d+\]\s+)?(?:\[|\(|level=|lvl=|severity=)?(TRACE|TRC|DEBUG|DBG|INFO|INF|LOG|NOTICE|WARN|WARNING|WRN|ERROR|ERR|ERRO|FATAL|FATA|CRIT|CRITICAL|PANIC|EMERG|ALERT|SEVERE)(?:\]|\))?(?:[:\s]|$)",
    )
    .unwrap()
});

static HTTP_5XX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:\b(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+\S+(?:\s+HTTP/\d(?:\.\d)?)?"?\s+5\d\d\b|(?i:\b(?:status|status_code|http_status)[=:]\s*5\d\d\b))"#,
    )
    .unwrap()
});

static HTTP_STATUS_SPAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:\b(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+\S+(?:\s+HTTP/\d(?:\.\d)?)?"?\s+([1-5]\d\d)\b|(?i:\b(?:status|status_code|http_status)[=:]\s*([1-5]\d\d)\b))"#,
    )
    .unwrap()
});

static EXCEPTION_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z][A-Za-z0-9_]*(?:Error|Exception|Fault|Failure)\b").unwrap()
});

static GOROUTINE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^goroutine\s+\d+\s+\[").unwrap());

static INLINE_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:\d{4}[-/]\d{2}[-/]\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?|\d{2}/(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)/\d{4}:\d{2}:\d{2}:\d{2}(?:\s+[+-]\d{4})?)\b",
    )
    .unwrap()
});

static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b").unwrap()
});

static KV_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(req(?:uest)?_?id|trace_?id|span_?id|corr(?:elation)?_?id|session_?id|job_?id|task_?id|tx(?:n)?_?id|pod_?uid|cid|rid|x-request-id)([=:])(?:"[^"\s]+"|[A-Za-z0-9._-]+)"#,
    )
    .unwrap()
});

static IPV6_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[(?:[0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}\](?::\d{1,5})?|::ffff:\d{1,3}(?:\.\d{1,3}){3}")
        .unwrap()
});

static IPV4_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d{1,5})?\b").unwrap());

static HEX_ADDR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b0x[0-9a-fA-F]{4,}\b").unwrap());

static PREFIXED_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Za-z][A-Za-z0-9_]{1,15}[-_])([0-9a-fA-F]{6,}|\d{3,})\b").unwrap()
});

static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:(?:\d+h)?(?:\d+m)?\d+(?:\.\d+)?(?:ns|us|µs|μs|ms|s)|\d+(?:\.\d+)?[mh])\b")
        .unwrap()
});

static BYTE_SIZE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\s?(?:B|(?i:[kmgt]i?b|bytes?))\b").unwrap());

static BRACKET_PID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\d+\]").unwrap());

static STANDALONE_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\b").unwrap());

static ZERO_ERROR_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:0\s+(?:errors?|warnings?|failures?|failed)|(?:errors?|warnings?|failures?|failed|err_count|error_count|fail_count|warn_count)\s*[=:]\s*0)\b")
        .unwrap()
});

static EXIT_NONZERO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bexit(?:ed)?(?:\s+(?:with\s+)?(?:status|code))?\s+[1-9]\d*\b").unwrap()
});

static STRONG_ERROR_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:error|fatal|panic|panicked|exception|traceback|failed|failure|oomkilled|segfault|sigsegv|sigkill|econnrefused|etimedout|enomem|eaddrinuse)\b")
        .unwrap()
});

static STRONG_WARN_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:warn|warning|deprecated)\b").unwrap());

#[inline]
fn strip_ansi(raw: &str) -> Cow<'_, str> {
    if raw.contains('\x1b') {
        crate::capture::ANSI.replace_all(raw, "")
    } else {
        Cow::Borrowed(raw)
    }
}

#[inline]
fn contains_ascii_icase(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.len() < needle_lower.len() {
        return false;
    }
    let first_lo = needle_lower[0];
    let first_hi = first_lo.to_ascii_uppercase();
    let limit = haystack.len() - needle_lower.len();
    let mut i = 0;
    while i <= limit {
        let b = haystack[i];
        if (b == first_lo || b == first_hi)
            && haystack[i..i + needle_lower.len()].eq_ignore_ascii_case(needle_lower)
        {
            return true;
        }
        i += 1;
    }
    false
}

#[inline]
fn looks_like_klog(body: &str) -> bool {
    let b = body.as_bytes();
    b.len() >= 18
        && matches!(b[0], b'I' | b'W' | b'E' | b'F')
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4].is_ascii_digit()
        && matches!(b[5], b' ' | b'\t')
        && body.contains(']')
}

#[inline]
fn may_start_with_ts(line: &str) -> bool {
    let b = line.as_bytes();
    let Some(&first) = b.first() else {
        return false;
    };
    if first.is_ascii_digit() {
        return b.len() >= 9 && (b[2] == b':' || b[2] == b'/' || b[4] == b'-' || b[4] == b'/');
    }
    if first == b'[' {
        return b.len() >= 11
            && (b[1].is_ascii_digit()
                || matches!(b[1], b'J' | b'F' | b'M' | b'A' | b'S' | b'O' | b'N' | b'D'));
    }
    b.len() >= 15
        && matches!(first, b'J' | b'F' | b'M' | b'A' | b'S' | b'O' | b'N' | b'D')
        && b[3].is_ascii_whitespace()
}

#[inline]
fn may_start_with_level(line: &str) -> bool {
    let Some(&b) = line.as_bytes().first() else {
        return false;
    };
    matches!(
        b.to_ascii_uppercase(),
        b'[' | b'('
            | b'T'
            | b'D'
            | b'I'
            | b'L'
            | b'N'
            | b'W'
            | b'E'
            | b'F'
            | b'C'
            | b'P'
            | b'A'
            | b'S'
    )
}

#[inline]
fn may_have_exception_name(line: &str) -> bool {
    line.contains("Error")
        || line.contains("Exception")
        || line.contains("Fault")
        || line.contains("Failure")
}

#[inline]
fn may_have_http_status(text: &str) -> bool {
    ((text.contains('=') || text.contains(':')) && contains_ascii_icase(text.as_bytes(), b"status"))
        || text.contains("GET")
        || text.contains("POST")
        || text.contains("PUT")
        || text.contains("PATCH")
        || text.contains("DELETE")
        || text.contains("HEAD")
        || text.contains("OPTIONS")
}

#[inline]
fn looks_like_dart_frame(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix('#') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let after = &rest[digits..];
    after.starts_with([' ', '\t'])
        && after.ends_with(')')
        && (after.contains("(package:") || after.contains("(dart:") || after.contains(".dart:"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Severity {
    Routine,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineRef {
    stream: String,
    start: usize,
    end: usize,
}

impl LineRef {
    fn single(stream: &str, line: usize) -> Self {
        Self {
            stream: stream.to_owned(),
            start: line,
            end: line,
        }
    }

    fn fmt_bracketed(&self) -> String {
        if self.start == self.end {
            format!("[{}:{}]", self.stream, self.start)
        } else {
            format!("[{}:{}-{}]", self.stream, self.start, self.end)
        }
    }

    fn fmt_bare(&self) -> String {
        if self.start == self.end {
            format!("{}:{}", self.stream, self.start)
        } else {
            format!("{}:{}-{}", self.stream, self.start, self.end)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ErrorKey {
    severity: Severity,
    service: Option<String>,
    signature: String,
}

struct ErrorEntry {
    severity: Severity,
    service: Option<String>,
    header: String,
    frames: Vec<String>,
    omitted_frames: usize,
    count: usize,
    first: LineRef,
    last: LineRef,
    order: usize,
}

struct OpenBlock {
    severity: Severity,
    service: Option<String>,
    stream: String,
    start_line: usize,
    end_line: usize,
    header: String,
    norm_header: String,
    frames_head: Vec<String>,
    frames_tail: VecDeque<String>,
    total_frames: usize,
    has_traceback: bool,
    expect_chained_traceback: bool,
    order: usize,
}

impl OpenBlock {
    fn push_frame(&mut self, line: &str, line_no: usize) {
        self.end_line = line_no;
        self.total_frames += 1;
        let trimmed = line.trim_start();
        self.expect_chained_traceback = trimmed
            .starts_with("During handling of the above exception")
            || trimmed.starts_with("The above exception was the direct cause");
        if trimmed.starts_with("Traceback")
            || trimmed.starts_with("File \"")
            || trimmed.starts_with("at ")
        {
            self.has_traceback = true;
        }
        let clipped = truncate_line(line, MAX_FRAME_LINE_BYTES);
        if self.frames_head.len() < MAX_FRAMES_HEAD {
            self.frames_head.push(clipped);
        } else {
            if self.frames_tail.len() == MAX_FRAMES_TAIL {
                self.frames_tail.pop_front();
            }
            self.frames_tail.push_back(clipped);
        }
    }

    fn into_parts(self) -> (ErrorKey, ErrorEntry) {
        let kept = self.frames_head.len() + self.frames_tail.len();
        let omitted_frames = self.total_frames.saturating_sub(kept);
        let mut frames = self.frames_head;
        frames.extend(self.frames_tail);

        // Build a stable signature from header + key frames (first and last) so
        // distinct stack traces sharing a generic header like "Traceback (...):"
        // never collapse into one another, while identical repeats do.
        let mut sig = self.norm_header;
        if let Some(first_frame) = frames.first() {
            sig.push('\n');
            sig.push_str(&normalize_template(first_frame.trim()));
        }
        if frames.len() > 1
            && let Some(last_frame) = frames.last()
        {
            sig.push('\n');
            sig.push_str(&normalize_template(last_frame.trim()));
        }

        let line_ref = LineRef {
            stream: self.stream,
            start: self.start_line,
            end: self.end_line,
        };
        let key = ErrorKey {
            severity: self.severity,
            service: self.service.clone(),
            signature: sig,
        };
        let entry = ErrorEntry {
            severity: self.severity,
            service: self.service,
            header: truncate_line(&self.header, MAX_ERROR_LINE_BYTES),
            frames,
            omitted_frames,
            count: 1,
            first: line_ref.clone(),
            last: line_ref,
            order: self.order,
        };
        (key, entry)
    }
}

struct PatternEntry {
    service: Option<String>,
    template: String,
    sample: String,
    count: usize,
    streams: BTreeMap<String, (usize, usize)>,
    first_ts: Option<String>,
    last_ts: Option<String>,
    order: usize,
}

impl PatternEntry {
    fn record(&mut self, stream: &str, line_no: usize, ts: Option<&str>) {
        self.count += 1;
        if let Some((_, last)) = self.streams.get_mut(stream) {
            *last = line_no;
        } else {
            self.streams.insert(stream.to_owned(), (line_no, line_no));
        }
        if let Some(ts) = ts {
            if self.first_ts.is_none() {
                self.first_ts = Some(ts.to_owned());
            }
            match &mut self.last_ts {
                Some(buf) => {
                    buf.clear();
                    buf.push_str(ts);
                }
                None => self.last_ts = Some(ts.to_owned()),
            }
        }
    }

    fn pointer(&self) -> String {
        let mut out = String::from("[");
        for (i, (stream, (first, last))) in self.streams.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            if first == last {
                let _ = write!(out, "{stream}:{first}");
            } else {
                let _ = write!(out, "{stream}:{first}-{last}");
            }
        }
        out.push(']');
        out
    }

    fn time_span(&self) -> Option<String> {
        if self.count < 2 {
            return None;
        }
        let first = self.first_ts.as_deref()?;
        let last = self.last_ts.as_deref()?;
        format_time_span(first, last)
    }
}

struct TailLine {
    stream: String,
    line_no: usize,
    text: String,
}

struct Collector {
    services: Vec<String>,
    service_ids: HashMap<String, usize>,
    errors: Vec<ErrorEntry>,
    error_index: HashMap<ErrorKey, usize>,
    patterns: Vec<PatternEntry>,
    /// Per-service template -> index in `patterns`. Bucket `0` is `service == None`,
    /// bucket `svc_id + 1` is `Some(&services[svc_id])`. Enables zero-allocation
    /// `&str` lookups on already-seen templates.
    pattern_buckets: Vec<HashMap<String, usize>>,
    overflow_patterns: usize,
    /// Active error/warning blocks awaiting stack-trace continuations (typically 0..4).
    open_blocks: Vec<OpenBlock>,
    /// Reusable buffer for building normalized routine templates without per-line allocs.
    scratch: String,
    next_order: usize,
    non_empty_lines: usize,
    total_lines: usize,
}

impl Collector {
    fn new(expected_lines: usize) -> Self {
        let pat_cap = expected_lines.min(64);
        let mut pattern_buckets = Vec::with_capacity(8);
        pattern_buckets.push(HashMap::with_capacity(pat_cap));
        Self {
            services: Vec::with_capacity(8),
            service_ids: HashMap::with_capacity(8),
            errors: Vec::with_capacity(16),
            error_index: HashMap::with_capacity(16),
            patterns: Vec::with_capacity(pat_cap),
            pattern_buckets,
            overflow_patterns: 0,
            open_blocks: Vec::new(),
            scratch: String::with_capacity(256),
            next_order: 0,
            non_empty_lines: 0,
            total_lines: expected_lines,
        }
    }

    fn note_service(&mut self, service: Option<&str>) -> usize {
        let Some(svc) = service else {
            return 0;
        };
        if svc.is_empty() {
            return 0;
        }
        if let Some(&id) = self.service_ids.get(svc) {
            return id + 1;
        }
        let id = self.services.len();
        let owned = svc.to_owned();
        self.service_ids.insert(owned.clone(), id);
        self.services.push(owned);
        self.pattern_buckets.push(HashMap::with_capacity(16));
        id + 1
    }

    fn add_error_entry(&mut self, key: ErrorKey, entry: ErrorEntry) {
        if let Some(&idx) = self.error_index.get(&key) {
            let existing = &mut self.errors[idx];
            existing.count += 1;
            existing.last = entry.last;
        } else {
            let idx = self.errors.len();
            self.error_index.insert(key, idx);
            self.errors.push(entry);
        }
    }

    fn flush_open(&mut self, stream: &str, service: Option<&str>) {
        if self.open_blocks.is_empty() {
            return;
        }
        if let Some(pos) = self
            .open_blocks
            .iter()
            .position(|b| b.stream == stream && b.service.as_deref() == service)
        {
            let block = self.open_blocks.swap_remove(pos);
            let (key, entry) = block.into_parts();
            self.add_error_entry(key, entry);
        }
    }

    fn push_open_block(&mut self, block: OpenBlock) {
        if !self.open_blocks.is_empty()
            && let Some(pos) = self
                .open_blocks
                .iter()
                .position(|b| b.stream == block.stream && b.service == block.service)
        {
            let prev = self.open_blocks.swap_remove(pos);
            let (key, entry) = prev.into_parts();
            self.add_error_entry(key, entry);
        }
        self.open_blocks.push(block);
    }

    fn flush_all_open(&mut self) {
        if self.open_blocks.is_empty() {
            return;
        }
        let mut pending = std::mem::take(&mut self.open_blocks);
        pending.sort_by_key(|b| b.order);
        for block in pending {
            let (key, entry) = block.into_parts();
            self.add_error_entry(key, entry);
        }
    }

    /// Record a routine log event whose normalized template has already been
    /// written into `self.scratch`. Performs zero heap allocations when the
    /// `(service, template)` key has already been seen.
    fn record_routine_from_scratch(
        &mut self,
        stream: &str,
        line_no: usize,
        bucket: usize,
        service: Option<&str>,
        ts: Option<&str>,
        make_sample: impl FnOnce() -> String,
    ) {
        if let Some(&idx) = self.pattern_buckets[bucket].get(self.scratch.as_str()) {
            self.patterns[idx].record(stream, line_no, ts);
        } else if self.patterns.len() < MAX_TRACKED_PATTERNS {
            let order = self.next_order;
            self.next_order += 1;
            let mut streams = BTreeMap::new();
            streams.insert(stream.to_owned(), (line_no, line_no));
            let template = self.scratch.clone();
            let ts_owned = ts.map(str::to_owned);
            let entry = PatternEntry {
                service: service.map(str::to_owned),
                template: template.clone(),
                sample: make_sample(),
                count: 1,
                streams,
                first_ts: ts_owned.clone(),
                last_ts: ts_owned,
                order,
            };
            let idx = self.patterns.len();
            self.pattern_buckets[bucket].insert(template, idx);
            self.patterns.push(entry);
        } else {
            self.overflow_patterns += 1;
        }
    }
}

/// Fold structured container, Kubernetes, JSON/NDJSON, and timestamped/syslog
/// logs across `stdout` and `stderr` into a compact, error-first summary with
/// exact `[stream:line]` / `[stream:A-B]` archive pointers.
///
/// Returns `None` if the log has fewer than 8 lines, contains binary bytes,
/// cannot fit within `max_bytes`, or does not achieve meaningful compaction.
pub fn format(stdout: &str, stderr: &str, max_bytes: usize) -> Option<String> {
    format_streams(&[("stdout", stdout), ("stderr", stderr)], max_bytes)
}

/// Fold a single named log stream (such as `"stdout"` or `"stderr"`) into a
/// compact, error-first summary with exact `[stream:line]` / `[stream:A-B]`
/// archive pointers.
// Wired into capture.rs during Wave-2 integration.
#[allow(dead_code)]
pub fn format_stream(stream: &str, text: &str, max_bytes: usize) -> Option<String> {
    format_streams(&[(stream, text)], max_bytes)
}

fn format_streams(streams: &[(&str, &str)], max_bytes: usize) -> Option<String> {
    if max_bytes == 0 {
        return None;
    }
    let raw_bytes: usize = streams.iter().map(|(_, text)| text.len()).sum();
    if raw_bytes == 0 {
        return None;
    }

    let mut total_lines = 0usize;
    for (_, text) in streams {
        if text.contains('\0') {
            return None;
        }
        for line in text.split_inclusive('\n') {
            if line.len() > MAX_LINE_BYTES {
                return None;
            }
            total_lines += 1;
        }
    }
    if total_lines < MIN_LINES {
        return None;
    }

    let mut collector = Collector::new(total_lines);
    let mut tails_per_stream: Vec<(&str, VecDeque<(usize, &str)>)> =
        Vec::with_capacity(streams.len());

    for &(stream, text) in streams {
        if text.is_empty() {
            continue;
        }
        let compose_mode = detect_compose_stream(text);
        let mut tail: VecDeque<(usize, &str)> = VecDeque::with_capacity(MAX_TAIL_LINES);

        for (idx, raw) in text.split_inclusive('\n').enumerate() {
            let line_no = idx + 1;
            let stripped_ansi = strip_ansi(raw);
            let line = stripped_ansi.trim_end_matches(['\n', '\r']);
            if line.trim().is_empty() {
                // Blank line closes any open stack trace on the default (unprefixed) service.
                collector.flush_open(stream, None);
                continue;
            }
            collector.non_empty_lines += 1;

            if tail.len() == MAX_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back((line_no, raw));

            observe_line(&mut collector, stream, line_no, line, compose_mode);
        }

        if !tail.is_empty() {
            tails_per_stream.push((stream, tail));
        }
    }
    collector.flush_all_open();

    if collector.non_empty_lines == 0 {
        return None;
    }

    let distinct_groups = collector.errors.len() + collector.patterns.len();
    let total_error_events: usize = collector.errors.iter().map(|e| e.count).sum();
    let total_routine_events: usize = collector.patterns.iter().map(|p| p.count).sum();
    let folded_lines = (total_error_events + total_routine_events).saturating_sub(distinct_groups);

    // If the raw log already fits in max_bytes and nothing meaningfully folded,
    // keep the raw output verbatim.
    if raw_bytes <= max_bytes && folded_lines < 2 {
        return None;
    }

    let tail_lines = select_tail_lines(tails_per_stream);
    let rendered = render_report(&mut collector, &tail_lines, max_bytes, raw_bytes)?;

    if rendered.len() > max_bytes || rendered.len() >= raw_bytes {
        return None;
    }
    if raw_bytes <= max_bytes
        && (rendered.len() + 64 >= raw_bytes || rendered.len() * 100 > raw_bytes * 85)
    {
        return None;
    }

    Some(rendered)
}

fn detect_compose_stream(text: &str) -> bool {
    let mut non_empty = 0usize;
    let mut matched = 0usize;
    for raw in text.split_inclusive('\n') {
        let stripped = strip_ansi(raw);
        let line = stripped.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        non_empty += 1;
        if parse_compose_prefix(line).is_some() {
            matched += 1;
        }
    }
    matched >= 2 && matched * 5 >= non_empty * 2
}

fn parse_compose_prefix(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_alphanumeric) {
        return None;
    }
    let bar = line.find('|')?;
    let name = line[..bar].trim_end_matches([' ', '\t']);
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return None;
    }
    if !name.bytes().any(|b| b.is_ascii_alphabetic()) || is_level_word(name) {
        return None;
    }
    let after_bar = &line[bar + 1..];
    let rest = after_bar
        .strip_prefix(' ')
        .or_else(|| after_bar.strip_prefix('\t'))
        .unwrap_or(after_bar);
    Some((name, rest))
}

fn is_level_word(word: &str) -> bool {
    if !(3..=8).contains(&word.len()) {
        return false;
    }
    const LEVELS: &[&str] = &[
        "TRACE", "TRC", "DEBUG", "DBG", "INFO", "INF", "LOG", "NOTICE", "WARN", "WARNING", "WRN",
        "ERROR", "ERR", "ERRO", "FATAL", "FATA", "CRIT", "CRITICAL", "PANIC", "EMERG", "ALERT",
        "SEVERE", "FINE", "FINER", "FINEST", "ALL", "OFF",
    ];
    LEVELS.iter().any(|lvl| word.eq_ignore_ascii_case(lvl))
}

fn strip_container_prefix(line: &str, compose_mode: bool) -> (Option<&str>, &str) {
    if line.starts_with('[')
        && matches!(line.as_bytes().get(1), Some(b'p' | b'd' | b's' | b'j'))
        && let Some(caps) = K8S_PREFIX.captures(line)
    {
        let svc = caps.get(1).unwrap().as_str();
        let rest = caps.get(2).unwrap().as_str();
        return (Some(svc), rest);
    }
    if compose_mode && let Some((name, rest)) = parse_compose_prefix(line) {
        return (Some(name), rest);
    }
    (None, line)
}

fn handle_klog_line(
    collector: &mut Collector,
    stream: &str,
    line_no: usize,
    prefix_service: Option<&str>,
    caps: &regex::Captures<'_>,
) {
    collector.flush_open(stream, prefix_service);
    let sev_char = caps.get(1).unwrap().as_str();
    let ts = caps.get(3).map(|m| m.as_str());
    let src = caps.get(4).unwrap().as_str();
    let msg = caps.get(5).unwrap().as_str();
    let level_label = match sev_char {
        "E" => "ERROR",
        "F" => "FATAL",
        "W" => "WARN",
        _ => "INFO",
    };
    let severity = match sev_char {
        "E" | "F" => Severity::Error,
        "W" => Severity::Warning,
        _ => classify_plain_severity(msg, true),
    };
    let bucket = collector.note_service(prefix_service);
    let mut full = String::with_capacity(level_label.len() + src.len() + msg.len() + 4);
    let _ = write!(full, "{level_label} {src}] {msg}");
    record_plain_line(
        collector,
        stream,
        line_no,
        bucket,
        prefix_service,
        &full,
        ts,
        severity,
    );
}

fn observe_line(
    collector: &mut Collector,
    stream: &str,
    line_no: usize,
    line: &str,
    compose_mode: bool,
) {
    let (prefix_service, body) = strip_container_prefix(line, compose_mode);
    if body.trim().is_empty() {
        collector.flush_open(stream, prefix_service);
        return;
    }

    // Check if this line is a JSON/NDJSON object (possibly after a leading timestamp).
    if let Some(parsed_json) = try_parse_json_log(body) {
        collector.flush_open(stream, prefix_service);
        handle_json_log(collector, stream, line_no, prefix_service, parsed_json);
        return;
    }

    // Handle klog/glog format: `E1125 10:00:01.123456 1 main.go:42] message`
    if looks_like_klog(body)
        && let Some(caps) = KLOG_PREFIX.captures(body)
    {
        handle_klog_line(collector, stream, line_no, prefix_service, &caps);
        return;
    }

    // Extract leading timestamp (only if line does not start with indentation).
    let (ts, after_ts) = if body.starts_with([' ', '\t']) {
        (None, body)
    } else {
        extract_leading_ts(body)
    };
    let had_ts = ts.is_some();

    // Also handle klog/glog wrapped by `kubectl logs --timestamps` / `docker logs -t`.
    if had_ts
        && looks_like_klog(after_ts)
        && let Some(caps) = KLOG_PREFIX.captures(after_ts)
    {
        handle_klog_line(collector, stream, line_no, prefix_service, &caps);
        return;
    }

    // Check syslog / journalctl `<host> <unit>[<pid>]: <msg>` when no container prefix was set.
    let (service, effective_body, syslog_pid) = if prefix_service.is_none() && had_ts {
        if let Some((unit, pid, msg)) = parse_syslog_unit(after_ts) {
            (Some(unit), msg, pid)
        } else {
            (None, after_ts, None)
        }
    } else {
        (prefix_service, after_ts, None)
    };

    // Check if this line continues an open error/warning block on the same (stream, service).
    // Checking AFTER timestamp and syslog unit extraction lets stack traces stay attached even
    // under `docker logs -t`, `kubectl logs --prefix --timestamps`, and `journalctl`.
    if !collector.open_blocks.is_empty()
        && let Some(pos) = collector
            .open_blocks
            .iter()
            .position(|b| b.stream == stream && b.service.as_deref() == service)
    {
        if is_continuation_line(effective_body, had_ts, &collector.open_blocks[pos]) {
            collector.open_blocks[pos].push_frame(effective_body, line_no);
            return;
        }
        let block = collector.open_blocks.swap_remove(pos);
        let (key, entry) = block.into_parts();
        collector.add_error_entry(key, entry);
    }

    let bucket = collector.note_service(service);

    let trimmed = effective_body.trim();
    let severity = classify_plain_severity(trimmed, false);
    if severity != Severity::Routine {
        let order = collector.next_order;
        collector.next_order += 1;
        let norm_header = normalize_template(trimmed);
        let display_error_body = match syslog_pid {
            Some(p) => format!("[pid {p}] {trimmed}"),
            None => trimmed.to_owned(),
        };
        let has_traceback = trimmed.starts_with("Traceback")
            || trimmed.starts_with("Stack trace:")
            || trimmed.starts_with("stack backtrace:")
            || (trimmed.starts_with("goroutine ") && GOROUTINE_HEADER.is_match(trimmed));
        let block = OpenBlock {
            severity,
            service: service.map(str::to_owned),
            stream: stream.to_owned(),
            start_line: line_no,
            end_line: line_no,
            header: display_error_body,
            norm_header,
            frames_head: Vec::new(),
            frames_tail: VecDeque::new(),
            total_frames: 0,
            has_traceback,
            expect_chained_traceback: false,
            order,
        };
        collector.push_open_block(block);
    } else {
        normalize_template_into(trimmed, &mut collector.scratch);
        collector.record_routine_from_scratch(stream, line_no, bucket, service, ts, || {
            collapse_spaces(trimmed)
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn record_plain_line(
    collector: &mut Collector,
    stream: &str,
    line_no: usize,
    bucket: usize,
    service: Option<&str>,
    body: &str,
    ts: Option<&str>,
    severity: Severity,
) {
    let trimmed = body.trim();
    if severity != Severity::Routine {
        let order = collector.next_order;
        collector.next_order += 1;
        let block = OpenBlock {
            severity,
            service: service.map(str::to_owned),
            stream: stream.to_owned(),
            start_line: line_no,
            end_line: line_no,
            header: trimmed.to_owned(),
            norm_header: normalize_template(trimmed),
            frames_head: Vec::new(),
            frames_tail: VecDeque::new(),
            total_frames: 0,
            has_traceback: false,
            expect_chained_traceback: false,
            order,
        };
        collector.push_open_block(block);
    } else {
        normalize_template_into(trimmed, &mut collector.scratch);
        collector.record_routine_from_scratch(stream, line_no, bucket, service, ts, || {
            collapse_spaces(trimmed)
        });
    }
}

fn extract_leading_ts(line: &str) -> (Option<&str>, &str) {
    if !may_start_with_ts(line) {
        return (None, line);
    }
    if let Some(caps) = LEADING_TS.captures(line) {
        let full = caps.get(0).unwrap();
        let rest = &line[full.end()..];
        if !rest.is_empty() {
            let ts = caps.get(1).map(|m| m.as_str());
            return (ts, rest);
        }
    }
    (None, line)
}

fn parse_syslog_unit(line: &str) -> Option<(&str, Option<&str>, &str)> {
    if !line.contains(": ")
        || !line
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return None;
    }
    let caps = SYSLOG_UNIT.captures(line)?;
    let host = caps.get(1)?.as_str();
    let unit = caps.get(2)?.as_str();
    let pid = caps.get(3).map(|m| m.as_str());
    let msg = caps.get(4)?.as_str();
    // Guard against log levels or timezones masquerading as syslog hostnames.
    if is_level_word(host)
        || is_level_word(unit)
        || matches!(host, "UTC" | "GMT" | "EST" | "PST" | "PDT" | "EDT" | "CET")
    {
        return None;
    }
    // Hostname must contain at least one letter (avoids `[1]` or numeric columns).
    if !host.bytes().any(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some((unit, pid, msg))
}

fn is_continuation_line(body: &str, had_ts: bool, open: &OpenBlock) -> bool {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return false;
    }
    // A fresh explicit log level (e.g. `INFO ...`, `ERROR ...`) always starts a new event.
    if (may_start_with_level(trimmed) && EXPLICIT_LEVEL.is_match(trimmed))
        || (looks_like_klog(trimmed) && KLOG_PREFIX.is_match(trimmed))
    {
        return false;
    }
    if trimmed.starts_with("Traceback (most recent call last):") {
        // Attach a traceback right after an ERROR header or Python chained-exception bridge,
        // but start a fresh block when a previous traceback already finished.
        return !open.has_traceback || open.expect_chained_traceback;
    }
    // Explicit stack-trace / traceback headers and trailers attach with or without timestamps.
    if trimmed.starts_with("Caused by:")
        || trimmed.starts_with("During handling of the above exception")
        || trimmed.starts_with("The above exception was the direct cause")
        || trimmed.starts_with("Stack trace:")
        || trimmed.starts_with("stack backtrace:")
        || trimmed.starts_with("Stack backtrace:")
        || trimmed.starts_with("note: run with `RUST_BACKTRACE")
        || (trimmed.starts_with("... ") && trimmed.ends_with(" more"))
        || (trimmed.starts_with("goroutine ") && GOROUTINE_HEADER.is_match(trimmed))
        || trimmed.starts_with("created by ")
        || trimmed == "<asynchronous suspension>"
        || looks_like_dart_frame(trimmed)
    {
        return true;
    }
    // Final Python/Ruby/Java exception line after a traceback block.
    if open.has_traceback
        && trimmed.contains(':')
        && may_have_exception_name(trimmed)
        && EXCEPTION_NAME.is_match(trimmed)
    {
        return true;
    }
    // Indented lines are stack frames, Python traceback lines, or error elaboration.
    // When a line carried a fresh timestamp, require >= 2 spaces or a tab so single-space
    // padding after a timestamp never absorbs an unrelated routine line.
    if had_ts {
        body.starts_with("  ") || body.starts_with('\t')
    } else {
        body.starts_with([' ', '\t'])
    }
}

struct ParsedJson<'a> {
    leading_ts: Option<&'a str>,
    map: Map<String, Value>,
}

fn try_parse_json_log(body: &str) -> Option<ParsedJson<'_>> {
    let trimmed = body.trim();
    // Fast path: JSON objects always end with `}` (even when preceded by a timestamp).
    if !trimmed.ends_with('}') {
        return None;
    }
    let (leading_ts, candidate) = if trimmed.starts_with('{') {
        (None, trimmed)
    } else if trimmed.contains('{') {
        let (ts, rest) = extract_leading_ts(trimmed);
        let rest_trimmed = rest.trim_start();
        if rest_trimmed.starts_with('{') {
            (ts, rest_trimmed)
        } else {
            return None;
        }
    } else {
        return None;
    };

    let Value::Object(map) = serde_json::from_str::<Value>(candidate).ok()? else {
        return None;
    };
    Some(ParsedJson { leading_ts, map })
}

fn extract_json_ts(map: &Map<String, Value>) -> Option<Cow<'_, str>> {
    for key in [
        "ts",
        "timestamp",
        "time",
        "@timestamp",
        "datetime",
        "date",
        "t",
    ] {
        if let Some(val) = map.get(key) {
            match val {
                Value::String(s) if !s.is_empty() => return Some(Cow::Borrowed(s.as_str())),
                Value::Number(n) => {
                    // Unix epoch in s/ms/us/ns as integer or float (Pino/Bunyan/Zap/K8s klog JSON).
                    let v_opt = n.as_u64().or_else(|| {
                        n.as_f64()
                            .filter(|f| f.is_finite() && *f >= 1_000_000_000.0)
                            .map(|f| f as u64)
                    });
                    if let Some(v) = v_opt
                        && v >= 1_000_000_000
                    {
                        let secs = if v >= 10_000_000_000_000_000 {
                            v / 1_000_000_000
                        } else if v >= 10_000_000_000_000 {
                            v / 1_000_000
                        } else if v >= 10_000_000_000 {
                            v / 1_000
                        } else {
                            v
                        };
                        let h = (secs / 3600) % 24;
                        let m = (secs / 60) % 60;
                        let s = secs % 60;
                        return Some(Cow::Owned(format!("{h:02}:{m:02}:{s:02}Z")));
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn extract_json_level(map: &Map<String, Value>) -> (Option<Cow<'static, str>>, Severity) {
    for key in [
        "level",
        "severity",
        "lvl",
        "log.level",
        "levelname",
        "priority",
    ] {
        if let Some(val) = map.get(key) {
            match val {
                Value::String(s) if !s.is_empty() => {
                    let t = s.trim();
                    if t.eq_ignore_ascii_case("info") || t.eq_ignore_ascii_case("inf") {
                        return (Some(Cow::Borrowed("INFO")), Severity::Routine);
                    }
                    if t.eq_ignore_ascii_case("debug") || t.eq_ignore_ascii_case("dbg") {
                        return (Some(Cow::Borrowed("DEBUG")), Severity::Routine);
                    }
                    if t.eq_ignore_ascii_case("trace") || t.eq_ignore_ascii_case("trc") {
                        return (Some(Cow::Borrowed("TRACE")), Severity::Routine);
                    }
                    if t.eq_ignore_ascii_case("warn")
                        || t.eq_ignore_ascii_case("warning")
                        || t.eq_ignore_ascii_case("wrn")
                    {
                        return (Some(Cow::Borrowed("WARN")), Severity::Warning);
                    }
                    if t.eq_ignore_ascii_case("error")
                        || t.eq_ignore_ascii_case("err")
                        || t.eq_ignore_ascii_case("erro")
                    {
                        return (Some(Cow::Borrowed("ERROR")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("fatal") || t.eq_ignore_ascii_case("fata") {
                        return (Some(Cow::Borrowed("FATAL")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("crit") {
                        return (Some(Cow::Borrowed("CRIT")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("critical") {
                        return (Some(Cow::Borrowed("CRITICAL")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("panic") {
                        return (Some(Cow::Borrowed("PANIC")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("dpanic") {
                        return (Some(Cow::Borrowed("DPANIC")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("emerg") {
                        return (Some(Cow::Borrowed("EMERG")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("emergency") {
                        return (Some(Cow::Borrowed("EMERGENCY")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("alert") {
                        return (Some(Cow::Borrowed("ALERT")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("severe") {
                        return (Some(Cow::Borrowed("SEVERE")), Severity::Error);
                    }
                    if t.eq_ignore_ascii_case("notice") {
                        return (Some(Cow::Borrowed("NOTICE")), Severity::Routine);
                    }
                    if t.eq_ignore_ascii_case("log") {
                        return (Some(Cow::Borrowed("LOG")), Severity::Routine);
                    }
                    return (Some(Cow::Owned(t.to_ascii_uppercase())), Severity::Routine);
                }
                Value::Number(n) => {
                    // Pino / Bunyan numeric log levels.
                    if let Some(v) = n.as_u64() {
                        return match v {
                            0..=15 => (Some(Cow::Borrowed("TRACE")), Severity::Routine),
                            16..=25 => (Some(Cow::Borrowed("DEBUG")), Severity::Routine),
                            26..=35 => (Some(Cow::Borrowed("INFO")), Severity::Routine),
                            36..=45 => (Some(Cow::Borrowed("WARN")), Severity::Warning),
                            46..=55 => (Some(Cow::Borrowed("ERROR")), Severity::Error),
                            _ => (Some(Cow::Borrowed("FATAL")), Severity::Error),
                        };
                    }
                }
                _ => {}
            }
        }
    }
    (None, Severity::Routine)
}

fn extract_json_string<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    for &key in keys {
        if let Some(Value::String(s)) = map.get(key)
            && !s.trim().is_empty()
        {
            return Some(s.trim());
        }
    }
    None
}

fn is_truthy_error_value(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_u64() != Some(0) && n.as_f64() != Some(0.0),
        Value::String(s) => {
            let t = s.trim();
            !t.is_empty() && !t.eq_ignore_ascii_case("null") && !t.eq_ignore_ascii_case("none")
        }
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

fn handle_json_log(
    collector: &mut Collector,
    stream: &str,
    line_no: usize,
    prefix_service: Option<&str>,
    parsed: ParsedJson<'_>,
) {
    let map = &parsed.map;
    let json_service = extract_json_string(
        map,
        &[
            "service",
            "service.name",
            "logger",
            "component",
            "module",
            "target",
            "app",
            "name",
            "subsystem",
        ],
    );
    let service = prefix_service.or(json_service);
    let bucket = collector.note_service(service);

    let (level_opt, mut severity) = extract_json_level(map);
    let msg = extract_json_string(map, &["msg", "message", "event", "text"]);

    // Check for non-empty error payload fields (`err`, `error`, `exception`, `stack`, etc.).
    let has_err_field = [
        "err",
        "error",
        "exception",
        "exc_info",
        "stack",
        "stacktrace",
        "traceback",
        "panic",
    ]
    .iter()
    .any(|k| map.get(*k).is_some_and(is_truthy_error_value));
    if has_err_field && severity < Severity::Error {
        severity = Severity::Error;
    }

    // Check for HTTP 5xx numeric status field.
    let has_5xx = ["status", "status_code", "http_status", "http.status_code"]
        .iter()
        .any(|k| {
            map.get(*k)
                .and_then(Value::as_u64)
                .is_some_and(|code| (500..=599).contains(&code))
        });
    if has_5xx && severity < Severity::Error {
        severity = Severity::Error;
    }

    // If no explicit level was set, infer severity from the message text.
    if level_opt.is_none()
        && let Some(m) = msg
    {
        severity = severity.max(classify_plain_severity(m, false));
    } else if severity == Severity::Routine
        && let Some(m) = msg
    {
        // Even on an INFO JSON entry, escalate unmistakable crash/panic/OOM messages.
        let mb = m.as_bytes();
        if contains_ascii_icase(mb, b"panic")
            || contains_ascii_icase(mb, b"oomkilled")
            || contains_ascii_icase(mb, b"out of memory")
            || contains_ascii_icase(mb, b"unhandled exception")
            || contains_ascii_icase(mb, b"fatal:")
        {
            severity = Severity::Error;
        }
    }

    let level_label = level_opt.unwrap_or(match severity {
        Severity::Error => Cow::Borrowed("ERROR"),
        Severity::Warning => Cow::Borrowed("WARN"),
        Severity::Routine => Cow::Borrowed("INFO"),
    });

    if severity != Severity::Routine {
        let (header, frames, omitted, sig) = format_json_error(map, &level_label, msg);
        let order = collector.next_order;
        collector.next_order += 1;
        let line_ref = LineRef::single(stream, line_no);
        let service_owned = service.map(str::to_owned);
        let key = ErrorKey {
            severity,
            service: service_owned.clone(),
            signature: sig,
        };
        let entry = ErrorEntry {
            severity,
            service: service_owned,
            header: truncate_line(&header, MAX_ERROR_LINE_BYTES),
            frames,
            omitted_frames: omitted,
            count: 1,
            first: line_ref.clone(),
            last: line_ref,
            order,
        };
        collector.add_error_entry(key, entry);
        return;
    }

    let ts = extract_json_ts(map).or_else(|| parsed.leading_ts.map(Cow::Borrowed));

    // Routine JSON log: fold by (service, level, normalized_msg) with zero alloc on hit.
    collector.scratch.clear();
    collector.scratch.push_str(&level_label);
    collector.scratch.push(' ');
    if let Some(m) = msg {
        append_normalized_template(m, &mut collector.scratch);
        collector.record_routine_from_scratch(
            stream,
            line_no,
            bucket,
            service,
            ts.as_deref(),
            || format!("{level_label} {}", collapse_spaces_cow(m)),
        );
    } else {
        let synth = synthesize_json_summary(map);
        append_normalized_template(&synth, &mut collector.scratch);
        collector.record_routine_from_scratch(
            stream,
            line_no,
            bucket,
            service,
            ts.as_deref(),
            || format!("{level_label} {synth}"),
        );
    }
}

fn format_json_error(
    map: &Map<String, Value>,
    level: &str,
    msg: Option<&str>,
) -> (String, Vec<String>, usize, String) {
    let mut header = String::from(level);
    let mut sig = String::from(level);
    if let Some(m) = msg {
        header.push(' ');
        header.push_str(&collapse_spaces_cow(m));
        sig.push(' ');
        append_normalized_template(m, &mut sig);
    }

    // Collect embedded stack traces (top-level or nested inside `err`/`error`).
    let mut stack_text: Option<&str> = None;
    for key in ["stack", "stacktrace", "traceback", "exc_info"] {
        if let Some(Value::String(s)) = map.get(key)
            && !s.trim().is_empty()
        {
            stack_text = Some(s.as_str());
            break;
        }
    }
    if stack_text.is_none() {
        for key in ["err", "error"] {
            if let Some(Value::Object(inner)) = map.get(key)
                && let Some(Value::String(s)) = inner
                    .get("stack")
                    .or_else(|| inner.get("stacktrace"))
                    .or_else(|| inner.get("traceback"))
                    .or_else(|| inner.get("exc_info"))
                && !s.trim().is_empty()
            {
                stack_text = Some(s.as_str());
                break;
            }
        }
    }

    // Render error fields first (preserving exact values), then short context fields.
    let mut details = Vec::new();
    let mut sig_details = Vec::new();
    let priority_keys = [
        "err",
        "error",
        "exception",
        "reason",
        "cause",
        "code",
        "status",
        "status_code",
        "method",
        "path",
        "route",
        "caller",
        "req_id",
        "request_id",
    ];
    let skip_keys = [
        "ts",
        "timestamp",
        "time",
        "@timestamp",
        "datetime",
        "date",
        "t",
        "level",
        "severity",
        "lvl",
        "log.level",
        "levelname",
        "priority",
        "msg",
        "message",
        "event",
        "text",
        "service",
        "service.name",
        "logger",
        "component",
        "module",
        "target",
        "app",
        "name",
        "subsystem",
        "stack",
        "stacktrace",
        "traceback",
        "exc_info",
        "pid",
        "hostname",
        "v",
    ];

    for &key in &priority_keys {
        if let Some(val) = map.get(key)
            && !val.is_null()
        {
            let formatted = format_json_field_value(val, true);
            if !formatted.is_empty() {
                details.push(format!("{key}={formatted}"));
                // Include semantic error fields in dedup signature, excluding volatile request IDs.
                if !matches!(key, "req_id" | "request_id") {
                    sig_details.push(format!("{key}={}", normalize_template(&formatted)));
                }
            }
        }
    }

    // Add remaining concise non-metadata fields so no useful error context is dropped.
    let mut extra_budget = 4usize;
    for (key, val) in map {
        if extra_budget == 0 {
            break;
        }
        if priority_keys.contains(&key.as_str())
            || skip_keys.contains(&key.as_str())
            || val.is_null()
        {
            continue;
        }
        let formatted = format_json_field_value(val, false);
        if !formatted.is_empty() && formatted.len() <= 80 {
            details.push(format!("{key}={formatted}"));
            if !matches!(
                key.as_str(),
                "trace_id"
                    | "span_id"
                    | "correlation_id"
                    | "rid"
                    | "cid"
                    | "id"
                    | "latency"
                    | "latency_ms"
                    | "duration"
                    | "duration_ms"
                    | "elapsed"
                    | "elapsed_ms"
            ) {
                sig_details.push(format!("{key}={}", normalize_template(&formatted)));
            }
            extra_budget -= 1;
        }
    }

    if !details.is_empty() {
        header.push_str(" | ");
        header.push_str(&details.join(" "));
    }
    if !sig_details.is_empty() {
        sig.push_str(" | ");
        sig.push_str(&sig_details.join(" "));
    }

    let (frames, omitted) = match stack_text {
        Some(raw_stack) => {
            let lines: Vec<String> = raw_stack
                .lines()
                .map(|l| l.trim_end())
                .filter(|l| !l.trim().is_empty())
                .map(|l| truncate_line(&format!("  {}", l.trim_start()), MAX_FRAME_LINE_BYTES))
                .collect();
            if lines.len() > MAX_FRAMES_HEAD + MAX_FRAMES_TAIL {
                let omitted = lines.len() - MAX_FRAMES_HEAD - MAX_FRAMES_TAIL;
                let mut kept = Vec::with_capacity(MAX_FRAMES_HEAD + MAX_FRAMES_TAIL);
                kept.extend_from_slice(&lines[..MAX_FRAMES_HEAD]);
                kept.extend_from_slice(&lines[lines.len() - MAX_FRAMES_TAIL..]);
                (kept, omitted)
            } else {
                (lines, 0)
            }
        }
        None => (Vec::new(), 0),
    };

    if let Some(first_frame) = frames.first() {
        sig.push('\n');
        append_normalized_template(first_frame.trim(), &mut sig);
    }

    (header, frames, omitted, sig)
}

fn format_json_field_value(val: &Value, strip_nested_stack: bool) -> String {
    match val {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            let clean = collapse_spaces_cow(s.trim());
            if clean.contains(' ') {
                format!("\"{clean}\"")
            } else {
                clean.into_owned()
            }
        }
        Value::Object(obj) if strip_nested_stack => {
            let mut filtered = Map::new();
            for (k, v) in obj {
                if !matches!(
                    k.as_str(),
                    "stack" | "stacktrace" | "traceback" | "exc_info"
                ) {
                    filtered.insert(k.clone(), v.clone());
                }
            }
            if filtered.is_empty() {
                String::new()
            } else {
                serde_json::to_string(&Value::Object(filtered)).unwrap_or_default()
            }
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn synthesize_json_summary(map: &Map<String, Value>) -> String {
    let skip = [
        "ts",
        "timestamp",
        "time",
        "@timestamp",
        "datetime",
        "date",
        "t",
        "level",
        "severity",
        "lvl",
        "log.level",
        "levelname",
        "priority",
        "service",
        "service.name",
        "logger",
        "component",
        "module",
        "target",
        "app",
        "name",
        "subsystem",
        "pid",
        "hostname",
        "v",
    ];
    let mut parts = Vec::new();
    for (k, v) in map {
        if skip.contains(&k.as_str()) || v.is_null() {
            continue;
        }
        let formatted = format_json_field_value(v, false);
        if !formatted.is_empty() {
            parts.push(format!("{k}={formatted}"));
        }
        if parts.len() >= 6 {
            break;
        }
    }
    if parts.is_empty() {
        "{}".to_owned()
    } else {
        parts.join(" ")
    }
}

fn classify_plain_severity(line: &str, explicit_info: bool) -> Severity {
    // 1. Check explicit log-level prefix (byte-guarded to skip numeric/non-level lines).
    let mut has_explicit_routine = explicit_info;
    if may_start_with_level(line)
        && let Some(caps) = EXPLICIT_LEVEL.captures(line)
    {
        let lvl = caps.get(1).unwrap().as_str();
        if lvl.eq_ignore_ascii_case("error")
            || lvl.eq_ignore_ascii_case("err")
            || lvl.eq_ignore_ascii_case("erro")
            || lvl.eq_ignore_ascii_case("fatal")
            || lvl.eq_ignore_ascii_case("fata")
            || lvl.eq_ignore_ascii_case("crit")
            || lvl.eq_ignore_ascii_case("critical")
            || lvl.eq_ignore_ascii_case("panic")
            || lvl.eq_ignore_ascii_case("emerg")
            || lvl.eq_ignore_ascii_case("alert")
            || lvl.eq_ignore_ascii_case("severe")
        {
            return Severity::Error;
        }
        if lvl.eq_ignore_ascii_case("warn")
            || lvl.eq_ignore_ascii_case("warning")
            || lvl.eq_ignore_ascii_case("wrn")
        {
            return Severity::Warning;
        }
        has_explicit_routine = true;
    }

    // 2. HTTP 5xx in access logs is always an error (requires ASCII `'5'` and HTTP verb/status key).
    if line.as_bytes().contains(&b'5') && may_have_http_status(line) && HTTP_5XX.is_match(line) {
        return Severity::Error;
    }

    // 3. Unmistakable crash / panic / traceback / OOM / runtime exception signals
    //    always escalate to Error even if prefixed with INFO.
    //    Lowercase into a stack buffer for typical lines (<= 512 B) to avoid heap allocs.
    let src = line.as_bytes();
    let mut stack = [0u8; 512];
    let heap: Vec<u8>;
    let lower_bytes: &[u8] = if src.len() <= stack.len() {
        for (dst, &b) in stack[..src.len()].iter_mut().zip(src) {
            *dst = b.to_ascii_lowercase();
        }
        &stack[..src.len()]
    } else {
        heap = src.to_ascii_lowercase();
        &heap
    };
    let lower = std::str::from_utf8(lower_bytes).unwrap_or(line);

    const CRASH_NEEDLES: &[&str] = &[
        "panic:",
        "panicked at",
        "traceback (most recent call last)",
        "unhandled rejection",
        "unhandledpromiserejection",
        "uncaught exception",
        "exception in thread",
        "fatal error",
        "fatal:",
        "segmentation fault",
        "segfault",
        "oomkilled",
        "oom-killer",
        "out of memory",
        "killed process",
        "cannot allocate memory",
        "crashloopbackoff",
        "back-off restarting failed container",
        "connection refused",
        "econnrefused",
        "eaddrinuse",
        "address already in use",
        "deadlock detected",
        "no space left on device",
        "read-only file system",
        "permission denied",
        "certificate has expired",
        "failed to ",
        "failed with ",
        "could not ",
        "unable to ",
    ];

    if CRASH_NEEDLES.iter().any(|needle| lower.contains(needle))
        || (line.starts_with("goroutine ") && GOROUTINE_HEADER.is_match(line))
        || (may_have_exception_name(line) && EXCEPTION_NAME.is_match(line))
        || (lower.contains("exit") && EXIT_NONZERO.is_match(line))
    {
        return Severity::Error;
    }

    // 4. For lines without an explicit INFO/DEBUG/TRACE/LOG prefix, also inspect
    //    general error/warning words (while ignoring `0 errors` / `errors=0` noise).
    if !has_explicit_routine {
        let cleaned = if src.contains(&b'0') {
            ZERO_ERROR_NOISE.replace_all(line, "")
        } else {
            Cow::Borrowed(line)
        };
        if STRONG_ERROR_WORD.is_match(&cleaned) {
            return Severity::Error;
        }
        if STRONG_WARN_WORD.is_match(&cleaned) {
            return Severity::Warning;
        }
    }

    Severity::Routine
}

/// Normalize volatile fields in a log line into stable template placeholders
/// while preserving `[redacted:...]` / `[REDACTED ...]` markers and HTTP status
/// codes verbatim.
fn normalize_template(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    append_normalized_template(line, &mut out);
    out
}

fn normalize_template_into(line: &str, out: &mut String) {
    out.clear();
    append_normalized_template(line, out);
}

fn append_normalized_template(line: &str, out: &mut String) {
    // Fast path: skip `REDACTED_MARKER` regex completely unless `[` + `redacted` appear.
    let has_redacted = line.contains('[') && contains_ascii_icase(line.as_bytes(), b"redacted");
    if !has_redacted {
        let norm = normalize_chunk(line);
        let collapsed = collapse_spaces_cow(norm.trim());
        out.push_str(&collapsed);
        return;
    }

    // Split around redaction markers so no normalization pass can ever touch them.
    let mut buf = String::with_capacity(line.len());
    let mut last = 0usize;
    for m in REDACTED_MARKER.find_iter(line) {
        if m.start() > last {
            buf.push_str(&normalize_chunk(&line[last..m.start()]));
        }
        buf.push_str(m.as_str());
        last = m.end();
    }
    if last < line.len() {
        buf.push_str(&normalize_chunk(&line[last..]));
    }
    out.push_str(&collapse_spaces_cow(buf.trim()));
}

fn normalize_chunk(chunk: &str) -> Cow<'_, str> {
    let mut has_digit = false;
    let mut has_colon = false;
    let mut has_eq = false;
    let mut has_dot = false;
    let mut has_slash = false;
    let mut has_dash = false;
    let mut has_underscore = false;
    let mut has_lbracket = false;
    for &b in chunk.as_bytes() {
        match b {
            b'0'..=b'9' => has_digit = true,
            b':' => has_colon = true,
            b'=' => has_eq = true,
            b'.' => has_dot = true,
            b'/' => has_slash = true,
            b'-' => has_dash = true,
            b'_' => has_underscore = true,
            b'[' => has_lbracket = true,
            _ => {}
        }
    }

    // Fast path: plain text with no digits and no `=`/`:` key-value IDs needs zero regexes.
    if !has_digit && !has_eq && !has_colon {
        return Cow::Borrowed(chunk);
    }

    let mut s = Cow::Borrowed(chunk);

    if has_digit && has_colon && (has_dash || has_slash) {
        s = cow_replace(&INLINE_TS, s, "<ts>");
    }
    if has_digit && has_dash && s.len() >= 36 {
        s = cow_replace(&UUID_RE, s, "<id>");
    }
    if has_eq || has_colon {
        s = cow_replace(&KV_ID_RE, s, "${1}${2}<id>");
    }
    if !has_digit {
        return s;
    }

    if has_colon && (has_lbracket || s.contains("::ffff:")) {
        s = cow_replace(&IPV6_RE, s, "<ip>");
    }
    if has_dot {
        s = cow_replace(&IPV4_RE, s, "<ip>");
    }
    if s.contains("0x") {
        s = cow_replace(&HEX_ADDR_RE, s, "<id>");
    }
    if s.len() >= 7 {
        s = cow_map(s, replace_hex_ids);
    }
    if has_dash || has_underscore {
        s = cow_map(s, replace_prefixed_ids);
    }
    if s.bytes().any(|b| b.is_ascii_digit()) {
        s = cow_replace(&DURATION_RE, s, "<dur>");
    } else {
        return s;
    }
    // Re-check digits after UUID/IP/ID/duration normalization: many log lines have none left!
    if !s.bytes().any(|b| b.is_ascii_digit()) {
        return s;
    }
    if s.contains(['b', 'B']) {
        s = cow_replace(&BYTE_SIZE_RE, s, "<size>");
    }
    if has_slash {
        s = cow_map(s, replace_path_numeric_segments);
    }
    if has_lbracket {
        s = cow_replace(&BRACKET_PID_RE, s, "[<id>]");
    }
    if s.bytes().any(|b| b.is_ascii_digit()) {
        s = cow_map(s, replace_numbers_preserving_context);
    }
    s
}

#[inline]
fn cow_replace<'a>(re: &Regex, input: Cow<'a, str>, rep: &str) -> Cow<'a, str> {
    match re.replace_all(&input, rep) {
        Cow::Borrowed(_) => input,
        Cow::Owned(owned) => Cow::Owned(owned),
    }
}

#[inline]
fn cow_map<'a>(input: Cow<'a, str>, f: impl FnOnce(&str) -> Cow<'_, str>) -> Cow<'a, str> {
    match f(&input) {
        Cow::Borrowed(_) => input,
        Cow::Owned(owned) => Cow::Owned(owned),
    }
}

#[inline]
fn is_word_boundary_before(text: &str, idx: usize) -> bool {
    if idx == 0 {
        return true;
    }
    let prev = text.as_bytes()[idx - 1];
    if prev.is_ascii() {
        !prev.is_ascii_alphanumeric() && prev != b'_'
    } else {
        text[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_')
    }
}

#[inline]
fn is_word_boundary_after(text: &str, idx: usize) -> bool {
    if idx >= text.len() {
        return true;
    }
    let next = text.as_bytes()[idx];
    if next.is_ascii() {
        !next.is_ascii_alphanumeric() && next != b'_'
    } else {
        text[idx..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_')
    }
}

/// Pure-byte replacement of mixed hex IDs (`>= 7` hex chars with both digit & alpha)
/// and long pure hex digests (`>= 16` chars with digit), returning `Cow::Borrowed`
/// when nothing is replaced.
fn replace_hex_ids(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut copied = 0usize;
    let mut out = String::new();

    while i < len {
        if bytes[i].is_ascii_hexdigit() {
            let start = i;
            let mut has_digit = false;
            let mut has_alpha = false;
            while i < len && bytes[i].is_ascii_hexdigit() {
                if bytes[i].is_ascii_digit() {
                    has_digit = true;
                } else {
                    has_alpha = true;
                }
                i += 1;
            }
            let tok_len = i - start;
            if tok_len >= 7
                && ((has_digit && has_alpha) || (tok_len >= 16 && has_digit))
                && is_word_boundary_before(text, start)
                && is_word_boundary_after(text, i)
            {
                if copied == 0 {
                    out.reserve(len);
                }
                out.push_str(&text[copied..start]);
                out.push_str("<id>");
                copied = i;
            }
        } else {
            i += 1;
        }
    }

    if copied == 0 {
        Cow::Borrowed(text)
    } else {
        out.push_str(&text[copied..]);
        Cow::Owned(out)
    }
}

fn replace_prefixed_ids(text: &str) -> Cow<'_, str> {
    let mut copied = 0usize;
    let mut out = String::new();
    for caps in PREFIXED_ID_RE.captures_iter(text) {
        let full = caps.get(0).unwrap();
        let prefix = caps.get(1).unwrap().as_str();
        let suffix = caps.get(2).unwrap().as_str();
        if !suffix.bytes().any(|b| b.is_ascii_digit()) {
            continue;
        }
        let stem = prefix.trim_end_matches(['-', '_']);
        // Preserve well-known protocol/encoding/crypto identifiers.
        const PRESERVE: &[&str] = &[
            "utf", "sha", "aes", "rsa", "md", "tls", "ssl", "http", "rfc", "iso", "x86", "arm",
            "amd", "win", "ipv", "base",
        ];
        if PRESERVE.iter().any(|p| stem.eq_ignore_ascii_case(p)) {
            continue;
        }
        if copied == 0 {
            out.reserve(text.len());
        }
        out.push_str(&text[copied..full.start()]);
        out.push_str(prefix);
        out.push_str("<id>");
        copied = full.end();
    }
    if copied == 0 {
        Cow::Borrowed(text)
    } else {
        out.push_str(&text[copied..]);
        Cow::Owned(out)
    }
}

/// Replace pure numeric URL path segments (`/users/12345/orders/99` -> `/users/<id>/orders/<id>`).
fn replace_path_numeric_segments(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0usize;
    let mut copied = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            // Skip protocol/version slashes like `HTTP/1.1` or `HTTP/2`.
            let is_http_ver = i >= 4
                && bytes[..i].ends_with(b"HTTP")
                && (i == 4 || !bytes[i - 5].is_ascii_alphanumeric());
            if !is_http_ver {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let boundary = j == bytes.len()
                    || matches!(
                        bytes[j],
                        b'/' | b'?' | b'#' | b' ' | b'\t' | b'"' | b'\'' | b')' | b',' | b']'
                    );
                if boundary {
                    if copied == 0 {
                        out.reserve(text.len());
                    }
                    out.push_str(&text[copied..=i]);
                    out.push_str("<id>");
                    copied = j;
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }

    if copied == 0 {
        Cow::Borrowed(text)
    } else {
        out.push_str(&text[copied..]);
        Cow::Owned(out)
    }
}

/// Replace standalone numbers with `<num>`, preserving HTTP status codes and
/// protocol/version numbers (such as `HTTP/1.1`, `v2`, `UTF-8`, `SHA256`).
fn replace_numbers_preserving_context(text: &str) -> Cow<'_, str> {
    let mut protected: Vec<(usize, usize)> = Vec::new();
    if may_have_http_status(text) {
        for caps in HTTP_STATUS_SPAN.captures_iter(text) {
            if let Some(m) = caps.get(1).or_else(|| caps.get(2)) {
                protected.push((m.start(), m.end()));
            }
        }
    }

    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut last = 0usize;

    for m in STANDALONE_NUM_RE.find_iter(text) {
        let start = m.start();
        let end = m.end();
        let is_protected = protected.iter().any(|&(ps, pe)| start == ps && end == pe)
            || is_version_or_proto_num(text, bytes, start);
        if is_protected {
            continue;
        }
        if last == 0 {
            out.reserve(text.len());
        }
        out.push_str(&text[last..start]);
        out.push_str("<num>");
        last = end;
    }

    if last == 0 {
        Cow::Borrowed(text)
    } else {
        out.push_str(&text[last..]);
        Cow::Owned(out)
    }
}

fn is_version_or_proto_num(text: &str, bytes: &[u8], start: usize) -> bool {
    if start == 0 {
        return false;
    }
    let before = &text[..start];
    if before.ends_with("HTTP/")
        || before.ends_with("http/")
        || before.ends_with("TLSv")
        || before.ends_with("UTF-")
        || before.ends_with("utf-")
        || before.ends_with("SHA-")
        || before.ends_with("sha-")
    {
        return true;
    }
    // Preserve `v1`, `v2.0`, etc. (note: `\b` wouldn't fire after `v`, but handle
    // dots inside `v1.2.3` where `.2` starts after `.`!).
    if bytes[start - 1] == b'.' {
        // Preserve semver/protocol dotted components like `HTTP/1.1` or `v1.2.3`.
        let prev = &bytes[..start - 1];
        if prev.ends_with(b"HTTP/1") || prev.ends_with(b"HTTP/2") {
            return true;
        }
        // Scan back over digits and dots to see if this belongs to a `v1.2.3` token.
        let mut p = start - 1;
        while p > 0 && (bytes[p - 1].is_ascii_digit() || bytes[p - 1] == b'.') {
            p -= 1;
        }
        if p > 0
            && matches!(bytes[p - 1], b'v' | b'V')
            && (p == 1 || !bytes[p - 2].is_ascii_alphanumeric())
        {
            return true;
        }
    }
    false
}

fn collapse_spaces_cow(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if matches!(bytes[i], b' ' | b'\t') && matches!(bytes[i + 1], b' ' | b'\t') {
            let mut out = String::with_capacity(text.len());
            out.push_str(&text[..i]);
            out.push(' ');
            i += 2;
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                i += 1;
            }
            let mut copied = i;
            while i + 1 < bytes.len() {
                if matches!(bytes[i], b' ' | b'\t') && matches!(bytes[i + 1], b' ' | b'\t') {
                    out.push_str(&text[copied..i]);
                    out.push(' ');
                    i += 2;
                    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                        i += 1;
                    }
                    copied = i;
                } else {
                    i += 1;
                }
            }
            out.push_str(&text[copied..]);
            return Cow::Owned(out);
        }
        i += 1;
    }
    Cow::Borrowed(text)
}

fn collapse_spaces(text: &str) -> String {
    collapse_spaces_cow(text).into_owned()
}

fn truncate_line(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let ellipsis = "…";
    let cut = text.floor_char_boundary(limit.saturating_sub(ellipsis.len()));
    let mut out = text[..cut].to_owned();
    out.push_str(ellipsis);
    out
}

fn format_time_span(first: &str, last: &str) -> Option<String> {
    let f = compact_ts(first);
    let l = compact_ts(last);
    if f.is_empty() || l.is_empty() || f == l {
        return None;
    }
    // If both share the same date prefix (`YYYY-MM-DD ` or `Mon DD `), show only the times.
    if let (Some((fd, ft)), Some((ld, lt))) = (split_date_time(&f), split_date_time(&l))
        && fd == ld
    {
        if ft == lt {
            return None;
        }
        return Some(format!("{ft}..{lt}"));
    }
    Some(format!("{f}..{l}"))
}

fn compact_ts(raw: &str) -> String {
    let s = raw.trim().trim_matches(['[', ']']);
    // Strip trailing timezone offset or `Z` / `UTC` and fractional seconds for display.
    let s = s
        .strip_suffix(" UTC")
        .or_else(|| s.strip_suffix(" GMT"))
        .unwrap_or(s);
    // Normalize ISO `T` separator to space.
    let s = if s.len() >= 19 && s.as_bytes().get(10) == Some(&b'T') {
        format!("{} {}", &s[..10], &s[11..])
    } else {
        s.to_owned()
    };
    // Trim fractional seconds + trailing Z/+HH:MM if present after `HH:MM:SS`.
    if let Some(dot) = s.find(['.', ',']) {
        let prefix = &s[..dot];
        if prefix.len() >= 8 && prefix.as_bytes()[prefix.len() - 3] == b':' {
            return prefix.to_owned();
        }
    }
    s.trim_end_matches('Z').to_owned()
}

fn split_date_time(ts: &str) -> Option<(&str, &str)> {
    // `2026-11-25 10:00:01` or `Nov 25 10:00:01`
    let idx = ts.rfind(' ')?;
    let date = ts[..idx].trim();
    let time = ts[idx + 1..].trim();
    if time.len() >= 8 && time.as_bytes()[2] == b':' && time.as_bytes()[5] == b':' {
        Some((date, &time[..8]))
    } else {
        None
    }
}

fn materialize_tail_line(stream: &str, line_no: usize, raw: &str) -> TailLine {
    let stripped = strip_ansi(raw);
    let line = stripped.trim_end_matches(['\n', '\r']);
    TailLine {
        stream: stream.to_owned(),
        line_no,
        text: line.to_owned(),
    }
}

fn select_tail_lines(mut per_stream: Vec<(&str, VecDeque<(usize, &str)>)>) -> Vec<TailLine> {
    if per_stream.is_empty() {
        return Vec::new();
    }
    if per_stream.len() == 1 {
        let (stream, queue) = per_stream.pop().unwrap();
        return queue
            .into_iter()
            .map(|(line_no, raw)| materialize_tail_line(stream, line_no, raw))
            .collect();
    }
    // Both stdout and stderr have content: keep up to 2 recent stdout + 1 recent stderr.
    let mut out = Vec::with_capacity(MAX_TAIL_LINES);
    let (stream1, mut first) = per_stream.remove(0);
    let (stream2, mut second) = per_stream.remove(0);
    let take_first = if second.is_empty() {
        MAX_TAIL_LINES
    } else {
        MAX_TAIL_LINES - 1
    };
    while first.len() > take_first {
        first.pop_front();
    }
    for (line_no, raw) in first {
        out.push(materialize_tail_line(stream1, line_no, raw));
    }
    if let Some((line_no, raw)) = second.pop_back() {
        out.push(materialize_tail_line(stream2, line_no, raw));
    }
    out
}

fn render_report(
    collector: &mut Collector,
    tail_lines: &[TailLine],
    max_bytes: usize,
    raw_bytes: usize,
) -> Option<String> {
    // Sort errors: Error before Warning, chronological within each tier.
    collector.errors.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.order.cmp(&b.order))
    });
    // Sort routine patterns: highest count first, deterministic first-seen tie-breaker.
    collector
        .patterns
        .sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.order.cmp(&b.order)));

    let header = build_header(collector);
    if header.len() > max_bytes {
        return None;
    }
    let mut out = String::with_capacity(max_bytes.min(raw_bytes));
    out.push_str(&header);

    // 1. Errors & Warnings section FIRST — routine chatter can never displace errors.
    if !collector.errors.is_empty() {
        let heading = "Errors & warnings:\n";
        if out.len() + heading.len() <= max_bytes {
            out.push_str(heading);
            let mut shown_errors = 0usize;
            for (idx, entry) in collector.errors.iter().enumerate() {
                let remaining_after = collector.errors.len() - idx - 1;
                let overflow_note_reserve = if remaining_after > 0 { 48 } else { 0 };
                let block = format_error_entry(entry);
                if out.len() + block.len() + overflow_note_reserve <= max_bytes {
                    out.push_str(&block);
                    shown_errors += 1;
                } else {
                    // Try fitting at least the header line of this error if stack frames made it big.
                    let header_only = format_error_header_only(entry);
                    if out.len() + header_only.len() + overflow_note_reserve <= max_bytes {
                        out.push_str(&header_only);
                        shown_errors += 1;
                    } else {
                        let clipped = collector.errors.len() - shown_errors;
                        let note = format!("  … +{clipped} more error/warning group(s) in log\n");
                        if out.len() + note.len() <= max_bytes {
                            out.push_str(&note);
                        }
                        break;
                    }
                }
            }
        }
    }

    // Reserve room for Recent tail if enough budget remains.
    let tail_block = if !tail_lines.is_empty() {
        build_tail_section(tail_lines)
    } else {
        String::new()
    };
    let tail_reserve = if !tail_block.is_empty() && out.len() + tail_block.len() + 180 <= max_bytes
    {
        tail_block.len()
    } else {
        0
    };

    // 2. Routine Patterns section within remaining budget.
    if !collector.patterns.is_empty() {
        let pattern_budget = max_bytes.saturating_sub(tail_reserve);
        let heading = "Routine patterns:\n";
        if out.len() + heading.len() <= pattern_budget {
            let mut section = String::from(heading);
            let mut shown_patterns = 0usize;
            for (idx, pat) in collector.patterns.iter().enumerate() {
                let remaining_after =
                    collector.patterns.len() - idx - 1 + collector.overflow_patterns;
                let overflow_reserve = if remaining_after > 0 { 36 } else { 0 };
                let line = format_pattern_line(pat);
                if out.len() + section.len() + line.len() + overflow_reserve <= pattern_budget {
                    section.push_str(&line);
                    shown_patterns += 1;
                } else {
                    let hidden =
                        collector.patterns.len() - shown_patterns + collector.overflow_patterns;
                    let note = format!("  … +{hidden} more pattern(s)\n");
                    if out.len() + section.len() + note.len() <= pattern_budget {
                        section.push_str(&note);
                    }
                    break;
                }
            }
            if shown_patterns == collector.patterns.len() && collector.overflow_patterns > 0 {
                let note = format!("  … +{} more pattern(s)\n", collector.overflow_patterns);
                if out.len() + section.len() + note.len() <= pattern_budget {
                    section.push_str(&note);
                }
            }
            if shown_patterns > 0 {
                out.push_str(&section);
            }
        }
    }

    // 3. Recent tail section if budget allows and it still preserves meaningful savings.
    if !tail_block.is_empty() && out.len() + tail_block.len() <= max_bytes {
        let keeps_savings = raw_bytes > max_bytes
            || (out.len() + tail_block.len() + 64 < raw_bytes
                && (out.len() + tail_block.len()) * 100 <= raw_bytes * 85);
        if keeps_savings {
            out.push_str(&tail_block);
        }
    }

    Some(out)
}

fn build_header(collector: &Collector) -> String {
    let mut header = format!("{} log lines", collector.total_lines);
    if !collector.services.is_empty() {
        let n = collector.services.len();
        let noun = if n == 1 { "service" } else { "services" };
        let shown: Vec<&str> = collector
            .services
            .iter()
            .take(MAX_HEADER_SERVICES)
            .map(String::as_str)
            .collect();
        let _ = write!(header, " ({n} {noun}: {}", shown.join(", "));
        if n > MAX_HEADER_SERVICES {
            let _ = write!(header, ", +{} more", n - MAX_HEADER_SERVICES);
        }
        header.push(')');
    }

    let err_groups = collector
        .errors
        .iter()
        .filter(|e| e.severity == Severity::Error)
        .count();
    let err_events: usize = collector
        .errors
        .iter()
        .filter(|e| e.severity == Severity::Error)
        .map(|e| e.count)
        .sum();
    let warn_groups = collector
        .errors
        .iter()
        .filter(|e| e.severity == Severity::Warning)
        .count();
    let warn_events: usize = collector
        .errors
        .iter()
        .filter(|e| e.severity == Severity::Warning)
        .map(|e| e.count)
        .sum();

    if err_groups > 0 {
        if err_events > err_groups {
            let noun = if err_groups == 1 { "error" } else { "errors" };
            let _ = write!(
                header,
                " · {err_groups} distinct {noun} ({err_events} total)"
            );
        } else {
            let noun = if err_groups == 1 { "error" } else { "errors" };
            let _ = write!(header, " · {err_groups} {noun}");
        }
    } else {
        header.push_str(" · 0 errors");
    }

    if warn_groups > 0 {
        if warn_events > warn_groups {
            let noun = if warn_groups == 1 {
                "warning"
            } else {
                "warnings"
            };
            let _ = write!(
                header,
                " · {warn_groups} distinct {noun} ({warn_events} total)"
            );
        } else {
            let noun = if warn_groups == 1 {
                "warning"
            } else {
                "warnings"
            };
            let _ = write!(header, " · {warn_groups} {noun}");
        }
    }

    let pat_count = collector.patterns.len() + collector.overflow_patterns;
    if pat_count > 0 {
        let noun = if pat_count == 1 {
            "routine pattern"
        } else {
            "routine patterns"
        };
        let _ = write!(header, " · {pat_count} {noun}");
    }

    header.push('\n');
    header
}

fn format_error_header_only(entry: &ErrorEntry) -> String {
    let mut line = String::from("  ");
    if let Some(svc) = &entry.service {
        let _ = write!(line, "[{svc}] ");
    }
    line.push_str(&entry.header);
    line.push(' ');
    line.push_str(&entry.first.fmt_bracketed());
    if entry.count > 1 {
        let _ = write!(
            line,
            " [×{}, first {}, last {}]",
            entry.count,
            entry.first.fmt_bare(),
            entry.last.fmt_bare()
        );
    }
    line.push('\n');
    line
}

fn format_error_entry(entry: &ErrorEntry) -> String {
    let mut block = format_error_header_only(entry);
    if entry.frames.is_empty() {
        return block;
    }
    if entry.omitted_frames == 0 {
        for frame in &entry.frames {
            let _ = writeln!(block, "    {}", frame.trim_end());
        }
    } else {
        for frame in &entry.frames[..MAX_FRAMES_HEAD.min(entry.frames.len())] {
            let _ = writeln!(block, "    {}", frame.trim_end());
        }
        let _ = writeln!(
            block,
            "    … +{} frame(s) omitted {}",
            entry.omitted_frames,
            entry.first.fmt_bracketed()
        );
        for frame in &entry.frames[MAX_FRAMES_HEAD.min(entry.frames.len())..] {
            let _ = writeln!(block, "    {}", frame.trim_end());
        }
    }
    block
}

fn format_pattern_line(pat: &PatternEntry) -> String {
    let body = if pat.count == 1 {
        truncate_line(&pat.sample, MAX_TEMPLATE_BYTES)
    } else {
        truncate_line(&pat.template, MAX_TEMPLATE_BYTES)
    };
    let mut line = format!("  ×{} ", pat.count);
    if let Some(svc) = &pat.service {
        let _ = write!(line, "[{svc}] ");
    }
    line.push_str(&body);
    if let Some(span) = pat.time_span() {
        let _ = write!(line, " ({span})");
    }
    let _ = writeln!(line, " {}", pat.pointer());
    line
}

fn build_tail_section(tail_lines: &[TailLine]) -> String {
    let mut out = String::from("Recent:\n");
    for item in tail_lines {
        let clean = truncate_line(&collapse_spaces(item.text.trim()), MAX_TAIL_LINE_BYTES);
        let _ = writeln!(out, "  [{}:{}] {clean}", item.stream, item.line_no);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_compose_interleaved_three_services_folds_and_preserves_all_errors() {
        let mut stdout = String::new();
        let mut line_no = 0usize;
        let mut push = |s: &str| {
            stdout.push_str(s);
            stdout.push('\n');
            line_no += 1;
            line_no
        };

        let mut db_err_first = 0usize;
        let mut db_err_last = 0usize;
        let mut oom_line = 0usize;
        let mut warn_line = 0usize;
        let mut py_err_start = 0usize;
        let mut py_err_end = 0usize;

        for i in 0..110 {
            let sec = i % 60;
            let min = i / 60;
            push(&format!(
                "api-1     | 2026-11-25T10:{min:02}:{sec:02}Z INFO GET /health 200 {:.1}ms req_id=req-{i:04}",
                0.8 + (i % 7) as f64 * 0.2
            ));
            push(&format!(
                "worker-1  | 2026-11-25T10:{min:02}:{sec:02}Z INFO processed job 550e8400-e29b-41d4-a716-44665544{:04x} in {}ms",
                i,
                10 + (i % 35)
            ));
            push(&format!(
                "db-1      | 2026-11-25T10:{min:02}:{sec:02}Z LOG: checkpoint complete: wrote {} buffers (1.2%)",
                100 + i
            ));

            if i == 20 || i == 40 || i == 60 {
                let l = push(&format!(
                    "api-1     | 2026-11-25T10:{min:02}:{sec:02}Z ERROR db connection refused to 10.0.0.5:5432 attempt={}",
                    i / 20
                ));
                if db_err_first == 0 {
                    db_err_first = l;
                }
                db_err_last = l;
            }
            if i == 55 {
                // Interleaved traceback on api-1 with worker-1 INFO line right in the middle!
                py_err_start = push(&format!(
                    "api-1     | 2026-11-25T10:{min:02}:{sec:02}Z ERROR unhandled request error"
                ));
                push("api-1     | Traceback (most recent call last):");
                push(
                    "worker-1  | 2026-11-25T10:00:55Z INFO processed job 550e8400-e29b-41d4-a716-44665544beef in 15ms",
                );
                push("api-1     |   File \"/app/server.py\", line 88, in handle");
                push("api-1     |     return charge_card(order_id)");
                py_err_end = push("api-1     | ValueError: negative total amount");
            }
            if i == 75 {
                warn_line = push(
                    "db-1      | 2026-11-25T10:01:15Z WARN slow query took 4200ms on table orders",
                );
            }
            if i == 90 {
                oom_line = push(
                    "worker-1  | 2026-11-25T10:01:30Z FATAL OutOfMemoryError: Java heap space",
                );
            }
        }

        let raw_len = stdout.len();
        assert!(line_no > 330);
        let out = format(&stdout, "", 8192).expect("compose logs should fold");
        let reduction = 100.0 * (1.0 - out.len() as f64 / raw_len as f64);
        if std::env::var_os("LOGVIEW_BENCH").is_some() {
            println!(
                "BENCH docker_compose ({line_no} lines): {raw_len} B -> {} B ({reduction:.1}% reduction)\n{out}",
                out.len()
            );
        }
        assert!(
            reduction >= 80.0,
            "expected >= 80% reduction, got {reduction:.1}% ({} -> {} bytes)\n{out}",
            raw_len,
            out.len()
        );

        // Header identifies all 3 services and counts.
        assert!(out.contains("3 services: api-1, worker-1, db-1"), "{out}");

        // Errors appear BEFORE routine patterns.
        let err_pos = out.find("Errors & warnings:").expect("errors section");
        let pat_pos = out.find("Routine patterns:").expect("patterns section");
        assert!(err_pos < pat_pos, "{out}");

        // Repeated connection-refused error folded with ×3 and exact first/last pointers.
        assert!(
            out.contains(&format!(
                "[stdout:{db_err_first}] [×3, first stdout:{db_err_first}, last stdout:{db_err_last}]"
            )),
            "{out}"
        );
        // Interleaved traceback stays attached to api-1 with exact line span!
        assert!(
            out.contains(&format!("[stdout:{py_err_start}-{py_err_end}]")),
            "{out}"
        );
        assert!(out.contains("ValueError: negative total amount"), "{out}");
        assert!(out.contains("File \"/app/server.py\", line 88"), "{out}");

        // Distinct OOM fatal error and DB warning preserved verbatim with exact line pointers.
        assert!(
            out.contains(&format!(
                "FATAL OutOfMemoryError: Java heap space [stdout:{oom_line}]"
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "WARN slow query took 4200ms on table orders [stdout:{warn_line}]"
            )),
            "{out}"
        );

        // Routine templates folded across interleaving, preserving HTTP 200 status code.
        assert!(
            out.contains("×110 [api-1] INFO GET /health 200 <dur>"),
            "{out}"
        );
        assert!(
            out.contains("×111 [worker-1] INFO processed job <id> in <dur>"),
            "{out}"
        );
        assert!(
            out.contains("×110 [db-1] LOG: checkpoint complete"),
            "{out}"
        );
    }

    #[test]
    fn kubectl_prefix_multi_pod_folds_health_checks_and_surfaces_oom_crash() {
        let mut stdout = String::new();
        let mut line_no = 0usize;
        let mut crash_line = 0usize;

        for i in 1..=90 {
            let s = i % 60;
            let m = i / 60;
            stdout.push_str(&format!(
                "[pod/api-7f9b/app] 2026-11-25T10:{m:02}:{s:02}Z INFO GET /readyz 200 0.{}ms\n",
                i % 9 + 1
            ));
            line_no += 1;
            stdout.push_str(&format!(
                "[pod/worker-3c2a/worker] 2026-11-25T10:{m:02}:{s:02}Z INFO polled queue depth={} in {}ms\n",
                i % 5,
                2 + (i % 8)
            ));
            line_no += 1;
            if i == 64 {
                stdout.push_str(
                    "[pod/worker-3c2a/worker] 2026-11-25T10:01:04Z FATAL container OOMKilled (exit code 137)\n",
                );
                line_no += 1;
                crash_line = line_no;
            }
        }

        let raw_len = stdout.len();
        let out = format(&stdout, "", 8192).expect("kubectl logs should fold");
        let reduction = 100.0 * (1.0 - out.len() as f64 / raw_len as f64);
        if std::env::var_os("LOGVIEW_BENCH").is_some() {
            println!(
                "BENCH kubectl_prefix ({line_no} lines): {raw_len} B -> {} B ({reduction:.1}% reduction)\n{out}",
                out.len()
            );
        }
        assert!(
            reduction >= 80.0,
            "expected >= 80% reduction, got {reduction:.1}%\n{out}"
        );
        assert!(out.contains("api-7f9b/app"), "{out}");
        assert!(out.contains("worker-3c2a/worker"), "{out}");
        assert!(
            out.contains(&format!(
                "[worker-3c2a/worker] FATAL container OOMKilled (exit code 137) [stdout:{crash_line}]"
            )),
            "{out}"
        );
        assert!(
            out.contains("×90 [api-7f9b/app] INFO GET /readyz 200 <dur>"),
            "{out}"
        );
        assert!(
            out.contains("×90 [worker-3c2a/worker] INFO polled queue depth=<num> in <dur>"),
            "{out}"
        );
    }

    #[test]
    fn json_ndjson_logs_fold_info_and_preserve_all_errors_with_fields() {
        let mut stdout = String::new();
        let mut line_no = 0usize;
        let mut err1_line = 0usize;
        let mut err2_line = 0usize;

        for i in 1..=200 {
            let s = i % 60;
            let m = i / 60;
            stdout.push_str(&format!(
                r#"{{"ts":"2026-11-25T10:{m:02}:{s:02}Z","level":"info","service":"checkout","msg":"request complete","req_id":"req-{i:04}","latency_ms":{:.1},"status":200}}"#,
                8.0 + (i % 15) as f64 * 0.5
            ));
            stdout.push('\n');
            line_no += 1;

            if i == 77 {
                stdout.push_str(
                    r#"{"ts":"2026-11-25T10:01:17Z","level":"error","service":"checkout","msg":"charge failed","err":"gateway timeout after 5000ms","code":"ETIMEDOUT","req_id":"req-0077"}"#,
                );
                stdout.push('\n');
                line_no += 1;
                err1_line = line_no;
            }
            if i == 155 {
                stdout.push_str(
                    r#"{"ts":"2026-11-25T10:02:35Z","level":"fatal","service":"checkout","msg":"database pool exhausted","err":"pq: sorry, too many clients already","stack":"Error: pool exhausted\n  at Pool.acquire (/app/db.js:91:11)\n  at pay (/app/checkout.js:42:5)"}"#,
                );
                stdout.push('\n');
                line_no += 1;
                err2_line = line_no;
            }
        }

        let raw_len = stdout.len();
        let out = format(&stdout, "", 8192).expect("ndjson logs should fold");
        let reduction = 100.0 * (1.0 - out.len() as f64 / raw_len as f64);
        if std::env::var_os("LOGVIEW_BENCH").is_some() {
            println!(
                "BENCH json_ndjson ({line_no} lines): {raw_len} B -> {} B ({reduction:.1}% reduction)\n{out}",
                out.len()
            );
        }
        assert!(
            reduction >= 80.0,
            "expected >= 80% reduction, got {reduction:.1}% ({} -> {} bytes)\n{out}",
            raw_len,
            out.len()
        );

        // Both error JSON objects preserved with their err fields and exact line pointers.
        assert!(
            out.contains(&format!("[stdout:{err1_line}]"))
                && out.contains("charge failed")
                && out.contains("gateway timeout after 5000ms")
                && out.contains("ETIMEDOUT"),
            "{out}"
        );
        assert!(
            out.contains(&format!("[stdout:{err2_line}]"))
                && out.contains("database pool exhausted")
                && out.contains("pq: sorry, too many clients already")
                && out.contains("at Pool.acquire (/app/db.js:91:11)"),
            "{out}"
        );

        // 200 info lines folded into 1 pattern with count, time span, and first-last span.
        assert!(
            out.contains(
                "×200 [checkout] INFO request complete (10:00:01..10:03:20) [stdout:1-202]"
            ),
            "{out}"
        );
    }

    #[test]
    fn journalctl_syslog_folds_repeated_units_and_keeps_failure() {
        let mut stdout = String::new();
        let mut line_no = 0usize;
        let mut fail_line = 0usize;

        for i in 0..80 {
            let s = i % 60;
            let m = i / 60;
            stdout.push_str(&format!(
                "Nov 25 10:{m:02}:{s:02} prod-01 nginx[{}]: 10.0.0.{} GET /api/items/{} 200 {}.1ms\n",
                1400 + (i % 4),
                10 + (i % 20),
                1000 + i,
                1 + (i % 9)
            ));
            line_no += 1;
            stdout.push_str(&format!(
                "Nov 25 10:{m:02}:{s:02} prod-01 systemd[1]: Started Session {} of User deploy.\n",
                500 + i
            ));
            line_no += 1;
            if i == 45 {
                stdout.push_str(
                    "Nov 25 10:00:45 prod-01 kernel: Out of memory: Killed process 9821 (worker) total-vm:4194304kB\n",
                );
                line_no += 1;
                fail_line = line_no;
            }
        }

        let raw_len = stdout.len();
        let out = format_stream("stdout", &stdout, 8192).expect("journalctl should fold");
        let reduction = 100.0 * (1.0 - out.len() as f64 / raw_len as f64);
        if std::env::var_os("LOGVIEW_BENCH").is_some() {
            println!(
                "BENCH journalctl ({line_no} lines): {raw_len} B -> {} B ({reduction:.1}% reduction)\n{out}",
                out.len()
            );
        }
        assert!(
            reduction >= 80.0,
            "expected >= 80% reduction, got {reduction:.1}%\n{out}"
        );
        assert!(
            out.contains(&format!(
                "[kernel] Out of memory: Killed process 9821 (worker) total-vm:4194304kB [stdout:{fail_line}]"
            )),
            "{out}"
        );
        assert!(
            out.contains("×80 [nginx] <ip> GET /api/items/<id> 200 <dur>"),
            "{out}"
        );
        assert!(
            out.contains("×80 [systemd] Started Session <num> of User deploy."),
            "{out}"
        );
    }

    #[test]
    fn short_and_compact_unique_logs_return_none() {
        // Fewer than 8 lines -> None even if repeated.
        let short = "2026-11-25T10:00:01Z INFO ping\n".repeat(7);
        assert_eq!(format(&short, "", 8192), None);

        // Already-compact unique lines under max_bytes -> None (no bloat).
        let unique = (1..=14)
            .map(|i| match i {
                1 => "booting kernel module alpha\n",
                2 => "mounting root filesystem ext4\n",
                3 => "configuring loopback interface\n",
                4 => "loading tls certificates from disk\n",
                5 => "connecting to primary postgres instance\n",
                6 => "applying schema migration create_users\n",
                7 => "warming redis cache entries\n",
                8 => "registering grpc reflection endpoints\n",
                9 => "starting background scheduler thread\n",
                10 => "opening prometheus metrics listener\n",
                11 => "binding http server socket\n",
                12 => "marking readiness probe healthy\n",
                13 => "subscribing to kafka orders topic\n",
                _ => "service initialization complete\n",
            })
            .collect::<String>();
        assert_eq!(format(&unique, "", 8192), None);
    }

    #[test]
    fn redaction_markers_are_never_mangled_or_collapsed() {
        let mut stdout = String::new();
        for i in 1..=30 {
            stdout.push_str(&format!(
                "api-1 | 2026-11-25T10:00:{:02}Z INFO auth ok token=[redacted:github-token:4f1c] key=[REDACTED API_KEY 9876] in {}ms\n",
                i % 60,
                i
            ));
        }
        stdout.push_str(
            "api-1 | 2026-11-25T10:00:31Z ERROR auth rejected header=[REDACTED BEARER 1234] secret=[redacted:secret:1d0d]\n",
        );

        let out = format(&stdout, "", 8192).expect("redacted log should fold");
        assert!(out.contains("[redacted:github-token:4f1c]"), "{out}");
        assert!(out.contains("[REDACTED API_KEY 9876]"), "{out}");
        assert!(out.contains("[REDACTED BEARER 1234]"), "{out}");
        assert!(out.contains("[redacted:secret:1d0d]"), "{out}");
        assert!(!out.contains("[redacted:github-token:<id>]"), "{out}");
        assert!(!out.contains("[REDACTED API_KEY <num>]"), "{out}");
    }

    #[test]
    fn stderr_and_stdout_streams_preserve_exact_stream_pointers() {
        let mut stdout = String::new();
        let mut stderr = String::new();
        for i in 1..=25 {
            stdout.push_str(&format!(
                "2026-11-25T10:00:{i:02}Z INFO tick worker={i} latency={i}.2ms\n"
            ));
        }
        stderr.push_str("2026-11-25T10:00:10Z INFO starting diagnostics\n");
        stderr.push_str("2026-11-25T10:00:12Z ERROR disk full on /var/lib/app\n");

        let out = format(&stdout, &stderr, 8192).expect("dual-stream log should fold");
        assert!(
            out.contains("ERROR disk full on /var/lib/app [stderr:2]"),
            "{out}"
        );
        assert!(
            out.contains(
                "×25 INFO tick worker=<num> latency=<dur> (10:00:01..10:00:25) [stdout:1-25]"
            ),
            "{out}"
        );
    }

    #[test]
    fn respects_tight_max_bytes_without_losing_errors() {
        let mut stdout = String::new();
        for i in 1..=100 {
            stdout.push_str(&format!(
                "api-1 | 2026-11-25T10:00:{:02}Z INFO pattern_{} val={i} took {i}ms\n",
                i % 60,
                i % 25
            ));
        }
        stdout.push_str("api-1 | 2026-11-25T10:01:41Z FATAL critical panic: nil pointer\n");

        let budget = 512;
        let out = format(&stdout, "", budget).expect("should fit in tight budget");
        assert!(out.len() <= budget, "len {} > {budget}\n{out}", out.len());
        assert!(
            out.contains("FATAL critical panic: nil pointer [stdout:101]"),
            "{out}"
        );
    }

    #[test]
    fn pino_numeric_levels_and_compose_json_logs_fold_cleanly() {
        let mut stdout = String::new();
        for i in 1..=40 {
            let ts = 1732528800000u64 + (i as u64) * 1000;
            stdout.push_str(&format!(
                "web-1 | {{\"time\":{ts},\"level\":30,\"msg\":\"handled webhook\",\"id\":\"wh-{i:04}\"}}\n"
            ));
        }
        stdout.push_str(
            "web-1 | {\"time\":1732528850000,\"level\":50,\"msg\":\"webhook signature invalid\",\"err\":\"hmac mismatch\"}\n",
        );

        let out = format(&stdout, "", 8192).expect("pino compose logs should fold");
        assert!(
            out.contains(
                "[web-1] ERROR webhook signature invalid | err=\"hmac mismatch\" [stdout:41]"
            ),
            "{out}"
        );
        assert!(out.contains("×40 [web-1] INFO handled webhook"), "{out}");
    }

    #[test]
    fn http_5xx_in_info_access_log_escalates_and_never_merges_with_200() {
        let mut stdout = String::new();
        for i in 1..=30 {
            stdout.push_str(&format!(
                "api-1 | 2026-11-25T10:00:{i:02}Z INFO GET /api/orders 200 {i}.1ms\n"
            ));
        }
        stdout.push_str("api-1 | 2026-11-25T10:00:31Z INFO POST /api/orders 503 412.0ms\n");

        let out = format(&stdout, "", 8192).expect("access log should fold");
        assert!(
            out.contains("[api-1] INFO POST /api/orders 503 412.0ms [stdout:31]"),
            "{out}"
        );
        assert!(
            out.contains("×30 [api-1] INFO GET /api/orders 200 <dur>"),
            "{out}"
        );
    }

    #[test]
    fn timestamped_k8s_and_journalctl_stack_traces_stay_attached() {
        let mut k8s = String::new();
        for i in 1..=25 {
            k8s.push_str(&format!(
                "[pod/api-7f9b/app] 2026-11-25T10:00:{i:02}Z INFO GET /health 200 0.{i}ms\n"
            ));
        }
        k8s.push_str("[pod/api-7f9b/app] 2026-11-25T10:00:26Z ERROR TypeError: cannot read id\n");
        k8s.push_str(
            "[pod/api-7f9b/app] 2026-11-25T10:00:26Z     at handleOrder (/app/orders.js:55:19)\n",
        );
        k8s.push_str(
            "[pod/api-7f9b/app] 2026-11-25T10:00:26Z     at processTicks (node:internal:95:5)\n",
        );

        let out = format(&k8s, "", 8192).expect("k8s timestamped stack should fold");
        assert!(out.contains("[stdout:26-28]"), "{out}");
        assert!(
            out.contains("at handleOrder (/app/orders.js:55:19)"),
            "{out}"
        );
        assert!(
            out.contains("at processTicks (node:internal:95:5)"),
            "{out}"
        );

        // Distinct Python tracebacks sharing `Traceback (most recent call last):` never merge.
        let mut py = String::new();
        for i in 1..=30 {
            py.push_str(&format!(
                "Nov 25 10:00:{i:02} prod-01 worker[42]: INFO heartbeat ok id={i}\n"
            ));
        }
        py.push_str("Nov 25 10:00:31 prod-01 worker[42]: Traceback (most recent call last):\n");
        py.push_str(
            "Nov 25 10:00:31 prod-01 worker[42]:   File \"/app/a.py\", line 10, in parse\n",
        );
        py.push_str("Nov 25 10:00:31 prod-01 worker[42]: KeyError: 'tenant_id'\n");
        py.push_str("Nov 25 10:00:32 prod-01 worker[42]: Traceback (most recent call last):\n");
        py.push_str(
            "Nov 25 10:00:32 prod-01 worker[42]:   File \"/app/b.py\", line 22, in charge\n",
        );
        py.push_str("Nov 25 10:00:32 prod-01 worker[42]: ZeroDivisionError: division by zero\n");

        let py_out = format(&py, "", 8192).expect("journalctl tracebacks should fold");
        assert!(py_out.contains("KeyError: 'tenant_id'"), "{py_out}");
        assert!(
            py_out.contains("ZeroDivisionError: division by zero"),
            "{py_out}"
        );
        assert!(py_out.contains("[stdout:31-33]"), "{py_out}");
        assert!(py_out.contains("[stdout:34-36]"), "{py_out}");
    }

    #[test]
    fn klog_ansi_and_binary_safety() {
        // ANSI-colored compose logs strip colors cleanly.
        let mut ansi = String::new();
        for i in 1..=25 {
            ansi.push_str(&format!(
                "\x1b[36mapi-1 |\x1b[0m 2026-11-25T10:00:{i:02}Z \x1b[32mINFO\x1b[0m GET /v1.2/ping 200 {i}ms\n"
            ));
        }
        ansi.push_str(
            "\x1b[36mapi-1 |\x1b[0m 2026-11-25T10:00:26Z \x1b[31mERROR\x1b[0m upstream refused\n",
        );
        let out = format(&ansi, "", 8192).expect("ansi compose log should fold");
        assert!(
            out.contains("[api-1] ERROR upstream refused [stdout:26]"),
            "{out}"
        );
        assert!(
            out.contains("×25 [api-1] INFO GET /v1.2/ping 200 <dur>"),
            "{out}"
        );

        // Kubernetes klog/glog format.
        let mut klog = String::new();
        for i in 1..=25 {
            klog.push_str(&format!(
                "I1125 10:00:{i:02}.123456 1 controller.go:88] synced pod default/api-{i:04}\n"
            ));
        }
        klog.push_str(
            "E1125 10:00:26.654321 1 controller.go:104] failed to reconcile pod default/db-0\n",
        );
        let kout = format(&klog, "", 8192).expect("klog should fold");
        assert!(
            kout.contains(
                "ERROR controller.go:104] failed to reconcile pod default/db-0 [stdout:26]"
            ),
            "{kout}"
        );
        assert!(
            kout.contains("×25 INFO controller.go:<num>] synced pod default/api-<id>"),
            "{kout}"
        );

        // Binary data with NUL returns None.
        let bin = "INFO ok\0binary\n".repeat(20);
        assert_eq!(format(&bin, "", 8192), None);
    }

    #[test]
    fn json_zap_pino_zerolog_slog_burst_dedup_pins_errors_warns_verbatim() {
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut line_no = 0usize;

        // 1) Zap float-second timestamps with repetitive info/debug bursts + 1 caller-annotated error.
        for i in 1..=60 {
            let ts = 1712001600.0 + (i as f64) * 1.25;
            stdout.push_str(&format!(
                r#"{{"level":"info","ts":{ts:.6},"service":"billing","caller":"billing/worker.go:42","msg":"processed invoice inv-{i:04} in {i}ms"}}"#
            ));
            stdout.push('\n');
            line_no += 1;
            stdout.push_str(&format!(
                r#"{{"level":"debug","ts":{ts:.6},"service":"billing","msg":"cache lookup key=user-{i:04} hit"}}"#
            ));
            stdout.push('\n');
            line_no += 1;
        }
        stdout.push_str(
            r#"{"level":"error","ts":1712001690.500000,"service":"billing","caller":"billing/charge.go:91","msg":"stripe charge declined","error":"card_declined","req_id":"req-9001"}"#,
        );
        stdout.push('\n');
        line_no += 1;
        let zap_err_line = line_no;

        // 2) Pino numeric levels (20=debug, 30=info, 40=warn, 60=fatal) + nested err stack.
        for i in 1..=40 {
            let ts_ms = 1712001700000u64 + (i as u64) * 1000;
            stdout.push_str(&format!(
                r#"{{"time":{ts_ms},"level":30,"service":"gateway","msg":"proxy ok 10.0.0.{} GET /orders/{} 200 {}ms"}}"#,
                (i % 20) + 1,
                1000 + i,
                i + 2
            ));
            stdout.push('\n');
            line_no += 1;
        }
        stdout.push_str(
            r#"{"time":1712001750000,"level":40,"service":"gateway","msg":"slow upstream","route":"/orders","latency_ms":2450}"#,
        );
        stdout.push('\n');
        line_no += 1;
        let pino_warn_line = line_no;

        // Distinct semantic warning with same msg but different route must NOT merge with /orders!
        stdout.push_str(
            r#"{"time":1712001751000,"level":40,"service":"gateway","msg":"slow upstream","route":"/refunds","latency_ms":3100}"#,
        );
        stdout.push('\n');
        line_no += 1;
        let pino_warn2_line = line_no;

        // 3) Kubernetes klog JSON with float ms timestamps + zerolog/slog fatal on stderr.
        let k8s_start = line_no + 1;
        for i in 1..=30 {
            let ts_ms = 1712001800000.25 + (i as f64) * 500.0;
            stdout.push_str(&format!(
                r#"{{"ts":{ts_ms:.3},"caller":"controller/sync.go:88","service":"k8s","msg":"Synced pod default/web-{i:04}","v":0}}"#
            ));
            stdout.push('\n');
            line_no += 1;
        }
        let k8s_end = line_no;
        stderr.push_str(
            r#"{"time":"2026-04-01T12:05:00Z","level":"FATAL","service":"gateway","msg":"tls cert expired","err":{"message":"x509: certificate has expired","stack":"Error: x509\n  at loadTls (/app/tls.js:18:9)"}}"#,
        );
        stderr.push('\n');

        let raw_len = stdout.len() + stderr.len();
        let out = format(&stdout, &stderr, 8192).expect("structured JSON log burst should fold");
        let reduction = 100.0 * (1.0 - out.len() as f64 / raw_len as f64);
        assert!(
            reduction >= 80.0,
            "expected >= 80% reduction, got {reduction:.1}%\n{out}"
        );

        // Info/debug bursts collapse into ×N templates with Zap float-ts time spans.
        assert!(
            out.contains("×60 [billing] INFO processed invoice inv-<id> in <dur> ("),
            "{out}"
        );
        assert!(
            out.contains("×60 [billing] DEBUG cache lookup key=user-<id> hit ("),
            "{out}"
        );
        assert!(
            out.contains("×40 [gateway] INFO proxy ok <ip> GET /orders/<id> 200 <dur> ("),
            "{out}"
        );
        assert!(
            out.contains("×30 [k8s] INFO Synced pod default/web-<id> (")
                && out.contains(&format!("[stdout:{k8s_start}-{k8s_end}]")),
            "{out}"
        );

        // Error, fatal, and BOTH distinct warnings are pinned verbatim with exact line refs!
        assert!(
            out.contains(&format!(
                "[billing] ERROR stripe charge declined | error=card_declined caller=billing/charge.go:91 req_id=req-9001 [stdout:{zap_err_line}]"
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "[gateway] WARN slow upstream | route=/orders latency_ms=2450 [stdout:{pino_warn_line}]"
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "[gateway] WARN slow upstream | route=/refunds latency_ms=3100 [stdout:{pino_warn2_line}]"
            )),
            "{out}"
        );
        assert!(
            out.contains("[gateway] FATAL tls cert expired")
                && out.contains("[stderr:1]")
                && out.contains("at loadTls (/app/tls.js:18:9)"),
            "{out}"
        );
    }

    #[test]
    fn klog_glog_with_timestamps_and_prefixed_pods_folds_info_and_pins_errors_warns() {
        let mut stdout = String::new();
        let mut line_no = 0usize;

        for i in 1..=50 {
            stdout.push_str(&format!(
                "2026-04-01T12:00:{:02}.123456Z I0401 12:00:{:02}.123456 12345 controller.go:88] synced pod default/worker-{i:04} in {i}ms\n",
                i % 60,
                i % 60,
            ));
            line_no += 1;
        }
        stdout.push_str(
            "2026-04-01T12:00:51.000001Z W0401 12:00:51.000001 12345 controller.go:112] retry backoff 4000ms for pod default/db-0\n",
        );
        line_no += 1;
        let warn_line = line_no;

        stdout.push_str(
            "2026-04-01T12:00:52.000002Z E0401 12:00:52.000002 12345 controller.go:140] reconcile failed for pod default/db-0: etcd timeout\n",
        );
        line_no += 1;
        let err_line = line_no;

        let raw_len = stdout.len();
        let out = format(&stdout, "", 8192).expect("timestamped klog should fold");
        let reduction = 100.0 * (1.0 - out.len() as f64 / raw_len as f64);
        assert!(
            reduction >= 80.0,
            "expected >= 80% reduction, got {reduction:.1}%\n{out}"
        );
        assert!(
            out.contains(&format!(
                "ERROR controller.go:140] reconcile failed for pod default/db-0: etcd timeout [stdout:{err_line}]"
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "WARN controller.go:112] retry backoff 4000ms for pod default/db-0 [stdout:{warn_line}]"
            )),
            "{out}"
        );
        assert!(
            out.contains("×50 INFO controller.go:<num>] synced pod default/worker-<id> in <dur>"),
            "{out}"
        );
    }
}
