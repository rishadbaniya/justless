#!/usr/bin/env python3
"""Real-agent A/B benchmark for Justless. Run `python3 scripts/agent_bench/run.py <command> --help`.

Commands: prepare, validate-tasks, review-tasks, cc-capture, parity-diff, sandbox-check,
dry-run, pilot, estimate, run, anchor, reconcile, report, export-audit.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import accounting  # noqa: E402
import api as api_module  # noqa: E402
import cc_hooks  # noqa: E402
import cc_profile  # noqa: E402
import cc_tools  # noqa: E402
import config  # noqa: E402
import fake_agent  # noqa: E402
import fake_api  # noqa: E402
import loop  # noqa: E402
import oracles  # noqa: E402
import sandbox  # noqa: E402
import tasks as tasks_module  # noqa: E402
import workspace  # noqa: E402

TERMINAL = {"success", "wrong_fix", "untouched_site", "build_broken", "test_tamper", "lint_remaining",
            "lint_suppressed", "wrong_answer", "partial_answer", "no_answer", "turn_limit", "token_limit",
            "run_budget", "wall_timeout", "max_tokens", "refusal", "harness_unrecoverable"}
# Re-run without using up the harness-error retry budget: a cap abort, or an infrastructure pause such as
# count_tokens failing after its retries.
RERUN = {"harness_error", "aborted_global_cap", "paused_infra"}
LIMIT_STOPS = {"turn_limit", "token_limit", "run_budget", "wall_timeout"}
GATE_MIN_RUNS = 20
GATE_POINTS = 0.02


def harness_commit():
    try:
        commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=str(config.REPO), capture_output=True,
                                text=True, check=True).stdout.strip()
        dirty = subprocess.run(["git", "status", "--porcelain", "--", "scripts/agent_bench"], cwd=str(config.REPO),
                               capture_output=True, text=True).stdout.strip()
        if dirty:
            digest = hashlib.sha256(subprocess.run(["git", "diff", "--", "scripts/agent_bench"], cwd=str(config.REPO),
                                                   capture_output=True).stdout).hexdigest()[:8]
            return commit[:12] + "-dirty-" + digest
        return commit[:12]
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def pin_binary(home, binary):
    """Copy the binary under test into the store under its SHA-256 and return the copy. Every run installs
    from the copy, so a rebuild at the original path during a campaign cannot change what later runs use."""
    binary = Path(binary).resolve()
    sha = config.sha256_file(binary)
    dest = Path(home) / "bins" / ("justless-" + sha[:16])
    if not dest.exists() or config.sha256_file(dest) != sha:
        dest.parent.mkdir(parents=True, exist_ok=True)
        tmp = dest.with_name(dest.name + ".tmp%d" % os.getpid())
        shutil.copy2(str(binary), str(tmp))
        os.chmod(str(tmp), 0o555)
        os.replace(str(tmp), str(dest))
    if config.sha256_file(dest) != sha:
        raise RuntimeError("the pinned justless copy does not match %s" % sha)
    return dest, sha


def install_pinned(source, dest, sha):
    workspace.install_binary(source, dest)
    if config.sha256_file(dest) != sha:
        raise RuntimeError("installed justless %s does not match the pinned sha256 %s" % (dest, sha[:16]))


# ----------------------------------------------------------------------------- snapshots

def own_repos(task):
    """Repository names whose Cargo git cache a run of this task must not read (vendor fixtures have none)."""
    return [] if task.get("source_root") else [task["repo"]]


def snapshot_dir(home, task):
    return Path(home) / "snapshots" / task["id"]


def build_snapshot(home, task, force=False):
    """Prepare <store>/snapshots/<id>/repo (+ target for Rust edit tasks) from the corpus or a fixture."""
    snap = snapshot_dir(home, task)
    repo = snap / "repo"
    if repo.exists() and not force:
        return snap
    if snap.exists():
        shutil.rmtree(str(snap))
    snap.mkdir(parents=True)
    source = Path(task["source_root"]) if task.get("source_root") else config.CORPUS / task["repo"]
    if not source.is_absolute():
        source = config.REPO / source
    workspace.copy_source(source, repo)
    setup = task.get("setup") or {}
    if setup.get("edits"):
        tasks_module.apply_edits(repo, setup["edits"])
    for rel, text in (setup.get("files") or {}).items():
        path = repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    workspace.normalize_mtimes(repo)
    (snap / "tree_hash").write_text(workspace.tree_hash(repo))
    return snap


def warm_target(home, task, runner_env_factory):
    """Build the snapshot's warm target in place so each run can clone it."""
    snap = snapshot_dir(home, task)
    if task["language"] != "rust" or not tasks_module.is_edit(task):
        return None
    target = snap / "target"
    marker = snap / "target.ok"
    if marker.exists():
        return target
    pristine_target = Path(home) / "pristine" / task["repo"] / "target"
    if task["family"] == "large" and not target.exists() and pristine_target.exists():
        # APFS clone of the repository's warm target, so a workspace snapshot stores only its own crates.
        workspace.clone_tree(pristine_target, target)
        workspace.invalidate_local_fingerprints(snap / "repo", target)
    env = runner_env_factory(snap / "repo", target)
    commands = [["cargo", "test", "--offline", "--no-run"] + task["oracle"].get("cargo_scope", [])]
    if tasks_module.kind(task) == "lint":
        commands.append(["cargo", "clippy", "--offline", "--all-targets"] + task["oracle"].get("cargo_scope", []))
    for command in commands:
        proc = subprocess.run(command, cwd=str(snap / "repo"), env=env, capture_output=True, timeout=3600)
        if proc.returncode != 0 and tasks_module.kind(task) != "lint" and "--no-run" not in command:
            raise RuntimeError("warm build failed: %s" % proc.stderr.decode()[-800:])
    # A library without a committed Cargo.lock gets one from the build; record the snapshot as runs see it.
    workspace.normalize_mtimes(snap / "repo")
    (snap / "tree_hash").write_text(workspace.tree_hash(snap / "repo"))
    marker.write_text(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
    return target


def host_env(root, target):
    """Environment for snapshot builds outside a run (no sandbox; store paths are fine here)."""
    home = Path(root).parent / "build-home"
    return workspace.tool_env(root, home, make_shared_toolbin(), target)


_toolbin_lock = threading.Lock()


def make_shared_toolbin():
    path = config.HOME / "toolbin"
    with _toolbin_lock:
        if not path.exists():
            workspace.make_toolbin(path)
    return path


# ----------------------------------------------------------------------------- one run

class Settings:
    def __init__(self, args, campaign, profile, ledger, pricing, client_factory, fake=None, variant=None):
        self.args = args
        self.campaign = campaign
        self.profile = profile
        self.ledger = ledger
        self.pricing = pricing
        self.client_factory = client_factory
        self.fake = fake
        self.variant = variant
        self.home = Path(args.home)
        self.runroot = Path(args.runroot)
        if args.binary:
            self.binary, self.justless_sha = pin_binary(self.home, args.binary)
        else:
            self.binary, self.justless_sha = None, "none"
        self.cc_binary = args.cc_binary
        self.commit = harness_commit()
        self.harness = config.harness_tree_hash()
        self.dir = self.home / "campaigns" / campaign
        self.dir.mkdir(parents=True, exist_ok=True)
        self.logs = {name: loop.JsonlWriter(self.dir / ("%s.jsonl" % name))
                     for name in ("requests", "tools", "attempts", "results")}
        self.ce_trigger = getattr(args, "ce_trigger", None) or config.CE_PILOT_TRIGGER
        self.seed = getattr(args, "seed", None) or config.SEED
        self.max_output = getattr(args, "max_output_tokens", None) or config.MAX_OUTPUT_CAP
        self.limits = {"turn_limit": getattr(args, "max_turns", None) or config.LIMITS["turn_limit"],
                       "token_limit": getattr(args, "token_limit", None) or config.LIMITS["token_limit"],
                       "wall_timeout": config.LIMITS["wall_timeout"]}
        self.ignore_gate = bool(getattr(args, "ignore_gate", False))
        self.pause_reason = None
        self.task_locks = {}
        self.lock = threading.Lock()

    def run_budget(self, model):
        return getattr(self.args, "run_cap_usd", None) or config.MODELS[model]["run_budget"]

    def pause(self, reason):
        with self.lock:
            if self.pause_reason is None:
                self.pause_reason = reason

    def task_lock(self, task_id):
        with self.lock:
            return self.task_locks.setdefault(task_id, threading.Lock())


_count_lock = threading.Lock()


def make_token_counter(client, model, cache):
    """Claude Code's Read cap counter: the free count_tokens endpoint, cached by (model, sha256) so paired
    arms see the same cap decision. A failure becomes a harness error rather than a guess."""
    def counter(text):
        key = model + ":" + hashlib.sha256(text.encode("utf-8")).hexdigest()
        with _count_lock:
            data = {}
            if cache.exists():
                try:
                    data = json.loads(cache.read_text())
                except ValueError:
                    data = {}
            if key in data:
                return data[key]
        try:
            value = client.count_tokens(model, text)
        except api_module.APIError as error:
            # count_tokens already retried with backoff; a persistent failure pauses the campaign instead of
            # re-rolling the run, which would give the arm that reads more large files more re-rolls.
            raise loop.HarnessError("infra_pause", "count_tokens: %s" % str(error)[:300])
        with _count_lock:
            data = {}
            if cache.exists():
                try:
                    data = json.loads(cache.read_text())
                except ValueError:
                    data = {}
            data[key] = value
            tmp = cache.with_suffix(".tmp")
            tmp.write_text(json.dumps(data))
            os.replace(str(tmp), str(cache))
        return value
    return counter


def run_once(settings, task, model, effort_name, arm, rep, attempt, manifest, order_position=None, phase="main"):
    args = settings.args
    effort = settings.profile.effort(model) if effort_name == "cc-default" else effort_name
    rid = config.run_id(settings.campaign, model, effort_name, arm, task["id"], rep, manifest,
                        settings.justless_sha, settings.harness, settings.profile.capture_hash)
    nonce = config.nonce(rid, attempt)
    run_dir = settings.runroot / hashlib.sha256((rid + str(attempt)).encode()).hexdigest()[:12]
    if run_dir.exists():
        shutil.rmtree(str(run_dir))
    timings = {}
    t0 = time.time()
    snap = snapshot_dir(settings.home, task)
    repo = run_dir / "repo"
    workspace.clone_tree(snap / "repo", repo)
    target = run_dir / "target"
    if (snap / "target").exists():
        timings["target_clone"] = workspace.clone_tree(snap / "target", target)
        timings["fingerprints_invalidated"] = workspace.invalidate_local_fingerprints(repo, target)
    home = run_dir / "home"
    home.mkdir(parents=True, exist_ok=True)
    (home / ".claude" / "projects" / cc_profile.slug(repo.resolve()) / "memory").mkdir(parents=True, exist_ok=True)
    bindir = workspace.make_toolbin(run_dir / "bin")
    justless_dir = None
    allow_exec = []
    if config.ARMS[arm]["justless"]:
        justless_dir = run_dir / "jlbin"
        install_pinned(settings.binary, justless_dir / "justless", settings.justless_sha)
        allow_exec.append(justless_dir / "justless")
    env = workspace.tool_env(repo.resolve(), home, bindir, target, justless_dir, tmp=run_dir / "tmp")
    deny = sandbox.default_denies(config, [settings.home, settings.runroot])
    allow_read = sandbox.default_allows(config) + [Path(settings.cc_binary)]
    hidden = sandbox.cargo_git_denies(config.CARGO_HOME, own_repos(task), snap / "repo")
    text = sandbox.profile(run_dir, deny_read=deny, allow_read=allow_read, deny_after=hidden,
                           write_regexes=sandbox.cargo_write_regexes(config.CARGO_HOME), allow_exec=allow_exec)
    profile_path, profile_hash = sandbox.write_profile(run_dir, text)
    cargo_before = sandbox.cargo_home_state(config.CARGO_HOME)

    def runner(argv, timeout):
        proc = subprocess.run(sandbox.wrap(argv, profile_path), cwd=str(repo), env=env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
        return proc.returncode, proc.stdout.decode("utf-8", "replace")

    if config.ARMS[arm]["justless"]:
        proc = subprocess.run(sandbox.wrap([str(justless_dir / "justless"), "init", "--hooks", "--agent", "claude"],
                                           profile_path), cwd=str(repo), env=env, capture_output=True)
        if proc.returncode != 0:
            raise RuntimeError("justless init failed: %s" % proc.stderr.decode()[-400:])
    workspace.normalize_mtimes(repo)
    workspace.git_task_history(repo, task, env=workspace.git_env(repo, home))
    timings["setup_s"] = round(time.time() - t0, 3)
    t1 = time.time()
    if tasks_module.is_edit(task) and task["language"] == "rust":
        scope = task["oracle"].get("cargo_scope", [])
        code, output = runner(["cargo", "test", "--offline", "--no-run"] + scope, 3600)
        if code != 0:
            raise RuntimeError("warm-up build failed in the sandbox: %s" % output[-600:])
        if tasks_module.kind(task) == "lint":
            runner(["cargo", "clippy", "--offline", "--all-targets"] + scope, 3600)
    timings["warmup_s"] = round(time.time() - t1, 3)
    reference = workspace.tree_files(repo)
    tools_ws = cc_tools.Workspace(repo, run_dir, env, profile_path, settings.cc_binary,
                                  token_counter=None, deny=deny, model=model)
    if settings.fake is None and not getattr(args, "estimator", False):
        client_for_count = settings.client_factory()
        cache = settings.home / "count_tokens_cache.json"

        tools_ws.token_counter = make_token_counter(client_for_count, model, cache)
    hooks = None
    session_context = ""
    if config.ARMS[arm]["justless"]:
        hooks = cc_hooks.HookRunner(repo.resolve(), env, profile_path, session_id=nonce, effort=effort,
                                    transcript_path=home / "transcript.jsonl")
        (home / "transcript.jsonl").write_text("")
        t2 = time.time()
        session_context = hooks.session_start("startup")
        timings["session_start_s"] = round(time.time() - t2, 3)
    if settings.fake is not None:
        variant = settings.variant(task, arm) if callable(settings.variant) else (settings.variant or "correct")
        steps = fake_agent.VARIANTS[variant](task, config.ARMS[arm]["justless"])
        context = {"root": str(repo.resolve()), "store": str(settings.home), "runroot": str(settings.runroot),
                   "outside": str(settings.runroot.parent), "persist": str(tools_ws.persist_dir.resolve())}
        settings.fake.register(nonce, fake_api.Script(steps, context))
    else:
        variant = None
    spec = {"campaign": settings.campaign, "model": model, "effort": effort, "arm": arm, "run_id": rid,
            "attempt": attempt, "nonce": nonce, "home": str(home), "prompt": task["prompt"],
            "run_budget": settings.run_budget(model), "ce_trigger": settings.ce_trigger, "phase": phase,
            "max_output_tokens": settings.max_output}
    client = settings.client_factory()
    allowed = workspace.allowed_secret_texts(repo, load_allowlist(settings.home))
    run = loop.Run(spec, client, settings.profile, settings.ledger, settings.pricing, tools_ws, hooks,
                   settings.logs, limits=settings.limits, api_key=getattr(client, "api_key", None),
                   allowed_secrets=allowed)
    run.run(session_context)
    t3 = time.time()
    grade = None
    # A limit stop (turns, tokens, run budget, wall time), max_tokens or a refusal ends the run; the oracle
    # still grades the final repository and answer, so a fix already on disk counts. stopped_by records why.
    stopped_by = run.failure_mode or "end_turn"
    if run.harness_error is not None:
        kind = run.harness_error.kind
        failure_mode = {"aborted_global_cap": "aborted_global_cap", "infra_pause": "paused_infra"}.get(
            kind, "harness_error")
        if failure_mode == "paused_infra":
            settings.pause(str(run.harness_error))
        stopped_by = "harness"
    else:
        grade = oracles.grade(task, repo, reference, runner, run.final_text)
        failure_mode = grade["failure_mode"]
    timings["oracle_s"] = round(time.time() - t3, 3)
    cargo_after = sandbox.cargo_home_state(config.CARGO_HOME)
    store = settings.dir / "runs" / ("%s-%d" % (rid, attempt))
    store.mkdir(parents=True, exist_ok=True)
    transcript = json.dumps(run.transcript(), indent=1)
    key = getattr(client, "api_key", None)
    if key and key in transcript:
        transcript = transcript.replace(key, "[REDACTED]")
    (store / "transcript.json").write_text(transcript)
    if run.first_body is not None:
        first = json.dumps(run.first_body)
        if key and key in first:
            first = first.replace(key, "[REDACTED]")
        (store / "first_request.json").write_text(first)
    changes = oracles.tree_changes(repo, reference)
    diff_lines = []
    for rel, (before, after) in sorted(changes.items()):
        import difflib
        diff_lines += list(difflib.unified_diff((before or b"").decode("utf-8", "replace").split("\n"),
                                                (after or b"").decode("utf-8", "replace").split("\n"),
                                                "a/" + rel, "b/" + rel, lineterm="", n=2))
    (store / "diff.patch").write_text("\n".join(diff_lines))
    record = {
        "campaign": settings.campaign, "phase": phase, "agent": "harness", "model": model, "effort": effort,
        "effort_setting": effort_name, "arm": arm, "task_id": task["id"], "family": task["family"],
        "kind": tasks_module.kind(task), "repo": task["repo"], "language": task["language"], "style": task.get("style"),
        # Impact truth is the rg -w hits of a unique name, so every impact task is grep-solvable by construction.
        "grep_solvable": task.get("grep_solvable", True if task["family"] == "impact" else None),
        "rep": rep, "arm_order_position": order_position,
        "run_id": rid, "attempt": attempt, "nonce": nonce, "variant": variant,
        "success": bool(grade and grade["success"]) and failure_mode == "success",
        "failure_mode": failure_mode, "stopped_by": stopped_by, "limit_hit": stopped_by in LIMIT_STOPS,
        "oracle": grade,
        "harness_error": str(run.harness_error) if run.harness_error else None,
        "append_only_bug": bool(run.harness_error and run.harness_error.append_only),
        "totals": {k: (round(v, 6) if isinstance(v, float) else v) for k, v in run.totals.items()},
        "tool_counts": run.tool_counts,
        "hook_events": [e.record() for e in hooks.events] if hooks else [],
        "timings": timings, "diff_files": len(changes),
        "cargo_home_changed": sorted(k for k in set(cargo_before) | set(cargo_after)
                                     if cargo_before.get(k) != cargo_after.get(k)),
        "diff_lines": sum(1 for l in diff_lines if l[:1] in "+-" and not l.startswith(("+++", "---"))),
        "settings": {"limits": settings.limits, "run_budget": spec["run_budget"], "ce_trigger": settings.ce_trigger,
                     "seed": settings.seed, "max_output_tokens": settings.max_output, "nonce_salt": config.PROCESS_SALT},
        "pins": dict(block_pins(settings, manifest), profile_source=settings.profile.source,
                     patch_hash=settings.profile.patch_hash, sandbox_profile_hash=profile_hash,
                     model_profile=settings.profile.model_pins(model, settings.max_output),
                     anthropic_version=config.ANTHROPIC_VERSION, betas=config.BETAS, cc_version=config.CC_VERSION,
                     cc_binary=settings.cc_binary),
        "transcript": str(store / "transcript.json"),
    }
    if not getattr(args, "keep_runs", False):
        workspace.remove(run_dir)
    return record


def block_pins(settings, manifest):
    """The pins stats.pin_key pairs arms on, shared by graded runs and setup-failure records."""
    pricing = getattr(settings, "pricing", None)
    return {"justless_sha256": settings.justless_sha, "harness_tree": getattr(settings, "harness", None),
            "harness_commit": getattr(settings, "commit", None), "capture_hash": settings.profile.capture_hash,
            "manifest_hash": manifest, "pricing_hash": pricing.hash if pricing is not None else None}


def block_identity(settings, task, model, effort_name, arm, rep, manifest):
    """Fields every result record of a block carries, whether or not the run got far enough to be graded."""
    return {"campaign": settings.campaign, "agent": "harness", "model": model, "effort_setting": effort_name,
            "arm": arm, "task_id": task["id"], "family": task["family"], "kind": tasks_module.kind(task),
            "repo": task.get("repo"),
            "language": task.get("language"), "style": task.get("style"), "rep": rep,
            "pins": block_pins(settings, manifest)}


def terminal_ids(results_path):
    """{run_id: terminal row}, plus ("attempts", run_id) -> the highest attempt number written and
    ("errors", run_id) -> how many attempts ended in a harness error. Only harness errors use up the retry
    budget: a cap abort or an infrastructure pause is re-run without counting against the run."""
    done = {}
    if not results_path.exists():
        return done
    with open(str(results_path)) as handle:
        for line in handle:
            try:
                row = json.loads(line)
            except ValueError:
                continue
            key = row["run_id"]
            if row.get("failure_mode") in TERMINAL:
                done[key] = row
                continue
            done[("attempts", key)] = max(done.get(("attempts", key), 0), row.get("attempt", 1))
            if row.get("failure_mode") == "harness_error":
                done[("errors", key)] = done.get(("errors", key), 0) + 1
    return done


def schedule(settings, task_list, models, arms, reps, effort, manifest, workers, phase="main"):
    """Run every (model, task, rep) block with a seeded arm order; resumable; ITT after 3 attempts."""
    results_path = settings.dir / "results.jsonl"
    done = terminal_ids(results_path)
    seed = getattr(settings, "seed", config.SEED)
    blocks = []
    for model in models:
        for rep in range(1, reps + 1):
            for task in tasks_module.ordered(task_list, seed):
                blocks.append((model, task, rep))
    rows = []

    def block_job(model, task, rep):
        out = []
        if getattr(settings, "pause_reason", None):
            return out
        order = config.arm_order(model, task["id"], rep, arms, seed)
        with settings.task_lock(task["id"]):
            for position, arm in enumerate(order):
                rid = config.run_id(settings.campaign, model, effort, arm, task["id"], rep, manifest,
                                    settings.justless_sha, settings.harness, settings.profile.capture_hash)
                if rid in done:
                    continue
                attempt = done.get(("attempts", rid), 0) + 1
                errors = done.get(("errors", rid), 0)
                while True:
                    try:
                        record = run_once(settings, task, model, effort, arm, rep, attempt, manifest, position, phase)
                    except Exception as error:  # setup failures are harness errors too
                        # The same identity and pins as a graded run, so after three failures the run still
                        # pairs with the other arm as an intention-to-treat failure.
                        record = dict(block_identity(settings, task, model, effort, arm, rep, manifest),
                                      phase=phase, run_id=rid, attempt=attempt, failure_mode="harness_error",
                                      success=False, harness_error="%s: %s" % (type(error).__name__, error))
                    if record["failure_mode"] == "harness_error":
                        errors += 1
                        if errors >= config.MAX_ATTEMPTS:
                            record["failure_mode"] = "harness_unrecoverable"
                    record["harness_error_attempts"] = errors
                    settings.logs["results"].write(record)
                    out.append(record)
                    if record["failure_mode"] in ("aborted_global_cap", "paused_infra"):
                        return out
                    if record["failure_mode"] != "harness_error":
                        break
                    attempt += 1
        if phase != "dry-run" and not getattr(settings, "ignore_gate", True):
            reasons = campaign_gate(read_rows(results_path), model, arms)
            if reasons:
                settings.pause("gate: " + "; ".join(reasons))
        return out

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, workers)) as pool:
        futures = [pool.submit(block_job, *b) for b in blocks]
        for future in concurrent.futures.as_completed(futures):
            rows += future.result()
    return rows


