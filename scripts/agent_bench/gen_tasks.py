"""Deterministic task generation from the owner's corpus. It never invokes Justless.

Truth comes from rg, Python ast/tokenize and the independent Rust lexer and brace matcher in
oracles.py. Every candidate list is sorted, then shuffled with a seeded random.Random; the pool
size and the chosen index are stored with each task. Generated tasks, snapshots, truth and
hidden tests go to $AGENT_BENCH_HOME (never committed). `--public` regenerates
tasks/public.json from the repository fixtures (dry runs, unit tests and parity only).

    python3 gen_tasks.py --families bugfix,lint,location,impact [--repos REPO,...]
"""

import argparse
import ast
import io
import json
import os
import random
import re
import shutil
import subprocess
import sys
import time
import tokenize
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import baseline_build  # noqa: E402
import config  # noqa: E402
import oracles  # noqa: E402
import sandbox  # noqa: E402
import tasks as tasks_module  # noqa: E402
import workspace  # noqa: E402

PLAN = {
    "bugfix": {"prediction_market_workspace": 2, "crypto_rust_shared": 3, "trade_parser": 2, "video_tools": 1},
    "lint": {"prediction_market_workspace": 3, "crypto_rust_shared": 3, "trade_parser": 2},
    "location": {"prediction_market_workspace": 3, "trade_parser": 2, "video_tools": 2, "configs": 1},
    "impact": {"prediction_market_workspace": 3, "crypto_rust_shared": 1, "trade_parser": 1, "video_tools": 2,
               "configs": 1},
}
BACKFILL = ["crypto_rust_shared", "trade_parser", "prediction_market_workspace", "video_tools"]
SKIP_DIRS = {".git", "target", "node_modules", "__pycache__", ".venv", "venv", ".justless", "renders", "assets"}
COMMON_NAMES = {"new", "len", "get", "from", "into", "default", "clone", "build", "start", "update", "process",
                "handle", "parse", "format", "render", "values", "result", "config", "client", "status", "insert",
                "remove", "contains", "length", "string", "display", "create", "delete", "execute", "connect",
                "setup", "reset", "close", "write", "read", "send", "recv", "main", "init", "load", "save", "apply"}
STOPWORDS = set("the a an and or of to in for on at by with from is are was were be been this that these those "
                "not no it its as into than then there their your you we our failed error".split())
PARAPHRASE = {
    "log": "A user reports a log message about {words}. Which function emits it?",
    "error": "A user reports an error about {words}. Which function returns or raises it?",
    "panic": "The process stops with a message about {words}. Which function contains that check?",
    "keymap": "Which Neovim configuration function defines the mapping whose description mentions {words}?",
    "sql": "Which migration creates the table that stores {words}? Give the table name as the symbol.",
    "text": "Which function produces the text about {words}?",
}
VERBATIM = {
    "sql": "Which migration file creates the `{literal}` table? Give the table name as the symbol.",
    "keymap": "Where is the Neovim mapping described as `{literal}` defined?",
    "default": "A user pasted this line from the program's output: `{literal}`. Which function produces it?",
}
RG_ENV = {"PATH": "/opt/homebrew/bin:" + config.SYSTEM_PATH}


def template(name):
    return (config.PROMPTS / ("%s.txt" % name)).read_text().rstrip("\n")


def fill(text, **values):
    for key, value in values.items():
        text = text.replace("{%s}" % key, str(value))
    return text


def log(*parts):
    print(*parts, file=sys.stderr, flush=True)


def files(root, exts):
    root = Path(root)
    out = []
    for dirpath, dirnames, filenames in os.walk(str(root)):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS and not d.startswith("."))
        for name in sorted(filenames):
            if name.endswith(tuple(exts)):
                out.append((Path(dirpath) / name).relative_to(root).as_posix())
    return out


def rg_files(root, needle, word=False):
    argv = ["rg", "--hidden", "-l", "-F", "-g", "!target", "-g", "!.git", "-g", "!*.lock", "-g", "!.justless"]
    if word:
        argv.append("-w")
    argv += ["-e", needle, "."]
    proc = subprocess.run(argv, cwd=str(root), capture_output=True, text=True, env=RG_ENV)
    return sorted(p[2:] if p.startswith("./") else p for p in proc.stdout.split("\n") if p)


def rg_count(root, needle):
    argv = ["rg", "--hidden", "-F", "-c", "-g", "!target", "-g", "!.git", "-g", "!*.lock", "-e", needle, "."]
    proc = subprocess.run(argv, cwd=str(root), capture_output=True, text=True, env=RG_ENV)
    return sum(int(line.rsplit(":", 1)[1]) for line in proc.stdout.split("\n") if ":" in line)


def is_rust_test_path(rel):
    return bool(re.search(r"(^|/)(tests|benches|examples)/|(^|/)build\.rs$|_test\.rs$", rel))


def package_of(root, rel):
    path = (Path(root) / rel).parent
    while True:
        manifest = path / "Cargo.toml"
        if manifest.exists():
            m = re.search(r"\[package\][^\[]*?\bname\s*=\s*\"([^\"]+)\"", manifest.read_text(), re.S)
            if m:
                return m.group(1)
        if path == Path(root) or path == path.parent:
            return None
        path = path.parent


def line_at(text, offset):
    return text.count("\n", 0, offset)


# ----------------------------------------------------------------------------- Rust mutations

REL = re.compile(r" (<=|>=|==|!=|<|>) ")
LOGIC = re.compile(r" (&&|\|\|) ")
OFF1 = re.compile(r" ([+-]) 1(?![\w.])")
INDEX = re.compile(r"\[(\d+)\]")
ARM = re.compile(r"=> (\d+),")
SWAP = re.compile(r"\b([a-z_][a-z0-9_]*)\(([a-z_][a-z0-9_]*), ([a-z_][a-z0-9_]*)\)")
FLIP = {"<": "<=", "<=": "<", ">": ">=", ">=": ">", "==": "!=", "!=": "=="}
SYMMETRIC = {"max", "min", "eq", "ne", "add", "mul", "swap", "join", "zip", "chain", "hash", "assert_eq", "assert_ne",
             "format", "println", "write", "writeln"}


def rust_sites(root, rel):
    text = (Path(root) / rel).read_text(errors="replace")
    masked = oracles.rust_mask(text)
    regions = oracles.rust_test_regions(text)
    fns = [f for f in oracles.rust_functions(text) if not any(a <= f[2] <= b for a, b in regions)]
    out = []

    def owner(offset):
        best = None
        for fn in fns:
            if fn[4] <= offset <= fn[5] and (best is None or fn[4] > best[4]):
                best = fn
        return best

    def add(kind, offset, old, new):
        fn = owner(offset)
        if fn is None:
            return
        line = line_at(text, offset)
        if any(a <= line <= b for a, b in regions) or text[offset:offset + len(old)] != old:
            return
        out.append({"kind": kind, "file": rel, "offset": offset, "old": old, "new": new, "fn": fn[1],
                    "fn_span": [fn[2], fn[3]], "line": line})

    for m in REL.finditer(masked):
        add("relational", m.start(), m.group(0), " %s " % FLIP[m.group(1)])
    for m in LOGIC.finditer(masked):
        add("logical", m.start(), m.group(0), " || " if m.group(1) == "&&" else " && ")
    for m in OFF1.finditer(masked):
        add("off_by_one", m.start(), m.group(0)[:3], " - " if m.group(1) == "+" else " + ")
    for m in INDEX.finditer(masked):
        add("off_by_one", m.start(), m.group(0), "[%d]" % (int(m.group(1)) + 1))
    for m in ARM.finditer(masked):
        add("constant", m.start(), m.group(0), "=> %d," % (int(m.group(1)) + 1))
    for m in SWAP.finditer(masked):
        if m.group(2) != m.group(3) and m.group(1) not in SYMMETRIC and masked[m.start() - 1:m.start()] != "!":
            add("swap_args", m.start(), m.group(0), "%s(%s, %s)" % (m.group(1), m.group(3), m.group(2)))
    return out, text


