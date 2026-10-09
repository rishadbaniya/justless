"""Family oracles. They never run Justless; they read files and run cargo, clippy or unittest.

grade(task, root, reference, runner) returns {success, failure_mode, details}, where
`reference` maps relative path -> bytes for the tree the agent started from and `runner`
executes an argv inside the run's sandbox and returns (exit_code, combined_output).
"""

import difflib
import json
import os
import re
from pathlib import Path, PurePosixPath

IGNORED_PREFIXES = ("target/", ".justless/", ".git/", "tool-results/")
IGNORED_FILES = {"Cargo.lock"}
TEST_PATH = re.compile(r"(^|/)(tests|benches|bench_tests)/|_test\.rs$|(^|/)test_[^/]*\.py$|(^|/)[^/]*_test\.py$")
CONFIG_TAMPER = re.compile(r"(^|/)(build\.rs|rust-toolchain(\.toml)?|clippy\.toml|\.clippy\.toml)$|(^|/)\.cargo/config")
SUPPRESSION = re.compile(r"#!?\[\s*(allow|expect)\s*\(|clippy::")
TEST_ATTR = re.compile(r"#\[\s*(?:cfg\s*\(\s*(?:all\s*\(\s*)?test\b|(?:[A-Za-z_]\w*::)*test\s*[\](]|"
                       r"(?:[A-Za-z_]\w*::)*rstest\b)")
ADDED_TAMPER = re.compile(r"#\[\s*ignore\b|#\[\s*should_panic\b|cfg\s*\(\s*any\s*\(\s*\)\s*\)")
ANSWER_BLOCK = re.compile(r"```answer[^\n]*\n(.*?)```", re.S)
TEST_LINE = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored|bench)(?:,.*)?$")
RUNNING = re.compile(r"^\s*(?:Running|Doc-tests)\s+(?:unittests\s+)?(\S+)")


# ----------------------------------------------------------------------------- lexing

def rust_mask(text):
    """Replace comments, strings and char literals with spaces, keeping newlines and length."""
    out = list(text)
    i = 0
    n = len(text)

    def blank(a, b):
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif text.startswith("/*", i):
            depth = 1
            j = i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif c in "rb" and re.match(r"(?:br|rb|r)#*\"", text[i:i + 70]) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            m = re.match(r"(?:br|rb|r)(#*)\"", text[i:])
            hashes = m.group(1)
            end = text.find('"' + hashes, i + m.end())
            j = n if end < 0 else end + 1 + len(hashes)
            blank(i, j)
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            blank(i, j + 1)
            i = j + 1
        elif c == "'":
            if i + 1 < n and text[i + 1] == "\\":
                j = text.find("'", i + 2)
                j = n if j < 0 else j
                blank(i, j + 1)
                i = j + 1
            elif i + 2 < n and text[i + 2] == "'":
                blank(i, i + 3)
                i += 3
            else:
                i += 1
        else:
            i += 1
    return "".join(out)


def match_brace(masked, open_index):
    depth = 0
    for j in range(open_index, len(masked)):
        ch = masked[j]
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return j
    return len(masked) - 1


def line_of(text, index):
    return text.count("\n", 0, index)


def rust_test_regions(text):
    """Line ranges [start, end] (0-based, inclusive) of #[cfg(test)] items and #[test] functions."""
    masked = rust_mask(text)
    regions = []
    for match in TEST_ATTR.finditer(masked):
        start = match.start()
        j = match.end()
        depth = 0
        while j < len(masked):
            ch = masked[j]
            if ch in "([":
                depth += 1
            elif ch in ")]":
                depth -= 1
            elif depth <= 0 and ch == "{":
                end = match_brace(masked, j)
                regions.append((line_of(text, start), line_of(text, end)))
                break
            elif depth <= 0 and ch == ";":
                regions.append((line_of(text, start), line_of(text, j)))
                break
            j += 1
    return regions


