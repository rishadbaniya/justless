"""Mechanical checks over a dry run's logs (offline validation step 6).

  pairs      paired first requests differ only in allowed regions
  hooks      Justless arms rewrite Bash and outline a large Read; plain arms run no hook; no Grep|Glob event
  accounting the ledger, the per-run totals and costs recomputed from logged usage agree
  secrets    the planted parent-environment key appears in no request, transcript or log
  history    `env` shows no ANTHROPIC_* variable, `command -v justless` fails in plain arms, and
             `ls -lt` / `find -newer` do not single out a file
  ce         context-editing arms log applied_edits when the trigger is reached
"""

import json
import re
from pathlib import Path

import accounting
import fake_api

# Justless's session context: none on a fresh project, a `resume` pointer once it
# has runs or reads, and an index line from earlier builds.
CTX_RE = re.compile(r"\n\n(?:Justless indexed|`[^`\n]* resume` lists earlier)[^\n]*")


def read_jsonl(path):
    if not Path(path).exists():
        return []
    out = []
    with open(str(path)) as handle:
        for line in handle:
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    return out


def strip_cc(value):
    return fake_api.strip_cache_control(value)


def first_requests(fake_rows):
    """nonce -> first /v1/messages body."""
    out = {}
    for row in fake_rows:
        body = row.get("body") or {}
        if not row.get("path", "").startswith("/v1/messages") or not body.get("system"):
            continue
        text = json.dumps(body["system"][0])
        m = re.search(r"Session: ([0-9a-f]{16})", text)
        if m and m.group(1) not in out:
            out[m.group(1)] = body
    return out


def text_of(block):
    return block.get("text", "") if isinstance(block, dict) else str(block)


RUN_DIR = re.compile(r"[0-9a-f]{12}(?=/(?:repo|home)\b)")
RUN_SLUG = re.compile(r"[0-9a-f]{12}(?=-(?:repo|home)\b)")


def normalize_runs(value):
    """Each run lives in its own <runroot>/<12 hex> directory, the same length in every arm."""
    text = json.dumps(value)
    return json.loads(RUN_SLUG.sub("RUNDIR", RUN_DIR.sub("RUNDIR", text)))


def compare_pair(x, y, allow_ce):
    """Return a list of differences between two first requests outside the allowed regions."""
    problems = []
    x, y = normalize_runs(x), normalize_runs(y)
    sx, sy = strip_cc(x["system"]), strip_cc(y["system"])
    if sx[1:] != sy[1:]:
        problems.append("system differs beyond the nonce line")
    if strip_cc(x["tools"]) != strip_cc(y["tools"]):
        problems.append("tools differ")
    for key in sorted(set(x) | set(y)):
        if key in ("system", "tools", "messages"):
            continue
        if key == "context_management" and allow_ce:
            continue
        if x.get(key) != y.get(key):
            problems.append("field %s differs" % key)
    mx, my = strip_cc(x["messages"]), strip_cc(y["messages"])
    if len(mx) != len(my) or [m["role"] for m in mx] != [m["role"] for m in my]:
        return problems + ["message layout differs"]

    def without_claude_md(blocks):
        return [b for b in blocks if not text_of(b).startswith("<system-reminder>\nCodebase and user instructions")]
    if without_claude_md(mx[0]["content"]) != without_claude_md(my[0]["content"]):
        problems.append("first user message differs outside the CLAUDE.md block")
    env_x = CTX_RE.sub("", text_of(mx[1]["content"][0]))
    env_y = CTX_RE.sub("", text_of(my[1]["content"][0]))
    if env_x != env_y:
        problems.append("environment message differs outside the SessionStart context")
    return problems


