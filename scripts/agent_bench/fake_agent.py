"""Scripted agents for --dry-run. They exercise the real loop, tools, hooks and oracles.

Variants per task: correct (reads then edits), unread_edit (edits without reading),
wrong, tamper, qualified (answers in tool-printed forms), cfgtest (a #[cfg(test)] twin of the
buggy function) and probe (environment, leakage and history checks). Each returns a
list of fake_api steps; paths use the {root} placeholder.
"""

import json

from fake_api import final, tool_use
from tasks import kind

FENCE = "```"


def answer_block(value):
    return "%sanswer\n%s\n%s" % (FENCE, json.dumps(value), FENCE)


def _bash(command, timeout=600000):
    return tool_use("Bash", {"command": command, "description": "Run a command", "timeout": timeout})


def _test_command(task):
    if kind(task) == "lint":
        return task.get("visible_command") or "cargo clippy --offline --all-targets -- -D warnings"
    return task.get("visible_command") or "cargo test --offline"


def _persisted_read(tool_id, command):
    """A command, then a Read of its saved output as an agent does after Claude Code's 2 KB preview. The Read
    is taken only when the result really was saved (fake_api's when_persisted); output shown inline, or
    compacted by a hook below the threshold, leaves nothing to read."""
    step = _bash(command)
    step["content"][-1]["id"] = tool_id
    return [step, {"when_persisted": tool_use("Read", {"file_path": "{persist}/%s.txt" % tool_id})}]


def large_plain_path(task):
    """The large family's scripted plain path: the outputs a plain agent reads on the way to the fix, with no
    narrowing the prompt does not give. dry-run measures its tool bytes."""
    k = kind(task)
    if k == "callers":
        symbol = task["oracle"]["symbol"]
        files = sorted({u["file"] for u in task["truth"]["required"]})
        steps = [_bash("grep -rnw --include='*.rs' '%s' ." % symbol)]
        steps += [tool_use("Read", {"file_path": "{root}/" + f}) for f in files]
        return steps + [final("These functions.\n" + answer_block({"callers": [
            {"file": u["file"], "symbol": u["symbol"]} for u in task["truth"]["required"]]}))]
    files = sorted({p["file"] for p in task["controls"]["inverse"]})
    if k == "lint":
        steps = _persisted_read("toolu_large_lint_1", _test_command(task))
        steps += [tool_use("Read", {"file_path": "{root}/" + f}) for f in files]
        steps += _edits(task)
        steps.append(_bash(_test_command(task)))
        steps.append(_bash(" ".join(task["oracle"]["test_command"])))
        return steps + [final("Fixed it.")]
    steps = [_bash(_test_command(task))]
    if k == "regression":
        steps.append(_bash("git show --stat HEAD"))
        steps += _persisted_read("toolu_large_show_1", "git show HEAD")
    else:
        steps.append(_bash("%s 2>&1 | grep -E '^test .* FAILED$|panicked at'" % _test_command(task)))
    steps += [tool_use("Read", {"file_path": "{root}/" + f}) for f in files]
    steps += _edits(task)
    steps.append(_bash(_test_command(task)))
    return steps + [final("Fixed it.")]


def _edits(task, reverse=False):
    pairs = (task.get("controls") or {}).get("inverse") or []
    out = []
    for pair in pairs:
        old, new = (pair["new"], pair["old"]) if reverse else (pair["old"], pair["new"])
        out.append(tool_use("Edit", {"file_path": "{root}/" + pair["file"], "old_string": old, "new_string": new}))
    return out


def correct(task, read_first=True):
    family = kind(task)
    if task["family"] == "large" and read_first:
        return large_plain_path(task)
    if family == "callers":
        return [final("These functions.\n" + answer_block({"callers": [
            {"file": u["file"], "symbol": u["symbol"]} for u in task["truth"]["required"]]}))]
    if family in ("bugfix", "lint", "regression"):
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
    family = kind(task)
    if family == "regression":
        # Reverting the whole cleanup commit fixes the tests but throws the cleanup away.
        return [_bash(_test_command(task)), _bash("git checkout HEAD~1 -- ."), _bash(_test_command(task)),
                final("Reverted the commit.")]
    if family == "callers":
        first = task["truth"]["required"][0]
        return [final(answer_block({"callers": [{"file": first["file"], "symbol": first["symbol"]}]}))]
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
    family = kind(task)
    if family in ("bugfix", "regression"):
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