def read_rows(path):
    rows = []
    if Path(path).exists():
        with open(str(path)) as handle:
            for line in handle:
                try:
                    rows.append(json.loads(line))
                except ValueError:
                    pass
    return rows


def campaign_gate(rows, model, arms, min_runs=GATE_MIN_RUNS, points=GATE_POINTS):
    """The pre-registered pause rule for one model's harness runs: an arm's unrecoverable harness-error rate
    above 2%, or arms differing by more than 2 points in that rate or in their limit-hit rate. Evaluated once
    every arm has min_runs terminal runs, so one early event does not pause a campaign."""
    latest = {}
    for row in rows:
        if row.get("model") == model and (row.get("agent") or "harness") == "harness" \
                and row.get("failure_mode") in TERMINAL:
            latest[row["run_id"]] = row
    by_arm = {}
    for row in latest.values():
        by_arm.setdefault(row["arm"], []).append(row)
    if any(len(by_arm.get(arm, [])) < min_runs for arm in arms):
        return []
    unrecoverable = {a: sum(r["failure_mode"] == "harness_unrecoverable" for r in by_arm[a]) / float(len(by_arm[a]))
                     for a in arms}
    limits = {a: sum(bool(r.get("limit_hit") or r.get("failure_mode") in LIMIT_STOPS) for r in by_arm[a])
              / float(len(by_arm[a])) for a in arms}
    reasons = []
    for arm, rate in sorted(unrecoverable.items()):
        if rate > points:
            reasons.append("arm %s unrecoverable harness errors %.1f%%" % (arm, 100 * rate))
    if max(unrecoverable.values()) - min(unrecoverable.values()) > points:
        reasons.append("unrecoverable rates differ by more than 2 points: %s" % json.dumps(unrecoverable))
    if max(limits.values()) - min(limits.values()) > points:
        reasons.append("limit-hit rates differ by more than 2 points: %s" % json.dumps(
            {a: round(v, 4) for a, v in limits.items()}))
    return reasons


