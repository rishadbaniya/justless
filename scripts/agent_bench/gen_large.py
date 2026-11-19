"""Deterministic generation of the large-output family (task set `large`). It never invokes Justless.

The 32 main tasks show each run only a few kilobytes of tool output, while Justless compacts outputs above about
4 KB and the owner's own sessions produce tens of megabytes of shell output a week. These eight tasks are built
so that the plain path to a solution reads large output:

- bugfix: a seeded bug in a large file of the big workspace, reported as `cargo test --workspace` over every
  crate, with no package named;
- lint: one clippy rule to fix in a tree that keeps dozens of other warnings, so the clippy report is long;
- callers: every function that uses a uniquely defined function, which takes a wide search plus the context of
  each hit to name the enclosing function;
- regression: a behavior change hidden in a long behavior-preserving cleanup commit, found through its diff.

Candidates are sorted, then drawn with a seeded random.Random; every verdict comes from cargo, clippy, rg and the
Rust lexer in oracles.py. Tasks go to <store>/tasks/large/, outside the main manifest.

    python3 gen_large.py [--repos REPO,...] [--kinds bugfix,lint,callers,regression]
"""

import argparse
import collections
import difflib
import json
import random
import re
import shutil
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import baseline_build  # noqa: E402
import config  # noqa: E402
import gen_tasks  # noqa: E402
import oracles  # noqa: E402
import tasks as tasks_module  # noqa: E402
import workspace  # noqa: E402

# (kind, repository, count). A kind that finds too few candidates in its repository is filled from FALLBACK.
PLAN = [("bugfix", "prediction_market_workspace", 2), ("lint", "prediction_market_workspace", 1),
        ("lint", "crypto_rust_shared", 1), ("callers", "prediction_market_workspace", 1),
        ("callers", "crypto_rust_shared", 1), ("regression", "crypto_rust_shared", 1),
        ("regression", "prediction_market_workspace", 1)]
FALLBACK = "prediction_market_workspace"
TASK_SET = "large"

BUG_FILE_MIN = 30000          # bytes of the mutated file in a bugfix task
RULE_SITES = 5                # instances of the rule to fix
RULE_FILES_MIN = 3
NOISE_SITES = 48              # other warnings the tree keeps
NOISE_PER_FILE = 2
CALLERS_MIN, CALLERS_MAX = 8, 40
CALLER_FILES_MIN = 4
CALLER_BYTES_MIN = 100000     # total size of the files that hold the callers
CLEANUP_EDITS = 60
CLEANUP_PER_FILE = 3
CLEANUP_KEPT = 0.8            # share of the cleanup a regression fix must keep
SPACING = 200                 # characters between two injected edits in one file
COMMIT_MESSAGE = "refactor: spell out returns, length checks and closures"
# Clippy rewrites that never change behavior and rarely change types; ptr_arg changes a signature.
SAFE_LINTS = {"clippy::needless_return", "clippy::let_and_return", "clippy::unused_unit", "clippy::bool_comparison",
              "clippy::redundant_static_lifetimes", "clippy::len_zero", "clippy::redundant_closure",
              "clippy::iter_count"}
WHY = {
    "bugfix": "the workspace test run lists every crate's tests; the failure is buried in it and the bug sits in a "
              "large file",
    "lint": "the clippy report holds dozens of other warnings around the rule to fix",
    "callers": "a wide search, then the context of every hit, to name each enclosing function",
    "regression": "the cleanup commit's diff is long and the behavior change is one hunk in it",
}


def log(*parts):
    print(*parts, file=sys.stderr, flush=True)


def scope_of(repo):
    return ["--workspace"] if repo in baseline_build.WORKSPACE_REPOS else []


def prepare_work(repo):
    """A fresh work copy and a clone of the pristine warm target with the repository's own crates invalidated:
    Cargo's fingerprints for path packages ignore the path, so a target shared between copies would replay
    stale diagnostics and test binaries."""
    base = config.HOME / "pristine" / repo
    work = base / "work_large"
    if work.exists():
        shutil.rmtree(str(work))
    workspace.clone_tree(base / "repo", work)
    target = base / "large_target"
    if not target.exists():
        workspace.clone_tree(base / "target", target)
    workspace.invalidate_local_fingerprints(work, target)
    return base, work, target


