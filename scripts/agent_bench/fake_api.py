"""A local fake Messages API for offline validation.

It serves the benchmark harness (non-streaming JSON) and real Claude Code (SSE).
Responses come from scripted agents registered per conversation. The server

- returns deterministic usage from a simulated prefix cache with a 5-minute TTL,
- HMAC-signs every thinking block over the request prefix that produced it and
  rejects a replay whose prefix changed with the API's 400 text,
- simulates server-side `clear_tool_uses_20250919` and reports `applied_edits`,
- serves `count_tokens`, and
- records every request with credentials removed.

Nothing here talks to the network beyond 127.0.0.1.
"""

import base64
import copy
import hashlib
import hmac
import http.server
import json
import math
import re
import threading
import time

HMAC_KEY = b"agent-bench-fake-thinking-v1"
BYTES_PER_TOKEN = 3.5
MIN_CACHEABLE = 512
CLEARED = "[cleared by context management]"
SESSION_RE = re.compile(r"Session: ([0-9a-f]{16})")
SECRET_HEADERS = {"x-api-key", "authorization", "cookie", "proxy-authorization"}


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def strip_cache_control(value):
    if isinstance(value, dict):
        return {k: strip_cache_control(v) for k, v in value.items() if k != "cache_control"}
    if isinstance(value, list):
        return [strip_cache_control(v) for v in value]
    return value


def tokens_of(text):
    data = text.encode("utf-8") if isinstance(text, str) else text
    return max(1, int(math.ceil(len(data) / BYTES_PER_TOKEN)))


def thinking_signature(model, system, tools, messages):
    prefix = canonical([model, strip_cache_control(system), strip_cache_control(tools),
                        strip_cache_control(messages)])
    digest = hmac.new(HMAC_KEY, prefix.encode("utf-8"), hashlib.sha256).digest()
    return base64.b64encode(b"fake1" + digest).decode()


def binding_error(body):
    """Return the 400 message for the first thinking block whose prefix changed, or None."""
    messages = body.get("messages") or []
    for i, message in enumerate(messages):
        if message.get("role") != "assistant" or not isinstance(message.get("content"), list):
            continue
        for j, block in enumerate(message["content"]):
            if block.get("type") != "thinking":
                continue
            expected = thinking_signature(body.get("model"), body.get("system"), body.get("tools"),
                                          messages[:i])
            if block.get("signature") != expected:
                return ("messages.%d.content.%d: Invalid `signature` in `thinking` block. The block is "
                        "bound to a different conversation. Remove the block, or set "
                        "`thinking.block_binding.prefix_mismatch_behavior` to \"drop_block\"." % (i, j))
    return None


def segments(body):
    """Flatten a request into cacheable segments in render order: tools, system, messages."""
    out = []
    auto = isinstance(body.get("cache_control"), dict)
    for tool in body.get("tools") or []:
        out.append([canonical(strip_cache_control(tool)), tool.get("cache_control"), "tools"])
    system = body.get("system")
    if isinstance(system, str):
        out.append([canonical(system), None, "system"])
    else:
        for block in system or []:
            out.append([canonical(strip_cache_control(block)), block.get("cache_control"), "system"])
    for message in body.get("messages") or []:
        content = message.get("content")
        if isinstance(content, str):
            out.append([canonical([message.get("role"), content]), None, "messages"])
            continue
        for block in content or []:
            out.append([canonical([message.get("role"), strip_cache_control(block)]),
                        block.get("cache_control"), "messages"])
    if auto and out:
        out[-1][1] = out[-1][1] or body["cache_control"]
    return out


def apply_context_edits(body):
    """Simulate clear_tool_uses_20250919 on a copy. Returns (body, applied_edits)."""
    management = body.get("context_management") or {}
    applied = []
    for edit in management.get("edits") or []:
        if edit.get("type") != "clear_tool_uses_20250919":
            continue
        trigger = (edit.get("trigger") or {}).get("value", 100000)
        keep = (edit.get("keep") or {}).get("value", 3)
        at_least = (edit.get("clear_at_least") or {}).get("value", 0)
        total = sum(tokens_of(s[0]) for s in segments(body))
        if total <= trigger:
            continue
        results = []
        for mi, message in enumerate(body.get("messages") or []):
            if message.get("role") != "user" or not isinstance(message.get("content"), list):
                continue
            for bi, block in enumerate(message["content"]):
                if block.get("type") == "tool_result":
                    results.append((mi, bi))
        eligible = results[:-keep] if keep else results
        if not eligible:
            continue
        edited = copy.deepcopy(body)
        cleared_tokens = 0
        count = 0
        for mi, bi in eligible:
            block = edited["messages"][mi]["content"][bi]
            if block.get("content") == CLEARED:
                continue
            before = tokens_of(canonical(block.get("content")))
            block["content"] = CLEARED
            cleared_tokens += before - tokens_of(canonical(CLEARED))
            count += 1
        if count == 0 or cleared_tokens < at_least:
            continue
        body = edited
        applied.append({"type": "clear_tool_uses_20250919", "cleared_tool_uses": count,
                        "cleared_input_tokens": cleared_tokens})
    return body, applied