def external_test_modules(rel, text):
    """Files that hold `#[cfg(test)] mod name;` modules declared in rel."""
    masked = rust_mask(text)
    out = []
    for match in re.finditer(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:pub\s+)?mod\s+(\w+)\s*;", masked):
        name = match.group(1)
        base = PurePosixPath(rel).parent
        stem = PurePosixPath(rel).stem
        if stem not in ("lib", "main", "mod"):
            base = base / stem
        out += [str(base / (name + ".rs")), str(base / name / "mod.rs")]
    return out


def rust_functions(text):
    """[(name, qualified, start_line, end_line)] for fn items, qualified with their impl type."""
    masked = rust_mask(text)
    impls = []
    for match in re.finditer(r"\bimpl\b(?:\s*<[^{};]*?>)?\s+(?:[\w:<>,\s&']+?\s+for\s+)?([A-Za-z_]\w*)[^{;]*\{", masked):
        open_index = match.end() - 1
        impls.append((match.group(1), open_index, match_brace(masked, open_index)))
    out = []
    for match in re.finditer(r"\bfn\s+([A-Za-z_]\w*)", masked):
        j = match.end()
        depth = 0
        body = None
        while j < len(masked):
            ch = masked[j]
            if ch in "(<[":
                depth += 1
            elif ch in ")>]":
                depth -= 1
            elif ch == ";" and depth <= 0:
                break
            elif ch == "{" and depth <= 0:
                body = j
                break
            j += 1
        if body is None:
            continue
        end = match_brace(masked, body)
        owner = None
        for name, a, b in impls:
            if a < match.start() < b:
                owner = name
        name = match.group(1)
        out.append((name, "%s::%s" % (owner, name) if owner else name, line_of(text, match.start()),
                    line_of(text, end), match.start(), end))
    return out


# ----------------------------------------------------------------------------- diffs

def tree_changes(root, reference):
    """{rel: (before_bytes_or_None, after_bytes_or_None)} for changed, added and deleted files."""
    root = Path(root)
    current = {}
    for dirpath, dirnames, filenames in os.walk(str(root)):
        rel_dir = os.path.relpath(dirpath, str(root))
        rel_dir = "" if rel_dir == "." else rel_dir.replace(os.sep, "/") + "/"
        dirnames[:] = [d for d in dirnames if not (rel_dir + d + "/").startswith(IGNORED_PREFIXES)]
        for name in filenames:
            rel = rel_dir + name
            if rel.startswith(IGNORED_PREFIXES) or PurePosixPath(rel).name in IGNORED_FILES:
                continue
            path = Path(dirpath) / name
            if path.is_symlink() or not path.is_file():
                continue
            current[rel] = path.read_bytes()
    changes = {}
    for rel in set(current) | set(reference):
        if rel.startswith(IGNORED_PREFIXES) or PurePosixPath(rel).name in IGNORED_FILES:
            continue
        before = reference.get(rel)
        after = current.get(rel)
        if before != after:
            changes[rel] = (before, after)
    return changes


def changed_ranges(before, after):
    """Baseline line ranges touched by the edit: (start, end_exclusive, kind)."""
    a = before.decode("utf-8", "replace").split("\n") if before is not None else []
    b = after.decode("utf-8", "replace").split("\n") if after is not None else []
    out = []
    for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes():
        if tag != "equal":
            out.append((i1, i2, tag, b[j1:j2]))
    return out


def overlaps(ranges, regions):
    for i1, i2, tag, _ in ranges:
        for start, end in regions:
            if tag == "insert":
                if start < i1 <= end:
                    return True
            elif i1 <= end and i2 - 1 >= start:
                return True
    return False


def added_lines(changes):
    out = []
    for rel, (before, after) in changes.items():
        for _, _, _, lines in changed_ranges(before, after):
            out += [(rel, line) for line in lines]
    return out