def known_failures(baseline):
    """Prompt text for tests that already failed at baseline, so success is achievable and unambiguous."""
    flaky = set(baseline.get("flaky") or [])
    names = sorted(n for n, s in baseline["test_names"].items() if s == "FAILED" and n not in flaky)
    if not names:
        return ""
    shown = []
    for name in names:
        binary, _, test = name.partition("::")
        shown.append("`%s` in package `%s`" % (test or name, binary.split("@", 1)[0]))
    verb = "test %s already failed" % shown[0] if len(shown) == 1 else "tests %s already failed" % ", ".join(shown)
    return " The %s before this change and depends on the machine; leave it." % verb


def main_task_files(home):
    """Files the main task set mutates, so the large family never reuses a main task's site."""
    out = set()
    for task in tasks_module.load_private(home, "main"):
        oracle = task.get("oracle") or {}
        if oracle.get("mutated_file"):
            out.add(oracle["mutated_file"])
        for edit in (task.get("setup") or {}).get("edits") or []:
            out.add(edit["file"])
    return out


def render_failure(code, output):
    """What Claude Code shows for a failing command: the first and last 5,000 characters of the first 30,000."""
    body = output[:30000].rstrip()
    full = "Exit code %d" % code + ("\n" + body if body else "")
    if len(full) > 10000:
        return full[:5000] + "\n\n... [truncated] ...\n\n" + full[-5000:]
    return full


def strip(edit):
    return {k: edit[k] for k in ("file", "offset", "old", "new") if k in edit}


def apply_texts(pristine, edits):
    """{rel: text with the edits applied} for the files the edits touch."""
    out = {}
    for rel in sorted({e["file"] for e in edits}):
        text = (Path(pristine) / rel).read_text(errors="replace")
        for e in sorted([e for e in edits if e["file"] == rel], key=lambda e: e["offset"], reverse=True):
            text = text[:e["offset"]] + e["new"] + text[e["offset"] + len(e["old"]):]
        out[rel] = text
    return out


def diff_bytes(pristine, edits):
    """Approximate size of `git show` for the edits: unified diffs with three lines of context."""
    total = 200
    for rel, after in apply_texts(pristine, edits).items():
        before = (Path(pristine) / rel).read_text(errors="replace")
        lines = list(difflib.unified_diff(before.split("\n"), after.split("\n"), "a/" + rel, "b/" + rel, lineterm="",
                                          n=3))
        total += len("diff --git a/%s b/%s\nindex 0000000..0000000 100644\n" % (rel, rel))
        total += sum(len(line) + 1 for line in lines)
    return total


def spaced(chosen, candidate, gap=SPACING):
    return all(c["file"] != candidate["file"] or abs(c["offset"] - candidate["offset"]) >= gap for c in chosen)


def rewrite_candidates(pristine, exposed, avoid, lints):
    """Behavior-preserving rewrites outside tests, as dicts with lint, file, offset, old, new and package."""
    out = []
    for rel in gen_tasks.files(pristine, [".rs"]):
        if gen_tasks.is_rust_test_path(rel) or rel in exposed or rel in avoid:
            continue
        text = (Path(pristine) / rel).read_text(errors="replace")
        regions = oracles.rust_test_regions(text)
        package = gen_tasks.package_of(pristine, rel)
        if not package:
            continue
        for lint, offset, old, new in gen_tasks.lint_transforms(text):
            line = gen_tasks.line_at(text, offset)
            if lint in lints and not any(a <= line <= b for a, b in regions):
                out.append({"lint": lint, "file": rel, "offset": offset, "old": old, "new": new, "package": package})
    out.sort(key=lambda c: (c["file"], c["offset"], c["lint"]))
    return out


# ----------------------------------------------------------------------------- bugs