def current_pins(settings, args, models, manifest):
    """Campaign-wide pins and per-(model, effort) cell pins for a paid invocation."""
    arms = args.arms.split(",")
    base = {"justless_sha256": settings.justless_sha, "harness_tree": settings.harness,
            "capture_hash": settings.profile.capture_hash, "manifest_hash": manifest,
            "pricing_hash": settings.pricing.hash, "seed": settings.seed,
            "anthropic_version": config.ANTHROPIC_VERSION, "betas": config.BETAS, "cc_version": config.CC_VERSION}
    cells = {}
    for model in models:
        # Limits are per model: the pilot sets a token limit for each.
        cell = {"max_output_tokens": settings.max_output, "run_budget": settings.run_budget(model),
                "limits": settings.limits, "profile": settings.profile.model_pins(model, settings.max_output)}
        if any(config.ARMS[a]["ce"] for a in arms):
            cell["ce_trigger"] = settings.ce_trigger
        cells["%s|%s" % (model, args.effort)] = cell
    return base, cells


def check_pins(directory, base, cells, new_pins=False):
    """Compare an invocation's pins with the campaign's pins.json and refuse a silent change. Keys a stored
    generation does not have yet are added (the anchor pins fewer keys than the harness); --new-pins starts
    a new generation, which the report keeps apart."""
    path = Path(directory) / "pins.json"
    stored = json.loads(path.read_text()) if path.exists() else {"generations": []}
    generations = stored["generations"]
    if generations and not new_pins:
        current = generations[-1]
        diffs = ["%s: %s -> %s" % (k, json.dumps(current["base"][k]), json.dumps(v)) for k, v in sorted(base.items())
                 if k in current["base"] and current["base"][k] != v]
        for key, cell in sorted(cells.items()):
            old = current["cells"].get(key) or {}
            diffs += ["%s %s: %s -> %s" % (key, k, json.dumps(old[k]), json.dumps(v)) for k, v in sorted(cell.items())
                      if k in old and old[k] != v]
        if diffs:
            raise SystemExit("this invocation's pins differ from campaign %s:\n  %s\nRestore them, or pass "
                             "--new-pins to start a new generation (the report never pairs runs across "
                             "generations)." % (Path(directory).name, "\n  ".join(diffs)))
        for k, v in base.items():
            current["base"].setdefault(k, v)
        for key, cell in cells.items():
            current["cells"].setdefault(key, {}).update(cell)
    else:
        generations.append({"started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "base": base,
                            "cells": cells})
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(stored, indent=1, sort_keys=True))
    os.replace(str(tmp), str(path))
    return stored


