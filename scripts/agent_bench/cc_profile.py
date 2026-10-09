"""The request profile: Claude Code's system prompt, tool definitions and first messages.

With a capture in $AGENT_BENCH_HOME/cc_capture (cc_capture.py), the harness uses Claude
Code's own text, templated on the run's paths, with one fixed patch that removes the
billing header block and text about tools the harness does not provide. Without one
(CI, or before the capture), it uses the committed paraphrase, for dry runs only.
"""

import hashlib
import json
import re
from pathlib import Path

import config

TOOL_NAMES = ["Bash", "Edit", "Read", "Write"]
# The fixed text patch, applied identically in every arm. Its hash is recorded.
SYSTEM_PATCH = [
    ("# Session-specific guidance\n - When the user types `/<skill-name>`, invoke it via Skill. Only use skills "
     "listed in the user-invocable skills section — don't guess.\n\n", ""),
]
ENV_CUT_START = "\n\nThe following deferred tools"
ENV_CUT_END = "\n\nToday's date is"
CLAUDE_MD_HEADER = ("Codebase and user instructions are shown below. Be sure to adhere to these instructions. "
                    "IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly "
                    "as written.")
IMPORT_RE = re.compile(r"(?<![\w`@])@((?:\.{0,2}/)?[\w./-]+\.[A-Za-z0-9]+)")


def slug(path):
    return re.sub(r"[^A-Za-z0-9]", "-", str(path))


def templatize(text, root, home):
    text = text.replace(slug(root), "{slug}")
    text = text.replace(str(root), "{root}")
    return text.replace(str(home), "{home}")


def fill(text, root, home, nonce=""):
    return text.replace("{slug}", slug(root)).replace("{root}", str(root)).replace("{home}", str(home))


def claude_md_sections(root, max_depth=5):
    """CLAUDE.md files Claude Code would load for a project root, with @imports after the importer."""
    root = Path(root)
    seen = set()
    sections = []

    def visit(path, depth):
        real = path.resolve()
        if real in seen or depth > max_depth or not real.is_file():
            return
        try:
            real.relative_to(root.resolve())
        except ValueError:
            return
        seen.add(real)
        text = real.read_text(errors="replace")
        sections.append((path, text))
        in_fence = False
        for line in text.split("\n"):
            if line.strip().startswith("```"):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            stripped = re.sub(r"`[^`]*`", "", line)
            for match in IMPORT_RE.finditer(stripped):
                visit((path.parent / match.group(1)), depth + 1)

    visit(root / "CLAUDE.md", 0)
    return sections


def claude_md_block(root):
    sections = claude_md_sections(root)
    if not sections:
        return None
    parts = [CLAUDE_MD_HEADER]
    for path, text in sections:
        parts.append("Contents of %s (project instructions, checked into the codebase):\n\n%s" % (path, text.rstrip("\n")))
    return "<system-reminder>\n%s\n</system-reminder>" % "\n\n".join(parts)


class Profile:
    def __init__(self, source, system_blocks, tools, env_by_model, attribution_by_model, effort_by_model,
                 max_tokens_by_model, base_context_management, capture_hash):
        self.source = source
        self.system_blocks = system_blocks
        self.tools_list = tools
        self.env_by_model = env_by_model
        self.attribution_by_model = attribution_by_model
        self.effort_by_model = effort_by_model
        self.max_tokens_by_model = max_tokens_by_model
        self.base_context_management = base_context_management
        self.capture_hash = capture_hash
        self.patch_hash = hashlib.sha256(json.dumps(SYSTEM_PATCH).encode()).hexdigest()[:16]

    def system(self, nonce, root, home):
        blocks = [{"type": "text", "text": "Session: %s" % nonce}]
        for block in self.system_blocks:
            out = {"type": "text", "text": fill(block["text"], root, home)}
            if block.get("cache_control"):
                out["cache_control"] = block["cache_control"]
            blocks.append(out)
        return blocks

    def tools(self):
        return json.loads(json.dumps(self.tools_list))

    def effort(self, model):
        return self.effort_by_model.get(model) or config.MODELS[model]["api_default_effort"]

    def max_tokens(self, model, cap=config.MAX_OUTPUT_CAP):
        return min(int(self.max_tokens_by_model.get(model) or cap), int(cap))

    def model_pins(self, model, cap=config.MAX_OUTPUT_CAP):
        """What this profile sends for a model, for every result's pins."""
        env = self.env_by_model.get(model) or self.env_by_model.get("default")
        return {"effort": self.effort(model), "max_tokens": self.max_tokens(model, cap),
                "env_hash": config.sha256_json(env)[:12],
                "attribution_hash": config.sha256_json(self.attribution_by_model.get(model))[:12]}

    def first_messages(self, model, root, home, prompt, session_context=""):
        content = []
        block = claude_md_block(root)
        if block:
            content.append({"type": "text", "text": block})
        attribution = self.attribution_by_model.get(model)
        if attribution:
            content.append({"type": "text", "text": attribution})
        content.append({"type": "text", "text": prompt})
        env = fill(self.env_by_model.get(model) or self.env_by_model.get("default", ""), root, home)
        head, sep, tail = env.partition(ENV_CUT_END)
        if session_context:
            head = head + "\n\n" + session_context
        env = head + sep + tail
        return [{"role": "user", "content": content},
                {"role": "system", "content": [{"type": "text", "text": env}]}]