def find_bugs(repo, base, work, target, baseline, count, rng, file_ok, avoid):
    """Validated mutations as in the main bug-fix family (deterministic failure, a second-mutation control),
    restricted to files that file_ok accepts. Returns (accepted, pool, attempts)."""
    pristine = base / "repo"
    flaky = set(baseline.get("flaky") or [])
    sites = []
    for rel in gen_tasks.files(pristine, [".rs"]):
        if gen_tasks.is_rust_test_path(rel) or rel in avoid or not file_ok(rel):
            continue
        found, _ = gen_tasks.rust_sites(pristine, rel)
        package = gen_tasks.package_of(pristine, rel)
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
    order = gen_tasks.balanced(list(sites), rng)
    exposed = gen_tasks.exposed_files(repo, pristine, sorted({s["file"] for s in sites}))
    accepted, attempts, used = [], [], set()
    for index, candidate in enumerate(order):
        if len(accepted) >= count:
            break
        if candidate["file"] in exposed or candidate["file"] in used:
            continue
        result, reason = gen_tasks.validate_rust_candidate(repo, work, target, candidate, sites_by_fn, rng, flaky)
        attempts.append({"index": index, "kind": candidate["kind"], "accepted": result is not None, "reason": reason})
        log("large bug", repo, candidate["kind"], "%s:%d" % (candidate["file"], candidate["line"] + 1), reason or "ok")
        if result is not None:
            used.add(candidate["file"])
            accepted.append((index, candidate, result))
    return accepted, pool, attempts


def bug_oracle(baseline, candidate, failing, pristine, repo, span=None, anchor=None):
    return {"test_command": baseline["test_command"], "mutated_file": candidate["file"],
            "mutated_span": span or candidate["fn_span"], "mutated_fn": candidate["fn"],
            "anchor_line": anchor, "noop_comment": " // checked",
            "tamper_edit": gen_tasks.tamper_edit(pristine, failing.split("::", 1)[1] if "::" in failing else None),
            "cargo_scope": scope_of(repo)}


def gen_bugfix(repo, count, seed, avoid, start=1, label=""):
    base, work, target = prepare_work(repo)
    pristine = base / "repo"
    baseline = json.loads((base / "baseline.json").read_text())
    rng = random.Random("%d|large-bugfix|%s%s" % (seed, repo, label))
    accepted, pool, attempts = find_bugs(
        repo, base, work, target, baseline, count, rng,
        lambda rel: (pristine / rel).stat().st_size >= BUG_FILE_MIN, avoid)
    command = " ".join(baseline["test_command"])
    out = []
    for n, (index, candidate, result) in enumerate(accepted, start):
        text = (pristine / candidate["file"]).read_text(errors="replace")
        mutated, original = gen_tasks.unique_context(text, candidate["offset"], candidate["old"], candidate["new"])
        gen_tasks.apply_one(work, candidate)
        code, output, _ = baseline_build.run(baseline["test_command"], work, target)
        gen_tasks.revert_one(work, candidate)
        shown = render_failure(code, output)
        visible = any(name.split("::")[-1] in shown for name in result["failing"])
        out.append({
            "id": "large-bugfix-%s-%02d" % (repo, n), "family": "large", "kind": "bugfix", "repo": repo,
            "language": "rust", "package": candidate["package"], "visible_command": command,
            "prompt": gen_tasks.fill(gen_tasks.template("large_bugfix"), command=command,
                                     known=known_failures(baseline)),
            "setup": {"kind": "mutation", "edits": [strip(candidate)]},
            "oracle": bug_oracle(baseline, candidate, result["failing"][0], pristine, repo,
                                 anchor=mutated.split("\n")[0]),
            "baseline": {"test_names": baseline["test_names"], "flaky": sorted(baseline.get("flaky") or []),
                         "tree_hash": workspace.tree_hash(pristine)},
            "controls": {"inverse": [{"file": candidate["file"], "old": mutated, "new": original}],
                         "inverse_files": {candidate["file"]: text}, "second_mutation": result["control"],
                         "failing_tests": result["failing"]},
            "large": {"why": WHY["bugfix"], "file_bytes": len(text.encode("utf-8")),
                      "test_output_bytes": len(output.encode("utf-8")), "failure_in_excerpt": visible},
            "generator": {"seed": seed, "candidate_index": index, "candidate_pool_size": pool,
                          "kind": candidate["kind"], "attempts": attempts},
        })
    shutil.rmtree(str(work), ignore_errors=True)
    return out


# ----------------------------------------------------------------------------- lint

def edit_spans(texts, edits):
    """(file, first line, last line), 1-based, of each edit in the mutated texts."""
    out = []
    for e in edits:
        text = texts[e["file"]]
        out.append((e["file"], gen_tasks.line_at(text, e["new_offset"]) + 1,
                    gen_tasks.line_at(text, e["new_offset"] + len(e["new"])) + 1))
    return out