def frozen_manifest(home, task_set="main"):
    path = tasks_module.private_dir(home, task_set) / "_manifest.json"
    if not path.exists():
        return None
    return json.loads(path.read_text()).get("hash")


def require_frozen_manifest(home, manifest, task_set="main"):
    """The main campaign and its report run only on the task set frozen by review-tasks --approve."""
    frozen = frozen_manifest(home, task_set)
    if frozen != manifest:
        raise SystemExit("the task manifest %s is not the frozen manifest %s; run review-tasks --approve after "
                         "the review, or restore the frozen tasks" % (manifest, frozen))


# ----------------------------------------------------------------------------- commands

def task_set(args):
    return getattr(args, "task_set", None) or "main"


def load_tasks(args, analysis=True):
    public = tasks_module.load_public()
    private = tasks_module.load_private(args.home, task_set(args))
    selector = getattr(args, "tasks", None) or "all"
    if selector == "all-public":
        chosen = public
    elif selector == "all-private":
        chosen = private
    elif selector == "all":
        chosen = private if analysis else public + private
    else:
        ids = set(selector.split(","))
        chosen = [t for t in public + private if t["id"] in ids]
    families = getattr(args, "families", None)
    if families:
        allowed = set(families.split(","))
        chosen = [t for t in chosen if t["family"] in allowed]
    per_family = getattr(args, "per_family", None)
    if per_family:
        chosen = tasks_module.first_per_family(chosen, per_family)
    return chosen