class PromptCache:
    """Prefix cache keyed by cumulative segment hashes at breakpoints, with TTLs."""

    def __init__(self, clock):
        self.clock = clock
        self.entries = {}
        self.lock = threading.Lock()

    def usage(self, body):
        segs = segments(body)
        hashes = []
        running = hashlib.sha256()
        cumulative = []
        total = 0
        for text, _, _ in segs:
            running.update(text.encode("utf-8"))
            hashes.append(running.copy().hexdigest())
            total += tokens_of(text)
            cumulative.append(total)
        now = self.clock()
        with self.lock:
            read_upto = -1
            for i in range(len(segs) - 1, -1, -1):
                expiry = self.entries.get(hashes[i])
                if expiry and expiry[0] > now:
                    read_upto = i
                    break
            breakpoints = [i for i, s in enumerate(segs) if s[1] and cumulative[i] >= MIN_CACHEABLE]
            last_bp = max(breakpoints) if breakpoints else -1
            read = cumulative[read_upto] if read_upto >= 0 else 0
            write_5m = write_1h = 0
            if last_bp > read_upto:
                written = cumulative[last_bp] - read
                ttl = (segs[last_bp][1] or {}).get("ttl", "5m")
                if ttl == "1h":
                    write_1h = written
                else:
                    write_5m = written
            uncached = total - max(read, cumulative[last_bp] if last_bp >= 0 else 0)
            for i in breakpoints:
                ttl = (segs[i][1] or {}).get("ttl", "5m")
                seconds = 3600 if ttl == "1h" else 300
                self.entries[hashes[i]] = (now + seconds, ttl)
            if read_upto >= 0:
                ttl = self.entries[hashes[read_upto]][1]
                self.entries[hashes[read_upto]] = (now + (3600 if ttl == "1h" else 300), ttl)
        return {
            "input_tokens": max(0, uncached),
            "cache_creation_input_tokens": write_5m + write_1h,
            "cache_read_input_tokens": read,
            "cache_creation": {"ephemeral_5m_input_tokens": write_5m, "ephemeral_1h_input_tokens": write_1h},
        }


def fill(value, context):
    """Substitute {name} placeholders in every string of a scripted step."""
    if isinstance(value, str):
        out = value
        for key, replacement in context.items():
            out = out.replace("{" + key + "}", str(replacement))
        return out
    if isinstance(value, list):
        return [fill(v, context) for v in value]
    if isinstance(value, dict):
        return {k: fill(v, context) for k, v in value.items()}
    return value


def last_tool_result(body):
    """The text of the last tool result in a request, or ''."""
    for message in reversed(body.get("messages") or []):
        if message.get("role") != "user" or not isinstance(message.get("content"), list):
            continue
        for block in reversed(message["content"]):
            if block.get("type") == "tool_result":
                content = block.get("content")
                if isinstance(content, list):
                    return "".join(b.get("text", "") for b in content if isinstance(b, dict))
                return content or ""
    return ""


class Script:
    """A scripted agent. `steps` is a list of step dicts or a callable(body, state) -> step."""

    def __init__(self, steps, context=None):
        self.steps = steps
        self.context = context or {}
        self.index = 0
        self.state = {}

    def next(self, body):
        if callable(self.steps):
            step = self.steps(body, self.state)
        elif self.index < len(self.steps):
            step = self.steps[self.index]
            # A step taken only when the previous tool result was saved to a file (Claude Code's 2 KB preview).
            while "when_persisted" in step:
                if "<persisted-output>" in last_tool_result(body):
                    step = step["when_persisted"]
                    break
                self.index += 1
                step = self.steps[self.index]
        else:
            step = {"content": [{"type": "text", "text": "Done."}], "stop_reason": "end_turn"}
        self.index += 1
        return fill(copy.deepcopy(step), self.context)