def verify_injection(work, target, command, base_msgs, chosen, rounds=8):
    """Apply all edits, run clippy once, and keep each edit only if exactly its own lint fires on its lines and
    nothing new fires elsewhere in its file. Repeats until every kept edit is accounted for."""
    current = [dict(c) for c in chosen]
    base_cf = collections.Counter((m["code"], m["file"]) for m in base_msgs)
    for _ in range(rounds):
        edits = tasks_module.with_new_offsets(current)
        tasks_module.apply_edits(work, edits)
        _, output, _ = baseline_build.run(command, work, target)
        texts = {rel: (Path(work) / rel).read_text(errors="replace") for rel in {e["file"] for e in edits}}
        tasks_module.apply_edits(work, edits, reverse=True)
        messages = oracles.clippy_messages(output)
        spans = edit_spans(texts, edits)
        matched = [[] for _ in edits]
        unexplained = []
        for m in messages:
            hits = [i for i, (rel, a, b) in enumerate(spans) if m["file"] == rel and a <= (m["line"] or 0) <= b]
            for i in hits:
                matched[i].append(m["code"])
            if not hits:
                unexplained.append(m)
        errors = {m["file"] for m in messages if m["level"] == "error"}
        if errors:
            # A crate that does not compile hides every crate that depends on it from clippy, so edits without
            # a message prove nothing this round: drop only the edits in files that report an error.
            bad = {i for i, e in enumerate(edits) if e["file"] in errors}
            if not bad:
                raise RuntimeError("clippy reports errors outside the injected files: %s" % sorted(errors)[:3])
        else:
            bad = {i for i, e in enumerate(edits) if matched[i] != [e["lint"]]}
            extra = collections.Counter((m["code"], m["file"]) for m in unexplained) - base_cf
            for (_, rel) in extra:
                bad |= {i for i, e in enumerate(edits) if e["file"] == rel}
        if not bad:
            return edits, messages
        log("large lint: dropping", len(bad), "of", len(edits))
        current = [{k: v for k, v in e.items() if k != "new_offset"} for i, e in enumerate(edits) if i not in bad]
    raise RuntimeError("injection did not settle")


def pick(cands, n, rng, per_file, chosen=(), key=None):
    """Up to n candidates, at most per_file per file and SPACING apart, drawn round-robin over `key` groups."""
    cands = list(cands)
    rng.shuffle(cands)
    groups = collections.OrderedDict()
    for c in cands:
        groups.setdefault(key(c) if key else "", []).append(c)
    names = sorted(groups)
    rng.shuffle(names)
    out = []
    taken = list(chosen)
    while len(out) < n and any(groups.values()):
        for name in names:
            while groups[name]:
                c = groups[name].pop(0)
                if sum(1 for o in out if o["file"] == c["file"]) < per_file and spaced(taken, c):
                    out.append(c)
                    taken.append(c)
                    break
            if len(out) >= n:
                break
    return out


