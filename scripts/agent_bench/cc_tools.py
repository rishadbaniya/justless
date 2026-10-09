"""Claude Code 2.1.327 tool parity: Bash, Read, Edit and Write.

Each behavior here was observed by cc_capture.py (real Claude Code against the fake API)
and is listed with its provenance in data/cc_parity.json. Tools return a ToolOutcome:
the structured output a PostToolUse hook sees, a renderer that turns it into the
tool_result text, and any mid-conversation system notes Claude Code adds after it.
"""

import difflib
import hashlib
import os
import re
import shlex
import signal
import subprocess
import tempfile
import time
from pathlib import Path

import config
import sandbox

BASH_INLINE_MAX = 30000
BASH_PREVIEW = 2000
FAIL_INLINE_MAX = 10000
FAIL_HALF = 5000
FAIL_READ_WINDOW = 30000
BASH_TIMEOUT_DEFAULT = 120000
BASH_TIMEOUT_MAX = 600000
BENIGN_EXIT1 = {"grep", "rg", "egrep", "fgrep", "find", "diff", "test", "["}
BENIGN_GIT = {"diff", "grep"}
READ_MAX_BYTES = 262144
READ_TOKEN_CAP = 25000
READ_DEFAULT_LIMIT = 2000
PARTIAL_FACTOR = 0.85
CONTEXT_WINDOW = 1000000
READ_SUFFIX = " (file state is current in your context \u2014 no need to Read it back)"
NO_OUTPUT = "(Bash completed with no output)"
NUDGE = ("The user hasn't heard from you in a while. As you continue, keep them updated when there's "
         "something to tell \u2014 a finding, a change of plan.")
NUDGE_AT = (5, 10, 15)
BACKGROUND_DISABLED = "Background tasks are disabled in this session."

SCHEMAS = {
    "Bash": {"required": {"command": str}, "optional": {"timeout": (int, float), "description": str,
                                                          "run_in_background": bool,
                                                          "dangerouslyDisableSandbox": bool}},
    "Read": {"required": {"file_path": str}, "optional": {"offset": int, "limit": int, "pages": str,
                                                           "allow_large": bool}},
    "Edit": {"required": {"file_path": str, "old_string": str, "new_string": str},
             "optional": {"replace_all": bool}},
    "Write": {"required": {"file_path": str, "content": str}, "optional": {}},
}
TYPE_NAMES = {str: "string", int: "number", float: "number", bool: "boolean"}


def tool_error(text):
    return "<tool_use_error>%s</tool_use_error>" % text


def validate(name, tool_input):
    """Return None or Claude Code's InputValidationError text."""
    schema = SCHEMAS.get(name)
    if schema is None:
        return tool_error("No such tool available: %s" % name)
    if not isinstance(tool_input, dict):
        return tool_error("InputValidationError: %s failed due to the following issue:\nThe input must be an object" % name)
    issues = []
    for key, kind in schema["required"].items():
        if key not in tool_input:
            issues.append("The required parameter `%s` is missing" % key)
    for key, value in tool_input.items():
        kind = schema["required"].get(key) or schema["optional"].get(key)
        if kind is None:
            issues.append("An unexpected parameter `%s` was provided" % key)
            continue
        kinds = kind if isinstance(kind, tuple) else (kind,)
        if isinstance(value, bool) and bool not in kinds:
            ok = False
        else:
            ok = isinstance(value, kinds)
        if not ok:
            issues.append("The parameter `%s` type is expected as `%s` but provided as `%s`" % (
                key, TYPE_NAMES.get(kinds[0], "value"), _json_type(value)))
    if not issues:
        return None
    plural = "issues" if len(issues) > 1 else "issue"
    return tool_error("InputValidationError: %s failed due to the following %s:\n%s" % (name, plural, "\n".join(issues)))


def _json_type(value):
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if value is None:
        return "null"
    return "object"


class ToolOutcome:
    """The result of one tool call before and after hooks."""

    def __init__(self, name, structured, render, is_error=False, notes=None, kind=None, extra=None):
        self.name = name
        self.structured = structured
        self.render = render
        self.is_error = is_error
        self.notes = list(notes or [])
        self.kind = kind
        self.extra = dict(extra or {})

    def text(self):
        return self.render(self.structured)


