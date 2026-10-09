"""Real Claude Code anchor: the same tasks, snapshots, sandbox, setup and oracles, with `claude -p` as the agent.

Claude Code talks to proxy.py on 127.0.0.1, which forwards to the upstream and logs every
request's usage. Claude Code only ever holds a dummy key: the proxy swaps in the campaign key,
so the key never enters the sandbox, the agent's shell or a transcript. With --dry the
upstream is fake_api.py with a scripted agent, which validates the recipe at no cost.
"""

import hashlib
import json
import os
import shutil
import subprocess
import time
from pathlib import Path

import accounting
import cc_profile
import config
import fake_agent
import fake_api
import oracles
import proxy as proxy_module
import sandbox
import tasks as tasks_module
import workspace

DUMMY_KEY = "sk-ant-dummy-anchor-key-000000000000000000"


class KeyProxy(proxy_module.Proxy):
    """Replaces the dummy key with the real one on the way out."""

    def __init__(self, upstream, log_path, real_key):
        proxy_module.Proxy.__init__(self, upstream, log_path)
        self.real_key = real_key
        handler = self.httpd.RequestHandlerClass
        original = handler._forward
        key = real_key

        def forward(inner, method):
            if key and "x-api-key" in inner.headers:
                del inner.headers["x-api-key"]
                inner.headers["x-api-key"] = key
            return original(inner, method)
        handler._forward = forward