def apply_one(root, edit):
    tasks_module.apply_edits(root, [edit])


def revert_one(root, edit):
    tasks_module.apply_edits(root, [dict(edit, old=edit["new"], new=edit["old"])])


def unique_context(text, offset, old, new):
    """The smallest run of whole lines around the edit whose mutated form is unique in the mutated file."""
    mutated = text[:offset] + new + text[offset + len(old):]
    start = mutated.rfind("\n", 0, offset) + 1
    end = mutated.find("\n", offset + len(new))
    end = len(mutated) if end < 0 else end
    for _ in range(8):
        if mutated.count(mutated[start:end]) == 1:
            break
        start = mutated.rfind("\n", 0, max(0, start - 1)) + 1
    original = text[start:end + len(old) - len(new)]
    return mutated[start:end], original


def validate_rust_candidate(repo, work, target, candidate, sites_by_fn, rng, flaky):
    command = baseline_build.test_command(repo, candidate["package"])
    apply_one(work, candidate)
    try:
        _, out1, _ = baseline_build.run(command, work, target)
        failed1 = sorted(n for n, s in oracles.parse_tests(out1).items() if s == "FAILED")
        if oracles.compile_failed(out1) or not failed1:
            return None, "does not compile" if oracles.compile_failed(out1) else "no failing test"
        if any(n in flaky for n in failed1):
            return None, "flaky failing test"
        _, out2, _ = baseline_build.run(command, work, target)
        failed2 = sorted(n for n, s in oracles.parse_tests(out2).items() if s == "FAILED")
        if failed1 != failed2:
            return None, "nondeterministic failures"
    finally:
        revert_one(work, candidate)
    others = [s for s in sites_by_fn.get((candidate["file"], candidate["fn"]), []) if s["offset"] != candidate["offset"]]
    if not others:
        return None, "no second site for the control"
    control = rng.choice(others)
    apply_one(work, control)
    try:
        _, out3, _ = baseline_build.run(command, work, target)
        failed3 = [n for n, s in oracles.parse_tests(out3).items() if s == "FAILED"]
        if not failed3 and not oracles.compile_failed(out3):
            return None, "second-mutation control passed"
    finally:
        revert_one(work, control)
    return {"failing": failed1, "control": {k: control[k] for k in ("kind", "file", "offset", "old", "new")}}, None


def balanced(sites, rng):
    rng.shuffle(sites)
    kinds = sorted({s["kind"] for s in sites})
    rng.shuffle(kinds)
    queues = {k: [s for s in sites if s["kind"] == k] for k in kinds}
    order = []
    while any(queues.values()):
        for kind in kinds:
            if queues[kind]:
                order.append(queues[kind].pop(0))
    return order


def work_copy(base, name):
    work = base / name
    if work.exists():
        shutil.rmtree(str(work))
    workspace.clone_tree(base / "repo", work)
    target = base / "work_target"
    if not target.exists() and (base / "target").exists():
        workspace.clone_tree(base / "target", target)
    return work, target


def exposed_files(repo, root, rels):
    """Files whose current content is stored in a Cargo git database that a run of this repository can still
    read. A mutation or lint injection there would leave its answer key reachable, so generation skips them."""
    denies = sandbox.cargo_git_denies(config.CARGO_HOME, [repo], root)
    texts = {rel: (Path(root) / rel).read_bytes() for rel in rels}
    return set(sandbox.reachable_blobs(texts, config.CARGO_HOME, denies))


def gen_rust_bugfix(repo, count, seed):
    base = config.HOME / "pristine" / repo
    pristine = base / "repo"
    baseline = json.loads((base / "baseline.json").read_text())
    flaky = set(baseline.get("flaky") or [])
    rng = random.Random("%d|bugfix|%s" % (seed, repo))
    sites = []
    for rel in files(pristine, [".rs"]):
        if is_rust_test_path(rel):
            continue
        found, _ = rust_sites(pristine, rel)
        package = package_of(pristine, rel)
        for site in found:
            site["package"] = package
        sites += found
    broken = {n.split("@", 1)[0] for n, status in baseline["test_names"].items() if status == "FAILED"}
    sites = [s for s in sites if s["package"] and s["package"] not in broken
             and s["package"].replace("-", "_") not in broken]
    sites.sort(key=lambda s: (s["file"], s["offset"], s["kind"]))
    pool = len(sites)
    sites_by_fn = {}
    for site in sites:
        sites_by_fn.setdefault((site["file"], site["fn"]), []).append(site)
    order = balanced(list(sites), rng)
    exposed = exposed_files(repo, pristine, sorted({s["file"] for s in sites}))
    work, target = work_copy(base, "work_bugfix")
    accepted = []
    used_fns = set()
    used_files = set()
    attempts = []
    for index, candidate in enumerate(order):
        if len(accepted) >= count:
            break
        if candidate["file"] in exposed:
            continue
        if (candidate["file"], candidate["fn"]) in used_fns or candidate["file"] in used_files:
            continue
        result, reason = validate_rust_candidate(repo, work, target, candidate, sites_by_fn, rng, flaky)
        attempts.append({"index": index, "kind": candidate["kind"], "file": candidate["file"],
                         "line": candidate["line"], "accepted": result is not None, "reason": reason})
        log("bugfix", repo, candidate["kind"], "%s:%d" % (candidate["file"], candidate["line"] + 1), reason or "ok")
        if result is None:
            continue
        used_fns.add((candidate["file"], candidate["fn"]))
        used_files.add(candidate["file"])
        accepted.append((index, candidate, result))
    out = []
    for n, (index, candidate, result) in enumerate(accepted, 1):
        text = (pristine / candidate["file"]).read_text(errors="replace")
        mutated_snippet, original_snippet = unique_context(text, candidate["offset"], candidate["old"], candidate["new"])
        command = "cargo test --offline -p %s" % candidate["package"]
        failing = result["failing"][0]
        out.append({
            "id": "bugfix-%s-%02d" % (repo, n), "family": "bugfix", "repo": repo, "language": "rust",
            "prompt": fill(template("bugfix"), command=command), "package": candidate["package"],
            "visible_command": command,
            "setup": {"kind": "mutation", "edits": [{k: candidate[k] for k in ("file", "offset", "old", "new")}]},
            "oracle": {"test_command": baseline["test_command"], "mutated_file": candidate["file"],
                       "mutated_span": candidate["fn_span"], "mutated_fn": candidate["fn"],
                       "anchor_line": mutated_snippet.split("\n")[0], "noop_comment": " // checked",
                       "tamper_edit": tamper_edit(pristine, failing.split("::", 1)[1] if "::" in failing else None),
                       "cargo_scope": ["--workspace"] if repo in baseline_build.WORKSPACE_REPOS else []},
            "baseline": {"test_names": baseline["test_names"], "flaky": sorted(flaky),
                         "tree_hash": workspace.tree_hash(pristine)},
            "controls": {"inverse": [{"file": candidate["file"], "old": mutated_snippet, "new": original_snippet}],
                         "inverse_files": {candidate["file"]: text}, "second_mutation": result["control"],
                         "failing_tests": result["failing"]},
            "generator": {"seed": seed, "candidate_index": index, "candidate_pool_size": pool,
                          "kind": candidate["kind"], "attempts": attempts},
        })
    shutil.rmtree(str(work), ignore_errors=True)
    return out


