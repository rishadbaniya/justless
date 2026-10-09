#!/usr/bin/env python3
"""Real-agent A/B benchmark for Justless. Run `python3 scripts/agent_bench/run.py <command> --help`.

Commands: prepare, validate-tasks, review-tasks, cc-capture, parity-diff, sandbox-check,
dry-run, pilot, estimate, run, anchor, reconcile, report.
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
RERUN = {"harness_error", "aborted_global_cap"}


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


# ----------------------------------------------------------------------------- snapshots

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
    if task["language"] != "rust" or task["family"] not in ("bugfix", "lint"):
        return None
    target = snap / "target"
    marker = snap / "target.ok"
    if marker.exists():
        return target
    env = runner_env_factory(snap / "repo", target)
    commands = [["cargo", "test", "--offline", "--no-run"] + task["oracle"].get("cargo_scope", [])]
    if task["family"] == "lint":
        commands.append(["cargo", "clippy", "--offline", "--all-targets"] + task["oracle"].get("cargo_scope", []))
    for command in commands:
        proc = subprocess.run(command, cwd=str(snap / "repo"), env=env, capture_output=True, timeout=3600)
        if proc.returncode != 0 and task["family"] == "bugfix" and "--no-run" not in command:
            raise RuntimeError("warm build failed: %s" % proc.stderr.decode()[-800:])
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
        self.binary = Path(args.binary).resolve() if args.binary else None
        self.justless_sha = config.sha256_file(self.binary) if self.binary else "none"
        self.cc_binary = args.cc_binary
        self.commit = harness_commit()
        self.dir = self.home / "campaigns" / campaign
        self.dir.mkdir(parents=True, exist_ok=True)
        self.logs = {name: loop.JsonlWriter(self.dir / ("%s.jsonl" % name))
                     for name in ("requests", "tools", "attempts", "results")}
        self.ce_trigger = getattr(args, "ce_trigger", None) or config.CE_PILOT_TRIGGER
        self.task_locks = {}
        self.lock = threading.Lock()

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
            raise loop.HarnessError("count_tokens", str(error)[:300])
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
                        settings.justless_sha, settings.commit, settings.profile.capture_hash)
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
        workspace.install_binary(settings.binary, justless_dir / "justless")
        allow_exec.append(justless_dir / "justless")
    env = workspace.tool_env(repo.resolve(), home, bindir, target, justless_dir, tmp=run_dir / "tmp")
    deny = sandbox.default_denies(config, [settings.home, settings.runroot])
    allow_read = sandbox.default_allows(config) + [Path(settings.cc_binary)]
    text = sandbox.profile(run_dir, deny_read=deny, allow_read=allow_read, allow_write=[config.CARGO_HOME],
                           allow_exec=allow_exec)
    profile_path, profile_hash = sandbox.write_profile(run_dir, text)

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
    workspace.git_baseline(repo, env=workspace.git_env(repo, home))
    timings["setup_s"] = round(time.time() - t0, 3)
    t1 = time.time()
    if task["family"] in ("bugfix", "lint") and task["language"] == "rust":
        scope = task["oracle"].get("cargo_scope", [])
        code, output = runner(["cargo", "test", "--offline", "--no-run"] + scope, 3600)
        if code != 0:
            raise RuntimeError("warm-up build failed in the sandbox: %s" % output[-600:])
        if task["family"] == "lint":
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
                   "outside": str(settings.runroot.parent)}
        settings.fake.register(nonce, fake_api.Script(steps, context))
    else:
        variant = None
    spec = {"campaign": settings.campaign, "model": model, "effort": effort, "arm": arm, "run_id": rid,
            "attempt": attempt, "nonce": nonce, "home": str(home), "prompt": task["prompt"],
            "run_budget": getattr(args, "run_cap_usd", None) or config.MODELS[model]["run_budget"],
            "ce_trigger": settings.ce_trigger, "phase": phase}
    client = settings.client_factory()
    limits = {"turn_limit": args.max_turns}
    allowed = workspace.allowed_secret_texts(repo, load_allowlist(settings.home))
    run = loop.Run(spec, client, settings.profile, settings.ledger, settings.pricing, tools_ws, hooks,
                   settings.logs, limits=limits, api_key=getattr(client, "api_key", None), allowed_secrets=allowed)
    run.run(session_context)
    t3 = time.time()
    grade = None
    failure_mode = run.failure_mode
    if run.harness_error is not None:
        failure_mode = "aborted_global_cap" if run.harness_error.kind == "aborted_global_cap" else "harness_error"
    elif failure_mode is None:
        grade = oracles.grade(task, repo, reference, runner, run.final_text)
        failure_mode = grade["failure_mode"]
    timings["oracle_s"] = round(time.time() - t3, 3)
    store = settings.dir / "runs" / ("%s-%d" % (rid, attempt))
    store.mkdir(parents=True, exist_ok=True)
    transcript = json.dumps(run.transcript(), indent=1)
    key = getattr(client, "api_key", None)
    if key and key in transcript:
        transcript = transcript.replace(key, "[REDACTED]")
    (store / "transcript.json").write_text(transcript)
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
        "repo": task["repo"], "language": task["language"], "style": task.get("style"),
        "grep_solvable": task.get("grep_solvable"), "rep": rep, "arm_order_position": order_position,
        "run_id": rid, "attempt": attempt, "nonce": nonce, "variant": variant,
        "success": bool(grade and grade["success"]) and failure_mode == "success",
        "failure_mode": failure_mode, "oracle": grade,
        "harness_error": str(run.harness_error) if run.harness_error else None,
        "append_only_bug": bool(run.harness_error and run.harness_error.append_only),
        "totals": {k: (round(v, 6) if isinstance(v, float) else v) for k, v in run.totals.items()},
        "tool_counts": run.tool_counts,
        "hook_events": [e.record() for e in hooks.events] if hooks else [],
        "timings": timings, "diff_files": len(changes),
        "diff_lines": sum(1 for l in diff_lines if l[:1] in "+-" and not l.startswith(("+++", "---"))),
        "pins": {"justless_sha256": settings.justless_sha, "harness_commit": settings.commit,
                 "capture_hash": settings.profile.capture_hash, "profile_source": settings.profile.source,
                 "patch_hash": settings.profile.patch_hash, "pricing_hash": settings.pricing.hash,
                 "manifest_hash": manifest, "sandbox_profile_hash": profile_hash,
                 "anthropic_version": config.ANTHROPIC_VERSION, "betas": config.BETAS,
                 "cc_version": config.CC_VERSION, "cc_binary": settings.cc_binary},
        "transcript": str(store / "transcript.json"),
    }
    if not getattr(args, "keep_runs", False):
        workspace.remove(run_dir)
    return record


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
    blocks = []
    for model in models:
        for rep in range(1, reps + 1):
            for task in tasks_module.ordered(task_list):
                blocks.append((model, task, rep))
    rows = []

    def block_job(model, task, rep):
        out = []
        order = config.arm_order(model, task["id"], rep, arms)
        with settings.task_lock(task["id"]):
            for position, arm in enumerate(order):
                rid = config.run_id(settings.campaign, model, effort, arm, task["id"], rep, manifest,
                                    settings.justless_sha, settings.commit, settings.profile.capture_hash)
                if rid in done:
                    continue
                attempt = done.get(("attempts", rid), 0) + 1
                errors = done.get(("errors", rid), 0)
                while True:
                    try:
                        record = run_once(settings, task, model, effort, arm, rep, attempt, manifest, position, phase)
                    except Exception as error:  # setup failures are harness errors too
                        record = {"campaign": settings.campaign, "phase": phase, "model": model, "arm": arm,
                                  "task_id": task["id"], "family": task["family"], "rep": rep, "run_id": rid,
                                  "attempt": attempt, "failure_mode": "harness_error", "success": False,
                                  "harness_error": "%s: %s" % (type(error).__name__, error)}
                    if record["failure_mode"] == "harness_error":
                        errors += 1
                        if errors >= config.MAX_ATTEMPTS:
                            record["failure_mode"] = "harness_unrecoverable"
                    record["harness_error_attempts"] = errors
                    settings.logs["results"].write(record)
                    out.append(record)
                    if record["failure_mode"] == "aborted_global_cap":
                        return out
                    if record["failure_mode"] != "harness_error":
                        break
                    attempt += 1
        return out

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, workers)) as pool:
        futures = [pool.submit(block_job, *b) for b in blocks]
        for future in concurrent.futures.as_completed(futures):
            rows += future.result()
    return rows


# ----------------------------------------------------------------------------- commands

def load_tasks(args, analysis=True):
    public = tasks_module.load_public()
    private = tasks_module.load_private(args.home)
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
    return tasks_module.manifest_hash(tasks_module.load_public() + tasks_module.load_private(args.home))


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
                summary.append({"variant": variant, "task": row["task_id"], "arm": row["arm"],
                                "failure_mode": row["failure_mode"], "expected": want,
                                "ok": want is None or row["failure_mode"] == want,
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
    if harness_commit().find("dirty") >= 0 and not args.allow_dirty:
        raise SystemExit("the harness tree is dirty; commit or pass --allow-dirty")
    refuse_dry_campaign(args.home, args.campaign)
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
    rows = schedule(settings, task_list, models, args.arms.split(","), args.reps, args.effort, manifest,
                    args.workers, phase=phase)
    print("%d records written to %s" % (len(rows), settings.dir / "results.jsonl"))
    return 0


def load_allowlist(home):
    path = Path(home) / "secret_allowlist.json"
    if path.exists():
        return json.loads(path.read_text())
    return []


def cmd_prepare(args):
    import gen_tasks
    if args.generate:
        import baseline_build
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
    report = {"snapshots_match_stored_hash": snapshot_ok, "truth_check_failures": {k: v for k, v in
                                                                                   truth_failures.items() if v},
              "corpus_unchanged": corpus_after == corpus_before, "corpus_hashes": corpus_after}
    out = Path(args.home) / "campaigns" / args.campaign / "validate_report.json"
    out.write_text(json.dumps(report, indent=1))
    print(json.dumps({k: v for k, v in report.items() if k != "corpus_hashes"}, indent=1))
    ok = code == 0 and all(snapshot_ok.values()) and not report["truth_check_failures"] and report["corpus_unchanged"]
    return 0 if ok else 1


def cmd_estimate(args):
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
    return report.main(["--home", str(args.home), "--campaign", args.campaign] + (["--dry"] if args.dry else []))


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
        sp.add_argument("--families")
        sp.add_argument("--per-family", type=int)
        sp.add_argument("--model", default="claude-sonnet-5-5")
        sp.add_argument("--effort", default="cc-default", choices=config.EFFORTS)
        sp.add_argument("--arms", default="a,b")
        sp.add_argument("--reps", type=int, default=1)
        sp.add_argument("--workers", type=int, default=3)
        sp.add_argument("--max-turns", type=int, default=config.LIMITS["turn_limit"])
        sp.add_argument("--max-output-tokens", type=int, default=config.MAX_OUTPUT_CAP)
        sp.add_argument("--budget-usd", type=float, default=config.CAMPAIGN_CAP_USD)
        sp.add_argument("--run-cap-usd", type=float)
        sp.add_argument("--use-reserve", action="store_true")
        sp.add_argument("--api-base", default=config.API_BASE)
        sp.add_argument("--allow-dirty", action="store_true")
        sp.add_argument("--accept-deviation", action="append")
        sp.add_argument("--ce-trigger", type=int)
        sp.add_argument("--seed", type=int, default=config.SEED)
        sp.add_argument("--plan")
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
    dry.add_argument("--variants", default="correct,wrong,tamper,unread_edit,probe,qualified,cfgtest")
    common(sub.add_parser("pilot"))
    common(sub.add_parser("estimate")).add_argument("--assume", default="pilot", choices=["pilot", "prepilot"])
    common(sub.add_parser("run"))
    common(sub.add_parser("anchor")).add_argument("--dry-anchor", action="store_true",
                                                    help="upstream is the fake API with a scripted agent (free)")
    common(sub.add_parser("reconcile")).add_argument("--provider-usd", type=float)
    common(sub.add_parser("report")).add_argument("--dry", action="store_true")
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
    }
    return commands[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
