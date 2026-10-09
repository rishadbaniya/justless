"""The agent loop: append-only requests, Claude Code tool parity, hooks, caps and logging."""

import copy
import json
import threading
import time

import accounting
import api as api_module
import cc_tools
import config
import fake_api
import workspace as ws_module


class HarnessError(Exception):
    def __init__(self, kind, detail, append_only=False):
        Exception.__init__(self, "%s: %s" % (kind, detail))
        self.kind = kind
        self.append_only = append_only


class AppendOnlyGuard:
    """Asserts system and tools never change and every request extends the previous one by its response."""

    def __init__(self):
        self.previous = None

    @staticmethod
    def _canon(body):
        return (fake_api.canonical(fake_api.strip_cache_control(body.get("system"))),
                fake_api.canonical(fake_api.strip_cache_control(body.get("tools"))),
                [fake_api.canonical(fake_api.strip_cache_control(m)) for m in body.get("messages") or []],
                fake_api.canonical({k: v for k, v in body.items() if k not in ("system", "tools", "messages")}))

    def check(self, body):
        system, tools, messages, rest = self._canon(body)
        if self.previous is not None:
            p_system, p_tools, p_messages, p_rest, p_response = self.previous
            if system != p_system:
                raise HarnessError("append_only", "system changed", True)
            if tools != p_tools:
                raise HarnessError("append_only", "tools changed", True)
            if rest != p_rest:
                raise HarnessError("append_only", "request fields changed", True)
            if messages[:len(p_messages)] != p_messages:
                for i, (a, b) in enumerate(zip(messages, p_messages)):
                    if a != b:
                        raise HarnessError("append_only", "message %d edited" % i, True)
                raise HarnessError("append_only", "messages dropped", True)
            if p_response is not None:
                if len(messages) <= len(p_messages) or messages[len(p_messages)] != p_response:
                    raise HarnessError("append_only", "previous response not appended verbatim", True)
        self.previous = [system, tools, messages, rest, None]

    def record_response(self, content):
        self.previous[4] = fake_api.canonical({"role": "assistant", "content": content})


class JsonlWriter:
    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()

    def write(self, row):
        with self.lock:
            with open(str(self.path), "a") as handle:
                handle.write(json.dumps(row) + "\n")


def secret_hits(text, key, allowed=()):
    """Secret-looking strings in outbound text. Matches on reviewed allowlist lines are ignored."""
    hits = []
    if key and key in text:
        hits.append("api_key")
    for kind, pattern in ws_module.SECRET_PATTERNS[:6]:
        if any(m.group(0) not in allowed for m in pattern.finditer(text)):
            hits.append(kind)
    return hits


def with_breakpoint(messages):
    """A copy of the history with Claude Code's moving breakpoint on the last block of the last message."""
    out = copy.deepcopy(messages)
    last = out[-1]
    if isinstance(last["content"], str):
        last["content"] = [{"type": "text", "text": last["content"]}]
    last["content"][-1]["cache_control"] = {"type": "ephemeral"}
    return out