def _requests(path):
    rows = []
    with open(path) as handle:
        for line in handle:
            row = json.loads(line)
            if row.get("body") and row["path"].split("?")[0].endswith("/v1/messages"):
                rows.append(row)
    return rows


def _text(content):
    if isinstance(content, str):
        return content
    return "".join(b.get("text", "") for b in content if isinstance(b, dict))


def patch_env(text):
    start = text.find(ENV_CUT_START)
    end = text.find(ENV_CUT_END)
    if start >= 0 and end > start:
        text = text[:start] + text[end:]
    return text


def model_first_request(capture_dir, work, model):
    """The first request Claude Code sent for a model. Read from cc_capture/, where cc-capture writes it; an
    older store keeps it only in cc_capture_work/, so it is copied into cc_capture/ on first use. Missing in
    both places is an error: falling back would silently change effort, attribution and environment text."""
    frozen = capture_dir / ("first_request_model_%s.json" % model)
    if frozen.exists():
        return json.loads(frozen.read_text())["body"]
    log = work / ("requests_model_%s.jsonl" % model)
    rows = _requests(log) if log.exists() else []
    if not rows:
        raise ValueError("the Claude Code capture has no first request for %s (looked for %s and %s); "
                         "run cc-capture again" % (model, frozen, log))
    tmp = frozen.with_suffix(".tmp")
    tmp.write_text(json.dumps({"headers": rows[0].get("headers") or {}, "body": rows[0]["body"]}, indent=1))
    tmp.replace(frozen)
    return rows[0]["body"]


def profile_hash(system, tools, env_by_model, attribution_by_model, effort_by_model, max_tokens_by_model, base_cm):
    """A hash of the effective profile every request is built from, not of the capture files."""
    return config.sha256_json({"system": system, "tools": tools, "env": env_by_model,
                               "attribution": attribution_by_model, "effort": effort_by_model,
                               "max_tokens": max_tokens_by_model, "context_management": base_cm,
                               "patch": SYSTEM_PATCH})[:16]


def from_capture(home):
    capture_dir = Path(home) / "cc_capture"
    work = Path(home) / "cc_capture_work"
    capture = json.loads((capture_dir / "capture.json").read_text())
    first = json.loads((capture_dir / "first_request_tools.json").read_text())
    tools_meta = capture["sessions"]["tools"]
    root, cc_home = tools_meta["root"], tools_meta["home"]
    body = first["body"]
    system = []
    for block in body["system"][1:]:
        text = block["text"]
        for old, new in SYSTEM_PATCH:
            text = text.replace(old, new)
        out = {"type": "text", "text": templatize(text, root, cc_home)}
        if block.get("cache_control"):
            out["cache_control"] = block["cache_control"]
        system.append(out)
    tools = [t for t in body["tools"] if t.get("name") in TOOL_NAMES]
    env_by_model = {}
    attribution_by_model = {}
    effort_by_model = {}
    max_tokens_by_model = {}
    base_cm = body.get("context_management")
    for model in config.MODELS:
        mbody = model_first_request(capture_dir, work, model)
        env_text = patch_env(_text(mbody["messages"][1]["content"]))
        # Take the capture's paths from the captured text, so a moved store still templates correctly.
        found = re.search(r"Primary working directory: (\S+)", env_text)
        if not found:
            raise ValueError("captured environment for %s names no working directory" % model)
        mroot = found.group(1)
        mhome = str(Path(mroot).parent / ("home_model_" + model))
        env_by_model[model] = templatize(env_text, mroot, mhome)
        if "{root}" not in env_by_model[model]:
            raise ValueError("captured environment for %s did not template" % model)
        attribution_by_model[model] = _text(mbody["messages"][0]["content"][:1])
        effort_by_model[model] = (mbody.get("output_config") or {}).get("effort")
        max_tokens_by_model[model] = mbody.get("max_tokens")
    env_by_model["default"] = templatize(patch_env(_text(body["messages"][1]["content"])), root, cc_home)
    digest = profile_hash(system, tools, env_by_model, attribution_by_model, effort_by_model, max_tokens_by_model,
                          base_cm)
    return Profile("capture", system, tools, env_by_model, attribution_by_model, effort_by_model,
                   max_tokens_by_model, base_cm, digest)


def paraphrase():
    """Committed stand-in for dry runs: our own words, same structure."""
    text = (config.PROMPTS / "system_paraphrase.txt").read_text()
    tools = json.loads((config.DATA / "tools_paraphrase.json").read_text())
    env = ("# Environment\nYou have been invoked in the following environment: \n - Primary working directory: {root}\n"
           " - Is a git repository: true\n - Platform: darwin\n - Shell: zsh\n\nToday's date is %s." % config.FIXED_DATE)
    system = [{"type": "text", "text": "You are a coding agent working in a terminal.", "cache_control": {"type": "ephemeral"}},
              {"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}]
    effort = {m: "medium" for m in config.MODELS}
    max_tokens = {m: config.MAX_OUTPUT_CAP for m in config.MODELS}
    base_cm = {"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]}
    digest = profile_hash(system, tools, {"default": env}, {}, effort, max_tokens, base_cm)
    return Profile("paraphrase", system, tools, {"default": env}, {}, effort, max_tokens, base_cm,
                   "paraphrase-" + digest)


def load(home, source="auto"):
    capture = Path(home) / "cc_capture" / "capture.json"
    if source == "paraphrase" or (source == "auto" and not capture.exists()):
        return paraphrase()
    return from_capture(home)