def tamper(changes, reference):
    """Return a reason string if the diff tampers with tests or test configuration."""
    test_files = set()
    for rel, data in reference.items():
        if rel.endswith(".rs"):
            test_files.update(external_test_modules(rel, data.decode("utf-8", "replace")))
    for rel, (before, after) in sorted(changes.items()):
        if TEST_PATH.search(rel) or rel in test_files:
            return "edited test file %s" % rel
        if CONFIG_TAMPER.search(rel):
            return "edited test or build configuration %s" % rel
        if rel.endswith("Cargo.toml") and before is not None:
            for _, _, _, lines in changed_ranges(before, after):
                pass
            diff = difflib.unified_diff(before.decode("utf-8", "replace").split("\n"),
                                        (after or b"").decode("utf-8", "replace").split("\n"), lineterm="", n=0)
            for line in diff:
                if line[:1] in "+-" and not line.startswith(("+++", "---")):
                    if re.search(r"\b(test|harness|bench|autotests|autobenches|doctest)\b|\[lib\]|\[\[(test|bench)\]\]|\[lints",
                                 line):
                        return "edited test keys in %s" % rel
        if rel.endswith(".rs") and before is not None and after is not None:
            regions = rust_test_regions(before.decode("utf-8", "replace"))
            if regions and overlaps(changed_ranges(before, after), regions):
                return "edited test code in %s" % rel
    for rel, line in added_lines(changes):
        if ADDED_TAMPER.search(line):
            return "added test attribute in %s" % rel
    return None


def touched_span(changes, file, start, end):
    if file not in changes:
        return False
    before, after = changes[file]
    return overlaps(changed_ranges(before, after), [(start, end)])


def exact_revert(root, task):
    inverse = (task.get("controls") or {}).get("inverse_files") or {}
    for rel, text in inverse.items():
        path = Path(root) / rel
        if not path.exists() or path.read_text(errors="replace") != text:
            return False
    return bool(inverse)


# ----------------------------------------------------------------------------- cargo

def parse_tests(output):
    """{binary::test: status} from libtest output, attributing tests to the binary that ran them."""
    results = {}
    binary = "?"
    for line in output.split("\n"):
        line = line.rstrip("\r")
        m = RUNNING.match(line)
        if m:
            target = m.group(1)
            if line.strip().startswith("Doc-tests"):
                binary = "doc:" + target
            else:
                name = PurePosixPath(target.strip("()")).name
                binary = re.sub(r"-[0-9a-f]{16}$", "", name)
                inner = re.search(r"\(([^)]+)\)", line)
                if inner:
                    binary = re.sub(r"-[0-9a-f]{16}$", "", PurePosixPath(inner.group(1)).name)
                binary = "%s@%s" % (binary, target)
            continue
        m = TEST_LINE.match(line)
        if m:
            results["%s::%s" % (binary, m.group(1))] = m.group(2)
    return results


def missing_names(baseline, results, flaky):
    missing = []
    for name, status in baseline.items():
        if status != "ok" or name in flaky:
            continue
        if results.get(name) != "ok":
            missing.append(name)
    return missing


def compile_failed(output):
    return bool(re.search(r"^error(\[E\d+\])?:", output, re.M)) and "could not compile" in output


def run_rust_tests(task, runner):
    command = task["oracle"].get("test_command") or ["cargo", "test", "--offline", "--no-fail-fast"]
    code, output = runner(command, task["oracle"].get("test_timeout", 1800))
    return code, output, parse_tests(output)


def clippy_messages(output):
    """Clippy and compiler warnings and errors, once per (code, file, line): --all-targets compiles a
    library twice (lib and lib test), so each diagnostic can appear twice."""
    out = []
    seen = set()
    for line in output.split("\n"):
        if not line.startswith("{"):
            continue
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get("reason") != "compiler-message":
            continue
        message = item.get("message") or {}
        if message.get("level") in ("warning", "error"):
            code = (message.get("code") or {}).get("code")
            span = (message.get("spans") or [{}])[0] or {}
            key = (code, span.get("file_name"), span.get("line_start"), message.get("message"))
            if key in seen:
                continue
            seen.add(key)
            out.append({"code": code, "level": message.get("level"), "message": message.get("message"),
                        "file": span.get("file_name"), "line": span.get("line_start")})
    return out


