//! Reads usage and applied edits from a response while it streams to the client.
//! Server-sent events are parsed incrementally with bounded memory; content deltas
//! are skipped without being kept.

use serde_json::Value;

use super::plan::EDIT;

/// The longest line or JSON body kept for parsing.
const MAX_LINE: usize = 1024 * 1024;
const MAX_JSON: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observed {
    pub model: Option<String>,
    pub input: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub output: Option<u64>,
    /// The API's report for the tool-result clearing edit, when it applied one.
    pub cleared_uses: Option<u64>,
    pub cleared_tokens: Option<u64>,
}

impl Observed {
    fn usage(&mut self, usage: &Value) {
        let field = |key: &str| usage.get(key).and_then(Value::as_u64);
        self.input = field("input_tokens").or(self.input);
        self.cache_read = field("cache_read_input_tokens").or(self.cache_read);
        self.cache_write = field("cache_creation_input_tokens").or(self.cache_write);
        self.output = field("output_tokens").or(self.output);
    }

    fn context_management(&mut self, value: &Value) {
        for edit in value["applied_edits"].as_array().into_iter().flatten() {
            if edit["type"] == EDIT {
                self.cleared_uses = Some(
                    self.cleared_uses.unwrap_or(0)
                        + edit["cleared_tool_uses"].as_u64().unwrap_or(0),
                );
                self.cleared_tokens = Some(
                    self.cleared_tokens.unwrap_or(0)
                        + edit["cleared_input_tokens"].as_u64().unwrap_or(0),
                );
            }
        }
    }

    fn message(&mut self, message: &Value) {
        if let Some(model) = message["model"].as_str() {
            self.model = Some(model.to_owned());
        }
        self.usage(&message["usage"]);
        self.context_management(&message["context_management"]);
    }
}

pub enum Observer {
    Events(Events),
    Json(Vec<u8>, bool),
    /// A compressed response, relayed untouched and counted as unobserved.
    Encoded,
    Ignored,
}

impl Observer {
    pub fn new(content_type: Option<&[u8]>, content_encoding: Option<&[u8]>) -> Self {
        if content_encoding.is_some_and(|e| {
            let trimmed = e.trim_ascii();
            !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case(b"identity")
        }) {
            return Self::Encoded;
        }
        let kind = content_type
            .unwrap_or_default()
            .trim_ascii()
            .to_ascii_lowercase();
        if kind.starts_with(b"text/event-stream") {
            Self::Events(Events::default())
        } else if kind.starts_with(b"application/json") {
            Self::Json(Vec::new(), false)
        } else {
            Self::Ignored
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        match self {
            Self::Events(events) => events.feed(bytes),
            Self::Json(body, overflow) => {
                if body.len() + bytes.len() > MAX_JSON {
                    *overflow = true;
                } else {
                    body.extend_from_slice(bytes);
                }
            }
            Self::Encoded | Self::Ignored => {}
        }
    }

    /// `None` when the response could not be observed.
    pub fn finish(self) -> Option<Observed> {
        match self {
            Self::Events(mut events) => {
                events.finish();
                Some(events.observed)
            }
            Self::Json(body, false) => {
                let mut observed = Observed::default();
                if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                    observed.message(&value);
                }
                Some(observed)
            }
            Self::Json(_, true) | Self::Encoded => None,
            Self::Ignored => Some(Observed::default()),
        }
    }
}

#[derive(Default)]
pub struct Events {
    line: Vec<u8>,
    overflow: bool,
    event: Option<String>,
    data: Vec<u8>,
    pub observed: Observed,
}