def _fixed(text):
    return lambda _structured: text


def last_segment_program(command):
    """The program and first argument of the last top-level command in a list or pipeline."""
    parts = re.split(r"\|\||&&|;|\||\n", command)
    for part in reversed(parts):
        part = part.strip()
        if not part:
            continue
        try:
            words = shlex.split(part)
        except ValueError:
            words = part.split()
        words = [w for w in words if not re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", w)]
        if not words:
            continue
        program = os.path.basename(words[0])
        return program, (words[1] if len(words) > 1 else "")
    return "", ""


def benign_exit1(command):
    program, first = last_segment_program(command)
    if program in BENIGN_EXIT1:
        return True
    return program == "git" and first in BENIGN_GIT


def kb(size):
    return "%.1fKB" % (size / 1024.0)


class Workspace:
    """Per-run tool state: paths, environment, shell cwd and Read state."""

    def __init__(self, root, run_dir, env, profile_path=None, cc_binary=config.CC_BINARY,
                 token_counter=None, deny=(), model="claude-sonnet-5-5"):
        self.root = Path(os.path.realpath(str(root)))
        self.run_dir = Path(os.path.realpath(str(run_dir)))
        self.env = dict(env)
        self.profile_path = profile_path
        self.cc_binary = cc_binary
        self.cwd = self.root
        self.read_state = {}
        self.token_counter = token_counter or estimate_tokens
        self.deny = [os.path.realpath(str(d)) for d in deny]
        self.model = model
        self.prompt_tokens = 0
        self.persist_dir = self.run_dir / "tool-results"
        self.shell_dir = self.run_dir / "shell"
        self.shell_dir.mkdir(parents=True, exist_ok=True)
        self.calls_without_text = 0
        self.nudges = 0
        self.counter = 0
        self.prelude = self.shell_dir / "prelude.zsh"
        template = (config.DATA / "cc_shell_prelude.sh.in").read_text()
        self.prelude.write_text(template.replace("{cc_binary}", shlex.quote(cc_binary))
                                .replace("{path}", shlex.quote(self.env.get("PATH", config.SYSTEM_PATH))))

    # path policy -----------------------------------------------------------------
    def resolve(self, file_path):
        path = Path(file_path)
        if not path.is_absolute():
            path = self.cwd / path
        return Path(os.path.normpath(str(path)))

    def allowed(self, path, write=False):
        real = os.path.realpath(str(path))
        if real == str(self.run_dir) or real.startswith(str(self.run_dir) + os.sep):
            return True
        if write:
            return False
        return not any(real == d or real.startswith(d + os.sep) for d in self.deny)

    def inside_root(self, path):
        real = os.path.realpath(str(path))
        return real == str(self.root) or real.startswith(str(self.root) + os.sep)

    # Bash ------------------------------------------------------------------------
    def bash(self, tool_input, executed=None, tool_id="t"):
        executed = executed or tool_input
        if executed.get("run_in_background"):
            return ToolOutcome("Bash", {"stdout": "", "stderr": BACKGROUND_DISABLED, "interrupted": False,
                                        "isImage": False, "noOutputExpected": False},
                               _fixed(BACKGROUND_DISABLED), True, kind="background_rejected")
        command = executed["command"]
        timeout_ms = executed.get("timeout") or BASH_TIMEOUT_DEFAULT
        timeout_ms = max(1, min(int(timeout_ms), BASH_TIMEOUT_MAX))
        self.counter += 1
        cwd_file = self.shell_dir / ("cwd_%d" % self.counter)
        out_file = self.shell_dir / ("out_%d" % self.counter)
        script = ('__cc_cmd="$1"; __cc_cwd="$2"; __cc_cwdf="$3"; shift 3; '
                  'source "$__CC_PRELUDE" 2>/dev/null; unset __CC_PRELUDE; cd -- "$__cc_cwd" 2>/dev/null; '
                  'eval "$__cc_cmd" < /dev/null; __cc_ec=$?; pwd -P >| "$__cc_cwdf" 2>/dev/null; exit $__cc_ec')
        argv = ["/bin/zsh", "-c", script, "zsh", command, str(self.cwd), str(cwd_file)]
        env = dict(self.env)
        env["__CC_PRELUDE"] = str(self.prelude)
        env["PWD"] = str(self.cwd)
        started = time.time()
        timed_out = False
        with open(str(out_file), "wb") as sink:
            proc = subprocess.Popen(sandbox.wrap(argv, self.profile_path), cwd=str(self.cwd), env=env,
                                    stdin=subprocess.DEVNULL, stdout=sink, stderr=subprocess.STDOUT,
                                    start_new_session=True)
            try:
                code = proc.wait(timeout=timeout_ms / 1000.0)
            except subprocess.TimeoutExpired:
                timed_out = True
                try:
                    os.killpg(proc.pid, signal.SIGTERM)
                except OSError:
                    pass
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()
                code = 143
        duration = time.time() - started
        raw = out_file.read_bytes()
        out_file.unlink()
        output = raw.decode("utf-8", "replace")
        notes = []
        reset_note = ""
        previous = self.cwd
        new_cwd = None
        if cwd_file.exists():
            new_cwd = cwd_file.read_text().strip() or None
            cwd_file.unlink()
        if not timed_out and code == 0 and new_cwd:
            if self.inside_root(new_cwd):
                self.cwd = Path(os.path.realpath(new_cwd))
            else:
                self.cwd = self.root
                reset_note = "Shell cwd was reset to %s" % self.root
        elif not timed_out and new_cwd and not self.inside_root(new_cwd) and code == 0:
            self.cwd = self.root
        if self.cwd != previous:
            notes.append("# Environment update\n - Primary working directory: %s (was %s)" % (self.cwd, previous))
        valid = (code == 0 or (code == 1 and benign_exit1(command))) and not timed_out
        structured = {"stdout": output, "stderr": "", "interrupted": timed_out, "isImage": False,
                      "noOutputExpected": False}
        render_meta = {}

        def render(value):
            text = value.get("stdout", "")
            if value.get("stderr"):
                text = text + ("\n" if text and not text.endswith("\n") else "") + value["stderr"]
            if valid:
                body = text.rstrip()
                if reset_note:
                    body = (body + "\n" if body else "") + reset_note
                if not body:
                    render_meta["render"] = "empty"
                    return NO_OUTPUT
                if len(body) > BASH_INLINE_MAX:
                    self.persist_dir.mkdir(parents=True, exist_ok=True)
                    path = self.persist_dir / ("%s.txt" % tool_id)
                    path.write_text(body)
                    cut = body[:BASH_PREVIEW]
                    if "\n" in cut:
                        cut = cut[:cut.rfind("\n")]
                    render_meta["render"] = "persisted"
                    return ("<persisted-output>\nOutput too large (%s). Full output saved to: %s\n\n"
                            "Preview (first 2KB):\n%s\n...\n</persisted-output>" % (
                                kb(len(body.encode("utf-8"))), path, cut))
                render_meta["render"] = "inline"
                return body
            body = text[:FAIL_READ_WINDOW].rstrip()
            full = "Exit code %d" % code + ("\n" + body if body else "")
            if timed_out:
                full += "\nCommand timed out after %s" % _seconds(timeout_ms)
            if reset_note:
                full += "\n" + reset_note
            if len(full) > FAIL_INLINE_MAX:
                render_meta["render"] = "excerpt"
                return "%s\n\n... [%d characters truncated] ...\n\n%s" % (
                    full[:FAIL_HALF], len(full) - FAIL_INLINE_MAX, full[-FAIL_HALF:])
            render_meta["render"] = "inline"
            return full

        outcome = ToolOutcome("Bash", structured, render, not valid, notes,
                              kind="valid" if code == 0 else ("benign_exit1" if valid else
                                                              ("timeout" if timed_out else "failure")),
                              extra={"exit_code": code, "duration_s": round(duration, 3),
                                     "raw_bytes": len(raw), "render_meta": render_meta,
                                     "denials": sandbox.count_denials(output)})
        return outcome

    # Read ------------------------------------------------------------------------
    def read(self, tool_input):
        path = self.resolve(tool_input["file_path"])
        offset = tool_input.get("offset")
        limit = tool_input.get("limit")
        allow_large = bool(tool_input.get("allow_large"))
        ranged = offset is not None or limit is not None
        if not self.allowed(path):
            text = "EPERM: operation not permitted, open '%s'" % path
            return ToolOutcome("Read", None, _fixed(text), True, kind="denied", extra={"denials": 1})
        if not path.exists():
            text = "File does not exist. Note: your current working directory is %s." % self.cwd
            return ToolOutcome("Read", None, _fixed(text), True, kind="error")
        if path.is_dir():
            text = "EISDIR: illegal operation on a directory, read '%s'" % path
            return ToolOutcome("Read", None, _fixed(text), True, kind="error")
        data = path.read_bytes()
        if not ranged and not allow_large and len(data) > READ_MAX_BYTES:
            text = ("File content (%s) exceeds maximum allowed size (256KB). Use offset and limit parameters "
                    "to read specific portions of the file, or search for specific content instead of reading "
                    "the whole file. To read it anyway, up to what still fits in your context, call Read again "
                    "with allow_large: true \u2014 only if you genuinely need all of it or the user asked for the "
                    "whole file." % kb(len(data)))
            return ToolOutcome("Read", None, _fixed(text), True, kind="error")
        content = data.decode("utf-8", "replace")
        if content == "":
            self._remember(path, partial=False)
            text = "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>"
            return ToolOutcome("Read", None, _fixed(text), False, kind="empty")
        lines = content.split("\n")
        total = len(lines)
        start = max(1, int(offset or 1))
        if start > total:
            text = ("<system-reminder>Warning: the file exists but is shorter than the provided offset (%d). "
                    "The file has %d lines.</system-reminder>" % (start, total))
            return ToolOutcome("Read", None, _fixed(text), False, kind="past_end")
        if allow_large and limit is None:
            count = total - start + 1
        else:
            count = int(limit) if limit is not None else READ_DEFAULT_LIMIT
        selected = lines[start - 1:start - 1 + count]
        notes = []
        partial = False
        kind = "allow_large" if allow_large else ("partial_range" if ranged else "full")
        if not allow_large:
            body = "\n".join(selected)
            tokens = None
            if not ranged:
                if len(data) > READ_TOKEN_CAP:
                    tokens = self.token_counter(content)
                if tokens is not None and tokens > READ_TOKEN_CAP:
                    budget = len(data) * READ_TOKEN_CAP / float(tokens) * PARTIAL_FACTOR
                    used = 0
                    keep = 0
                    for line in lines:
                        cost = len(line.encode("utf-8")) + 1
                        if used + cost > budget:
                            break
                        used += cost
                        keep += 1
                    keep = max(1, keep)
                    selected = lines[:keep]
                    partial = True
                    kind = "partial_view"
                    notes.append(
                        "[Truncated: PARTIAL view \u2014 %s: showing lines 1-%d of %d total (%d tokens, cap %d). "
                        "Call Read with offset=%d limit=%d for the next page, or Grep to find a specific section. "
                        "To read it anyway, up to what still fits in your context, call Read again with "
                        "allow_large: true \u2014 only if you genuinely need all of it or the user asked for the "
                        "whole file. Do NOT answer from this page alone if the answer may be further in the file.]"
                        % (path, keep, total, tokens, READ_TOKEN_CAP, keep + 1, keep))
            elif len(body.encode("utf-8")) > READ_TOKEN_CAP:
                tokens = self.token_counter(body)
                if tokens > READ_TOKEN_CAP:
                    text = ("File content (%d tokens) exceeds maximum allowed tokens (%d). Use offset and limit "
                            "parameters to read specific portions of the file, or search for specific content "
                            "instead of reading the whole file." % (tokens, READ_TOKEN_CAP))
                    return ToolOutcome("Read", None, _fixed(text), True, kind="error")
        self._remember(path, partial=partial, ranged=ranged)
        structured = {"type": "text", "file": {"filePath": str(path), "content": "\n".join(selected),
                                               "numLines": len(selected), "startLine": start,
                                               "totalLines": total}}
        return ToolOutcome("Read", structured, render_read, False, notes, kind=kind,
                           extra={"raw_bytes": len(data), "partial": partial})

    def _remember(self, path, partial=False, ranged=False, own_write=False):
        try:
            stat = os.stat(str(path))
            digest = hashlib.sha256(Path(path).read_bytes()).hexdigest()
        except OSError:
            return
        previous = self.read_state.get(str(path))
        self.read_state[str(path)] = {
            "mtime": stat.st_mtime_ns, "size": stat.st_size, "sha": digest,
            "partial_only": partial and (previous is None or previous.get("partial_only", False)),
            "text": Path(path).read_text(errors="replace") if stat.st_size <= 2_000_000 else None,
        }

    # Edit ------------------------------------------------------------------------
    def edit(self, tool_input):
        path = self.resolve(tool_input["file_path"])
        old = tool_input["old_string"]
        new = tool_input["new_string"]
        replace_all = bool(tool_input.get("replace_all"))
        if old == new:
            return ToolOutcome("Edit", None, _fixed(tool_error(
                "No changes to make: old_string and new_string are exactly the same.")), True, kind="identical")
        if not self.allowed(path, write=True):
            return ToolOutcome("Edit", None, _fixed(tool_error("EPERM: operation not permitted, open '%s'" % path)),
                               True, kind="denied", extra={"denials": 1})
        known = str(path) in self.read_state
        if not path.exists():
            if old == "":
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(new)
                self._remember(path)
                text = "The file %s has been updated successfully." % path
                return ToolOutcome("Edit", self._edit_struct(path, old, new, ""), _fixed(text), False, kind="created")
            text = tool_error("File does not exist. Note: your current working directory is %s." % self.cwd)
            return ToolOutcome("Edit", None, _fixed(text), True, kind="missing")
        if path.is_dir():
            return ToolOutcome("Edit", None, _fixed(tool_error("EISDIR: illegal operation on a directory, read '%s'"
                                                               % path)), True, kind="error")
        original = path.read_text(errors="replace")
        count = original.count(old) if old else 0
        if count == 0:
            return ToolOutcome("Edit", None, _fixed(tool_error("String to replace not found in file.\nString: %s" % old)),
                               True, kind="not_found")
        if count > 1 and not replace_all:
            text = tool_error("Found %d matches of the string to replace, but replace_all is false. To replace all "
                              "occurrences, set replace_all to true. To replace only one occurrence, please provide "
                              "more context to uniquely identify the instance.\nString: %s" % (count, old))
            return ToolOutcome("Edit", None, _fixed(text), True, kind="multiple")
        updated = original.replace(old, new) if replace_all else original.replace(old, new, 1)
        mode = os.stat(str(path)).st_mode
        path.write_text(updated)
        os.chmod(str(path), mode)
        stale = known and self.read_state[str(path)]["sha"] != hashlib.sha256(original.encode()).hexdigest()
        self._remember(path)
        if replace_all:
            text = "The file %s has been updated. All occurrences were successfully replaced." % path
        else:
            text = "The file %s has been updated successfully." % path
        if known:
            text += READ_SUFFIX
        return ToolOutcome("Edit", self._edit_struct(path, old, new, original), _fixed(text), False,
                           kind="ok_unread" if not known else ("ok_stale" if stale else "ok"))

    @staticmethod
    def _edit_struct(path, old, new, original):
        return {"filePath": str(path), "oldString": old, "newString": new, "originalFile": original,
                "structuredPatch": [], "userModified": False, "replaceAll": False}

    # Write -----------------------------------------------------------------------
    def write(self, tool_input):
        path = self.resolve(tool_input["file_path"])
        content = tool_input["content"]
        if not self.allowed(path, write=True):
            return ToolOutcome("Write", None, _fixed(tool_error("EPERM: operation not permitted, open '%s'" % path)),
                               True, kind="denied", extra={"denials": 1})
        state = self.read_state.get(str(path))
        if path.exists() and state is not None and state.get("partial_only"):
            return ToolOutcome("Write", None, _fixed(tool_error(
                "File has not been read yet. Read it first before writing to it.")), True, kind="partial_refused")
        existed = path.exists()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        self._remember(path)
        if existed:
            text = "The file %s has been updated successfully.%s" % (path, READ_SUFFIX)
        else:
            text = "File created successfully at: %s%s" % (path, READ_SUFFIX)
        return ToolOutcome("Write", {"type": "update" if existed else "create", "filePath": str(path),
                                     "content": content}, _fixed(text), False, kind="update" if existed else "create")

    # notes after a batch ---------------------------------------------------------
    def changed_files_notes(self, skip=()):
        """Claude Code's note for files it has read that changed on disk outside Edit/Write."""
        notes = []
        for key in sorted(self.read_state):
            if key in skip:
                continue
            state = self.read_state[key]
            try:
                stat = os.stat(key)
            except OSError:
                continue
            if stat.st_mtime_ns == state["mtime"] and stat.st_size == state["size"]:
                continue
            data = Path(key).read_bytes()
            digest = hashlib.sha256(data).hexdigest()
            if digest == state["sha"]:
                state["mtime"] = stat.st_mtime_ns
                continue
            new_text = data.decode("utf-8", "replace")
            notes.append("Note: %s changed on disk since you last read it. That's usually deliberate, so take it as "
                         "the current state rather than reverting it; if the change looks wrong, say so rather than "
                         "undoing it yourself \u2014 otherwise no need to call it out. Here are the relevant changes "
                         "(shown with line numbers):\n%s" % (key, changed_snippet(state.get("text"), new_text)))
            self._remember(key)
        return notes

    def nudge_after(self, results, had_text):
        """The progress nudge after 5, 10 and 15 tool results without assistant text."""
        if had_text:
            self.calls_without_text = 0
        out = []
        for _ in range(results):
            self.calls_without_text += 1
            if self.calls_without_text in NUDGE_AT and self.nudges < len(NUDGE_AT):
                self.nudges += 1
                out.append(NUDGE)
        return out[:1]


def changed_snippet(old_text, new_text, context=3, whole_max=40):
    new_lines = new_text.split("\n")
    if new_lines and new_lines[-1] == "":
        new_lines = new_lines[:-1]
    if old_text is None or len(new_lines) <= whole_max:
        return "\n".join("%d\t%s" % (i + 1, line) for i, line in enumerate(new_lines))
    old_lines = old_text.split("\n")
    matcher = difflib.SequenceMatcher(None, old_lines, new_lines, autojunk=False)
    keep = set()
    for tag, _, _, j1, j2 in matcher.get_opcodes():
        if tag == "equal":
            continue
        for j in range(max(0, j1 - context), min(len(new_lines), max(j2, j1 + 1) + context)):
            keep.add(j)
    out = []
    last = None
    for j in sorted(keep):
        if last is not None and j != last + 1:
            out.append("...")
        out.append("%d\t%s" % (j + 1, new_lines[j]))
        last = j
    return "\n".join(out)


def render_read(structured):
    file = structured["file"]
    lines = file["content"].split("\n")
    start = int(file.get("startLine") or 1)
    return "\n".join("%d\t%s" % (start + i, line) for i, line in enumerate(lines))


def valid_read_output(value):
    """Read's output schema for updatedToolOutput."""
    if not isinstance(value, dict) or value.get("type") != "text":
        return False
    file = value.get("file")
    if not isinstance(file, dict):
        return False
    return (isinstance(file.get("filePath"), str) and isinstance(file.get("content"), str)
            and all(isinstance(file.get(k), int) and not isinstance(file.get(k), bool)
                    for k in ("numLines", "startLine", "totalLines")))


def valid_bash_output(value):
    return isinstance(value, dict) and isinstance(value.get("stdout"), str) and isinstance(value.get("stderr", ""), str)


OUTPUT_SCHEMAS = {"Read": valid_read_output, "Bash": valid_bash_output}


def estimate_tokens(text):
    """Dry-run Read counter: bytes/2.0 over-counts, so more reads become PARTIAL (conservative for Justless)."""
    return int(len(text.encode("utf-8")) / 2.0)


def _seconds(ms):
    seconds = ms / 1000.0
    return "%ds" % seconds if seconds == int(seconds) else "%.1fs" % seconds