def gen_lint(repo, seed, avoid, n=1, label=""):
    base, work, target = prepare_work(repo)
    pristine = base / "repo"
    baseline = json.loads((base / "baseline.json").read_text())
    scope = scope_of(repo)
    json_cmd = ["cargo", "clippy", "--offline", "--all-targets", "--message-format=json"] + scope
    code, output, _ = baseline_build.run(json_cmd, work, target)
    base_msgs = oracles.clippy_messages(output)
    if oracles.compile_failed(output):
        raise RuntimeError("baseline clippy does not compile for %s" % repo)
    base_counts = oracles.lint_counts(base_msgs)
    rng = random.Random("%d|large-lint|%s%s" % (seed, repo, label))
    exposed = gen_tasks.exposed_files(repo, pristine, gen_tasks.files(pristine, [".rs"]))
    cands = rewrite_candidates(pristine, exposed, avoid, SAFE_LINTS)
    by_lint = collections.defaultdict(list)
    for c in cands:
        by_lint[c["lint"]].append(c)
    eligible = sorted(l for l, cs in by_lint.items() if not base_counts.get(l)
                      and len({c["file"] for c in cs}) >= RULE_FILES_MIN + 2
                      and (not scope or len({c["package"] for c in cs}) >= 2))
    rng.shuffle(eligible)
    log("large lint", repo, "baseline warnings", len(base_msgs), "eligible rules", eligible)
    for rule in eligible:
        targets = pick(by_lint[rule], RULE_SITES + 2, rng, 1)
        noise = pick([c for c in cands if c["lint"] != rule], NOISE_SITES + 12, rng, NOISE_PER_FILE, targets,
                     key=lambda c: c["lint"])
        try:
            edits, messages = verify_injection(work, target, json_cmd, base_msgs, targets + noise)
        except RuntimeError as error:
            log("large lint", repo, rule, error)
            continue
        kept_rule = [e for e in edits if e["lint"] == rule][:RULE_SITES]
        kept_noise = [e for e in edits if e["lint"] != rule][:NOISE_SITES]
        if len(kept_rule) < RULE_SITES - 1 or len({e["file"] for e in kept_rule}) < RULE_FILES_MIN \
                or len(kept_noise) < NOISE_SITES * 2 // 3:
            log("large lint", repo, rule, "too few kept", len(kept_rule), len(kept_noise))
            continue
        try:
            edits, messages = verify_injection(work, target, json_cmd, base_msgs,
                                               [{k: v for k, v in e.items() if k != "new_offset"}
                                                for e in kept_rule + kept_noise], rounds=1)
        except RuntimeError as error:
            log("large lint", repo, rule, "trimmed set", error)
            continue
        tasks_module.apply_edits(work, edits)
        _, human, _ = baseline_build.run(["cargo", "clippy", "--offline", "--all-targets"] + scope, work, target)
        _, tout, _ = baseline_build.run(baseline["test_command"], work, target)
        tasks_module.apply_edits(work, edits, reverse=True)
        missing = oracles.missing_names(baseline["test_names"], oracles.parse_tests(tout),
                                        set(baseline.get("flaky") or []))
        if missing:
            log("large lint", repo, rule, "tests changed", missing[:3])
            continue
        rule_edits = [e for e in edits if e["lint"] == rule]
        texts = apply_texts(pristine, edits)
        noise_only = apply_texts(pristine, [e for e in edits if e["lint"] != rule])
        inverse = [gen_tasks.lint_inverse(texts[e["file"]], e) for e in rule_edits]
        inverse_files = {rel: noise_only.get(rel, (pristine / rel).read_text(errors="replace"))
                         for rel in sorted({e["file"] for e in rule_edits})}
        command = " ".join(["cargo", "clippy", "--offline", "--all-targets"] + scope)
        shutil.rmtree(str(work), ignore_errors=True)
        return [{
            "id": "large-lint-%s-%02d" % (repo, n), "family": "large", "kind": "lint", "repo": repo,
            "language": "rust", "visible_command": command,
            "prompt": gen_tasks.fill(gen_tasks.template("large_lint"), command=command, rule=rule,
                                     known=known_failures(baseline)),
            "setup": {"kind": "lint_injection", "edits": [dict(strip(e), new_offset=e["new_offset"]) for e in edits]},
            "oracle": {"clippy_command": json_cmd, "rule": rule, "start_counts": oracles.lint_counts(messages),
                       "test_command": baseline["test_command"], "injected_lints": [rule] * len(rule_edits),
                       "noise_lints": sorted(e["lint"] for e in edits if e["lint"] != rule),
                       "cargo_scope": scope_of(repo)},
            "baseline": {"test_names": baseline["test_names"], "flaky": baseline.get("flaky") or [],
                         "clippy_counts": base_counts, "tree_hash": workspace.tree_hash(pristine)},
            "controls": {"inverse": inverse, "inverse_files": inverse_files},
            "large": {"why": WHY["lint"], "clippy_human_bytes": len(human.encode("utf-8")),
                      "warnings": len(messages), "rule_sites": len(rule_edits)},
            "generator": {"seed": seed, "label": label, "rule_pool": eligible, "candidates": len(cands)},
        }]
    shutil.rmtree(str(work), ignore_errors=True)
    return []


# ----------------------------------------------------------------------------- callers