def full_manifest(args):
    """Run IDs hash the whole task set, not the selected subset, so resuming with a different
    --tasks, --families or --per-family selection never re-runs finished work."""
    return tasks_module.manifest_hash(tasks_module.load_public()
                                      + tasks_module.load_private(args.home, task_set(args)))


def paid_spend(home, exclude=None):
    """Spend recorded by paid requests in every campaign (dry-run usage is fake and never counts)."""
    return accounting.paid_spend(home, exclude)


def refuse_dry_campaign(home, campaign):
    """Paid commands never share a campaign directory with dry-run or validation data."""
    directory = Path(home) / "campaigns" / campaign
    if accounting.directory_is_dry(directory):
        raise SystemExit("campaign %s holds dry-run data (%s); choose another --campaign for paid runs" % (
            campaign, "DRY_RUN marker" if (directory / "DRY_RUN").exists() else "fake_requests.jsonl"))
    ledger = directory / "ledger.jsonl"
    if ledger.exists():
        for line in open(str(ledger)):
            try:
                if accounting.entry_is_fake(json.loads(line)):
                    raise SystemExit("campaign %s has fake ledger entries; choose another --campaign" % campaign)
            except ValueError:
                continue


def mark_dry(directory):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "DRY_RUN").write_text("fake model; never paid spend\n")


def make_settings(args, campaign, fake=None, variant=None):
    profile = cc_profile.load(args.home, getattr(args, "profile_source", "auto"))
    pricing = accounting.Pricing()
    if fake is not None:
        mark_dry(Path(args.home) / "campaigns" / campaign)
    home = args.home
    ledger = accounting.Ledger(Path(args.home) / "campaigns" / campaign / "ledger.jsonl", args.budget_usd,
                               reserve_usd=config.RESERVE_USD if args.budget_usd >= config.CAMPAIGN_CAP_USD else 0.0,
                               use_reserve=args.use_reserve, fake=fake is not None,
                               prior_fn=None if fake is not None else (lambda: paid_spend(home, exclude=campaign)),
                               global_cap_usd=None if fake is not None else config.CAMPAIGN_CAP_USD)
    if fake is not None:
        def factory():
            return api_module.Client("sk-ant-fake-dry-run-key-000000000000", base_url=fake.base_url,
                                     sleep=lambda s: None, max_attempts=3)
    else:
        key = os.environ.get("ANTHROPIC_API_KEY")
        if not key:
            raise SystemExit("ANTHROPIC_API_KEY is required for paid commands")

        def factory():
            return api_module.Client(key, base_url=args.api_base)
    return Settings(args, campaign, profile, ledger, pricing, factory, fake, variant)


def cmd_dry_run(args):
    task_list = load_tasks(args, analysis=False)
    if not task_list:
        raise SystemExit("no tasks selected")
    os.environ["AGENT_BENCH_DRY_RUN"] = "1"
    planted = "sk-ant-FAKE-planted-parent-env-secret-0000000000"
    os.environ["ANTHROPIC_API_KEY"] = planted
    campaign_dir = Path(args.home) / "campaigns" / args.campaign
    mark_dry(campaign_dir)
    fake = fake_api.FakeAPI(0, str(campaign_dir / "fake_requests.jsonl")).start()
    variants = args.variants.split(",")
    summary = []
    try:
        for variant in variants:
            chosen = [t for t in task_list if fake_agent.applies(variant, t)]
            if not chosen:
                continue
            settings = make_settings(args, "%s-%s" % (args.campaign, variant), fake, variant)
            manifest = full_manifest(args)
            rows = schedule(settings, chosen, [args.model], args.arms.split(","), args.reps, args.effort,
                            manifest, args.workers, phase="dry-run")
            for row in rows:
                want = fake_agent.expected(variant, next(t for t in task_list if t["id"] == row["task_id"]))
                stop = fake_agent.EXPECTED_STOP.get(variant)
                summary.append({"variant": variant, "task": row["task_id"], "arm": row["arm"],
                                "failure_mode": row["failure_mode"], "expected": want,
                                "stopped_by": row.get("stopped_by"),
                                "ok": (want is None or row["failure_mode"] == want)
                                and (stop is None or row.get("stopped_by") == stop),
                                "cost": row.get("totals", {}).get("cost_usd"),
                                "requests": row.get("totals", {}).get("requests"),
                                "tool_counts": row.get("tool_counts"), "error": row.get("harness_error")})
    finally:
        fake.stop()
    out = Path(args.home) / "campaigns" / args.campaign / "dry_run_summary.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(summary, indent=1))
    bad = [s for s in summary if not s["ok"]]
    for s in summary:
        print("%-8s %-40s %s %-16s expected %-16s %s %s" % (
            s["variant"], s["task"], s["arm"], s["failure_mode"], s["expected"], "ok" if s["ok"] else "MISMATCH",
            (s.get("error") or "")[:160]))
    print("%d runs, %d mismatches; summary at %s" % (len(summary), len(bad), out))
    import dry_checks
    checks = dry_checks.check(args.home, args.campaign, variants)
    (out.parent / "dry_run_checks.json").write_text(json.dumps(checks, indent=1))
    failed_pairs = [p for p in checks["pairs"] if not p["ok"]]
    print(json.dumps({"checks_ok": checks["ok"], "pairs_checked": len(checks["pairs"]),
                      "pairs_failed": failed_pairs[:5], "hooks": checks["hooks"], "accounting": checks["accounting"],
                      "ce_applied_edits": checks["ce"], "planted_key_hits": len(checks["secrets"]["planted_key_hits"]),
                      "history": {k: v[:4] for k, v in checks["history"].items()}}, indent=1))
    return 1 if bad or not checks["ok"] else 0


