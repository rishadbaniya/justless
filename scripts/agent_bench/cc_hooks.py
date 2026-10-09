"""A Claude Code hook runner driven by the project's .claude/settings.json.

It follows the contract observed in cc_capture.py: hooks run as `sh -c COMMAND` in the
project root with the event JSON on stdin; PreToolUse updatedInput replaces the whole
executed input; PostToolUse updatedToolOutput replaces the structured output when it
matches the tool's output schema and is ignored otherwise; additionalContext becomes a
mid-conversation system note '{Event}:{Tool} hook additional context: {text}'; the
SessionStart context joins the environment message. Errors pass through and are logged.
"""

import json
import re
import subprocess
import time
import uuid
from pathlib import Path

import cc_tools
import sandbox

CONTEXT_FILE_LIMIT = 10000
CONTEXT_PREVIEW = 2000


class HookEvent:
    def __init__(self, event, matcher_value, command, duration, status, output=None, error=None,
                 timed_out=False, applied=None, schema_rejected=False):
        self.event = event
        self.matcher_value = matcher_value
        self.command = command
        self.duration = duration
        self.status = status
        self.output = output
        self.error = error
        self.timed_out = timed_out
        self.applied = applied or []
        self.schema_rejected = schema_rejected

    def record(self):
        return {"event": self.event, "match": self.matcher_value, "command": self.command,
                "duration_s": round(self.duration, 3), "exit": self.status, "timed_out": self.timed_out,
                "error": self.error, "applied": self.applied, "schema_rejected": self.schema_rejected}