impl Events {
    pub fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match bytes.iter().position(|&b| b == b'\n') {
                Some(end) => {
                    self.push(&bytes[..end]);
                    let line = std::mem::take(&mut self.line);
                    if !std::mem::take(&mut self.overflow) {
                        self.line_done(&line);
                    }
                    bytes = &bytes[end + 1..];
                }
                None => {
                    self.push(bytes);
                    break;
                }
            }
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.overflow || self.line.len() + bytes.len() > MAX_LINE {
            // Content deltas can be long; nothing in them is needed.
            self.overflow = true;
            self.line.clear();
        } else {
            self.line.extend_from_slice(bytes);
        }
    }

    fn wanted(&self) -> bool {
        self.event
            .as_deref()
            .is_none_or(|e| matches!(e, "message_start" | "message_delta"))
    }

    fn line_done(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            self.dispatch();
            return;
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let value = &line[colon + 1..];
                (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &b""[..]),
        };
        match field {
            b"event" => self.event = Some(String::from_utf8_lossy(value).into_owned()),
            b"data" if self.wanted() => {
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                if self.data.len() + value.len() <= MAX_LINE {
                    self.data.extend_from_slice(value);
                }
            }
            _ => {}
        }
    }

    /// Flushes any trailing line/event at EOF when a stream ended without a final
    /// blank line separator.
    pub fn finish(&mut self) {
        if !self.line.is_empty() && !std::mem::take(&mut self.overflow) {
            let line = std::mem::take(&mut self.line);
            self.line_done(&line);
        }
        self.dispatch();
    }

    fn dispatch(&mut self) {
        let event = self.event.take();
        let data = std::mem::take(&mut self.data);
        if data.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&data) else {
            return;
        };
        let kind = event
            .as_deref()
            .or_else(|| value["type"].as_str())
            .unwrap_or_default();
        match kind {
            "message_start" => self.observed.message(&value["message"]),
            "message_delta" => {
                self.observed.usage(&value["usage"]);
                self.observed
                    .context_management(&value["context_management"]);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":2310,\"cache_read_input_tokens\":15420,\"cache_creation_input_tokens\":81233,\"output_tokens\":1}}}\n\n",
        "event: ping\ndata: {\"type\": \"ping\"}\n\n",
        "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\r\n\r\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":812},",
        "\"context_management\":{\"applied_edits\":[{\"type\":\"clear_thinking_20251015\",\"cleared_thinking_turns\":3,\"cleared_input_tokens\":15000},",
        "{\"type\":\"clear_tool_uses_20250919\",\"cleared_tool_uses\":52,\"cleared_input_tokens\":171402}]}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    fn expected() -> Observed {
        Observed {
            model: Some("claude-opus-5-5".into()),
            input: Some(2310),
            cache_read: Some(15420),
            cache_write: Some(81233),
            output: Some(812),
            cleared_uses: Some(52),
            cleared_tokens: Some(171_402),
        }
    }

    #[test]
    fn reads_usage_and_edits_from_events_split_anywhere() {
        for size in [1, 2, 7, 64, STREAM.len()] {
            let mut observer = Observer::new(Some(b"text/event-stream; charset=utf-8"), None);
            for piece in STREAM.as_bytes().chunks(size) {
                observer.feed(piece);
            }
            assert_eq!(observer.finish(), Some(expected()), "chunk size {size}");
        }
    }

    #[test]
    fn reads_json_responses_and_skips_compressed_ones() {
        let body = serde_json::json!({"model":"claude-opus-5-5","usage":{"input_tokens":2310,"cache_read_input_tokens":15420,"cache_creation_input_tokens":81233,"output_tokens":812},
            "context_management":{"applied_edits":[{"type":"clear_tool_uses_20250919","cleared_tool_uses":52,"cleared_input_tokens":171402}]}});
        let mut observer = Observer::new(Some(b"application/json"), Some(b"identity"));
        observer.feed(&serde_json::to_vec(&body).unwrap());
        assert_eq!(observer.finish(), Some(expected()));
        let mut gzip = Observer::new(Some(b"application/json"), Some(b"gzip"));
        gzip.feed(b"\x1f\x8b");
        assert_eq!(gzip.finish(), None);
    }

    #[test]
    fn long_delta_lines_are_dropped_without_losing_later_events() {
        let mut events = Events::default();
        events.feed(b"event: content_block_delta\ndata: ");
        events.feed(&vec![b'x'; MAX_LINE + 10]);
        events.feed(b"\n\nevent: message_delta\ndata: {\"usage\":{\"output_tokens\":5}}\n\n");
        assert_eq!(events.observed.output, Some(5));
        assert!(events.line.capacity() <= MAX_LINE * 2);
    }

    #[test]
    fn flushes_trailing_event_at_eof_and_handles_empty_encoding() {
        let mut observer = Observer::new(Some(b" text/event-stream; charset=utf-8 "), Some(b" "));
        observer.feed(
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42},\"context_management\":{\"applied_edits\":[{\"type\":\"clear_tool_uses_20250919\",\"cleared_tool_uses\":3,\"cleared_input_tokens\":900}]}}",
        );
        let observed = observer.finish().expect("observed");
        assert_eq!(observed.output, Some(42));
        assert_eq!(observed.cleared_uses, Some(3));
        assert_eq!(observed.cleared_tokens, Some(900));
    }
}
