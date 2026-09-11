//! Decides, for one Messages request, which old tool results the API should clear.
//!
//! Everything here is a pure function of the request, its headers, the stored
//! conversation state, the options and the clock. The request's `system`, `tools`
//! and `messages` are never rewritten: the only change is an added
//! `context_management` edit, spliced into the body byte for byte.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::{Value, json};

pub const BETA: &str = "context-management-2025-06-27";
pub const EDIT: &str = "clear_tool_uses_20250919";
/// Estimated tokens for an image block, which the byte count would overstate.
const IMAGE_TOKENS: u64 = 1600;
/// Estimated tokens for the placeholder the API leaves for a cleared result.
const PLACEHOLDER_TOKENS: u64 = 10;
/// A boundary must clear at least this many tool calls.
pub const MIN_BOUNDARY: usize = 4;

#[derive(Clone, Debug)]
pub struct Options {
    pub keep: usize,
    pub floor: u64,
    pub clear_at_least: u64,
    pub payback: u64,
    /// Seconds of idle time after which the cache counts as expired. `None` follows
    /// the request's cache TTL plus 15 seconds; `Some(0)` turns cold moves off.
    pub cold_after: Option<u64>,
    pub clear: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            keep: 12,
            floor: 100_000,
            clear_at_least: 20_000,
            payback: 12,
            cold_after: None,
            clear: true,
        }
    }
}

/// The top-level members of a JSON object, borrowed from the request body.
pub struct Body<'a> {
    text: &'a str,
    members: Vec<(String, &'a RawValue)>,
}

struct Members<'a>(Vec<(String, &'a RawValue)>);

impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Object;
        impl<'de> Visitor<'de> for Object {
            type Value = Members<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = HashSet::new();
                let mut members = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(de::Error::custom(format!("duplicate key {key}")));
                    }
                    let value: &'de RawValue = map.next_value()?;
                    members.push((key, value));
                }
                Ok(Members(members))
            }
        }
        deserializer.deserialize_map(Object)
    }
}

impl<'a> Body<'a> {
    /// Parses a JSON object, rejecting duplicate top-level keys, whose meaning
    /// would depend on the parser.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let Members(members) = serde_json::from_str(text).ok()?;
        Some(Self { text, members })
    }

    pub fn raw(&self, key: &str) -> Option<&'a RawValue> {
        self.members
            .iter()
            .find_map(|(name, value)| (name == key).then_some(*value))
    }

    pub fn value(&self, key: &str) -> Option<Value> {
        serde_json::from_str(self.raw(key)?.get()).ok()
    }

    fn span(&self, raw: &RawValue) -> (usize, usize) {
        // A borrowed RawValue is a slice of the body, so its offset is exact.
        let start = raw.get().as_ptr() as usize - self.text.as_ptr() as usize;
        (start, start + raw.get().len())
    }

    /// The body with one member's value replaced, or the member appended. Every
    /// other byte is kept.
    pub fn with_member(&self, key: &str, json: &str) -> Vec<u8> {
        let (start, end, insert) = match self.raw(key) {
            Some(raw) => {
                let (start, end) = self.span(raw);
                (start, end, json.to_owned())
            }
            None => {
                let close = self.text.trim_end().len() - 1;
                let comma = if self.members.is_empty() { "" } else { "," };
                let name = serde_json::to_string(key).expect("string key");
                (close, close, format!("{comma}{name}:{json}"))
            }
        };
        let mut out = Vec::with_capacity(self.text.len() + insert.len());
        out.extend_from_slice(&self.text.as_bytes()[..start]);
        out.extend_from_slice(insert.as_bytes());
        out.extend_from_slice(&self.text.as_bytes()[end..]);
        out
    }
}

/// One tool call in the conversation, in order.
pub struct Use {
    pub id: String,
    pub name: String,
    /// Message and block index of the call.
    pub call: (usize, usize),
    /// Message and block index of its result, when the request carries one.
    pub result: Option<(usize, usize)>,
    pub result_bytes: u64,
    pub result_tokens: u64,
}

/// What planning needs to know about a parsed Messages request.
pub struct Request {
    pub model: Option<String>,
    pub messages: Value,
    pub uses: Vec<Use>,
    pub est_prompt: u64,
    pub messages_tokens: u64,
    pub one_hour: bool,
    pub thread: bool,
    pub compaction: bool,
    pub context_management: Option<Value>,
}