def caller_units(root, name, definition_file):
    """(required, neutral, method_calls): one unit per function whose code mentions `name`; neutral units
    mention it only in comments or strings. The definition itself and mentions outside any function are left
    out. method_calls counts `.name` mentions, which may be another type's method of the same name."""
    required, neutral = {}, {}
    method_calls = 0
    for rel in gen_tasks.rg_files(root, name, word=True):
        if not rel.endswith(".rs"):
            continue
        text = (Path(root) / rel).read_text(errors="replace")
        masked = oracles.rust_mask(text)
        fns = oracles.rust_functions(text)
        for m in re.finditer(r"\b%s\b" % re.escape(name), text):
            owner = None
            for fn in fns:
                if fn[4] <= m.start() <= fn[5] and (owner is None or fn[4] > owner[4]):
                    owner = fn
            if owner is None or (rel == definition_file and owner[0] == name):
                continue
            unit = {"file": rel, "symbol": owner[1], "accepted": sorted({owner[1], owner[0]}),
                    "span": [owner[2] + 1, owner[3] + 1]}
            in_code = masked[m.start():m.end()] == m.group(0)
            if in_code and masked[:m.start()].rstrip().endswith("."):
                method_calls += 1
            (required if in_code else neutral).setdefault((rel, owner[1], owner[2]), unit)
    neutral = {k: v for k, v in neutral.items() if k not in required}
    order = sorted(required, key=lambda k: (k[0], k[2], k[1]))
    return ([required[k] for k in order], [neutral[k] for k in sorted(neutral, key=lambda k: (k[0], k[2], k[1]))],
            method_calls)


def free_function(root, rel, name):
    """True when `name` is defined in rel as a free function, not a method, so every unqualified call or path
    mention resolves to it."""
    text = (Path(root) / rel).read_text(errors="replace")
    found = [fn for fn in oracles.rust_functions(text) if fn[0] == name]
    return len(found) == 1 and found[0][1] == name and not re.search(
        r"\btrait\s+\w+[^{;]*\{[^}]*\bfn\s+%s\b" % re.escape(name), oracles.rust_mask(text))


def gen_callers(repo, seed, count=1, start=1, label="", skip=()):
    root = config.CORPUS / repo
    rng = random.Random("%d|large-callers|%s%s" % (seed, repo, label))
    pool = [d for d in gen_tasks.definitions(repo, root) if d["kind"] == "fn"]
    indices = list(range(len(pool)))
    rng.shuffle(indices)
    out = []
    for index in indices:
        if len(out) >= count:
            break
        d = pool[index]
        name = d["name"]
        if len(name) < 6 or name.lower() in gen_tasks.COMMON_NAMES or name in skip:
            continue
        if gen_tasks.definition_count(root, name, "rust") != 1:
            continue
        if not free_function(root, d["file"], name):
            continue
        required, neutral, method_calls = caller_units(root, name, d["file"])
        if method_calls:
            continue
        files = sorted({u["file"] for u in required})
        if not (CALLERS_MIN <= len(required) <= CALLERS_MAX and len(files) >= CALLER_FILES_MIN):
            continue
        size = sum((root / f).stat().st_size for f in files)
        if size < CALLER_BYTES_MIN:
            continue
        log("large callers", repo, "%d callers in %d files, %d bytes" % (len(required), len(files), size))
        out.append({
            "id": "large-callers-%s-%02d" % (repo, start + len(out)), "family": "large", "kind": "callers",
            "repo": repo, "language": "rust",
            "prompt": gen_tasks.fill(gen_tasks.template("large_callers"), symbol=name, file=d["file"]),
            "setup": {"kind": "none"},
            "oracle": {"recall": 0.9, "precision": 0.8, "symbol": name, "definition_file": d["file"]},
            "truth": {"required": required, "neutral": neutral},
            "grep_solvable": False,
            "large": {"why": WHY["callers"], "caller_files": len(files), "caller_file_bytes": size},
            "generator": {"seed": seed, "candidate_index": index, "candidate_pool_size": len(pool)},
        })
    return out


# ----------------------------------------------------------------------------- regression

ERROR_AT = re.compile(r"^\s*--> ([^:\s]+):(\d+):\d+", re.M)