# ----------------------------------------------------------------------------- answers

def parse_answer(text):
    blocks = ANSWER_BLOCK.findall(text or "")
    if not blocks:
        return None
    try:
        return json.loads(blocks[-1].strip())
    except ValueError:
        return None


def norm_path(path, root=None):
    path = str(path or "").strip().strip("`")
    if root and path.startswith(str(root)):
        path = path[len(str(root)):]
    while path.startswith("./"):
        path = path[2:]
    return path.lstrip("/")


def norm_symbol(symbol):
    symbol = str(symbol or "").strip().strip("`")
    symbol = re.sub(r"\(.*\)$", "", symbol)
    symbol = re.sub(r"<[^<>]*>", "", symbol)
    symbol = re.sub(r"<[^<>]*>", "", symbol)
    symbol = symbol.replace(".", "::").replace("()", "")
    return symbol.split(" ")[-1].strip(":")


def grade_location(task, final_text, root=None):
    truth = task["truth"]
    answer = parse_answer(final_text)
    if not isinstance(answer, dict) or not isinstance(answer.get("answers"), list) or not answer["answers"]:
        return {"success": False, "failure_mode": "no_answer", "details": {"answer": answer}}
    first = answer["answers"][0] if isinstance(answer["answers"][0], dict) else {}
    file_ok = norm_path(first.get("file"), root) == truth["file"]
    symbol = norm_symbol(first.get("symbol"))
    accepted = {norm_symbol(s) for s in truth.get("accepted_symbols") or [truth.get("symbol")] if s}
    if truth.get("symbol") is None:
        line = first.get("line")
        symbol_ok = isinstance(line, int) and truth["span"][0] - 3 <= line <= truth["span"][1] + 3
    else:
        symbol_ok = symbol in accepted
    success = file_ok and symbol_ok
    return {"success": success, "failure_mode": "success" if success else "wrong_answer",
            "details": {"answer": first, "file_ok": file_ok, "symbol_ok": symbol_ok}}


def grade_impact(task, final_text, root=None):
    truth = task["truth"]
    answer = parse_answer(final_text)
    if not isinstance(answer, dict) or not isinstance(answer.get("files"), list):
        return {"success": False, "failure_mode": "no_answer", "details": {"answer": answer},
                "recall": 0.0, "precision": 0.0, "f1": 0.0}
    files = {norm_path(f, root) for f in answer["files"] if isinstance(f, str)}
    required = set(truth["required"])
    neutral = set(truth.get("neutral") or [])
    hit = files & required
    recall = len(hit) / float(len(required)) if required else 1.0
    counted = files - neutral
    precision = len(hit) / float(len(counted)) if counted else 0.0
    f1 = 2 * recall * precision / (recall + precision) if recall + precision else 0.0
    thresholds = task["oracle"]
    success = recall >= thresholds.get("recall", 0.9) and precision >= thresholds.get("precision", 0.8)
    mode = "success" if success else ("partial_answer" if hit else "wrong_answer")
    return {"success": success, "failure_mode": mode, "recall": round(recall, 4), "precision": round(precision, 4),
            "f1": round(f1, 4), "details": {"missing": sorted(required - files), "extra": sorted(counted - required)}}


# ----------------------------------------------------------------------------- edit families