class FakeAPI:
    def __init__(self, port=0, log_path=None):
        self.offset = 0.0
        self.cache = PromptCache(self.now)
        self.scripts = {}
        self.requests = []
        self.count_token_calls = 0
        self.log_path = log_path
        self.lock = threading.Lock()
        handler = self._handler()
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", port), handler)
        self.httpd.daemon_threads = True
        self.port = self.httpd.server_address[1]
        self.thread = None

    @property
    def base_url(self):
        return "http://127.0.0.1:%d" % self.port

    def now(self):
        return time.time() + self.offset

    def advance(self, seconds):
        self.offset += seconds

    def register(self, key, script):
        with self.lock:
            self.scripts[key] = script

    def start(self):
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()
        return self

    def stop(self):
        self.httpd.shutdown()
        self.httpd.server_close()

    def _record(self, entry):
        with self.lock:
            self.requests.append(entry)
            if self.log_path:
                with open(self.log_path, "a") as handle:
                    handle.write(json.dumps(entry) + "\n")

    def script_for(self, body):
        text = ""
        system = body.get("system")
        if isinstance(system, str):
            text = system
        elif isinstance(system, list):
            text = "\n".join(b.get("text", "") for b in system if isinstance(b, dict))
        match = SESSION_RE.search(text)
        with self.lock:
            if match and match.group(1) in self.scripts:
                return self.scripts[match.group(1)]
            return self.scripts.get("default")

    def respond(self, body):
        """Return (status, response_json, step)."""
        script = self.script_for(body)
        if script is None:
            return 404, {"type": "error", "error": {"type": "not_found_error",
                                                     "message": "no scripted agent for this session"}}, {}
        thinking = body.get("thinking") or {}
        mismatch = binding_error(body)
        if mismatch is not None:
            behavior = (thinking.get("block_binding") or {}).get("prefix_mismatch_behavior", "error")
            if behavior == "error":
                return 400, {"type": "error", "error": {"type": "invalid_request_error", "message": mismatch}}, {}
        step = script.next(body)
        if step.get("advance_clock"):
            self.advance(step["advance_clock"])
        if step.get("error"):
            error = step["error"]
            return error.get("status", 500), error.get("body") or {
                "type": "error", "error": {"type": "api_error", "message": "scripted error"}}, step
        edited, applied = apply_context_edits(body)
        usage = self.cache.usage(edited)
        content = []
        if thinking.get("type") in ("adaptive", "enabled"):
            content.append({"type": "thinking", "thinking": "",
                            "signature": thinking_signature(body.get("model"), body.get("system"),
                                                            body.get("tools"), body.get("messages") or [])})
        tool_index = 0
        for block in step.get("content") or []:
            block = dict(block)
            if block.get("type") == "tool_use" and not block.get("id"):
                tool_index += 1
                seed = canonical([len(body.get("messages") or []), tool_index, block.get("name")])
                block["id"] = "toolu_fake_" + hashlib.sha256(seed.encode()).hexdigest()[:20]
            content.append(block)
        output = 40 + sum(tokens_of(canonical(b)) for b in content if b.get("type") != "thinking")
        usage["output_tokens"] = output
        usage["service_tier"] = "standard"
        for key, value in (step.get("usage") or {}).items():
            usage[key] = value
        message = {
            "id": "msg_fake_" + hashlib.sha256(canonical(body.get("messages") or []).encode()).hexdigest()[:20],
            "type": "message", "role": "assistant", "model": body.get("model"),
            "content": content,
            "stop_reason": step.get("stop_reason") or ("tool_use" if any(
                b.get("type") == "tool_use" for b in content) else "end_turn"),
            "stop_sequence": None, "stop_details": step.get("stop_details"),
            "usage": usage,
        }
        if "thinking-binding-controls-2026-08-01" in (body.get("_betas") or []):
            message["input_transformations"] = []
        if body.get("context_management"):
            message["context_management"] = {"applied_edits": applied}
        return 200, message, step

    def _handler(self):
        api = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def _headers(self):
                return {k: v for k, v in self.headers.items() if k.lower() not in SECRET_HEADERS}

            def _send(self, status, data, ctype="application/json", extra=None):
                self.send_response(status)
                self.send_header("content-type", ctype)
                self.send_header("content-length", str(len(data)))
                self.send_header("request-id", "req_fake_%d" % len(api.requests))
                for key, value in (extra or {}).items():
                    self.send_header(key, value)
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                api._record({"method": "GET", "path": self.path, "headers": self._headers(), "t": api.now()})
                self._send(200, b"{}")

            def do_POST(self):
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw.decode("utf-8"))
                except ValueError:
                    body = None
                path = self.path.split("?", 1)[0]
                if not isinstance(body, dict):
                    api._record({"method": "POST", "path": self.path, "headers": self._headers(),
                                 "raw": raw[:2000].decode("utf-8", "replace"), "t": api.now()})
                    return self._send(400, b'{"type":"error","error":{"type":"invalid_request_error","message":"bad json"}}')
                if path.endswith("/count_tokens"):
                    api.count_token_calls += 1
                    api._record({"method": "POST", "path": self.path, "headers": self._headers(),
                                 "body": body, "t": api.now()})
                    total = sum(tokens_of(s[0]) for s in segments(body))
                    return self._send(200, json.dumps({"input_tokens": total}).encode())
                if not path.endswith("/messages"):
                    api._record({"method": "POST", "path": self.path, "headers": self._headers(),
                                 "body": body, "t": api.now()})
                    return self._send(200, b"{}")
                betas = [b.strip() for b in (self.headers.get("anthropic-beta") or "").split(",") if b.strip()]
                annotated = dict(body)
                annotated["_betas"] = betas
                status, message, step = api.respond(annotated)
                api._record({"method": "POST", "path": self.path, "headers": self._headers(), "body": body,
                             "status": status, "response": message, "t": api.now()})
                extra = {}
                if status in (429, 529) and step.get("retry_after") is not None:
                    extra["retry-after"] = str(step["retry_after"])
                if status != 200 or not body.get("stream"):
                    return self._send(status, json.dumps(message).encode(), extra=extra)
                return self._send(200, sse(message), "text/event-stream")

        return Handler