def gen_regression(repo, seed, avoid, n=1, label=""):
    base, work, target = prepare_work(repo)
    pristine = base / "repo"
    baseline = json.loads((base / "baseline.json").read_text())
    flaky = set(baseline.get("flaky") or [])
    rng = random.Random("%d|large-regression|%s%s" % (seed, repo, label))
    exposed = gen_tasks.exposed_files(repo, pristine, gen_tasks.files(pristine, [".rs"]))
    cands = rewrite_candidates(pristine, exposed, avoid, SAFE_LINTS)
    per_file = collections.Counter(c["file"] for c in cands)
    accepted, pool, attempts = find_bugs(repo, base, work, target, baseline, 1, rng,
                                         lambda rel: per_file[rel] >= 2, avoid)
    if not accepted:
        shutil.rmtree(str(work), ignore_errors=True)
        return []
    index, bug, result = accepted[0]
    bug_edit = {"lint": None, "file": bug["file"], "offset": bug["offset"], "old": bug["old"], "new": bug["new"]}
    near = [c for c in cands if c["file"] == bug["file"]]
    chosen = pick(near, 2, rng, 2, [dict(bug_edit)], key=None)
    others = [c for c in cands if c["file"] != bug["file"]]
    chosen += pick(others, CLEANUP_EDITS - len(chosen), rng, CLEANUP_PER_FILE, chosen + [dict(bug_edit)],
                   key=lambda c: c["lint"])
    for _ in range(6):
        edits = tasks_module.with_new_offsets(chosen)
        tasks_module.apply_edits(work, edits)
        _, output, _ = baseline_build.run(baseline["test_command"], work, target)
        tasks_module.apply_edits(work, edits, reverse=True)
        if oracles.compile_failed(output):
            broken = {m.group(1) for m in ERROR_AT.finditer(output)}
            log("large regression: cleanup does not compile in", sorted(broken)[:5])
            keep = [c for c in chosen if c["file"] not in broken]
            if len(keep) == len(chosen):
                raise RuntimeError("cleanup does not compile: %s" % output[-1500:])
            chosen = keep
            continue
        missing = oracles.missing_names(baseline["test_names"], oracles.parse_tests(output), flaky)
        if missing:
            # A rewrite clippy calls equivalent can still change what a test observes (a source-text check, a
            # custom len). Drop the cleanup in the packages whose tests changed and try again.
            packages = {n.split("@", 1)[0] for n in missing}
            keep = [c for c in chosen if c["package"] not in packages and c["package"].replace("-", "_") not in packages]
            log("large regression: cleanup changed tests in", sorted(packages))
            if len(keep) == len(chosen):
                raise RuntimeError("cleanup changed test results: %s" % missing[:5])
            chosen = keep
            continue
        break
    else:
        raise RuntimeError("cleanup did not settle")
    full = tasks_module.with_new_offsets(chosen + [bug_edit])
    tasks_module.apply_edits(work, full)
    _, output, _ = baseline_build.run(baseline["test_command"], work, target)
    tasks_module.apply_edits(work, full, reverse=True)
    failed = sorted(n for n, s in oracles.parse_tests(output).items() if s == "FAILED")
    want = sorted(set(result["failing"]) | {n for n, s in baseline["test_names"].items() if s == "FAILED"})
    if oracles.compile_failed(output) or not set(result["failing"]) <= set(failed) or not set(failed) <= set(want):
        shutil.rmtree(str(work), ignore_errors=True)
        raise RuntimeError("bug and cleanup together do not fail as the bug alone: %s" % failed[:5])
    texts = apply_texts(pristine, full)
    cleanup_only = apply_texts(pristine, [e for e in full if e["lint"]])
    bug_final = next(e for e in full if not e["lint"])
    inverse = gen_tasks.lint_inverse(texts[bug["file"]], bug_final)
    fns = oracles.rust_functions(texts[bug["file"]])
    owner = None
    for fn in fns:
        if fn[4] <= bug_final["new_offset"] <= fn[5] and (owner is None or fn[4] > owner[4]):
            owner = fn
    contexts = []
    for e in full:
        if e["lint"]:
            context = gen_tasks.lint_inverse(texts[e["file"]], e)["old"]
            contexts.append({"file": e["file"], "text": context,
                             "line": gen_tasks.line_at(texts[e["file"]], e["new_offset"]) + 1})
    command = " ".join(baseline["test_command"])
    oracle = bug_oracle(baseline, bug, result["failing"][0], pristine, repo, span=[owner[2], owner[3]],
                        anchor=inverse["old"].split("\n")[0])
    oracle.update(cleanup_kept=CLEANUP_KEPT, cleanup_contexts=contexts)
    size = diff_bytes(pristine, full)
    shutil.rmtree(str(work), ignore_errors=True)
    return [{
        "id": "large-regression-%s-%02d" % (repo, n), "family": "large", "kind": "regression", "repo": repo,
        "language": "rust", "package": bug["package"], "visible_command": command,
        "prompt": gen_tasks.fill(gen_tasks.template("large_regression"), command=command,
                                 known=known_failures(baseline)),
        "setup": {"kind": "history", "commit_message": COMMIT_MESSAGE,
                  "edits": [dict(strip(e), new_offset=e["new_offset"]) for e in full]},
        "oracle": oracle,
        "baseline": {"test_names": baseline["test_names"], "flaky": sorted(flaky),
                     "tree_hash": workspace.tree_hash(pristine)},
        "controls": {"inverse": [inverse],
                     "inverse_files": {bug["file"]: cleanup_only.get(bug["file"],
                                                                     (pristine / bug["file"]).read_text(errors="replace"))},
                     "second_mutation": result["control"], "failing_tests": result["failing"]},
        "large": {"why": WHY["regression"], "diff_bytes": size, "cleanup_edits": len(contexts),
                  "cleanup_files": len({c["file"] for c in contexts})},
        "generator": {"seed": seed, "candidate_index": index, "candidate_pool_size": pool, "kind": bug["kind"],
                      "attempts": attempts, "cleanup_pool": len(cands)},
    }]