def cmd_run(args, phase="main"):
    task_list = load_tasks(args)
    if not task_list:
        raise SystemExit("no tasks selected")
    if args.binary is None and any(config.ARMS[a]["justless"] for a in args.arms.split(",")):
        raise SystemExit("--binary is required for arms b and d")
    if args.binary and "qa/bin" in str(args.binary):
        raise SystemExit("refusing the stale qa/bin build; build Justless from the commit under test")
    commit = harness_commit()
    if (commit == "unknown" or "dirty" in commit) and not args.allow_dirty:
        raise SystemExit("the harness tree is %s; commit it (or pass --allow-dirty, which the pins record)" % (
            "dirty" if "dirty" in commit else "not in a Git checkout"))
    refuse_dry_campaign(args.home, args.campaign)
    if phase == "main":
        require_frozen_manifest(args.home, full_manifest(args), task_set(args))
    parity = Path(args.home) / "cc_capture" / "parity_report.json"
    if not parity.exists() and not args.accept_deviation:
        raise SystemExit("run parity-diff first (or pass --accept-deviation with the documented items)")
    for task in task_list:
        snap = snapshot_dir(args.home, task)
        if not (snap / "repo").exists():
            raise SystemExit("snapshot missing for %s; run prepare" % task["id"])
        hits = workspace.secret_scan(snap / "repo", allowlist=load_allowlist(args.home))
        if hits:
            raise SystemExit("secret scan hits in %s: %s (review and add to secret_allowlist.json)" % (
                task["id"], hits[:3]))
    settings = make_settings(args, args.campaign)
    manifest = full_manifest(args)
    models = args.model.split(",")
    base, cells = current_pins(settings, args, models, manifest)
    check_pins(settings.dir, base, cells, args.new_pins)
    rows = schedule(settings, task_list, models, args.arms.split(","), args.reps, args.effort, manifest,
                    args.workers, phase=phase)
    print("%d records written to %s" % (len(rows), settings.dir / "results.jsonl"))
    if settings.pause_reason:
        print("CAMPAIGN PAUSED: %s\nReview the runs, then resume (pass --ignore-gate to continue past a gate "
              "after review)." % settings.pause_reason)
        return 3
    return 0


def load_allowlist(home):
    path = Path(home) / "secret_allowlist.json"
    if path.exists():
        return json.loads(path.read_text())
    return []


def cmd_prepare(args):
    import gen_tasks
    if args.generate and task_set(args) == "large":
        import gen_large
        gen_large.main(["--home", str(args.home)] + (["--repos", args.repos] if args.repos else []))
    elif args.generate:
        import baseline_build
        if "large" in (args.families or "").split(","):
            raise SystemExit("the large family is its own task set: pass --task-set large")
        repos = args.repos.split(",") if args.repos else sorted(set(r for f in gen_tasks.PLAN.values() for r in f))
        for repo in repos:
            base = Path(args.home) / "pristine" / repo
            if not (base / "repo").exists():
                workspace.copy_source(config.CORPUS / repo, base / "repo")
                workspace.normalize_mtimes(base / "repo")
            if repo in ("prediction_market_workspace", "crypto_rust_shared", "trade_parser") \
                    and not (base / "baseline.json").exists():
                baseline_build.main([repo, "--runs", "3"])
        gen_tasks.main(["--home", str(args.home), "--families", args.families or "bugfix,lint,location,impact"]
                       + (["--repos", args.repos] if args.repos else []))
    task_list = load_tasks(args, analysis=False)
    for task in task_list:
        snap = build_snapshot(args.home, task, force=args.force)
        warm_target(args.home, task, host_env)
        hits = workspace.secret_scan(snap / "repo", allowlist=load_allowlist(args.home))
        print("%-44s snapshot ok  tree %s  secret hits %d" % (task["id"], (snap / "tree_hash").read_text()[:12],
                                                               len(hits)))
    return 0


def cmd_validate_tasks(args):
    """Every task through the exact run-time path (clone, install, mtimes, baseline commit, sandbox):
    noop must fail, correct must succeed, tamper and wrong must fail with their modes; truth is perturbed."""
    import gen_tasks
    task_list = load_tasks(args, analysis=False)
    corpus_repos = sorted({t["repo"] for t in task_list if not t.get("source_root")})
    corpus_before = {r: workspace.tree_hash(config.CORPUS / r) for r in corpus_repos}
    snapshot_ok = {}
    for task in task_list:
        snap = snapshot_dir(args.home, task)
        stored = (snap / "tree_hash").read_text() if (snap / "tree_hash").exists() else None
        snapshot_ok[task["id"]] = stored is not None and stored == workspace.tree_hash(snap / "repo")
    args.variants = args.variants or "noop,correct,tamper,wrong,qualified,cfgtest"
    args.arms = args.arms or "a"
    code = cmd_dry_run(args)
    truth_failures = {t["id"]: gen_tasks.truth_checks(t) for t in task_list}
    corpus_after = {r: workspace.tree_hash(config.CORPUS / r) for r in corpus_repos}
    exposure = {}
    for task in task_list:
        if tasks_module.is_edit(task):
            hidden = sandbox.cargo_git_denies(config.CARGO_HOME, own_repos(task), snapshot_dir(args.home, task) / "repo")
            found = sandbox.answer_key_exposure(task, config.CARGO_HOME, hidden)
            if found:
                exposure[task["id"]] = ["%s in %s" % pair for pair in found]
    report = {"snapshots_match_stored_hash": snapshot_ok, "truth_check_failures": {k: v for k, v in
                                                                                   truth_failures.items() if v},
              "corpus_unchanged": corpus_after == corpus_before, "corpus_hashes": corpus_after,
              # Unmutated originals still readable in a Cargo git database the build needs. Not a failure of
              # the run path, but such a task should be regenerated with its injection moved elsewhere.
              "answer_key_exposure": exposure}
    out = Path(args.home) / "campaigns" / args.campaign / "validate_report.json"
    out.write_text(json.dumps(report, indent=1))
    print(json.dumps({k: v for k, v in report.items() if k != "corpus_hashes"}, indent=1))
    ok = code == 0 and all(snapshot_ok.values()) and not report["truth_check_failures"] and report["corpus_unchanged"]
    return 0 if ok else 1