def anchor_once(args, task, arm, rep, ledger, pricing, upstream, real_key, results_path, fake=None):
    import run as run_module
    rid = hashlib.sha256(("anchor|%s|%s|%s|%d" % (args.campaign, task["id"], arm, rep)).encode()).hexdigest()[:16]
    run_dir = Path(args.runroot) / ("cc" + rid[:10])
    if run_dir.exists():
        shutil.rmtree(str(run_dir))
    snap = run_module.snapshot_dir(args.home, task)
    repo = run_dir / "repo"
    workspace.clone_tree(snap / "repo", repo)
    target = run_dir / "target"
    if (snap / "target").exists():
        workspace.clone_tree(snap / "target", target)
        workspace.invalidate_local_fingerprints(repo, target)
    home = run_dir / "home"
    bindir = workspace.make_toolbin(run_dir / "bin")
    jl = None
    allow_exec = []
    if config.ARMS[arm]["justless"]:
        jl = run_dir / "jlbin"
        workspace.install_binary(Path(args.binary).resolve(), jl / "justless")
        allow_exec.append(jl / "justless")
    env = workspace.tool_env(repo.resolve(), home, bindir, target, jl, tmp=run_dir / "tmp")
    deny = sandbox.default_denies(config, [Path(args.home), Path(args.runroot)])
    cc_dir = Path(args.cc_binary).parent
    text = sandbox.profile(run_dir, deny_read=deny, allow_read=sandbox.default_allows(config) + [cc_dir],
                           allow_write=[config.CARGO_HOME], allow_exec=allow_exec + [Path(args.cc_binary)])
    profile_path, profile_hash = sandbox.write_profile(run_dir, text)

    def runner(argv, timeout):
        proc = subprocess.run(sandbox.wrap(argv, profile_path), cwd=str(repo), env=env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
        return proc.returncode, proc.stdout.decode("utf-8", "replace")

    if jl:
        runner([str(jl / "justless"), "init", "--hooks", "--agent", "claude"], 120)
    workspace.normalize_mtimes(repo)
    workspace.git_baseline(repo, env=workspace.git_env(repo, home))
    if task["family"] in ("bugfix", "lint") and task["language"] == "rust":
        runner(["cargo", "test", "--offline", "--no-run"] + task["oracle"].get("cargo_scope", []), 3600)
    reference = workspace.tree_files(repo)
    log = run_dir / "proxy.jsonl"
    proxy = KeyProxy(upstream, log, real_key).start()
    if fake is not None:
        steps = fake_agent.VARIANTS["correct"](task, bool(jl))
        fake.register("default", fake_api.Script(steps, {"root": str(repo.resolve())}))
    cc_env = dict(env, ANTHROPIC_API_KEY=DUMMY_KEY, ANTHROPIC_BASE_URL=proxy.base_url,
                  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1",
                  CLAUDE_CODE_TMPDIR=str(run_dir / "cctmp"), NO_PROXY="127.0.0.1,localhost",
                  no_proxy="127.0.0.1,localhost")
    (run_dir / "cctmp").mkdir(parents=True, exist_ok=True)
    argv = [args.cc_binary, "-p", task["prompt"], "--model", args.model, "--output-format", "stream-json", "--verbose",
            "--permission-mode", "bypassPermissions", "--max-turns", str(args.max_turns)]
    started = time.time()
    try:
        proc = subprocess.run(sandbox.wrap(argv, profile_path), cwd=str(repo), env=cc_env, stdin=subprocess.DEVNULL,
                              capture_output=True, timeout=config.LIMITS["wall_timeout"])
        stream, timed_out = proc.stdout.decode("utf-8", "replace"), False
        errors = proc.stderr.decode("utf-8", "replace")
    except subprocess.TimeoutExpired as expired:
        stream, timed_out = (expired.stdout or b"").decode("utf-8", "replace"), True
        errors = (expired.stderr or b"").decode("utf-8", "replace")
    wall = time.time() - started
    proxy.stop()
    final_text = ""
    result_event = {}
    for line in stream.split("\n"):
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if event.get("type") == "result":
            result_event = event
            final_text = event.get("result") or ""
    requests = [json.loads(l) for l in open(str(log))] if log.exists() else []
    totals = {"requests": len(requests), "cost_usd": 0.0, "input_tokens": 0, "cache_creation_input_tokens": 0,
              "cache_read_input_tokens": 0, "output_tokens": 0, "prompt_tokens": 0, "wall_s": round(wall, 3)}
    for row in requests:
        usage = row.get("usage") or {}
        model = row.get("model") or args.model
        if model not in config.MODELS:
            model = args.model
        cost = pricing.cost(model, usage)
        totals["cost_usd"] += cost
        for key in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"):
            totals[key] += int(usage.get(key) or 0)
        totals["prompt_tokens"] += accounting.prompt_tokens(usage)
        ledger.record({"run_id": rid, "attempt": 1, "campaign": args.campaign, "phase": "anchor", "model": model,
                       "arm": arm, "cost_usd": cost})
    if timed_out:
        mode, grade = "wall_timeout", None
    elif result_event.get("subtype") == "error_max_turns":
        mode, grade = "turn_limit", None
    else:
        grade = oracles.grade(task, repo, reference, runner, final_text)
        mode = grade["failure_mode"]
    record = {"campaign": args.campaign, "phase": "anchor", "agent": "claude-code", "model": args.model, "arm": arm,
              "task_id": task["id"], "family": task["family"], "repo": task["repo"], "language": task["language"],
              "rep": rep, "run_id": rid, "attempt": 1, "success": bool(grade and grade["success"]),
              "failure_mode": mode, "oracle": grade, "totals": totals,
              "claude_md_seen": any(r.get("has_claude_md") for r in requests),
              "efforts_sent": sorted({str(r.get("effort")) for r in requests}),
              "models_sent": sorted({str(r.get("model")) for r in requests}),
              "pins": {"cc_binary": args.cc_binary, "sandbox_profile_hash": profile_hash}}
    with open(str(results_path), "a") as handle:
        handle.write(json.dumps(record) + "\n")
    store = Path(args.home) / "campaigns" / args.campaign / "anchor_runs" / rid
    store.mkdir(parents=True, exist_ok=True)
    (store / "stream.jsonl").write_text(stream)
    (store / "stderr.txt").write_text(errors[-20000:])
    shutil.copy(str(log), str(store / "proxy.jsonl")) if log.exists() else None
    if not args.keep_runs:
        workspace.remove(run_dir)
    return record


def main(args):
    import run as run_module
    task_list = run_module.load_tasks(args, analysis=not args.dry_anchor)
    if args.per_family:
        task_list = tasks_module.first_per_family(task_list, args.per_family)
    campaign_dir = Path(args.home) / "campaigns" / args.campaign
    campaign_dir.mkdir(parents=True, exist_ok=True)
    pricing = accounting.Pricing()
    fake = None
    if args.dry_anchor:
        fake = fake_api.FakeAPI(0, str(campaign_dir / "fake_requests.jsonl")).start()
    ledger = accounting.Ledger(
        campaign_dir / "ledger.jsonl", args.budget_usd,
        reserve_usd=config.RESERVE_USD if args.budget_usd >= config.CAMPAIGN_CAP_USD else 0.0,
        use_reserve=args.use_reserve, prior_usd=0.0 if fake else run_module.paid_spend(args.home, exclude=args.campaign),
        global_cap_usd=None if fake else config.CAMPAIGN_CAP_USD)
    if fake is not None:
        upstream, key = fake.base_url, None
    else:
        key = os.environ.get("ANTHROPIC_API_KEY")
        if not key:
            raise SystemExit("ANTHROPIC_API_KEY is required for the anchor")
        upstream = args.api_base
    rows = []
    try:
        for task in tasks_module.ordered(task_list):
            for arm in config.arm_order(args.model, task["id"], 1, args.arms.split(",")):
                ledger.check(pricing.worst_case(args.model, 200000, 32000))
                record = anchor_once(args, task, arm, 1, ledger, pricing, upstream, key,
                                     campaign_dir / "results.jsonl", fake)
                rows.append(record)
                print("%-40s %s %-14s cost %.4f claude_md_seen=%s" % (task["id"], arm, record["failure_mode"],
                                                                     record["totals"]["cost_usd"], record["claude_md_seen"]))
    finally:
        if fake is not None:
            fake.stop()
    return 0