def sse(message):
    """Render a complete message as Messages API SSE events."""
    events = []
    start = dict(message)
    start["content"] = []
    start["stop_reason"] = None
    usage = dict(message["usage"])
    final_output = usage.get("output_tokens", 0)
    usage["output_tokens"] = 1
    start["usage"] = usage
    events.append(("message_start", {"type": "message_start", "message": start}))
    for index, block in enumerate(message["content"]):
        kind = block.get("type")
        if kind == "thinking":
            events.append(("content_block_start", {"type": "content_block_start", "index": index,
                                                   "content_block": {"type": "thinking", "thinking": "", "signature": ""}}))
            events.append(("content_block_delta", {"type": "content_block_delta", "index": index,
                                                   "delta": {"type": "signature_delta", "signature": block["signature"]}}))
        elif kind == "text":
            events.append(("content_block_start", {"type": "content_block_start", "index": index,
                                                   "content_block": {"type": "text", "text": ""}}))
            events.append(("content_block_delta", {"type": "content_block_delta", "index": index,
                                                   "delta": {"type": "text_delta", "text": block.get("text", "")}}))
        elif kind == "tool_use":
            events.append(("content_block_start", {"type": "content_block_start", "index": index,
                                                   "content_block": {"type": "tool_use", "id": block["id"],
                                                                     "name": block["name"], "input": {}}}))
            events.append(("content_block_delta", {"type": "content_block_delta", "index": index,
                                                   "delta": {"type": "input_json_delta",
                                                             "partial_json": json.dumps(block.get("input") or {})}}))
        events.append(("content_block_stop", {"type": "content_block_stop", "index": index}))
    events.append(("message_delta", {"type": "message_delta",
                                     "delta": {"stop_reason": message["stop_reason"], "stop_sequence": None},
                                     "usage": {"output_tokens": final_output}}))
    events.append(("message_stop", {"type": "message_stop"}))
    return b"".join(("event: %s\ndata: %s\n\n" % (name, json.dumps(data))).encode() for name, data in events)


def tool_use(name, tool_input, text=None):
    """A scripted step with one tool call."""
    content = [{"type": "text", "text": text}] if text else []
    content.append({"type": "tool_use", "name": name, "input": tool_input})
    return {"content": content, "stop_reason": "tool_use"}


def final(text):
    return {"content": [{"type": "text", "text": text}], "stop_reason": "end_turn"}


if __name__ == "__main__":
    import argparse
    import sys

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--log")
    parser.add_argument("--script", help="JSON file with a list of scripted steps")
    args = parser.parse_args()
    api = FakeAPI(args.port, args.log)
    if args.script:
        with open(args.script) as handle:
            api.register("default", Script(json.load(handle)))
    print(api.base_url, flush=True)
    sys.stdout.flush()
    api.httpd.serve_forever()