def estimate_large(args):
    """The large family's plan priced from its scripted plain paths (dry-run --variants correct), or from the
    pre-measurement assumption when no dry run is given."""
    task_list = tasks_module.load_private(args.home, "large")
    source = "pre-measurement assumption"
    paths = {t["id"]: accounting.LARGE_PREPILOT_PATH for t in task_list}
    if getattr(args, "from_dry", None):
        measured = {}
        for name in args.from_dry.split(","):
            measured.update(accounting.dry_run_paths(Path(args.home) / "campaigns" / name))
        missing = sorted(set(paths) - set(measured))
        if missing:
            raise SystemExit("no successful dry-run measurement for %s in %s" % (", ".join(missing), args.from_dry))
        paths = {k: measured[k] for k in paths}
        source = "dry run %s" % args.from_dry
    out = accounting.estimate_large(paths, spent=paid_spend(args.home), use_reserve=args.use_reserve)
    out["source"] = source
    out["paths"] = paths
    print(json.dumps(out, indent=1))
    return 0


def cmd_estimate(args):
    if task_set(args) == "large":
        return estimate_large(args)
    per_run = accounting.PREPILOT
    source = "pre-pilot assumptions"
    if args.assume != "prepilot":
        per_run, source = pilot_per_run(args)
    spent = paid_spend(args.home)
    pilot_total, _ = accounting.plan_cost(accounting.PILOT_PLAN, per_run)
    result = accounting.estimate(per_run, spent=spent, use_reserve=args.use_reserve)
    limit = accounting.enforced_limit(use_reserve=args.use_reserve)
    out = {"source": source, "spent_usd": round(spent, 4), "enforced_limit_usd": limit,
           "pilot_expected_usd": round(pilot_total, 2), "main": result}
    if args.assume != "prepilot":
        path = Path(args.home) / "campaigns" / args.campaign / "results.jsonl"
        rows = [json.loads(l) for l in open(str(path))] if path.exists() else []
        peaks = [r["totals"]["max_prompt_tokens"] for r in rows if r.get("arm") == "a" and r.get("totals")
                 and r.get("model") == "claude-haiku-5-5"]
        out["ce_trigger_rule"] = {"plain_arm_peaks": sorted(peaks), "T": config.ce_trigger_rule(peaks)}
        out["token_limit_rule"] = {}
        for model in config.MODELS:
            cumulative = [r["totals"]["prompt_tokens"] for r in rows if r.get("arm") == "a" and r.get("totals")
                          and r.get("model") == model and (r.get("agent") or "harness") == "harness"]
            if cumulative:
                out["token_limit_rule"][model] = {"plain_arm_runs": len(cumulative), "max": max(cumulative),
                                                  "token_limit": config.token_limit_rule(cumulative)}
        sonnet = out["token_limit_rule"].get("claude-sonnet-5-5")
        if sonnet and "claude-opus-5-5" not in out["token_limit_rule"]:
            out["token_limit_rule"]["claude-opus-5-5"] = dict(sonnet, note="no Opus pilot runs; the Sonnet value")
        # Effort rule: after dropping Opus, a plan that still does not fit moves every arm to medium effort.
        without_opus = accounting.estimate(per_run, spent=spent, use_reserve=args.use_reserve,
                                           plan=[c for c in accounting.MAIN_PLAN if c["name"] != "opus"])
        out["effort_rule"] = {"expected_without_opus": without_opus["expected_usd"],
                              "switch_to_medium": without_opus["expected_usd"] > limit - spent}
    print(json.dumps(out, indent=1))
    return 0


def pilot_per_run(args):
    path = Path(args.home) / "campaigns" / args.campaign / "results.jsonl"
    rows = [json.loads(l) for l in open(path)] if path.exists() else []
    per_run = json.loads(json.dumps(accounting.PREPILOT))
    for model in per_run:
        for family in per_run[model]:
            costs = sorted(r["totals"]["cost_usd"] for r in rows if r.get("model") == model
                           and r.get("family") == family and r.get("totals"))
            if costs:
                idx = int(round(0.9 * (len(costs) - 1))) if len(costs) >= 4 else len(costs) // 2
                per_run[model][family] = costs[idx] * (1.0 if len(costs) >= 4 else 1.3)
    sonnet = per_run["claude-sonnet-5-5"]
    per_run["claude-opus-5-5"] = {f: v * 2.0 * 1.3 for f, v in sonnet.items()}
    return per_run, "pilot (%d runs)" % len(rows)


def cmd_report(args):
    import report
    return report.main(["--home", str(args.home), "--campaign", args.campaign, "--task-set", task_set(args)]
                       + (["--dry"] if args.dry else []))


def cmd_export_audit(args):
    """Every successful edit-family run whose diff is not an exact revert, exported for a blind review: the
    patches are named by a random-looking ID and list the task but not the arm; key.json maps IDs to runs."""
    import report
    rows = report.harness_only(report.final_rows(read_rows(Path(args.home) / "campaigns" / args.campaign
                                                           / "results.jsonl")))
    chosen = [r for r in rows if r.get("success") and (r.get("kind") or r.get("family")) in tasks_module.EDIT_KINDS
              and not ((r.get("oracle") or {}).get("details") or {}).get("exact_revert")]
    out = Path(args.home) / "reports" / args.campaign / "audit"
    if out.exists():
        shutil.rmtree(str(out))
    out.mkdir(parents=True)
    salt = os.urandom(8).hex()
    index, key = [], {}
    def blind_id(row):
        return hashlib.sha256((salt + row["run_id"] + str(row.get("attempt"))).encode()).hexdigest()[:10]

    for row in sorted(chosen, key=blind_id):
        blind = blind_id(row)
        patch = Path(row.get("transcript") or "").parent / "diff.patch"
        text = patch.read_text() if row.get("transcript") and patch.exists() else "(diff missing)\n"
        (out / ("%s.patch" % blind)).write_text("# task %s\n%s" % (row["task_id"], text))
        index.append({"id": blind, "task": row["task_id"], "family": row["family"]})
        key[blind] = {"run_id": row["run_id"], "attempt": row.get("attempt"), "model": row.get("model"),
                      "arm": row.get("arm"), "rep": row.get("rep")}
    (out / "index.json").write_text(json.dumps(index, indent=1))
    (out.parent / "audit_key.json").write_text(json.dumps(key, indent=1, sort_keys=True))
    print("%d non-revert successful edits exported to %s (key in %s)" % (len(index), out, out.parent / "audit_key.json"))
    return 0