def tamper_edit(root, test_name):
    """An edit that adds #[ignore] to the first failing test, for the dry-run tamper variant."""
    if not test_name:
        return None
    short = test_name.split("::")[-1]
    for rel in files(root, [".rs"]):
        text = (Path(root) / rel).read_text(errors="replace")
        m = re.search(r"#\[(?:tokio::)?test\]\s*\n(\s*)(?:async\s+)?fn %s\b" % re.escape(short), text)
        if m and text.count(m.group(0)) == 1:
            return {"file": rel, "old": m.group(0), "new": m.group(0).replace("test]", "test]\n#[ignore]", 1)}
    return None


# ----------------------------------------------------------------------------- Python bugfix

# The Python bug-fix target (module, pure functions, module-level names they need, and test inputs) names
# private code, so it lives in $AGENT_BENCH_HOME/private/python_task.json rather than in the repository:
# {"repo", "module", "functions": [...], "names": [...], "visible_cases": {fn: [args...]}, "hidden_cases": {...}}
def py_spec():
    path = config.HOME / "private" / "python_task.json"
    if not path.exists():
        raise SystemExit("missing %s; see the comment above py_spec() in gen_tasks.py" % path)
    return json.loads(path.read_text())


PY_LOADER = '''import ast, datetime, re, unittest
from pathlib import Path

SOURCE = Path(__file__).resolve().parents[1] / {module!r}
NAMES = set({names!r})


def load():
    tree = ast.parse(SOURCE.read_text())
    keep = [n for n in tree.body if (isinstance(n, ast.FunctionDef) and n.name in NAMES) or
            (isinstance(n, ast.Assign) and all(isinstance(t, ast.Name) and t.id in NAMES for t in n.targets))]
    namespace = {{"datetime": datetime, "re": re}}
    exec(compile(ast.Module(body=keep, type_ignores=[]), str(SOURCE), "exec"), namespace)
    return namespace


M = load()


def norm(value):
    if isinstance(value, tuple):
        return [norm(v) for v in value]
    if isinstance(value, datetime.date):
        return value.isoformat()
    return value


class {cls}(unittest.TestCase):
'''


def py_namespace(root, spec):
    import datetime as _dt
    names = set(spec["names"])
    tree = ast.parse((Path(root) / spec["module"]).read_text())
    keep = [n for n in tree.body if (isinstance(n, ast.FunctionDef) and n.name in names) or
            (isinstance(n, ast.Assign) and all(isinstance(t, ast.Name) and t.id in names for t in n.targets))]
    namespace = {"datetime": _dt, "re": re}
    exec(compile(ast.Module(body=keep, type_ignores=[]), spec["module"], "exec"), namespace)
    return namespace


def py_norm(value):
    import datetime as _dt
    if isinstance(value, tuple):
        return [py_norm(v) for v in value]
    if isinstance(value, _dt.date):
        return value.isoformat()
    return value


def py_test_file(root, spec, cases, cls):
    namespace = py_namespace(root, spec)
    lines = [PY_LOADER.format(module=spec["module"], names=sorted(spec["names"]), cls=cls)]
    n = 0
    for func, inputs in sorted(cases.items()):
        for value in inputs:
            n += 1
            args = value if isinstance(value, list) else [value]
            expected = py_norm(namespace[func](*args))
            lines.append("    def test_%s_%02d(self):\n        self.assertEqual(norm(M[%r](*%r)), %r)\n" % (
                func, n, func, args, expected))
    lines.append("\nif __name__ == \"__main__\":\n    unittest.main()\n")
    return "\n".join(lines)


def py_sites(root, spec):
    source = (Path(root) / spec["module"]).read_text()
    tree = ast.parse(source)
    spans = {n.name: (n.lineno - 1, n.end_lineno - 1) for n in tree.body
             if isinstance(n, ast.FunctionDef) and n.name in spec["functions"]}
    starts = [0]
    for line in source.split("\n"):
        starts.append(starts[-1] + len(line) + 1)
    out = []
    tokens = list(tokenize.generate_tokens(io.StringIO(source).readline))
    for i, tok in enumerate(tokens):
        row = tok.start[0] - 1
        fn = next((name for name, (a, b) in spans.items() if a <= row <= b), None)
        if fn is None:
            continue
        offset = starts[row] + tok.start[1]
        if tok.type == tokenize.NUMBER and tok.string.isdigit():
            prev = tokens[i - 1]
            if prev.string in ("-", "+") and tok.string == "1":
                out.append({"kind": "off_by_one", "offset": starts[prev.start[0] - 1] + prev.start[1], "old": prev.string,
                            "new": "+" if prev.string == "-" else "-", "fn": fn, "fn_span": list(spans[fn])})
            else:
                out.append({"kind": "constant", "offset": offset, "old": tok.string, "new": str(int(tok.string) + 1),
                            "fn": fn, "fn_span": list(spans[fn])})
        elif tok.type == tokenize.OP and tok.string in FLIP:
            out.append({"kind": "relational", "offset": offset, "old": tok.string, "new": FLIP[tok.string], "fn": fn,
                        "fn_span": list(spans[fn])})
    for site in out:
        site["file"] = spec["module"]
        site["line"] = source.count("\n", 0, site["offset"])
    out.sort(key=lambda s: (s["offset"], s["kind"]))
    return out, source


def run_unittest(root):
    proc = subprocess.run(["python3", "-m", "unittest", "discover", "-s", "bench_tests", "-t", "."], cwd=str(root),
                          capture_output=True, text=True, env={"PATH": config.SYSTEM_PATH, "PYTHONDONTWRITEBYTECODE": "1"})
    return proc.returncode, proc.stdout + proc.stderr


