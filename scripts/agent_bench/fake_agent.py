"""Scripted agents for --dry-run. They exercise the real loop, tools, hooks and oracles.

Variants per task: correct (reads then edits), unread_edit (edits without reading),
wrong, tamper, and probe (environment, leakage and history checks). Each returns a
list of fake_api steps; paths use the {root} placeholder.
"""

import json

from fake_api import final, tool_use

FENCE = "```"


def answer_block(value):
    return "%sanswer\n%s\n%s" % (FENCE, json.dumps(value), FENCE)


def _bash(command, timeout=600000):
    return tool_use("Bash", {"command": command, "description": "Run a command", "timeout": timeout})


def _test_command(task):
    if task["family"] == "lint":
        return "cargo clippy --offline --all-targets -- -D warnings"
    return task.get("visible_command") or "cargo test --offline"


def _edits(task, reverse=False):
    pairs = (task.get("controls") or {}).get("inverse") or []
    out = []
    for pair in pairs:
        old, new = (pair["new"], pair["old"]) if reverse else (pair["old"], pair["new"])
        out.append(tool_use("Edit", {"file_path": "{root}/" + pair["file"], "old_string": old, "new_string": new}))
    return out


def correct(task, read_first=True):
    family = task["family"]
    if family in ("bugfix", "lint"):
        steps = [_bash(_test_command(task))]
        files = sorted({p["file"] for p in task["controls"]["inverse"]})
        if read_first:
            steps += [tool_use("Read", {"file_path": "{root}/" + f}) for f in files]
        steps += _edits(task)
        steps.append(_bash(_test_command(task)))
        if family == "lint":
            steps.append(_bash("cargo test --offline"))
        steps.append(final("Fixed it."))
        return steps
    if family == "location":
        truth = task["truth"]
        needle = truth.get("literal", "")[:40].replace("'", "")
        return [_bash("grep -rn -F '%s' ." % needle),
                tool_use("Read", {"file_path": "{root}/" + truth["file"]}),
                final("Found it.\n" + answer_block({"answers": [{"file": truth["file"], "symbol": truth.get("symbol") or "",
                                                                 "line": truth["span"][0]}]}))]
    truth = task["truth"]
    return [_bash("grep -rlw '%s' ." % task["oracle"]["symbol"]),
            final("These files.\n" + answer_block({"files": truth["required"]}))]


def wrong(task):
    family = task["family"]
    if family == "bugfix":
        oracle = task["oracle"]
        return [_bash(_test_command(task)),
                tool_use("Edit", {"file_path": "{root}/" + oracle["mutated_file"],
                                  "old_string": oracle["anchor_line"],
                                  "new_string": oracle["anchor_line"] + oracle["noop_comment"]}),
                _bash(_test_command(task)), final("Tried a fix.")]
    if family == "lint":
        return [_bash(_test_command(task)), final("Left it.")]
    if family == "location":
        truth = task["truth"]
        return [final(answer_block({"answers": [{"file": "README.md", "symbol": "nothing"},
                                                {"file": truth["file"], "symbol": truth.get("symbol") or ""}]}))]
    return [final(answer_block({"files": [task["oracle"]["definition_file"]]}))]


def tamper(task):
    family = task["family"]
    if family == "bugfix":
        oracle = task["oracle"]
        if task["language"] == "python":
            return [tool_use("Write", {"file_path": "{root}/bench_tests/test_extra.py", "content": "pass\n"}),
                    final("Changed the tests.")]
        target = oracle.get("tamper_edit")
        if target:
            return [tool_use("Edit", {"file_path": "{root}/" + target["file"], "old_string": target["old"],
                                      "new_string": target["new"]}), final("Ignored the failing test.")]
        return [tool_use("Write", {"file_path": "{root}/tests/bench_tamper.rs", "content": "#[test]\nfn t() {}\n"}),
                final("Added a test file.")]
    if family == "lint":
        files = sorted({p["file"] for p in task["controls"]["inverse"]})
        path = "{root}/" + files[0]
        return [tool_use("Read", {"file_path": path}),
                _bash("printf '#![allow(clippy::all)]\\n' | cat - %s > %s.new && mv %s.new %s" % (
                    files[0], files[0], files[0], files[0])),
                final("Silenced it.")]
    return wrong(task)


def probe(task, justless):
    """Leakage, environment and history checks. Results are asserted by run.py dry-run."""
    steps = [
        _bash("env | cut -d= -f1 | sort | tr '\\n' ' '"),
        _bash("command -v justless; echo exit=$?"),
        _bash("ls -lt | head -5"),
        _bash("touch probe_marker.txt && find . -newer .git/HEAD -type f -not -path './target/*' "
              "-not -path './.git/*' -not -path './.justless/*'; echo done"),
        _bash("git log --oneline | head -3; git status --short | head -5"),
        _bash("cat {store}/tasks/%s.json 2>&1 | head -2; ls {runroot} 2>&1 | head -3" % task["id"]),
        _bash("curl -sS -m 5 https://example.com -o /dev/null; echo curl_exit=$?"),
        _bash("echo leak > {outside}/leak.txt; echo write_exit=$?"),
    ]
    if justless:
        steps.append(_bash("justless --version"))
    steps.append(final("Probe done."))
    return steps


def noop(task):
    """Runs the visible check and changes nothing: the mutation must make the oracle fail."""
    if task["family"] in ("bugfix", "lint"):
        return [_bash(_test_command(task)), final("No change.")]
    return [final("No answer.")]


def bulk_read(task):
    """Reads several files whole, so server-side context editing has old tool results to clear."""
    files = list((task.get("truth") or {}).get("required") or [])
    if not files:
        files = sorted({p["file"] for p in (task.get("controls") or {}).get("inverse") or []})
    steps = [tool_use("Read", {"file_path": "{root}/" + f}) for f in files[:8]]
    steps.append(_bash("git status --short"))
    steps.append(final("Read everything."))
    return steps


VARIANTS = {
    "bulk_read": lambda task, justless: bulk_read(task),
    "noop": lambda task, justless: noop(task),
    "correct": lambda task, justless: correct(task, True),
    "unread_edit": lambda task, justless: correct(task, False),
    "wrong": lambda task, justless: wrong(task),
    "tamper": lambda task, justless: tamper(task),
    "probe": probe,
}

EXPECTED = {
    "noop": {"bugfix": "wrong_fix", "lint": "lint_remaining", "location": "no_answer", "impact": "no_answer"},
    "correct": "success",
    "unread_edit": "success",
    "wrong": {"bugfix": "wrong_fix", "lint": "lint_remaining", "location": "wrong_answer", "impact": "partial_answer"},
    "tamper": {"bugfix": "test_tamper", "lint": "lint_suppressed", "location": "wrong_answer",
               "impact": "partial_answer"},
}


def expected(variant, task):
    value = EXPECTED.get(variant)
    if isinstance(value, dict):
        return value[task["family"]]
    return value