# ----------------------------------------------------------------------------- main

def generate(kind, repo, count, seed, used, start=1, label=""):
    """At most `count` tasks of one kind from one repository, numbered from `start`, avoiding the files and
    symbols in `used`. A repository that yields no valid task returns none, and the plan fills from FALLBACK."""
    try:
        if kind == "bugfix":
            return gen_bugfix(repo, count, seed, used["files"], start, label)
        if kind == "lint":
            return gen_lint(repo, seed, used["files"], start, label)
        if kind == "callers":
            return gen_callers(repo, seed, count, start, label, used["symbols"])
        return gen_regression(repo, seed, used["files"], start, label)
    except RuntimeError as error:
        log("large", kind, repo, "no task:", str(error)[:500])
        return []


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, default=config.HOME)
    parser.add_argument("--repos")
    parser.add_argument("--kinds", default=",".join(tasks_module.LARGE_KINDS))
    parser.add_argument("--seed", type=int, default=config.SEED)
    args = parser.parse_args(argv)
    if shutil.which("justless", path=config.SYSTEM_PATH):
        raise SystemExit("justless must not be on the generator's PATH")
    config.HOME = Path(args.home)
    kinds = args.kinds.split(",")
    repos = set(args.repos.split(",")) if args.repos else None
    used = {"files": main_task_files(args.home), "symbols": set()}
    numbers = collections.Counter()
    for task in tasks_module.load_private(args.home, TASK_SET):
        plan_repo = (task.get("generator") or {}).get("plan", [None, task["repo"]])[1]
        if task["kind"] in kinds and (repos is None or plan_repo in repos):
            # Regenerated below: drop the old file so a rerun draws exactly as the first run did.
            (tasks_module.private_dir(args.home, TASK_SET) / ("%s.json" % task["id"])).unlink()
            continue
        used["files"].update(e["file"] for e in (task.get("setup") or {}).get("edits") or [])
        used["symbols"].add((task.get("oracle") or {}).get("symbol"))
        # Kept tasks keep their numbers, so a partial rerun numbers new tasks as a full run would.
        numbers[(task["kind"], task["repo"])] += 1
    produced = []
    for kind, repo, count in PLAN:
        if kind not in kinds or (repos and repo not in repos):
            continue
        made = generate(kind, repo, count, args.seed, used, numbers[(kind, repo)] + 1)
        if len(made) < count and repo != FALLBACK:
            log("large", kind, repo, "made", len(made), "of", count, "; filling from", FALLBACK)
            made += generate(kind, FALLBACK, count - len(made), args.seed, used, numbers[(kind, FALLBACK)] + 1,
                             label="|fallback")
        for task in made:
            numbers[(kind, task["repo"])] += 1
            used["files"].update(e["file"] for e in (task.get("setup") or {}).get("edits") or [])
            used["symbols"].add((task.get("oracle") or {}).get("symbol"))
            task["generator"]["plan"] = [kind, repo, count]
            tasks_module.save_private(args.home, tasks_module.validate(task), TASK_SET)
            produced.append(task["id"])
        log("large", kind, repo, "made", len(made), "of", count)
    print(json.dumps({"produced": produced}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