class HookRunner:
    def __init__(self, root, env, profile_path=None, session_id=None, transcript_path=None,
                 permission_mode="bypassPermissions", effort="medium", context_dir=None):
        self.root = Path(root)
        self.env = dict(env)
        self.profile_path = profile_path
        self.session_id = session_id or str(uuid.uuid4())
        self.transcript_path = str(transcript_path or (self.root.parent / "home" / "transcript.jsonl"))
        self.permission_mode = permission_mode
        self.effort = effort
        self.prompt_id = str(uuid.uuid4())
        self.context_dir = Path(context_dir or self.root.parent / "hook-context")
        self.settings = self._load()
        self.events = []

    def _load(self):
        path = self.root / ".claude" / "settings.json"
        if not path.exists():
            return {}
        try:
            return json.loads(path.read_text()).get("hooks") or {}
        except ValueError:
            return {}

    @property
    def active(self):
        return bool(self.settings)

    def handlers(self, event, value):
        out = []
        for group in self.settings.get(event) or []:
            matcher = group.get("matcher") or ""
            if matcher and matcher != "*":
                try:
                    if not re.search(matcher, value or ""):
                        continue
                except re.error:
                    if matcher != value:
                        continue
            for handler in group.get("hooks") or []:
                if handler.get("type", "command") == "command":
                    out.append(handler)
        return out

    def base(self, event):
        return {"session_id": self.session_id, "transcript_path": self.transcript_path, "cwd": str(self.root),
                "hook_event_name": event}

    def _run(self, handler, payload, event, value, wait=True):
        command = handler["command"]
        timeout = None if handler.get("async") else float(handler.get("timeout") or 600)
        started = time.time()
        argv = sandbox.wrap(["/bin/sh", "-c", command], self.profile_path)
        try:
            proc = subprocess.run(argv, cwd=str(self.root), env=self.env, input=json.dumps(payload).encode(),
                                  capture_output=True, timeout=timeout)
            status, stdout, stderr, timed_out = proc.returncode, proc.stdout, proc.stderr, False
        except subprocess.TimeoutExpired as expired:
            status, stdout, stderr, timed_out = None, expired.stdout or b"", expired.stderr or b"", True
        duration = time.time() - started
        text = stdout.decode("utf-8", "replace").strip()
        parsed = None
        error = None
        if timed_out:
            error = "timeout"
        elif status not in (0, None):
            error = "exit %s: %s" % (status, stderr.decode("utf-8", "replace")[:300])
        elif text.startswith("{"):
            try:
                parsed = json.loads(text)
            except ValueError:
                error = "unparsable output"
        record = HookEvent(event, value, command, duration, status, parsed if parsed is not None else text,
                           error, timed_out)
        self.events.append(record)
        return record, parsed, text

    def session_start(self, source="startup"):
        """Run SessionStart hooks to completion. Returns the joined additional context."""
        contexts = []
        for handler in self.handlers("SessionStart", source):
            payload = dict(self.base("SessionStart"), source=source)
            record, parsed, text = self._run(handler, payload, "SessionStart", source)
            if record.error:
                continue
            if isinstance(parsed, dict):
                ctx = (parsed.get("hookSpecificOutput") or {}).get("additionalContext")
                if ctx:
                    contexts.append(ctx)
                    record.applied.append("additionalContext")
            elif text:
                contexts.append(text)
                record.applied.append("stdout")
        return "\n\n".join(contexts)

    def pre_tool(self, name, tool_input, tool_use_id):
        """Returns (executed_input, notes, blocked_text)."""
        executed = tool_input
        notes = []
        for handler in self.handlers("PreToolUse", name):
            payload = dict(self.base("PreToolUse"), prompt_id=self.prompt_id, permission_mode=self.permission_mode,
                           effort={"level": self.effort}, tool_name=name, tool_input=executed,
                           tool_use_id=tool_use_id)
            record, parsed, text = self._run(handler, payload, "PreToolUse", name)
            if record.status == 2:
                return executed, notes, record.error or "blocked by hook"
            if not isinstance(parsed, dict):
                continue
            specific = parsed.get("hookSpecificOutput") or {}
            if specific.get("permissionDecision") == "deny":
                record.applied.append("deny")
                return executed, notes, specific.get("permissionDecisionReason") or "denied by hook"
            if isinstance(specific.get("updatedInput"), dict):
                executed = specific["updatedInput"]
                record.applied.append("updatedInput")
            if specific.get("additionalContext"):
                notes.append(self.context_note("PreToolUse", name, specific["additionalContext"]))
                record.applied.append("additionalContext")
        return executed, notes, None

    def post_tool(self, name, tool_input, outcome, tool_use_id, duration_ms):
        """May replace outcome.structured. Returns notes."""
        notes = []
        if outcome.structured is None:
            return notes
        for handler in self.handlers("PostToolUse", name):
            payload = dict(self.base("PostToolUse"), prompt_id=self.prompt_id, permission_mode=self.permission_mode,
                           effort={"level": self.effort}, tool_name=name, tool_input=tool_input,
                           tool_response=outcome.structured, tool_use_id=tool_use_id, duration_ms=duration_ms)
            record, parsed, text = self._run(handler, payload, "PostToolUse", name)
            if not isinstance(parsed, dict):
                continue
            specific = parsed.get("hookSpecificOutput") or {}
            if "updatedToolOutput" in specific:
                check = cc_tools.OUTPUT_SCHEMAS.get(name)
                if check is None or check(specific["updatedToolOutput"]):
                    before = len(outcome.text().encode("utf-8"))
                    outcome.structured = specific["updatedToolOutput"]
                    outcome.extra["hook_replaced"] = True
                    outcome.extra["hook_bytes_removed"] = before - len(outcome.text().encode("utf-8"))
                    record.applied.append("updatedToolOutput")
                else:
                    record.schema_rejected = True
            if specific.get("additionalContext"):
                notes.append(self.context_note("PostToolUse", name, specific["additionalContext"]))
                record.applied.append("additionalContext")
        return notes

    def context_note(self, event, tool, text):
        text = text.replace("<system-reminder>", "&lt;system-reminder&gt;").replace(
            "</system-reminder>", "&lt;/system-reminder&gt;")
        if len(text) > CONTEXT_FILE_LIMIT:
            self.context_dir.mkdir(parents=True, exist_ok=True)
            path = self.context_dir / ("%s-%d.txt" % (event, len(self.events)))
            path.write_text(text)
            text = ("<persisted-output>\nOutput too large (%s). Full output saved to: %s\n\nPreview (first 2KB):\n%s\n"
                    "...\n</persisted-output>" % (cc_tools.kb(len(text.encode())), path, text[:CONTEXT_PREVIEW]))
        return "%s:%s hook additional context: %s" % (event, tool, text)