impl Request {
    /// Reads the members planning uses. `Err` carries the pass-through reason.
    pub fn read(body: &Body) -> Result<Self, &'static str> {
        let raw_messages = body.raw("messages").ok_or("no-messages")?;
        let messages: Value = serde_json::from_str(raw_messages.get()).map_err(|_| "unparsed")?;
        let list = messages.as_array().ok_or("no-messages")?;
        let mut uses: Vec<Use> = Vec::new();
        let mut index: HashMap<&str, usize> = HashMap::new();
        for (i, message) in list.iter().enumerate() {
            let assistant = message["role"] == "assistant";
            for (j, block) in message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let kind = block["type"].as_str().unwrap_or_default();
                if assistant && matches!(kind, "tool_use" | "server_tool_use" | "mcp_tool_use") {
                    if let Some(id) = block["id"].as_str()
                        && !index.contains_key(id)
                    {
                        index.insert(id, uses.len());
                        uses.push(Use {
                            id: id.to_owned(),
                            name: block["name"].as_str().unwrap_or_default().to_owned(),
                            call: (i, j),
                            result: None,
                            result_bytes: 0,
                            result_tokens: 0,
                        });
                    }
                } else if (kind == "tool_result" || (assistant && kind.ends_with("_tool_result")))
                    && let Some(&k) = block["tool_use_id"].as_str().and_then(|id| index.get(id))
                    && uses[k].result.is_none()
                {
                    let content = &block["content"];
                    let bytes = json_len(content);
                    uses[k].result = Some((i, j));
                    uses[k].result_bytes = bytes;
                    uses[k].result_tokens = estimate(content, bytes);
                }
            }
        }
        let mut one_hour = false;
        let mut est_prompt = 0;
        for key in ["system", "tools"] {
            if let Some(raw) = body.raw(key) {
                est_prompt += (raw.get().len() as u64).div_ceil(4);
                one_hour |= body.value(key).is_some_and(|v| has_one_hour(&v));
            }
        }
        one_hour |= has_one_hour(&messages)
            || body
                .value("cache_control")
                .is_some_and(|v| v["ttl"] == "1h");
        let messages_tokens = estimate(&messages, raw_messages.get().len() as u64);
        Ok(Self {
            model: body
                .value("model")
                .and_then(|v| v.as_str().map(str::to_owned)),
            uses,
            est_prompt: est_prompt + messages_tokens,
            messages_tokens,
            one_hour,
            thread: body.raw("thread").is_some(),
            compaction: body.raw("compaction").is_some(),
            context_management: body.value("context_management"),
            messages,
        })
    }

    pub fn block(&self, (message, block): (usize, usize)) -> &Value {
        &self.messages[message]["content"][block]
    }

    /// The conversation key: the first tool call's ID. It changes after Claude Code
    /// compacts, and forks share it.
    pub fn key(&self) -> Option<&str> {
        self.uses.first().map(|u| u.id.as_str())
    }

    /// Whether the client already manages context with an edit other than
    /// thinking clearing. `None` means the field is malformed.
    pub fn client_edits(&self) -> bool {
        let Some(cm) = &self.context_management else {
            return false;
        };
        let Some(object) = cm.as_object() else {
            return true;
        };
        match object.get("edits") {
            None => false,
            Some(Value::Array(edits)) => edits.iter().any(|edit| {
                !edit["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("clear_thinking_"))
            }),
            Some(_) => true,
        }
    }
}

fn has_one_hour(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (key == "cache_control" && value["ttl"] == "1h") || has_one_hour(value)
        }),
        Value::Array(items) => items.iter().any(has_one_hour),
        _ => false,
    }
}

struct Counter(u64);

impl io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The compact serialized length of a value.
pub fn json_len(value: &Value) -> u64 {
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).expect("serialize to counter");
    counter.0
}