def grade_bugfix(task, root, reference, runner):
    changes = tree_changes(root, reference)
    oracle = task["oracle"]
    details = {"changed_files": sorted(changes)}
    reason = tamper(changes, reference)
    if task["language"] == "python":
        hidden = oracle.get("hidden_tests") or {}
        visible = oracle.get("visible_tests") or {}
        if reason is None:
            for rel in changes:
                if rel.startswith("bench_tests/"):
                    reason = "edited harness tests"
        for rel, text in list(visible.items()) + list(hidden.items()):
            path = Path(root) / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        if reason:
            return {"success": False, "failure_mode": "test_tamper", "details": dict(details, tamper=reason)}
        code, output = runner(oracle["test_command"], 600)
        details["tests_exit"] = code
        details["output_tail"] = output[-1500:]
        touched = touched_span(changes, oracle["mutated_file"], *oracle["mutated_span"])
        details["touched_site"] = touched
        if code != 0:
            return {"success": False, "failure_mode": "wrong_fix", "details": details}
        if not touched:
            return {"success": False, "failure_mode": "untouched_site", "details": details}
        details["exact_revert"] = exact_revert(root, task)
        return {"success": True, "failure_mode": "success", "details": details}
    if reason:
        return {"success": False, "failure_mode": "test_tamper", "details": dict(details, tamper=reason)}
    code, output, results = run_rust_tests(task, runner)
    flaky = set(task.get("baseline", {}).get("flaky") or [])
    missing = missing_names(task["baseline"]["test_names"], results, flaky)
    details.update(tests_exit=code, tests_seen=len(results), missing=missing[:20], missing_count=len(missing),
                   failed=sorted(n for n, s in results.items() if s == "FAILED")[:20])
    touched = touched_span(changes, oracle["mutated_file"], *oracle["mutated_span"])
    details["touched_site"] = touched
    details["exact_revert"] = exact_revert(root, task)
    if code != 0 and compile_failed(output):
        details["output_tail"] = output[-1500:]
        return {"success": False, "failure_mode": "build_broken", "details": details}
    if missing or not results:
        return {"success": False, "failure_mode": "wrong_fix", "details": details}
    if not touched:
        return {"success": False, "failure_mode": "untouched_site", "details": details}
    return {"success": True, "failure_mode": "success", "details": details}


def grade_lint(task, root, reference, runner):
    changes = tree_changes(root, reference)
    details = {"changed_files": sorted(changes)}
    for rel, line in added_lines(changes):
        if SUPPRESSION.search(line):
            return {"success": False, "failure_mode": "lint_suppressed",
                    "details": dict(details, suppression="%s: %s" % (rel, line.strip()[:120]))}
    reason = tamper(changes, reference)
    if reason:
        mode = "lint_suppressed" if "clippy" in reason or "lints" in reason else "test_tamper"
        return {"success": False, "failure_mode": mode, "details": dict(details, tamper=reason)}
    command = task["oracle"].get("clippy_command") or [
        "cargo", "clippy", "--offline", "--all-targets", "--message-format=json", "--", "-D", "warnings"]
    code, output = runner(command, 1800)
    messages = clippy_messages(output)
    details["clippy_exit"] = code
    details["clippy_residue"] = messages[:20]
    if code != 0 or messages:
        if compile_failed(output) and not any((m.get("code") or "").startswith("clippy::") for m in messages):
            return {"success": False, "failure_mode": "build_broken", "details": details}
        return {"success": False, "failure_mode": "lint_remaining", "details": details}
    code, output, results = run_rust_tests(task, runner)
    flaky = set(task.get("baseline", {}).get("flaky") or [])
    missing = missing_names(task["baseline"]["test_names"], results, flaky)
    details.update(tests_exit=code, missing=missing[:20], missing_count=len(missing))
    if code != 0 and compile_failed(output):
        return {"success": False, "failure_mode": "build_broken", "details": details}
    if missing or not results:
        return {"success": False, "failure_mode": "wrong_fix", "details": details}
    return {"success": True, "failure_mode": "success", "details": details}


def grade(task, root, reference, runner, final_text):
    family = task["family"]
    if family == "bugfix":
        return grade_bugfix(task, root, reference, runner)
    if family == "lint":
        return grade_lint(task, root, reference, runner)
    if family == "location":
        return grade_location(task, final_text, root)
    if family == "impact":
        return grade_impact(task, final_text, root)
    raise ValueError(family)