def gen_python_bugfix(count, seed):
    spec = py_spec()
    repo = spec["repo"]
    work = config.HOME / "pristine" / repo / "work_py"
    if work.exists():
        shutil.rmtree(str(work))
    workspace.copy_source(config.CORPUS / repo, work)
    visible = py_test_file(work, spec, spec["visible_cases"], "VisibleTests")
    hidden = py_test_file(work, spec, spec["hidden_cases"], "HiddenTests")
    (work / "bench_tests").mkdir(exist_ok=True)
    (work / "bench_tests" / "__init__.py").write_text("")
    (work / "bench_tests" / "test_visible.py").write_text(visible)
    (work / "bench_tests" / "test_hidden.py").write_text(hidden)
    code, out = run_unittest(work)
    if code != 0:
        raise RuntimeError("pristine Python tests fail: %s" % out[-800:])
    (work / "bench_tests" / "test_hidden.py").unlink()
    sites, source = py_sites(work, spec)
    pool = len(sites)
    order = balanced(list(sites), random.Random("%d|bugfix|%s" % (seed, repo)))
    accepted = []
    attempts = []
    for index, site in enumerate(order):
        if len(accepted) >= count:
            break
        apply_one(work, site)
        code, _ = run_unittest(work)
        revert_one(work, site)
        attempts.append({"index": index, "kind": site["kind"], "line": site["line"], "accepted": code != 0})
        log("bugfix", repo, site["kind"], "%s:%d" % (site["fn"], site["line"] + 1), "ok" if code else "tests pass")
        if code != 0:
            accepted.append((index, site))
    out_tasks = []
    command = "python3 -m unittest discover -s bench_tests -t ."
    for n, (index, site) in enumerate(accepted, 1):
        mutated_snippet, original_snippet = unique_context(source, site["offset"], site["old"], site["new"])
        out_tasks.append({
            "id": "bugfix-%s-%02d" % (repo, n), "family": "bugfix", "repo": repo, "language": "python",
            "prompt": fill(template("bugfix_py"), command=command), "visible_command": command,
            "setup": {"kind": "mutation", "edits": [{k: site[k] for k in ("file", "offset", "old", "new")}],
                      "files": {"bench_tests/__init__.py": "", "bench_tests/test_visible.py": visible}},
            "oracle": {"test_command": ["python3", "-m", "unittest", "discover", "-s", "bench_tests", "-t", "."],
                       "visible_tests": {"bench_tests/test_visible.py": visible},
                       "hidden_tests": {"bench_tests/test_hidden.py": hidden},
                       "mutated_file": site["file"], "mutated_span": site["fn_span"], "mutated_fn": site["fn"],
                       "anchor_line": mutated_snippet.split("\n")[0], "noop_comment": "  # checked"},
            "baseline": {"test_names": {}, "flaky": []},
            "controls": {"inverse": [{"file": site["file"], "old": mutated_snippet, "new": original_snippet}],
                         "inverse_files": {site["file"]: source}},
            "generator": {"seed": seed, "candidate_index": index, "candidate_pool_size": pool, "attempts": attempts,
                          "disclosure": "unittest is not a Justless command profile"},
        })
    shutil.rmtree(str(work), ignore_errors=True)
    return out_tasks


# ----------------------------------------------------------------------------- lint injection

def lint_transforms(text):
    """Behavior-preserving rewrites that trigger default clippy lints, as (lint, offset, old, new)."""
    masked = oracles.rust_mask(text)
    regions = oracles.rust_test_regions(text)
    out = []

    def in_test(offset):
        line = line_at(text, offset)
        return any(a <= line <= b for a, b in regions)

    for m in re.finditer(r"(!?)\b([a-z_][a-z0-9_]*(?:\.[a-z_][a-z0-9_]*)*)\.is_empty\(\)", masked):
        if not in_test(m.start()):
            op = "!=" if m.group(1) else "=="
            out.append(("clippy::len_zero", m.start(), m.group(0), "%s.len() %s 0" % (m.group(2), op)))
    for m in re.finditer(r"\bif ([a-z_][a-z0-9_]*) \{", masked):
        if not in_test(m.start()) and m.group(1) not in ("let", "true", "false", "mut", "self"):
            out.append(("clippy::bool_comparison", m.start(), m.group(0), "if %s == true {" % m.group(1)))
    for m in re.finditer(r"\b([a-z_][a-z0-9_]*)\.len\(\)", masked):
        if not in_test(m.start()):
            out.append(("clippy::iter_count", m.start(), m.group(0), "%s.iter().count()" % m.group(1)))
    for m in re.finditer(r"\b(const|static) ([A-Z_][A-Z0-9_]*): &(str|\[)", masked):
        out.append(("clippy::redundant_static_lifetimes", m.start(), m.group(0),
                    "%s %s: &'static %s" % (m.group(1), m.group(2), m.group(3))))
    for m in re.finditer(r"\.(map|and_then|filter_map|for_each)\(([A-Za-z_][\w:]*)\)", masked):
        if not in_test(m.start()) and (m.group(2)[0].islower() or "::" in m.group(2)):
            out.append(("clippy::redundant_closure", m.start(), m.group(0), ".%s(|x| %s(x))" % (m.group(1), m.group(2))))
    for m in re.finditer(r"\bfn ([a-z_][a-z0-9_]*)\(([^()]*)\) \{", masked):
        if not in_test(m.start()):
            out.append(("clippy::unused_unit", m.start(), m.group(0), "fn %s(%s) -> () {" % (m.group(1), m.group(2))))
    for m in re.finditer(r"(\w+): &\[([A-Za-z0-9_<>]+)\]([,)])", masked):
        if not in_test(m.start()):
            out.append(("clippy::ptr_arg", m.start(), m.group(0), "%s: &Vec<%s>%s" % (m.group(1), m.group(2), m.group(3))))
    for name, qualified, start, end, s_off, e_off in oracles.rust_functions(text):
        if any(a <= start <= b for a, b in regions):
            continue
        before = text.rfind("\n", 0, e_off)
        prev_start = text.rfind("\n", 0, before) + 1
        last = text[prev_start:before]
        stripped = last.strip()
        if (stripped and not stripped.endswith((";", "{", "}", ",")) and not stripped.startswith(("//", "#", "}"))
                and text[before + 1:e_off].strip() == "" and masked[prev_start:before].strip() == stripped
                and re.match(r"^[A-Za-z_(&*!]", stripped) and "return" not in stripped and "let " not in stripped):
            indent = last[:len(last) - len(last.lstrip())]
            out.append(("clippy::needless_return", prev_start, last, "%sreturn %s;" % (indent, stripped)))
            out.append(("clippy::let_and_return", prev_start, last,
                        "%slet result = %s;\n%sresult" % (indent, stripped, indent)))
    # Matches come from the masked text; keep only those with no comment or string inside.
    return [c for c in out if text[c[1]:c[1] + len(c[2])] == c[2]]


def clippy_codes(output):
    return [m["code"] for m in oracles.clippy_messages(output)]


def lint_inverse(mutated, edit):
    """An Edit (old in the mutated file -> new) that undoes one injection, unique in the mutated file."""
    start = mutated.rfind("\n", 0, edit["new_offset"]) + 1
    end = mutated.find("\n", edit["new_offset"] + len(edit["new"]))
    end = len(mutated) if end < 0 else end
    for _ in range(8):
        if mutated.count(mutated[start:end]) == 1:
            break
        start = mutated.rfind("\n", 0, max(0, start - 1)) + 1
    old = mutated[start:end]
    rel_start = edit["new_offset"] - start
    return {"file": edit["file"], "old": old, "new": old[:rel_start] + edit["old"] + old[rel_start + len(edit["new"]):]}