/// Estimated tokens: UTF-8 bytes / 4, except images (1,600 tokens) and thinking
/// blocks ((thinking + signature) / 4), whose encoded bytes say little about it.
pub fn estimate(value: &Value, bytes: u64) -> u64 {
    fn special(value: &Value, removed: &mut u64, added: &mut u64) {
        match value {
            Value::Object(object) => {
                let text = |key: &str| object.get(key).and_then(Value::as_str).map_or(0, str::len);
                match object.get("type").and_then(Value::as_str) {
                    Some("image") => {
                        *removed += json_len(value);
                        *added += IMAGE_TOKENS;
                    }
                    Some("thinking") => {
                        *removed += json_len(value);
                        *added += ((text("thinking") + text("signature")) / 4) as u64;
                    }
                    Some("redacted_thinking") => {
                        *removed += json_len(value);
                        *added += (text("data") / 4) as u64;
                    }
                    _ => object.values().for_each(|v| special(v, removed, added)),
                }
            }
            Value::Array(items) => items.iter().for_each(|v| special(v, removed, added)),
            _ => {}
        }
    }
    let (mut removed, mut added) = (0, 0);
    special(value, &mut removed, &mut added);
    bytes.saturating_sub(removed).div_ceil(4) + added
}

/// Cache reads as a fraction of the input price.
pub fn read_ratio(model: &str) -> f64 {
    let prefixed = |prefixes: &[&str]| prefixes.iter().any(|p| model.starts_with(p));
    if prefixed(&[
        "claude-fable-5-1",
        "claude-fable-5-2",
        "claude-mythos-5-1",
        "claude-mythos-5-2",
    ]) {
        0.025
    } else if prefixed(&["claude-opus-5-5", "claude-sonnet-5-5"]) {
        0.05
    } else {
        0.10
    }
}

/// Cache writes as a fraction of the input price.
pub fn write_ratio(one_hour: bool) -> f64 {
    if one_hour { 2.0 } else { 1.25 }
}

/// The smallest ratio of newly cleared tokens to the messages layer after the move
/// that repays the move's cache rewrite within `requests` warm requests.
pub fn payback_ratio(model: &str, one_hour: bool, requests: u64) -> f64 {
    let (w, r) = (write_ratio(one_hour), read_ratio(model));
    (w - r) / (r * requests as f64)
}

/// The persisted state of one conversation.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Conversation {
    /// Tool-use IDs of successive boundaries, oldest first. Append-only.
    pub boundaries: Vec<String>,
    /// When a request for each model last started, in Unix milliseconds.
    pub last_start: BTreeMap<String, u64>,
    pub client_managed: bool,
    #[serde(default)]
    pub touched: u64,
}