def check(home, base_campaign, variants):
    base = Path(home) / "campaigns"
    fake_rows = read_jsonl(base / base_campaign / "fake_requests.jsonl")
    firsts = first_requests(fake_rows)
    report = {"pairs": [], "hooks": {}, "accounting": {}, "secrets": {}, "history": {}, "ce": {}}
    all_rows = []
    for variant in variants:
        directory = base / ("%s-%s" % (base_campaign, variant))
        rows = [r for r in read_jsonl(directory / "results.jsonl") if r.get("nonce")]
        for row in rows:
            row["_variant"] = variant
        all_rows += rows
        tools = read_jsonl(directory / "tools.jsonl")
        requests = read_jsonl(directory / "requests.jsonl")
        ledger = read_jsonl(directory / "ledger.jsonl")
        pricing = accounting.Pricing()
        recomputed = 0.0
        model_of = {r["run_id"]: r["model"] for r in rows}
        for q in requests:
            recomputed += pricing.cost(model_of.get(q["run_id"], "claude-haiku-5-5"), q["usage"])
        report["accounting"][variant] = {
            "ledger": round(sum(float(e.get("cost_usd") or 0) for e in ledger), 8),
            "results": round(sum(float((r.get("totals") or {}).get("cost_usd") or 0) for r in rows), 8),
            "recomputed_from_usage": round(recomputed, 8)}
        hook_runs = {}
        for r in rows:
            arm = r["arm"]
            item = hook_runs.setdefault(arm, {"hook_events": 0, "bash_rewritten": 0, "read_outlined": 0,
                                              "grep_glob_events": 0})
            item["hook_events"] += len(r.get("hook_events") or [])
            item["bash_rewritten"] += (r.get("tool_counts") or {}).get("Bash:rewritten", 0)
            item["read_outlined"] += (r.get("tool_counts") or {}).get("Read:outlined", 0)
            item["grep_glob_events"] += sum(1 for e in r.get("hook_events") or [] if e.get("match") in ("Grep", "Glob"))
        report["hooks"][variant] = hook_runs
        ce = {}
        for q in requests:
            if q.get("applied_edits"):
                ce[q["run_id"]] = ce.get(q["run_id"], 0) + len(q["applied_edits"])
        arms = {r["run_id"]: r["arm"] for r in rows}
        report["ce"][variant] = {arm: sum(v for k, v in ce.items() if arms.get(k) == arm) for arm in "abcd"}
    groups = {}
    for row in all_rows:
        groups.setdefault((row["_variant"], row["task_id"], row.get("rep")), {})[row["arm"]] = row
    for key, arms in sorted(groups.items()):
        for x, y, allow_ce in (("b", "a", False), ("d", "c", False), ("c", "a", True), ("d", "b", True)):
            if x in arms and y in arms:
                bx, by = firsts.get(arms[x]["nonce"]), firsts.get(arms[y]["nonce"])
                if bx is None or by is None:
                    continue
                problems = compare_pair(bx, by, allow_ce)
                report["pairs"].append({"variant": key[0], "task": key[1], "pair": "%s/%s" % (x, y),
                                        "ok": not problems, "problems": problems})
    planted = "sk-ant-FAKE-planted-parent-env-secret"
    hits = []
    for path in list(base.glob("%s*/**/*" % base_campaign)):
        if path.is_file():
            try:
                if planted in path.read_text(errors="ignore"):
                    hits.append(str(path))
            except OSError:
                pass
    report["secrets"] = {"planted_key_hits": hits}
    history = {"env_anthropic": [], "justless_visible_in_plain": [], "ls_dates": [], "find_newer": []}
    for row in all_rows:
        if row["_variant"] != "probe" or not row.get("transcript") or not Path(row["transcript"]).exists():
            continue
        messages = json.loads(Path(row["transcript"]).read_text())["messages"]
        calls = {}
        for message in messages:
            if message["role"] == "assistant":
                for block in message["content"]:
                    if block.get("type") == "tool_use":
                        calls[block["id"]] = block["input"].get("command", "")
            elif message["role"] == "user" and isinstance(message["content"], list):
                for block in message["content"]:
                    if block.get("type") != "tool_result":
                        continue
                    command = calls.get(block["tool_use_id"], "")
                    output = block.get("content") or ""
                    tag = "%s %s" % (row["task_id"], row["arm"])
                    if command.startswith("env "):
                        if "ANTHROPIC" in output:
                            history["env_anthropic"].append(tag)
                    elif command.startswith("command -v justless"):
                        if row["arm"] in ("a", "c") and "exit=0" in output:
                            history["justless_visible_in_plain"].append(tag)
                    elif command.startswith("ls -lt"):
                        dates = {" ".join(line.split()[5:8]) for line in output.split("\n")[1:] if line.strip()}
                        history["ls_dates"].append([tag, sorted(dates)])
                    elif command.startswith("touch probe_marker.txt"):
                        # Positive control: the file the probe just touched must be the only newer file.
                        found = [l.strip() for l in output.split("\n") if l.strip() and l.strip() != "done"]
                        if found != ["./probe_marker.txt"]:
                            history["find_newer"].append([tag, found[:5]])
    report["history"] = history
    # Search positive control: in the reference location runs the sandboxed grep (Claude Code's ugrep)
    # must print the truth file.
    import tasks as tasks_module
    truth = {t["id"]: t for t in tasks_module.load_public() + tasks_module.load_private(home)}
    search = {"checked": 0, "missing_truth": []}
    for row in all_rows:
        task = truth.get(row["task_id"])
        if row["_variant"] != "correct" or not task or task["family"] != "location" or not row.get("transcript"):
            continue
        if not Path(row["transcript"]).exists():
            continue
        messages = json.loads(Path(row["transcript"]).read_text())["messages"]
        first = next((b for m in messages if m["role"] == "user" and isinstance(m["content"], list)
                      for b in m["content"] if b.get("type") == "tool_result"), None)
        search["checked"] += 1
        if first is None or first.get("is_error") or task["truth"]["file"] not in (first.get("content") or ""):
            search["missing_truth"].append("%s %s" % (row["task_id"], row["arm"]))
    report["search"] = search
    ok = (all(p["ok"] for p in report["pairs"]) and not hits and not history["env_anthropic"]
          and not history["justless_visible_in_plain"] and not history["find_newer"]
          and not report["search"]["missing_truth"]
          and all(len(d[1]) <= 1 for d in history["ls_dates"])
          and all(abs(v["ledger"] - v["results"]) < 1e-4 and abs(v["ledger"] - v["recomputed_from_usage"]) < 1e-6
                  for v in report["accounting"].values()))
    for variant, arms in report["hooks"].items():
        for arm, item in arms.items():
            if arm in ("a", "c") and item["hook_events"]:
                ok = False
            if item["grep_glob_events"]:
                ok = False
    report["ok"] = ok
    return report