def gen_lint(repo, count, seed):
    base = config.HOME / "pristine" / repo
    pristine = base / "repo"
    baseline = json.loads((base / "baseline.json").read_text())
    rng = random.Random("%d|lint|%s" % (seed, repo))
    work, target = work_copy(base, "work_lint")
    packages = {}
    for rel in files(pristine, [".rs"]):
        if not is_rust_test_path(rel):
            package = package_of(pristine, rel)
            if package:
                packages.setdefault(package, []).append(rel)
    eligible = []
    for package in sorted(packages):
        command = ["cargo", "clippy", "--offline", "--all-targets", "-p", package, "--message-format=json",
                   "--", "-D", "warnings"]
        code, out, _ = baseline_build.run(command, work, target)
        clean = code == 0 and not clippy_codes(out)
        log("lint eligibility", repo, package, "clean" if clean else "not clean")
        if clean:
            eligible.append(package)
    order = list(eligible)
    rng.shuffle(order)
    exposed = exposed_files(repo, pristine, sorted({rel for rels in packages.values() for rel in rels}))
    tasks_out = []
    used_files = set()
    verdicts = {}
    # Single-package repositories yield several tasks, each injected into files no earlier task used.
    queue = [p for _ in range(count) for p in order]
    for package in queue:
        if len(tasks_out) >= count:
            break
        candidates = []
        for rel in packages[package]:
            if rel in used_files:
                continue
            text = (pristine / rel).read_text(errors="replace")
            for lint, offset, old, new in lint_transforms(text):
                candidates.append({"lint": lint, "file": rel, "offset": offset, "old": old, "new": new})
        candidates.sort(key=lambda c: (c["file"], c["offset"], c["lint"]))
        rng.shuffle(candidates)
        command = ["cargo", "clippy", "--offline", "--all-targets", "-p", package, "--message-format=json",
                   "--", "-D", "warnings"]
        singles = []
        used = {}
        for candidate in candidates:
            if len(singles) >= 5:
                break
            if candidate["file"] in exposed:
                continue
            if used.get(candidate["lint"], 0) >= 2 or any(
                    s["file"] == candidate["file"] and abs(s["offset"] - candidate["offset"]) < 200 for s in singles):
                continue
            key = (candidate["file"], candidate["offset"], candidate["lint"])
            if key not in verdicts:
                apply_one(work, candidate)
                _, out, _ = baseline_build.run(command, work, target)
                revert_one(work, candidate)
                verdicts[key] = clippy_codes(out)
            codes = verdicts[key]
            ok = codes == [candidate["lint"]]
            log("lint", repo, package, candidate["lint"], candidate["file"], "ok" if ok else codes[:3])
            if ok:
                singles.append(candidate)
                used[candidate["lint"]] = used.get(candidate["lint"], 0) + 1
        if len(singles) < 3 or len({s["file"] for s in singles}) < 2:
            continue
        chosen = singles[:rng.randint(3, min(5, len(singles)))]
        if len({s["file"] for s in chosen}) < 2:
            chosen = singles[:5]
        edits = tasks_module.with_new_offsets([{k: c[k] for k in ("file", "offset", "old", "new")} for c in chosen])
        tasks_module.apply_edits(work, edits)
        # In a workspace other members carry their own warnings, so the prompt and the oracle name the package.
        scope = ["-p", package] if repo in baseline_build.WORKSPACE_REPOS else []
        # Without -D warnings, so a lint in the library does not stop the binaries that use it from being checked.
        _, out, _ = baseline_build.run(["cargo", "clippy", "--offline", "--all-targets", "--message-format=json"]
                                       + scope, work, target)
        codes = sorted(clippy_codes(out))
        _, tout, _ = baseline_build.run(baseline["test_command"], work, target)
        missing = oracles.missing_names(baseline["test_names"], oracles.parse_tests(tout),
                                        set(baseline.get("flaky") or []))
        tasks_module.apply_edits(work, edits, reverse=True)
        if codes != sorted(c["lint"] for c in chosen) or missing:
            log("lint combined rejected", package, codes, len(missing))
            continue
        inverse = []
        inverse_files = {}
        for rel in sorted({e["file"] for e in edits}):
            original = (pristine / rel).read_text(errors="replace")
            inverse_files[rel] = original
            mutated = original
            for e in sorted([e for e in edits if e["file"] == rel], key=lambda e: e["offset"], reverse=True):
                mutated = mutated[:e["offset"]] + e["new"] + mutated[e["offset"] + len(e["old"]):]
            for e in sorted([e for e in edits if e["file"] == rel], key=lambda e: e["offset"]):
                inverse.append(lint_inverse(mutated, e))
        n = len(tasks_out) + 1
        used_files.update(e["file"] for e in edits)
        tasks_out.append({
            "id": "lint-%s-%02d" % (repo, n), "family": "lint", "repo": repo, "language": "rust", "package": package,
            "prompt": fill(template("lint"), command=" ".join(["cargo", "clippy", "--offline", "--all-targets"] + scope
                                                               + ["--", "-D", "warnings"])),
            "setup": {"kind": "lint_injection", "edits": edits},
            "oracle": {"clippy_command": ["cargo", "clippy", "--offline", "--all-targets", "--message-format=json"]
                       + scope + ["--", "-D", "warnings"], "test_command": baseline["test_command"],
                       "injected_lints": [c["lint"] for c in chosen],
                       "cargo_scope": ["--workspace"] if repo in baseline_build.WORKSPACE_REPOS else []},
            "baseline": {"test_names": baseline["test_names"], "flaky": baseline.get("flaky") or [],
                         "clippy_clean_package": package, "tree_hash": workspace.tree_hash(pristine)},
            "controls": {"inverse": inverse, "inverse_files": inverse_files},
            "generator": {"seed": seed, "eligible_packages": eligible, "candidates": len(candidates)},
        })
    shutil.rmtree(str(work), ignore_errors=True)
    return tasks_out


# ----------------------------------------------------------------------------- location

RUST_LIT = re.compile(r"\b(error|warn|info|debug|trace|panic|bail|anyhow|eprintln|println|expect|context|"
                      r"with_context|format_err|ensure)!?\(\s*(?:target:\s*\"[^\"]*\",\s*)?(?:[^\"()]{0,40},\s*)?"
                      r"\"((?:[^\"\\]|\\.){12,200})\"")
CATEGORY = {"error": "log", "warn": "log", "info": "log", "debug": "log", "trace": "log", "eprintln": "log",
            "println": "log", "panic": "panic", "expect": "panic", "bail": "error", "anyhow": "error",
            "context": "error", "with_context": "error", "format_err": "error", "ensure": "error"}


def static_segment(literal):
    parts = re.split(r"\{[^{}]*\}|\\[nt\"\\]", literal)
    parts = [p.strip(" :;,.-=") for p in parts]
    parts = [p for p in parts if len(p) >= 16 and re.search(r"[A-Za-z]{3}", p) and len(p.split()) >= 3]
    return max(parts, key=len) if parts else None


def content_words(text):
    return [w for w in re.findall(r"[A-Za-z][A-Za-z_]{3,}", text) if w.lower() not in STOPWORDS]


def grep_solvable(root, question, truth_file):
    """Does rg -F on some content word of 5+ characters find the truth file among <= 20 hits?"""
    for word in sorted(set(re.findall(r"[A-Za-z_][A-Za-z0-9_]{4,}", question))):
        if word.lower() in STOPWORDS or word.lower() in ("answer", "symbol", "function", "files", "which",
                                                         "repository", "pasted", "produces", "output", "program"):
            continue
        hits = rg_files(root, word)
        if truth_file in hits and len(hits) <= 20:
            return True
    return False