impl Conversation {
    /// The number of leading tool calls that stay cleared: one past the latest
    /// stored boundary still present, so forks and rewinds stay consistent.
    pub fn boundary(&self, uses: &[Use]) -> usize {
        let positions: HashMap<&str, usize> = uses
            .iter()
            .enumerate()
            .map(|(i, u)| (u.id.as_str(), i))
            .collect();
        self.boundaries
            .iter()
            .rev()
            .find_map(|id| positions.get(id.as_str()))
            .map_or(0, |i| i + 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The main conversation, a subagent or a workflow: may move the boundary.
    Mover,
    /// Token counts, compaction, side requests and forks: reuse the boundary.
    ReadOnly,
}

impl Role {
    pub fn of(messages_path: bool, class: Option<&str>, agent_type: Option<&str>) -> Self {
        let mover = messages_path
            && agent_type != Some("fork")
            && class.is_none_or(|c| matches!(c, "main" | "subagent" | "workflow"));
        if mover { Self::Mover } else { Self::ReadOnly }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Mover => "mover",
            Self::ReadOnly => "read-only",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The conversation's cache had expired, so the request rewrites it anyway.
    Cold,
    /// A warm move whose cache rewrite pays back within `--payback` requests.
    Payback,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Payback => "payback",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Move {
    pub kind: Kind,
    /// Tool results newly cleared by this move, their bytes and estimated tokens.
    pub results: u64,
    pub bytes: u64,
    pub tokens: u64,
    /// Estimated tokens of the messages layer after the move, which the request
    /// writes to the cache again.
    pub rewrite: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub before: usize,
    pub boundary: usize,
    pub moved: Option<Move>,
    pub cold: bool,
    /// Newly cleared tokens over the messages layer after the best candidate move.
    pub ratio: Option<f64>,
}

impl Decision {
    /// Why no edit is sent, once the boundary is too small to clear.
    pub fn idle_reason(&self, req: &Request, options: &Options) -> &'static str {
        if req.est_prompt < options.floor {
            "below-floor"
        } else {
            "nothing-to-clear"
        }
    }
}

/// Plans the boundary for a request that passed every gate.
pub fn decide(
    req: &Request,
    model: &str,
    conversation: Option<&Conversation>,
    role: Role,
    options: &Options,
    now_ms: u64,
) -> Decision {
    let before = conversation.map_or(0, |c| c.boundary(&req.uses));
    let ttl = if req.one_hour { 3600 } else { 300 };
    let cold_after = options.cold_after.unwrap_or(ttl + 15);
    let cold = cold_after > 0
        && conversation.is_some_and(|c| {
            c.last_start
                .get(model)
                .is_none_or(|&start| now_ms.saturating_sub(start) > cold_after * 1000)
        });
    let mut decision = Decision {
        before,
        boundary: before,
        moved: None,
        cold,
        ratio: None,
    };
    let count = req.uses.len();
    if role != Role::Mover || count <= options.keep {
        return decision;
    }
    let target = count - options.keep;
    if target <= before || target < MIN_BOUNDARY {
        return decision;
    }
    let newly = &req.uses[before..target];
    let tokens: u64 = newly.iter().map(|u| u.result_tokens).sum();
    let cleared = &req.uses[..target];
    let rewrite = req
        .messages_tokens
        .saturating_sub(cleared.iter().map(|u| u.result_tokens).sum())
        + PLACEHOLDER_TOKENS * cleared.iter().filter(|u| u.result.is_some()).count() as u64;
    let ratio = tokens as f64 / rewrite.max(1) as f64;
    decision.ratio = Some(ratio);
    let pays_back =
        options.payback > 0 && ratio >= payback_ratio(model, req.one_hour, options.payback);
    if req.est_prompt >= options.floor && tokens >= options.clear_at_least && (cold || pays_back) {
        decision.boundary = target;
        decision.moved = Some(Move {
            kind: if cold { Kind::Cold } else { Kind::Payback },
            results: newly.iter().filter(|u| u.result.is_some()).count() as u64,
            bytes: newly.iter().map(|u| u.result_bytes).sum(),
            tokens,
            rewrite,
        });
    }
    decision
}

/// The clearing edit for `uses` calls of which the first `boundary` stay cleared.
/// Trigger and keep grow together as calls are appended, so the cleared set stays
/// put until the next move.
pub fn edit(uses: usize, boundary: usize, clear_at_least: u64) -> Value {
    json!({
        "type": EDIT,
        "trigger": {"type": "tool_uses", "value": uses.saturating_sub(3).max(1)},
        "keep": {"type": "tool_uses", "value": uses - boundary},
        "clear_at_least": {"type": "input_tokens", "value": clear_at_least},
    })
}

/// The body with `edit` appended after the client's own edits.
pub fn splice(body: &Body, existing: Option<&Value>, edit: Value) -> Option<Vec<u8>> {
    let mut cm = existing.cloned().unwrap_or_else(|| json!({}));
    cm.as_object_mut()?
        .entry("edits")
        .or_insert_with(|| json!([]))
        .as_array_mut()?
        .push(edit);
    Some(body.with_member("context_management", &serde_json::to_string(&cm).ok()?))
}

/// Adds the context-management beta unless a value already names it. Existing
/// values are never removed or reordered.
pub fn merge_beta(headers: &mut Vec<(String, Vec<u8>)>) {
    let named = |name: &str| name.eq_ignore_ascii_case("anthropic-beta");
    let present = headers.iter().filter(|(n, _)| named(n)).any(|(_, value)| {
        value
            .split(|&b| b == b',')
            .any(|part| part.trim_ascii() == BETA.as_bytes())
    });
    if present {
        return;
    }
    match headers.iter_mut().rev().find(|(n, _)| named(n)) {
        Some((_, value)) if !value.trim_ascii().is_empty() => {
            value.extend_from_slice(format!(",{BETA}").as_bytes());
        }
        Some((_, value)) => *value = BETA.as_bytes().to_vec(),
        None => headers.push(("anthropic-beta".into(), BETA.as_bytes().to_vec())),
    }
}

/// A stable hash of a call's name and input, with object keys sorted, for counting
/// calls that repeat a cleared one.
pub fn call_hash(name: &str, input: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let sorted: BTreeMap<&String, Value> =
                    object.iter().map(|(k, v)| (k, canonical(v))).collect();
                serde_json::to_value(sorted).unwrap_or(Value::Null)
            }
            Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    serde_json::to_string(&canonical(input))
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(results: &[usize]) -> Value {
        let mut messages = vec![json!({"role":"user","content":"start"})];
        for (i, size) in results.iter().enumerate() {
            messages.push(json!({"role":"assistant","content":[
                {"type":"text","text":"calling"},
                {"type":"tool_use","id":format!("toolu_{i:02}"),"name":"Read","input":{"file_path":format!("src/{i}.rs")}}
            ]}));
            messages.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":format!("toolu_{i:02}"),"content":"x".repeat(*size)}
            ]}));
        }
        json!({"model":"claude-opus-5-5","max_tokens":1024,"system":"be brief","messages":messages})
    }

    fn request(body: &Value) -> Request {
        let bytes = serde_json::to_vec(body).unwrap();
        Request::read(&Body::parse(&bytes).unwrap()).unwrap()
    }

    fn options() -> Options {
        Options {
            keep: 2,
            floor: 1000,
            clear_at_least: 100,
            payback: 12,
            cold_after: Some(60),
            clear: true,
        }
    }

    #[test]
    fn enumerates_client_server_and_mcp_tool_uses() {
        let body = json!({"model":"claude-opus-5-5","messages":[
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"","signature":"sig"},
                {"type":"tool_use","id":"toolu_a","name":"Bash","input":{"command":"ls"}},
                {"type":"server_tool_use","id":"srvtoolu_b","name":"web_search","input":{"query":"q"}},
                {"type":"web_search_tool_result","tool_use_id":"srvtoolu_b","content":[{"type":"web_search_result","url":"u"}]},
                {"type":"mcp_tool_use","id":"mcptoolu_c","name":"lookup","server_name":"s","input":{}},
                {"type":"mcp_tool_result","tool_use_id":"mcptoolu_c","content":[{"type":"text","text":"found"}]}
            ]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_a","content":"file"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"toolu_d","name":"Read","input":{}}]}
        ]});
        let req = request(&body);
        let ids: Vec<_> = req.uses.iter().map(|u| u.id.as_str()).collect();
        assert_eq!(ids, ["toolu_a", "srvtoolu_b", "mcptoolu_c", "toolu_d"]);
        assert_eq!(req.key(), Some("toolu_a"));
        assert_eq!(req.uses[0].result, Some((1, 0)));
        assert_eq!(req.uses[1].result, Some((0, 3)));
        assert_eq!(req.uses[2].result, Some((0, 5)));
        assert_eq!(req.uses[3].result, None);
        assert_eq!(req.uses[0].result_bytes, 6);
    }

    #[test]
    fn estimates_images_and_thinking_separately() {
        let image = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"A".repeat(40_000)}});
        assert_eq!(estimate(&image, json_len(&image)), IMAGE_TOKENS);
        let thinking =
            json!({"type":"thinking","thinking":"a".repeat(40),"signature":"s".repeat(400)});
        assert_eq!(estimate(&thinking, json_len(&thinking)), 110);
        let text = json!("x".repeat(398));
        assert_eq!(estimate(&text, json_len(&text)), 100);
        let both = json!([text, image]);
        assert_eq!(estimate(&both, json_len(&both)), 101 + IMAGE_TOKENS);
    }

    #[test]
    fn payback_ratios_match_the_pricing_table() {
        let close = |a: f64, b: f64| (a - b).abs() < 0.001;
        for (model, five, hour) in [
            ("claude-fable-5-2", 4.083, 6.583),
            ("claude-mythos-5-1-preview", 4.083, 6.583),
            ("claude-opus-5-5", 2.0, 3.25),
            ("claude-sonnet-5-5", 2.0, 3.25),
            ("claude-haiku-5-5", 0.958, 1.583),
            ("claude-opus-4-8", 0.958, 1.583),
        ] {
            assert!(close(payback_ratio(model, false, 12), five), "{model}");
            assert!(close(payback_ratio(model, true, 12), hour), "{model}");
        }
        // As a share of the messages layer before the move: rho / (1 + rho).
        let share = |rho: f64| rho / (1.0 + rho);
        assert!(close(
            share(payback_ratio("claude-opus-5-5", false, 12)),
            2.0 / 3.0
        ));
        assert!((share(payback_ratio("claude-fable-5-2", false, 12)) - 0.80).abs() < 0.005);
        assert!((share(payback_ratio("claude-fable-5-2", true, 12)) - 0.87).abs() < 0.005);
        assert!((share(payback_ratio("claude-sonnet-5-5", true, 12)) - 0.76).abs() < 0.005);
        assert!((share(payback_ratio("claude-haiku-5-5", false, 12)) - 0.49).abs() < 0.005);
        assert!((share(payback_ratio("claude-haiku-5-5", true, 12)) - 0.61).abs() < 0.005);
    }

    #[test]
    fn warm_moves_need_the_payback_share_and_cold_moves_do_not() {
        let opts = options();
        let warm = Conversation {
            last_start: [("claude-opus-5-5".to_string(), 1_000_000)].into(),
            ..Default::default()
        };
        let rho = payback_ratio("claude-opus-5-5", false, 12);
        // Eight results of 4,000 bytes with keep 2: six cleared against two kept.
        let req = request(&conversation(&[4000; 8]));
        let decision = decide(
            &req,
            "claude-opus-5-5",
            Some(&warm),
            Role::Mover,
            &opts,
            1_010_000,
        );
        assert!(!decision.cold);
        assert!(decision.ratio.unwrap() >= rho, "{decision:?}");
        assert_eq!(decision.boundary, 6);
        assert_eq!(decision.moved.as_ref().unwrap().kind, Kind::Payback);
        // Seven results of 2,000 bytes with keep 3: warm payback fails, but an
        // expired cache moves anyway.
        let req = request(&conversation(&[2000; 7]));
        let opts = Options {
            keep: 3,
            ..options()
        };
        let warm_decision = decide(
            &req,
            "claude-opus-5-5",
            Some(&warm),
            Role::Mover,
            &opts,
            1_010_000,
        );
        assert!(warm_decision.ratio.unwrap() < rho, "{warm_decision:?}");
        assert_eq!((warm_decision.boundary, warm_decision.moved), (0, None));
        let cold = decide(
            &req,
            "claude-opus-5-5",
            Some(&warm),
            Role::Mover,
            &opts,
            1_070_000,
        );
        assert!(cold.cold);
        assert_eq!(cold.boundary, 4);
        let moved = cold.moved.unwrap();
        assert_eq!(moved.kind, Kind::Cold);
        assert_eq!(
            (moved.results, moved.bytes, moved.tokens),
            (4, 4 * 2002, 4 * 501)
        );
        assert_eq!(
            moved.rewrite,
            req.messages_tokens - 4 * 501 + 4 * PLACEHOLDER_TOKENS
        );
        // --payback 0 turns warm moves off; --cold-after 0 turns cold moves off.
        let req = request(&conversation(&[4000; 8]));
        let opts = Options {
            payback: 0,
            ..options()
        };
        let none = decide(
            &req,
            "claude-opus-5-5",
            Some(&warm),
            Role::Mover,
            &opts,
            1_010_000,
        );
        assert_eq!(none.moved, None);
        let opts = Options {
            payback: 0,
            cold_after: Some(0),
            ..options()
        };
        let late = decide(
            &req,
            "claude-opus-5-5",
            Some(&warm),
            Role::Mover,
            &opts,
            9_000_000,
        );
        assert!(!late.cold);
        assert_eq!(late.moved, None);
    }

    #[test]
    fn floors_minimums_and_unknown_conversations() {
        let req = request(&conversation(&[2000; 6]));
        let mut opts = options();
        opts.payback = 1000;
        // An unknown conversation counts as warm.
        let first = decide(&req, "claude-haiku-4-5", None, Role::Mover, &opts, 5);
        assert!(!first.cold);
        assert_eq!(first.boundary, 4);
        opts.floor = req.est_prompt + 1;
        let below = decide(&req, "claude-haiku-4-5", None, Role::Mover, &opts, 5);
        assert_eq!(below.boundary, 0);
        assert_eq!(below.idle_reason(&req, &opts), "below-floor");
        opts.floor = 1000;
        opts.clear_at_least = 2005;
        assert_eq!(
            decide(&req, "claude-haiku-4-5", None, Role::Mover, &opts, 5).boundary,
            0
        );
        // Fewer than four calls to clear never moves.
        opts.clear_at_least = 1;
        opts.keep = 3;
        let small = decide(&req, "claude-haiku-4-5", None, Role::Mover, &opts, 5);
        assert_eq!(small.boundary, 0);
        assert_eq!(small.idle_reason(&req, &opts), "nothing-to-clear");
    }

    #[test]
    fn boundaries_never_move_back_and_survive_rewinds() {
        let req = request(&conversation(&[2000; 6]));
        let mut state = Conversation {
            boundaries: vec!["toolu_03".into(), "toolu_05".into()],
            last_start: [("claude-haiku-4-5".to_string(), 0)].into(),
            ..Default::default()
        };
        assert_eq!(state.boundary(&req.uses), 6);
        let opts = Options {
            payback: 1000,
            ..options()
        };
        let decision = decide(
            &req,
            "claude-haiku-4-5",
            Some(&state),
            Role::Mover,
            &opts,
            1,
        );
        assert_eq!(
            (decision.before, decision.boundary, decision.moved),
            (6, 6, None)
        );
        // A rewind drops toolu_05, so the latest boundary still present applies.
        let rewound = request(&conversation(&[2000; 5]));
        assert_eq!(state.boundary(&rewound.uses), 4);
        state.boundaries = vec!["toolu_gone".into()];
        assert_eq!(state.boundary(&rewound.uses), 0);
    }

    #[test]
    fn model_changes_count_as_cold_and_read_only_roles_never_move() {
        let req = request(&conversation(&[2000; 6]));
        let opts = Options {
            payback: 0,
            ..options()
        };
        let state = Conversation {
            last_start: [("claude-opus-5-5".to_string(), 1_000)].into(),
            ..Default::default()
        };
        let switched = decide(
            &req,
            "claude-sonnet-5-5",
            Some(&state),
            Role::Mover,
            &opts,
            2_000,
        );
        assert!(switched.cold);
        assert_eq!(switched.boundary, 4);
        let read_only = decide(
            &req,
            "claude-sonnet-5-5",
            Some(&state),
            Role::ReadOnly,
            &opts,
            2_000,
        );
        assert_eq!(read_only.boundary, 0);
        assert_eq!(Role::of(true, None, None), Role::Mover);
        assert_eq!(Role::of(true, Some("subagent"), None), Role::Mover);
        assert_eq!(
            Role::of(true, Some("workflow"), Some("general")),
            Role::Mover
        );
        assert_eq!(Role::of(true, Some("main"), Some("fork")), Role::ReadOnly);
        assert_eq!(Role::of(true, Some("compaction"), None), Role::ReadOnly);
        assert_eq!(Role::of(true, Some("auxiliary"), None), Role::ReadOnly);
        assert_eq!(Role::of(false, None, None), Role::ReadOnly);
    }

    #[test]
    fn edit_values_track_the_boundary() {
        let edit_64 = edit(64, 52, 20_000);
        assert_eq!(edit_64["trigger"], json!({"type":"tool_uses","value":61}));
        assert_eq!(edit_64["keep"], json!({"type":"tool_uses","value":12}));
        assert_eq!(
            edit_64["clear_at_least"],
            json!({"type":"input_tokens","value":20000})
        );
        let edit_66 = edit(66, 52, 20_000);
        assert_eq!(edit_66["trigger"]["value"], 63);
        assert_eq!(edit_66["keep"]["value"], 14);
        let keys: Vec<_> = edit_66.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["type", "trigger", "keep", "clear_at_least"]);
    }

    #[test]
    fn splices_without_touching_other_members() {
        let compact = br#"{"model":"claude-opus-5-5","system":[{"type":"text","text":"caf\u00e9 \"q\""}],"tools":[],"messages":[{"role":"user","content":"1.50e3"}],"max_tokens":12345678901234567890}"#;
        let pretty = b"{\n  \"model\": \"claude-opus-5-5\",\n  \"system\": \"s\\u0041\",\n  \"messages\": [ {\"role\": \"user\", \"content\": \"hi\"} ],\n  \"context_management\": { \"edits\": [ {\"type\": \"clear_thinking_20251015\", \"keep\": \"all\"} ] }\n}\n";
        for input in [compact.as_slice(), pretty.as_slice()] {
            let body = Body::parse(input).unwrap();
            let existing = body.value("context_management");
            let out = splice(&body, existing.as_ref(), edit(10, 6, 100)).unwrap();
            let spliced = Body::parse(&out).unwrap();
            for key in ["model", "system", "tools", "messages", "max_tokens"] {
                assert_eq!(
                    body.raw(key).map(|r| r.get()),
                    spliced.raw(key).map(|r| r.get()),
                    "{key}"
                );
            }
            let cm = spliced.value("context_management").unwrap();
            let edits = cm["edits"].as_array().unwrap();
            assert_eq!(edits.last().unwrap()["type"], EDIT);
            if existing.is_some() {
                assert_eq!(edits[0]["type"], "clear_thinking_20251015");
                assert_eq!(edits.len(), 2);
            } else {
                assert_eq!(edits.len(), 1);
            }
            let original = String::from_utf8_lossy(input);
            let spliced_text = String::from_utf8_lossy(&out);
            let span = body.raw("messages").unwrap().get();
            assert!(original.contains(span) && spliced_text.contains(span));
        }
        assert!(Body::parse(br#"{"a":1,"a":2}"#).is_none());
        assert!(Body::parse(b"[1]").is_none());
        assert!(Body::parse(b"{\"a\":1} x").is_none());
        assert!(Body::parse(b"\xff").is_none());
    }

    #[test]
    fn merges_the_beta_once() {
        let mut none = vec![("x-api-key".to_string(), b"k".to_vec())];
        merge_beta(&mut none);
        assert_eq!(none[1], ("anthropic-beta".into(), BETA.as_bytes().to_vec()));
        let mut other = vec![("Anthropic-Beta".to_string(), b"oauth-2025-04-20".to_vec())];
        merge_beta(&mut other);
        assert_eq!(other[0].1, format!("oauth-2025-04-20,{BETA}").as_bytes());
        let mut present = vec![(
            "anthropic-beta".to_string(),
            format!("a, {BETA} ,b").into_bytes(),
        )];
        let before = present.clone();
        merge_beta(&mut present);
        assert_eq!(present, before);
        let mut two = vec![
            ("anthropic-beta".to_string(), b"a".to_vec()),
            ("anthropic-beta".to_string(), b"b ,  c".to_vec()),
        ];
        merge_beta(&mut two);
        assert_eq!(two[0].1, b"a");
        assert_eq!(two[1].1, format!("b ,  c,{BETA}").as_bytes());
        let mut split = vec![
            ("anthropic-beta".to_string(), BETA.as_bytes().to_vec()),
            ("anthropic-beta".to_string(), b"b".to_vec()),
        ];
        merge_beta(&mut split);
        assert_eq!(split[1].1, b"b");
    }

    #[test]
    fn detects_client_managed_context_and_one_hour_caches() {
        let mut body = conversation(&[10]);
        assert!(!request(&body).client_edits());
        body["context_management"] =
            json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]});
        assert!(!request(&body).client_edits());
        for kind in [
            "clear_tool_uses_20250919",
            "compact_20260112",
            "something_new",
        ] {
            body["context_management"] = json!({"edits":[{"type":kind}]});
            assert!(request(&body).client_edits(), "{kind}");
        }
        body["context_management"] = json!("bad");
        assert!(request(&body).client_edits());
        assert!(!request(&body).one_hour);
        body["system"] =
            json!([{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"1h"}}]);
        assert!(request(&body).one_hour);
    }

    #[test]
    fn call_hashes_ignore_key_order() {
        let a = json!({"command":"ls","timeout":5});
        let b: Value = serde_json::from_str(r#"{"timeout":5,"command":"ls"}"#).unwrap();
        assert_eq!(call_hash("Bash", &a), call_hash("Bash", &b));
        assert_ne!(call_hash("Bash", &a), call_hash("Read", &a));
    }
}