def cmd_sandbox_check(args):
    results = sandbox.self_test(config, Path(args.runroot) / "_sandbox_check")
    ok = all(r[1] for r in results)
    for name, passed, detail in results:
        print("%-28s %s  %s" % (name, "ok" if passed else "FAIL", detail.replace("\n", " | ")[:150]))
    return 0 if ok else 1


def cmd_cc_capture(args):
    import cc_capture
    return cc_capture.main(["--binary", args.binary, "--home", str(args.home), "--cc-binary", args.cc_binary])


def cmd_parity_diff(args):
    import parity_diff
    return parity_diff.main(["--home", str(args.home), "--binary", args.binary, "--cc-binary", args.cc_binary])


def cmd_anchor(args):
    import cc_anchor
    return cc_anchor.main(args)


def cmd_reconcile(args):
    path = Path(args.home) / "campaigns" / args.campaign / "ledger.jsonl"
    total = sum(float(json.loads(l).get("cost_usd") or 0) for l in open(path)) if path.exists() else 0.0
    report = {"campaign": args.campaign, "ledger_usd": round(total, 4), "provider_usd": args.provider_usd}
    if args.provider_usd is not None:
        gap = abs(total - args.provider_usd) / max(args.provider_usd, 1e-9)
        report["gap"] = round(gap, 4)
        report["ok"] = gap <= 0.02
    print(json.dumps(report, indent=1))
    return 0 if report.get("ok", True) else 1


def cmd_review(args):
    import gen_tasks
    return gen_tasks.review(args)


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)

    def common(sp):
        sp.add_argument("--home", type=Path, default=config.HOME)
        sp.add_argument("--runroot", type=Path, default=config.RUNROOT)
        sp.add_argument("--binary")
        sp.add_argument("--cc-binary", default=config.CC_BINARY)
        sp.add_argument("--campaign", help="default: dry for dry-run, validate for validate-tasks, anchor-dry for "
                        "anchor --dry-anchor, otherwise main")
        sp.add_argument("--tasks", default="all")
        sp.add_argument("--task-set", default="main", choices=sorted(tasks_module.TASK_SETS),
                        help="main: the 32 pre-registered tasks; large: the large-output family, with its own "
                        "directory, manifest and campaign")
        sp.add_argument("--families")
        sp.add_argument("--per-family", type=int)
        sp.add_argument("--model", default="claude-sonnet-5-5")
        sp.add_argument("--effort", default="cc-default", choices=config.EFFORTS)
        sp.add_argument("--arms", default="a,b")
        sp.add_argument("--reps", type=int, default=1)
        sp.add_argument("--workers", type=int, default=3)
        sp.add_argument("--max-turns", type=int, default=config.LIMITS["turn_limit"])
        sp.add_argument("--token-limit", type=int, default=config.LIMITS["token_limit"],
                        help="cumulative prompt tokens per run; set from the pilot (estimate prints the rule)")
        sp.add_argument("--max-output-tokens", type=int, default=config.MAX_OUTPUT_CAP,
                        help="caps Claude Code's max_tokens for every arm")
        sp.add_argument("--budget-usd", type=float, default=config.CAMPAIGN_CAP_USD)
        sp.add_argument("--run-cap-usd", type=float)
        sp.add_argument("--use-reserve", action="store_true")
        sp.add_argument("--api-base", default=config.API_BASE)
        sp.add_argument("--allow-dirty", action="store_true")
        sp.add_argument("--accept-deviation", action="append")
        sp.add_argument("--ce-trigger", type=int)
        sp.add_argument("--seed", type=int, default=config.SEED,
                        help="arm and block order; task subsets and the bootstrap keep the pre-registered seed")
        sp.add_argument("--new-pins", action="store_true",
                        help="start a new pin generation in this campaign instead of refusing changed pins")
        sp.add_argument("--ignore-gate", action="store_true",
                        help="continue past the harness-error and limit-hit pause rule after reviewing it")
        sp.add_argument("--keep-runs", action="store_true")
        sp.add_argument("--estimator", action="store_true", help="use the bytes/2 Read counter instead of count_tokens")
        sp.add_argument("--profile-source", default="auto", choices=["auto", "capture", "paraphrase"])
        return sp

    common(sub.add_parser("prepare")).add_argument("--generate", action="store_true")
    sub.choices["prepare"].add_argument("--repos")
    sub.choices["prepare"].add_argument("--force", action="store_true")
    common(sub.add_parser("validate-tasks")).add_argument("--variants")
    common(sub.add_parser("review-tasks")).add_argument("--approve", action="store_true")
    common(sub.add_parser("cc-capture"))
    common(sub.add_parser("parity-diff"))
    common(sub.add_parser("sandbox-check"))
    dry = common(sub.add_parser("dry-run"))
    dry.add_argument("--variants", default="correct,wrong,tamper,unread_edit,probe,qualified,cfgtest,overrun")
    common(sub.add_parser("pilot"))
    common(sub.add_parser("estimate")).add_argument("--assume", default="pilot", choices=["pilot", "prepilot"])
    sub.choices["estimate"].add_argument("--from-dry", help="large task set: the dry-run campaign directory "
                                         "holding the correct variant, such as dry-large-correct; several "
                                         "may be given, separated by commas")
    common(sub.add_parser("run"))
    common(sub.add_parser("anchor")).add_argument("--dry-anchor", action="store_true",
                                                    help="upstream is the fake API with a scripted agent (free)")
    common(sub.add_parser("reconcile")).add_argument("--provider-usd", type=float)
    common(sub.add_parser("report")).add_argument("--dry", action="store_true")
    common(sub.add_parser("export-audit"))
    return p


def main(argv=None):
    args = parser().parse_args(argv)
    args.home = Path(args.home)
    if args.campaign is None:
        # Dry and validation runs never default into a paid campaign's directory.
        args.campaign = {"dry-run": "dry", "validate-tasks": "validate"}.get(args.command) or (
            "anchor-dry" if getattr(args, "dry_anchor", False) else "main")
    commands = {
        "prepare": cmd_prepare, "validate-tasks": cmd_validate_tasks, "review-tasks": cmd_review,
        "cc-capture": cmd_cc_capture, "parity-diff": cmd_parity_diff, "sandbox-check": cmd_sandbox_check,
        "dry-run": cmd_dry_run, "pilot": lambda a: cmd_run(a, phase="pilot"), "estimate": cmd_estimate,
        "run": cmd_run, "anchor": cmd_anchor, "reconcile": cmd_reconcile, "report": cmd_report,
        "export-audit": cmd_export_audit,
    }
    return commands[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