def lua_mask(text):
    out = list(text)
    i = 0
    n = len(text)
    while i < n:
        if text.startswith("--[[", i) or text.startswith("[[", i):
            end = text.find("]]", i)
            end = n if end < 0 else end + 2
            for k in range(i, end):
                if out[k] != "\n":
                    out[k] = " "
            i = end
        elif text.startswith("--", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            for k in range(i, end):
                out[k] = " "
            i = end
        elif text[i] in "\"'":
            quote = text[i]
            j = i + 1
            while j < n and text[j] != quote and text[j] != "\n":
                j += 2 if text[j] == "\\" else 1
            for k in range(i, min(j + 1, n)):
                if out[k] != "\n":
                    out[k] = " "
            i = j + 1
        else:
            i += 1
    return "".join(out)


def lua_functions(text):
    """[(name, start_line, end_line)] (1-based) by keyword block matching on masked text."""
    masked = lua_mask(text)
    out = []
    stack = []
    for m in re.finditer(r"\b(function|if|do|repeat|end|until)\b", masked):
        word, offset = m.group(0), m.start()
        if word in ("function", "if", "do", "repeat"):
            name = None
            if word == "function":
                after = re.match(r"function\s+([\w.:]+)\s*\(", masked[offset:offset + 120])
                before = re.search(r"([\w.]+)\s*=\s*$", masked[max(0, offset - 80):offset])
                name = after.group(1) if after else (before.group(1) if before else None)
            stack.append((word, name, offset))
        elif stack:
            opener, name, start = stack.pop()
            if opener == "function" and name:
                out.append((name, line_at(text, start) + 1, line_at(text, offset) + 1))
    return out


def python_docstring_ids(node):
    out = set()
    for inner in ast.walk(node):
        body = getattr(inner, "body", None)
        if isinstance(body, list) and body and isinstance(body[0], ast.Expr) and isinstance(body[0].value, ast.Constant) \
                and isinstance(body[0].value.value, str):
            out.add(id(body[0].value))
    return out


def location_candidates(repo, root):
    out = []
    language = {"configs": "lua", "video_tools": "python"}.get(repo, "rust")
    if language == "rust":
        for rel in files(root, [".rs"]):
            if is_rust_test_path(rel):
                continue
            text = (Path(root) / rel).read_text(errors="replace")
            regions = oracles.rust_test_regions(text)
            fns = oracles.rust_functions(text)
            for m in RUST_LIT.finditer(text):
                needle = static_segment(m.group(2))
                line = line_at(text, m.start(2))
                if not needle or any(a <= line <= b for a, b in regions):
                    continue
                owner = None
                for fn in fns:
                    if fn[4] <= m.start() <= fn[5] and (owner is None or fn[4] > owner[4]):
                        owner = fn
                if owner:
                    out.append({"file": rel, "literal": needle, "category": CATEGORY.get(m.group(1), "text"),
                                "symbol": owner[1], "accepted_symbols": [owner[1], owner[0]],
                                "span": [owner[2] + 1, owner[3] + 1], "language": "rust", "line": line + 1})
        if repo == "trade_parser":
            for rel in files(root, [".sql"]):
                text = (Path(root) / rel).read_text(errors="replace")
                for m in re.finditer(r"CREATE TABLE (?:IF NOT EXISTS )?([a-z_][a-z0-9_]+)", text, re.I):
                    line = line_at(text, m.start()) + 1
                    out.append({"file": rel, "literal": m.group(1), "category": "sql", "symbol": m.group(1),
                                "accepted_symbols": [m.group(1)], "span": [line, line], "language": "sql", "line": line})
    elif language == "python":
        for rel in files(root, [".py"]):
            source = (Path(root) / rel).read_text(errors="replace")
            try:
                tree = ast.parse(source)
            except SyntaxError:
                continue
            classes = {id(n): c.name for c in ast.walk(tree) if isinstance(c, ast.ClassDef) for n in c.body}
            for node in ast.walk(tree):
                if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    continue
                owner = "%s.%s" % (classes[id(node)], node.name) if id(node) in classes else node.name
                docstrings = python_docstring_ids(node)
                inner_fns = [n for n in ast.walk(node) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))
                             and n is not node]
                for sub in ast.walk(node):
                    if not (isinstance(sub, ast.Constant) and isinstance(sub.value, str)) or id(sub) in docstrings:
                        continue
                    needle = static_segment(sub.value)
                    if not needle or any(n.lineno <= sub.lineno <= n.end_lineno for n in inner_fns):
                        continue
                    out.append({"file": rel, "literal": needle, "category": "text", "symbol": owner,
                                "accepted_symbols": [owner, node.name], "span": [node.lineno, node.end_lineno],
                                "language": "python", "line": sub.lineno})
    else:
        for rel in files(root, [".lua"]):
            text = (Path(root) / rel).read_text(errors="replace")
            spans = lua_functions(text)
            for m in re.finditer(r"desc\s*=\s*([\"'])((?:(?!\1).){12,120})\1", text):
                needle = static_segment(m.group(2)) or (m.group(2) if len(m.group(2)) >= 12 else None)
                if not needle:
                    continue
                line = line_at(text, m.start()) + 1
                owner = None
                for name, a, b in spans:
                    if a <= line <= b and (owner is None or a > owner[1]):
                        owner = (name, a, b)
                out.append({"file": rel, "literal": needle, "category": "keymap",
                            "symbol": owner[0] if owner else None,
                            "accepted_symbols": [owner[0], owner[0].split(".")[-1]] if owner else [],
                            "span": [owner[1], owner[2]] if owner else [line, line], "language": "lua", "line": line})
    out.sort(key=lambda c: (c["file"], c["line"], c["literal"]))
    return out


def gen_location(repo, count, seed):
    root = config.CORPUS / repo
    rng = random.Random("%d|location|%s" % (seed, repo))
    candidates = location_candidates(repo, root)
    indices = list(range(len(candidates)))
    rng.shuffle(indices)
    out = []
    used_files = set()
    want_sql = repo == "trade_parser"
    for index in indices:
        if len(out) >= count:
            break
        c = candidates[index]
        if c["file"] in used_files:
            continue
        if repo == "trade_parser":
            if want_sql and len(out) == count - 1 and c["category"] != "sql":
                continue
            if c["category"] == "sql" and not want_sql:
                continue
        if c["category"] != "sql" and rg_count(root, c["literal"]) != 1:
            continue
        if c["category"] == "sql" and len(rg_files(root, "CREATE TABLE " + c["literal"])) != 1:
            continue
        if c["category"] == "sql":
            want_sql = False
        used_files.add(c["file"])
        out.append((index, c))
    return out, len(candidates)