def qualified(task):
    """Answers in the forms navigation tools print: an absolute file:line path and a module-qualified symbol.
    Edit families run the correct fix. Every family must succeed."""
    family = kind(task)
    if family == "callers":
        units = task["truth"]["required"]
        entries = [{"file": ("{root}/" if i % 2 else "./") + u["file"],
                    "symbol": "%s::%s" % (u["file"].rsplit(".", 1)[0].replace("/", "::"), u["symbol"])}
                   for i, u in enumerate(units)]
        return [_bash("grep -rnw '%s' ." % task["oracle"]["symbol"]),
                final("These functions.\n" + answer_block({"callers": entries}))]
    if family == "location":
        truth = task["truth"]
        symbol = truth.get("symbol") or ""
        if symbol:
            module = truth["file"].rsplit(".", 1)[0].replace("/", "::")
            symbol = "%s::%s" % (module, symbol)
        path = "{root}/%s:%d" % (truth["file"], truth.get("line") or truth["span"][0])
        return [_bash("grep -rn -F '%s' ." % truth.get("literal", "")[:40].replace("'", "")),
                final("Found it.\n" + answer_block({"answers": [{"file": path, "symbol": symbol,
                                                                 "line": truth["span"][0]}]}))]
    if family == "impact":
        required = task["truth"]["required"]
        files = ["{root}/" + f for f in required[:1]] + ["./" + f for f in required[1:]]
        return [_bash("grep -rlw '%s' ." % task["oracle"]["symbol"]),
                final("These files.\n" + answer_block({"files": files}))]
    return correct(task, True)


def cfg_twin(task):
    """Keeps the bug in production code and the fix under #[cfg(test)]: tests pass, the oracle must refuse it."""
    oracle = task["oracle"]
    rel = oracle["mutated_file"]
    fixed = task["controls"]["inverse_files"][rel]
    mutated = fixed
    for pair in task["controls"]["inverse"]:
        if pair["file"] == rel:
            mutated = mutated.replace(pair["new"], pair["old"], 1)
    start, end = oracle["mutated_span"]
    good, bad = fixed.split("\n"), mutated.split("\n")
    twin = ["#[cfg(test)]"] + good[start:end + 1] + ["#[cfg(not(test))]"] + bad[start:end + 1]
    content = "\n".join(bad[:start] + twin + bad[end + 1:])
    return [_bash(_test_command(task)), tool_use("Write", {"file_path": "{root}/" + rel, "content": content}),
            _bash(_test_command(task)), final("Fixed it.")]


def overrun(task, justless):
    """Does the task, then keeps calling tools until the turn limit stops the run: the oracle must still
    grade the final state (success, stopped_by turn_limit)."""
    steps = [s for s in correct(task, True) if s.get("stop_reason") == "tool_use"]
    if kind(task) in ("location", "impact", "callers"):
        answer = correct(task, True)[-1]["content"][0]["text"]
        steps.append(tool_use("Bash", {"command": "true", "description": "Check", "timeout": 600000}, text=answer))
    return steps + [_bash("true")] * 80


def applies(variant, task):
    """Whether a validation variant is meaningful for a task: cfgtest needs a Rust bug fix, and qualified
    answers exist only for the answer families."""
    if variant == "cfgtest":
        return kind(task) in ("bugfix", "regression") and task["language"] == "rust"
    if variant == "qualified":
        return kind(task) in ("location", "impact", "callers")
    return True


def noop(task):
    """Runs the visible check and changes nothing: the mutation must make the oracle fail."""
    if kind(task) in ("bugfix", "lint", "regression"):
        return [_bash(_test_command(task)), final("No change.")]
    return [final("No answer.")]


def bulk_read(task):
    """Reads several files whole, so server-side context editing has old tool results to clear."""
    files = [f["file"] if isinstance(f, dict) else f for f in (task.get("truth") or {}).get("required") or []]
    files = sorted(set(files), key=files.index)
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
    "qualified": lambda task, justless: qualified(task),
    "cfgtest": lambda task, justless: cfg_twin(task),
    "overrun": overrun,
}

# Variants whose runs must end on a specific stop, besides their graded failure mode.
EXPECTED_STOP = {"overrun": "turn_limit"}

EXPECTED = {
    "noop": {"bugfix": "wrong_fix", "lint": "lint_remaining", "location": "no_answer", "impact": "no_answer",
             "callers": "no_answer", "regression": "wrong_fix"},
    "correct": "success",
    "unread_edit": "success",
    "wrong": {"bugfix": "wrong_fix", "lint": "lint_remaining", "location": "wrong_answer", "impact": "partial_answer",
              "callers": "partial_answer", "regression": "reverted_cleanup"},
    "tamper": {"bugfix": "test_tamper", "lint": "lint_suppressed", "location": "wrong_answer",
               "impact": "partial_answer", "callers": "partial_answer", "regression": "test_tamper"},
    "qualified": "success",
    "cfgtest": {"bugfix": "test_tamper", "regression": "test_tamper"},
    "overrun": "success",
}


def expected(variant, task):
    value = EXPECTED.get(variant)
    if isinstance(value, dict):
        return value.get(kind(task))
    return value