class Run:
    """One agent run in a prepared run directory."""

    def __init__(self, spec, client, profile, ledger, pricing, tools_ws, hooks, logs, limits=None, api_key=None,
                 clock=time.time, extra_notes=None, allowed_secrets=()):
        self.spec = spec
        self.client = client
        self.profile = profile
        self.ledger = ledger
        self.pricing = pricing
        self.ws = tools_ws
        self.hooks = hooks
        self.logs = logs
        self.limits = dict(config.LIMITS, **(limits or {}))
        self.api_key = api_key
        self.allowed_secrets = set(allowed_secrets)
        self.clock = clock
        self.guard = AppendOnlyGuard()
        self.messages = []
        self.totals = {"requests": 0, "input_tokens": 0, "cache_creation_input_tokens": 0,
                       "cache_read_input_tokens": 0, "output_tokens": 0, "prompt_tokens": 0, "cost_usd": 0.0,
                       "max_prompt_tokens": 0, "ce_edits": 0, "ce_cleared_tool_uses": 0, "ce_cleared_input_tokens": 0,
                       "ttl_expiry_rewrites": 0, "ttl_expiry_cost": 0.0, "api_s": 0.0, "tools_s": 0.0,
                       "hooks_s": 0.0, "retry_wait_s": 0.0, "tool_bytes_raw": 0, "tool_bytes_seen": 0,
                       "hook_bytes_removed": 0, "denials": 0, "input_transformations": 0, "stop_max_tokens": 0}
        self.tool_counts = {}
        self.failure_mode = None
        self.final_text = ""
        self.harness_error = None
        self.last_request_start = None
        self.first_body = None

    def body(self, session_context):
        model = self.spec["model"]
        if not self.messages:
            self.messages = self.profile.first_messages(model, self.ws.root, self.spec["home"], self.spec["prompt"],
                                                        session_context)
        body = {
            "model": model,
            "max_tokens": self.profile.max_tokens(model, self.spec.get("max_output_tokens") or config.MAX_OUTPUT_CAP),
            "thinking": {"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "error"}},
            "output_config": {"effort": self.spec["effort"]},
            "system": self.profile.system(self.spec["nonce"], self.ws.root, self.spec["home"]),
            "tools": self.profile.tools(),
            "messages": with_breakpoint(self.messages),
        }
        edits = list((self.profile.base_context_management or {}).get("edits") or [])
        extra = config.context_management(self.spec["arm"], self.spec.get("ce_trigger") or config.CE_PILOT_TRIGGER)
        if extra:
            edits += extra["edits"]
        if edits:
            body["context_management"] = {"edits": edits}
        return body

    def count(self, name, kind=None):
        key = name if kind is None else "%s:%s" % (name, kind)
        self.tool_counts[key] = self.tool_counts.get(key, 0) + 1

    def execute(self, block):
        """Run one tool_use block. Returns (tool_result, notes)."""
        name = block.get("name")
        tool_input = block.get("input") or {}
        tool_id = block.get("id")
        started = time.time()
        invalid = cc_tools.validate(name, tool_input)
        record = {"run_id": self.spec["run_id"], "attempt": self.spec["attempt"], "turn": self.totals["requests"],
                  "tool_use_id": tool_id, "name": name, "input": tool_input, "hooks": []}
        if invalid:
            self.count(name, "invalid")
            record.update(kind="invalid", is_error=True, bytes_seen=len(invalid))
            self.logs["tools"].write(record)
            return {"type": "tool_result", "tool_use_id": tool_id, "content": invalid, "is_error": True}, []
        notes = []
        executed = tool_input
        hook_start = len(self.hooks.events) if self.hooks else 0
        if self.hooks:
            executed, pre_notes, blocked = self.hooks.pre_tool(name, tool_input, tool_id)
            notes += pre_notes
            if blocked:
                record.update(kind="blocked", is_error=True)
                self.logs["tools"].write(record)
                return {"type": "tool_result", "tool_use_id": tool_id, "content": blocked, "is_error": True}, notes
            if executed is not tool_input:
                bad = cc_tools.validate(name, executed)
                if bad:
                    executed = tool_input
        tool_started = time.time()
        if name == "Bash":
            outcome = self.ws.bash(tool_input, executed, tool_id)
        elif name == "Read":
            try:
                outcome = self.ws.read(executed)
            except api_module.APIError as error:
                raise HarnessError("infra_pause", "count_tokens: %s" % str(error)[:300])
        elif name == "Edit":
            outcome = self.ws.edit(executed)
        else:
            outcome = self.ws.write(executed)
        tool_seconds = time.time() - tool_started
        raw_text = outcome.text()
        notes_after = []
        if self.hooks and outcome.structured is not None:
            notes_after = self.hooks.post_tool(name, executed, outcome, tool_id, int(tool_seconds * 1000))
        seen = outcome.text()
        notes = notes + notes_after + outcome.notes
        hook_events = self.hooks.events[hook_start:] if self.hooks else []
        hook_seconds = sum(e.duration for e in hook_events)
        self.totals["hooks_s"] += hook_seconds
        self.totals["tools_s"] += tool_seconds
        self.totals["tool_bytes_raw"] += len(raw_text.encode("utf-8"))
        self.totals["tool_bytes_seen"] += len(seen.encode("utf-8"))
        self.totals["hook_bytes_removed"] += max(0, outcome.extra.get("hook_bytes_removed", 0))
        self.totals["denials"] += outcome.extra.get("denials", 0)
        kind = outcome.kind
        if name == "Bash":
            command = executed.get("command", "")
            original = tool_input.get("command", "")
            if command != original:
                self.count("Bash", "rewritten")
            if original.strip().startswith("justless"):
                self.count("Bash", "justless_typed")
            program, _ = cc_tools.last_segment_program(original)
            if program in ("grep", "rg", "find", "egrep", "fgrep", "ag", "ack"):
                self.count("Bash", "search")
        if name == "Read" and outcome.extra.get("hook_replaced"):
            kind = "outlined"
        self.count(name)
        self.count(name, kind)
        record.update(executed_input=executed if executed is not tool_input else None, kind=kind,
                      is_error=outcome.is_error, exit_code=outcome.extra.get("exit_code"),
                      render=(outcome.extra.get("render_meta") or {}).get("render"),
                      duration_s=round(tool_seconds, 3), hook_s=round(hook_seconds, 3),
                      bytes_raw=len(raw_text.encode("utf-8")), bytes_seen=len(seen.encode("utf-8")),
                      hooks=[e.record() for e in hook_events], notes=len(notes),
                      denials=outcome.extra.get("denials", 0), partial=outcome.extra.get("partial"))
        self.logs["tools"].write(record)
        result = {"type": "tool_result", "tool_use_id": tool_id, "content": seen}
        if outcome.is_error:
            result["is_error"] = True
        return result, notes

    def stop(self, mode):
        self.failure_mode = mode
        return False

    def step(self, session_context, started):
        """One request. Returns True to continue."""
        if self.totals["requests"] >= self.limits["turn_limit"]:
            return self.stop("turn_limit")
        if self.totals["prompt_tokens"] >= self.limits["token_limit"]:
            return self.stop("token_limit")
        if self.totals["cost_usd"] >= self.spec["run_budget"]:
            return self.stop("run_budget")
        # A hang guard, not a pace limit: API retry back-off is not the agent's time.
        if time.time() - started - self.totals["retry_wait_s"] > self.limits["wall_timeout"]:
            return self.stop("wall_timeout")
        body = self.body(session_context)
        self.guard.check(body)
        if self.first_body is None:
            self.first_body = copy.deepcopy(body)
        new_bytes = len(json.dumps(self.messages[-1]).encode())
        estimate = self.totals.get("last_prompt", 0) + new_bytes // 2 + (0 if self.totals["requests"] else
                                                                            len(json.dumps(body).encode()) // 2)
        worst = self.pricing.worst_case(self.spec["model"], estimate, body["max_tokens"])
        try:
            reserved = self.ledger.check(worst)
        except accounting.BudgetExceeded:
            raise HarnessError("aborted_global_cap", "campaign cap reached")
        now = time.time()
        gap = None if self.last_request_start is None else now - self.last_request_start
        try:
            for message in self.messages[-2:]:
                if message["role"] == "assistant":
                    continue
                hits = secret_hits(json.dumps(message), self.api_key, self.allowed_secrets)
                if hits:
                    raise HarnessError("secret_blocked", ",".join(hits))
            self.last_request_start = now
            response, meta = self.client.create(body)
        except api_module.APIError as error:
            self.ledger.release(reserved)
            self.logs["attempts"].write({"run_id": self.spec["run_id"], "attempt": self.spec["attempt"],
                                         "turn": self.totals["requests"], "status": error.status,
                                         "error": str(error)[:500], "append_only": error.append_only_bug,
                                         "api_attempts": error.attempts})
            raise HarnessError("api_%s" % error.status, str(error)[:300], error.append_only_bug)
        except BaseException:
            self.ledger.release(reserved)
            raise
        usage = response.get("usage") or {}
        cost = self.pricing.cost(self.spec["model"], usage)
        prompt = accounting.prompt_tokens(usage)
        applied = ((response.get("context_management") or {}).get("applied_edits")) or []
        transformations = response.get("input_transformations")
        system_tokens_cached = self.totals.get("first_cache_read", None)
        ttl_rewrite = bool(gap is not None and gap > 300 and int(usage.get("cache_read_input_tokens") or 0)
                           <= (system_tokens_cached or 0))
        self.totals["requests"] += 1
        for key in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"):
            self.totals[key] += int(usage.get(key) or 0)
        self.totals["prompt_tokens"] += prompt
        self.totals["last_prompt"] = prompt
        self.totals["max_prompt_tokens"] = max(self.totals["max_prompt_tokens"], prompt)
        self.totals["cost_usd"] += cost
        self.totals["api_s"] += meta["latency_s"]
        self.totals["retry_wait_s"] += meta["retry_wait_s"]
        if self.totals["requests"] == 1:
            self.totals["first_cache_read"] = int(usage.get("cache_read_input_tokens") or 0)
        if ttl_rewrite:
            self.totals["ttl_expiry_rewrites"] += 1
            self.totals["ttl_expiry_cost"] += cost
        for edit in applied:
            self.totals["ce_edits"] += 1
            self.totals["ce_cleared_tool_uses"] += int(edit.get("cleared_tool_uses") or 0)
            self.totals["ce_cleared_input_tokens"] += int(edit.get("cleared_input_tokens") or 0)
        if transformations:
            self.totals["input_transformations"] += len(transformations)
        if response.get("stop_reason") == "max_tokens":
            self.totals["stop_max_tokens"] += 1
        tier = self.pricing.tier_index(self.spec["model"], prompt)
        self.ledger.record({"run_id": self.spec["run_id"], "attempt": self.spec["attempt"],
                            "campaign": self.spec["campaign"], "phase": self.spec.get("phase", "main"),
                            "model": self.spec["model"], "arm": self.spec["arm"], "cost_usd": cost},
                           reserved=reserved)
        self.logs["requests"].write({
            "run_id": self.spec["run_id"], "attempt": self.spec["attempt"], "turn": self.totals["requests"],
            "t": now, "gap_s": None if gap is None else round(gap, 3), "latency_s": meta["latency_s"],
            "retry_wait_s": meta["retry_wait_s"], "api_attempts": len(meta["attempts"]),
            "request_id": meta.get("request_id"), "stop_reason": response.get("stop_reason"),
            "stop_details": response.get("stop_details"), "usage": usage,
            "input_transformations": transformations, "applied_edits": applied, "ttl_expiry_rewrite": ttl_rewrite,
            "price_tier": tier, "prompt_tokens": prompt, "cost_usd": cost,
            "bytes": {"system": len(json.dumps(body["system"]).encode()), "tools": len(json.dumps(body["tools"]).encode()),
                      "messages": len(json.dumps(body["messages"]).encode()),
                      "tail": len(json.dumps(self.messages[-1]).encode())},
        })
        content = response.get("content") or []
        self.guard.record_response(content)
        self.messages.append({"role": "assistant", "content": content})
        stop_reason = response.get("stop_reason")
        texts = [b.get("text", "") for b in content if b.get("type") == "text"]
        if any(t.strip() for t in texts):
            self.final_text = "\n".join(texts)
        if stop_reason == "tool_use":
            uses = [b for b in content if b.get("type") == "tool_use"]
            results = []
            notes = []
            own_writes = set()
            for block in uses:
                result, block_notes = self.execute(block)
                results.append(result)
                notes += block_notes
                if block.get("name") in ("Edit", "Write"):
                    path = self.ws.resolve((block.get("input") or {}).get("file_path", ""))
                    own_writes.add(str(path))
            notes += self.ws.changed_files_notes(skip=own_writes)
            notes += self.ws.nudge_after(len(results), any(t.strip() for t in texts))
            self.messages.append({"role": "user", "content": results})
            if notes:
                self.messages.append({"role": "system", "content": [{"type": "text", "text": "\n\n".join(notes)}]})
            return True
        if stop_reason == "end_turn":
            return False
        if stop_reason == "refusal":
            return self.stop("refusal")
        if stop_reason == "max_tokens":
            return self.stop("max_tokens")
        raise HarnessError("stop_reason", str(stop_reason))

    def run(self, session_context=""):
        started = time.time()
        try:
            while self.step(session_context, started):
                pass
        except HarnessError as error:
            self.harness_error = error
        self.totals["wall_s"] = round(time.time() - started, 3)
        return self

    def transcript(self):
        return {"run_id": self.spec["run_id"], "attempt": self.spec["attempt"], "messages": self.messages}