def finalize_location(selected, seed):
    """Assign the 50/50 verbatim/paraphrase split across the family, then build prompts."""
    styles = ["verbatim"] * (len(selected) // 2) + ["paraphrase"] * (len(selected) - len(selected) // 2)
    random.Random("%d|location-styles" % seed).shuffle(styles)
    out = []
    counters = {}
    for (repo, index, pool, c), style in zip(selected, styles):
        counters[repo] = counters.get(repo, 0) + 1
        if style == "verbatim":
            question = VERBATIM.get(c["category"], VERBATIM["default"]).format(literal=c["literal"])
        else:
            words = content_words(c["literal"].replace("_", " ") if c["category"] == "sql" else c["literal"])
            chosen = sorted(set(words), key=lambda w: (-len(w), w))[:3]
            random.Random("%d|words|%s" % (seed, c["literal"])).shuffle(chosen)
            question = PARAPHRASE.get(c["category"], PARAPHRASE["text"]).format(
                words=", ".join(w.lower() for w in chosen) or "this behavior")
        prompt = fill(template("location" if c.get("symbol") else "location_line"), question=question)
        out.append({
            "id": "location-%s-%02d" % (repo, counters[repo]), "family": "location", "repo": repo,
            "language": c["language"], "style": style, "prompt": prompt, "setup": {"kind": "none"},
            "oracle": {"kind": "first_answer"},
            "truth": {k: c[k] for k in ("file", "symbol", "accepted_symbols", "span", "literal", "category", "line")},
            "grep_solvable": grep_solvable(config.CORPUS / repo, question, c["file"]),
            "generator": {"seed": seed, "candidate_index": index, "candidate_pool_size": pool},
        })
    return out


# ----------------------------------------------------------------------------- impact

def definitions(repo, root):
    language = {"configs": "lua", "video_tools": "python"}.get(repo, "rust")
    if language == "rust":
        pattern = re.compile(r"^\s*pub(?:\([^)]*\))?\s+(?:async\s+)?(fn|struct|enum|trait|const|type)\s+([A-Za-z_]\w*)",
                             re.M)
        exts = [".rs"]
    elif language == "python":
        pattern = re.compile(r"^(def|class)\s+([A-Za-z_]\w*)", re.M)
        exts = [".py"]
    else:
        pattern = re.compile(r"^\s*(function)\s+[A-Za-z_]\w*[.:]([A-Za-z_]\w*)\s*\(", re.M)
        exts = [".lua"]
    out = []
    for rel in files(root, exts):
        if language == "rust" and is_rust_test_path(rel):
            continue
        text = (Path(root) / rel).read_text(errors="replace")
        for m in pattern.finditer(text):
            out.append({"name": m.group(2), "file": rel, "kind": m.group(1), "language": language})
    out.sort(key=lambda d: (d["file"], d["name"]))
    return out


def definition_count(root, name, language):
    if language == "rust":
        regex = r"\b(fn|struct|enum|trait|const|static|type|mod|union)\s+%s\b|macro_rules!\s*%s\b" % (name, name)
    elif language == "python":
        regex = r"\b(def|class)\s+%s\b|^\s*%s\s*=" % (name, name)
    else:
        regex = r"\bfunction\s+([\w]+[.:])?%s\s*\(|\b%s\s*=\s*function" % (name, name)
    proc = subprocess.run(["rg", "--hidden", "-c", "-g", "!target", "-g", "!.git", "-e", regex, "."], cwd=str(root),
                          capture_output=True, text=True, env=RG_ENV)
    return sum(int(line.rsplit(":", 1)[1]) for line in proc.stdout.split("\n") if ":" in line)


def python_mask(source):
    try:
        tokens = list(tokenize.generate_tokens(io.StringIO(source).readline))
    except (tokenize.TokenError, IndentationError, SyntaxError):
        return source
    rows = [list(line) for line in source.split("\n")]
    for tok in tokens:
        if tok.type in (tokenize.STRING, tokenize.COMMENT):
            (r1, c1), (r2, c2) = tok.start, tok.end
            for r in range(r1, r2 + 1):
                row = rows[r - 1]
                for k in range(c1 if r == r1 else 0, min(c2 if r == r2 else len(row), len(row))):
                    row[k] = " "
    return "\n".join("".join(r) for r in rows)


def code_mentions(root, rel, name):
    text = (Path(root) / rel).read_text(errors="replace")
    if rel.endswith(".rs"):
        masked = oracles.rust_mask(text)
    elif rel.endswith(".py"):
        masked = python_mask(text)
    elif rel.endswith(".lua"):
        masked = lua_mask(text)
    else:
        return False
    return bool(re.search(r"\b%s\b" % re.escape(name), masked))


def gen_impact(repo, count, seed):
    root = config.CORPUS / repo
    rng = random.Random("%d|impact|%s" % (seed, repo))
    pool = definitions(repo, root)
    indices = list(range(len(pool)))
    rng.shuffle(indices)
    out = []
    used_files = set()
    for index in indices:
        if len(out) >= count:
            break
        d = pool[index]
        name = d["name"]
        if len(name) < 6 or name.lower() in COMMON_NAMES or d["file"] in used_files:
            continue
        if definition_count(root, name, d["language"]) != 1:
            continue
        others = [h for h in rg_files(root, name, word=True) if h != d["file"]]
        if not 3 <= len(others) <= 25:
            continue
        required = [d["file"]] + [rel for rel in others if code_mentions(root, rel, name)]
        neutral = [rel for rel in others if rel not in required]
        if len(required) - 1 < 3:
            continue
        used_files.add(d["file"])
        out.append({
            "id": "impact-%s-%02d" % (repo, len(out) + 1), "family": "impact", "repo": repo, "language": d["language"],
            "prompt": fill(template("impact"), symbol=name, file=d["file"]), "setup": {"kind": "none"},
            "oracle": {"recall": 0.9, "precision": 0.8, "symbol": name, "definition_file": d["file"]},
            "truth": {"required": sorted(required), "neutral": sorted(neutral)},
            # By construction: the truth is the `rg -w` hits minus comment-only mentions of a unique name.
            "grep_solvable": True,
            "generator": {"seed": seed, "candidate_index": index, "candidate_pool_size": len(pool), "kind": d["kind"]},
        })
    return out


# ----------------------------------------------------------------------------- public fixture tasks

def public_tasks():
    fixed = ("pub fn checkout_total(subtotal: u32, fee: u32) -> u32 {\n    apply_fee(subtotal, fee)\n}\n\n"
             "fn apply_fee(subtotal: u32, fee: u32) -> u32 {\n    subtotal + fee\n}\n")
    lint_pricing = fixed.replace("    apply_fee(subtotal, fee)\n", "    return apply_fee(subtotal, fee);\n")
    lint_obsolete = "pub fn obsolete_helper() -> u32 {\n    let result = 0;\n    result\n}\n"
    names = {"checkout@tests/checkout.rs::total_includes_fee": "ok"}
    command = "cargo test --offline -p checkout-fixture"
    note = {"note": "vendor fixture; dry runs, unit tests and parity only; never analyzed"}
    return [
        {"id": "bugfix-failing-01", "family": "bugfix", "repo": "fixture-failing", "language": "rust",
         "source_root": "fixtures/failing", "package": "checkout-fixture", "visible_command": command,
         "prompt": fill(template("bugfix"), command=command), "setup": {"kind": "fixture"},
         "oracle": {"test_command": ["cargo", "test", "--offline", "--no-fail-fast"], "mutated_file": "src/pricing.rs",
                    "mutated_span": [4, 6], "mutated_fn": "apply_fee", "anchor_line": "    subtotal + fee + 1",
                    "noop_comment": " // checked",
                    "tamper_edit": {"file": "tests/checkout.rs", "old": "#[test]\nfn total_includes_fee",
                                    "new": "#[test]\n#[ignore]\nfn total_includes_fee"}},
         "baseline": {"test_names": names, "flaky": []},
         "controls": {"inverse": [{"file": "src/pricing.rs", "old": "subtotal + fee + 1", "new": "subtotal + fee"}],
                      "inverse_files": {"src/pricing.rs": fixed}},
         "generator": note},
        {"id": "lint-failing-01", "family": "lint", "repo": "fixture-failing", "language": "rust",
         "source_root": "fixtures/failing", "package": "checkout-fixture",
         "prompt": fill(template("lint"), command="cargo clippy --offline --all-targets -- -D warnings"),
         "setup": {"kind": "lint_injection", "files": {"src/pricing.rs": lint_pricing, "src/obsolete.rs": lint_obsolete}},
         "oracle": {"clippy_command": ["cargo", "clippy", "--offline", "--all-targets", "--message-format=json", "--",
                                       "-D", "warnings"],
                    "test_command": ["cargo", "test", "--offline", "--no-fail-fast"],
                    "injected_lints": ["clippy::needless_return", "clippy::let_and_return"]},
         "baseline": {"test_names": names, "flaky": []},
         "controls": {"inverse": [{"file": "src/pricing.rs", "old": "    return apply_fee(subtotal, fee);\n",
                                   "new": "    apply_fee(subtotal, fee)\n"},
                                  {"file": "src/obsolete.rs", "old": "    let result = 0;\n    result\n", "new": "    0\n"}],
                      "inverse_files": {"src/pricing.rs": fixed,
                                        "src/obsolete.rs": "pub fn obsolete_helper() -> u32 {\n    0\n}\n"}},
         "generator": note},
        {"id": "location-mixed-01", "family": "location", "repo": "fixture-mixed", "language": "python",
         "source_root": "fixtures/mixed", "style": "verbatim",
         "prompt": fill(template("location"), question=VERBATIM["default"].format(
             literal="SELECT order_id FROM purchase_orders")),
         "setup": {"kind": "none"}, "oracle": {"kind": "first_answer"},
         "truth": {"file": "backend/orders.py", "symbol": "list_orders", "accepted_symbols": ["list_orders"],
                   "span": [6, 8], "literal": "SELECT order_id FROM purchase_orders", "category": "text", "line": 8},
         "grep_solvable": True, "generator": note},
        {"id": "impact-mixed-01", "family": "impact", "repo": "fixture-mixed", "language": "typescript",
         "source_root": "fixtures/mixed", "prompt": fill(template("impact"), symbol="formatOrder", file="frontend/format.ts"),
         "setup": {"kind": "none"},
         "oracle": {"recall": 0.9, "precision": 0.8, "symbol": "formatOrder", "definition_file": "frontend/format.ts"},
         "truth": {"required": ["frontend/format.ts", "frontend/orders.ts"], "neutral": []}, "grep_solvable": True,
         "generator": note},
    ]


# ----------------------------------------------------------------------------- review and checks

def answer_block(value):
    return "```answer\n%s\n```" % json.dumps(value)


def qualified_forms(truth):
    """(symbol, path) answers in the qualified forms Justless and grep print for a location truth."""
    path = truth["file"] + ":%d" % truth.get("line", truth["span"][0])
    symbol = truth.get("symbol")
    if not symbol:
        return [("", path)]
    module = re.sub(r"\.[A-Za-z0-9]+$", "", truth["file"]).replace("/", "::")
    dotted = re.sub(r"\.[A-Za-z0-9]+$", "", truth["file"]).replace("/", ".")
    return [("%s::%s" % (module, symbol), path), ("%s::%s" % (truth["file"], symbol), "./" + truth["file"]),
            ("%s.%s" % (dotted, symbol.replace("::", ".")), path)]


def truth_checks(task):
    """Perturbation checks for location and impact truth. Returns the names of failed checks."""
    failed = []
    if task["family"] == "location":
        truth = task["truth"]
        good = oracles.grade_location(task, answer_block({"answers": [
            {"file": truth["file"], "symbol": truth.get("symbol") or "", "line": truth["span"][0]}]}))
        bad_file = oracles.grade_location(task, answer_block({"answers": [
            {"file": truth["file"] + ".x", "symbol": truth.get("symbol") or ""}]}))
        bad_symbol = oracles.grade_location(task, answer_block({"answers": [
            {"file": truth["file"], "symbol": "zz_not_a_symbol", "line": -100}]}))
        if not good["success"]:
            failed.append("truth does not succeed")
        if bad_file["success"] or bad_symbol["success"]:
            failed.append("perturbed answer succeeds")
        # The forms navigation tools print: module-qualified symbols, a file prefix, and file:line paths.
        for symbol, path in qualified_forms(truth):
            if not oracles.grade_location(task, answer_block({"answers": [
                    {"file": path, "symbol": symbol, "line": truth["span"][0]}]}))["success"]:
                failed.append("qualified answer %r does not succeed" % symbol)
                break
    if task["family"] == "impact":
        required = task["truth"]["required"]
        neutral = task["truth"]["neutral"]
        if not oracles.grade_impact(task, answer_block({"files": required + neutral}))["success"]:
            failed.append("truth plus neutral does not succeed")
        if len(required) >= 10 and oracles.grade_impact(task, answer_block({"files": required[1:]}))["recall"] < 0.9:
            failed.append("required minus one below 0.9 recall")
        if oracles.grade_impact(task, answer_block({"files": required[:1]}))["success"]:
            failed.append("definition alone succeeds")
    return failed


def review(args):
    """Print every private task's question and truth for the blind lead review, and optionally freeze it."""
    task_list = tasks_module.load_private(args.home)
    print("Rubric: reword only for grammar or ambiguity; drop a location task only if two or more symbols plausibly "
          "answer it; check impact truth against the rg evidence; log every edit with a reason in "
          "<store>/tasks/_review_log.jsonl. Do not look at any arm result.")
    for task in task_list:
        print("=" * 80)
        print(task["id"], task.get("style") or "", "grep_solvable=%s" % task.get("grep_solvable"))
        print(task["prompt"])
        print("truth:", json.dumps(task.get("truth") or {"mutated_fn": task.get("oracle", {}).get("mutated_fn")}))
    # The same hash every run ID carries (public and private tasks), so run.py and report.py can compare it.
    public = tasks_module.load_public()
    manifest = tasks_module.manifest_hash(public + task_list)
    print("manifest hash:", manifest)
    if getattr(args, "approve", False):
        path = tasks_module.private_dir(args.home) / "_manifest.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({"hash": manifest, "private_hash": tasks_module.manifest_hash(task_list),
                                    "tasks": sorted(t["id"] for t in public + task_list),
                                    "frozen_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}, indent=1))
        print("frozen", path)
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, default=config.HOME)
    parser.add_argument("--families", default="bugfix,lint,location,impact")
    parser.add_argument("--repos")
    parser.add_argument("--seed", type=int, default=config.SEED)
    parser.add_argument("--public", action="store_true")
    args = parser.parse_args(argv)
    if shutil.which("justless", path=config.SYSTEM_PATH):
        raise SystemExit("justless must not be on the generator's PATH")
    if args.public:
        data = {"note": "Vendor fixtures. Used only for dry runs, unit tests and the parity capture; never analyzed.",
                "tasks": public_tasks()}
        config.PUBLIC_TASKS.write_text(json.dumps(data, indent=1, sort_keys=True) + "\n")
        print("wrote", config.PUBLIC_TASKS)
        return 0
    families = args.families.split(",")
    repos = set(args.repos.split(",")) if args.repos else None
    produced = []
    location_selected = []
    for family in families:
        for repo, count in sorted(PLAN[family].items()):
            if repos and repo not in repos:
                continue
            if family == "bugfix":
                made = gen_python_bugfix(count, args.seed) if repo == "video_tools" else gen_rust_bugfix(
                    repo, count, args.seed)
            elif family == "lint":
                made = gen_lint(repo, count, args.seed)
            elif family == "location":
                selected, pool = gen_location(repo, count, args.seed)
                location_selected += [(repo, index, pool, c) for index, c in selected]
                log(family, repo, "selected", len(selected), "of", count)
                continue
            else:
                made = gen_impact(repo, count, args.seed)
                for other in BACKFILL:
                    if len(made) >= count or other == repo or (repos and other not in repos):
                        continue
                    base = PLAN["impact"].get(other, 0)
                    extra = gen_impact(other, base + count - len(made), args.seed)[base:]
                    for task in extra:
                        task["id"] = "impact-%s-%02d" % (other, base + 1 + len([m for m in made if m["repo"] == other]))
                        task["generator"]["backfill_for"] = repo
                        made.append(task)
            for task in made:
                tasks_module.save_private(args.home, task)
                produced.append(task["id"])
            log(family, repo, "made", len(made), "of", count)
    if location_selected:
        for task in finalize_location(location_selected, args.seed):
            problems = truth_checks(task)
            if problems:
                log("location check failed", task["id"], problems)
            tasks_module.save_private(args.home, task)
            produced.append(task["id"])
    print(json.dumps({"produced": produced}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
