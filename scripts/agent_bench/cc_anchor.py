"""Real Claude Code anchor: the same tasks, snapshots, sandbox, setup and oracles, with `claude -p` as the agent.

Claude Code talks to proxy.py on 127.0.0.1, which forwards to the upstream and logs every
request's usage. Claude Code only ever holds a dummy key: the proxy swaps in the campaign key,
so the key never enters the sandbox, the agent's shell or a transcript. The proxy also enforces
the budget per request: it checks the campaign ledger and the run budget against the request's
worst case before forwarding, records the cost when the response completes, and answers locally
with an error once a cap is reached. With --dry the upstream is fake_api.py with a scripted agent,
which validates the recipe at no cost.
"""

import hashlib
import json
import os
import shutil
import subprocess
import threading
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
    """Replaces the dummy key with the real one on the way out and enforces the budget per request."""

    def __init__(self, upstream, log_path, real_key, ledger=None, pricing=None, model=None, run_budget=None,
                 entry=None):
        proxy_module.Proxy.__init__(self, upstream, log_path)
        self.real_key = real_key
        self.ledger = ledger
        self.pricing = pricing
        self.model = model
        self.run_budget = run_budget
        self.entry = dict(entry or {})
        self.spent = 0.0
        self.stopped = None
        self.budget_lock = threading.Lock()
        handler = self.httpd.RequestHandlerClass
        original = handler._forward
        key = real_key

        def forward(inner, method):
            if key and "x-api-key" in inner.headers:
                del inner.headers["x-api-key"]
                inner.headers["x-api-key"] = key
            return original(inner, method)
        handler._forward = forward

    def priced_model(self, model):
        return model if model in config.MODELS else self.model

    def before_messages(self, sent):
        if self.ledger is None:
            return None, None
        model = self.priced_model(sent.get("model"))
        estimate = len(json.dumps(sent).encode("utf-8")) // 2
        worst = self.pricing.worst_case(model, estimate, int(sent.get("max_tokens") or config.MAX_OUTPUT_CAP))
        with self.budget_lock:
            if self.stopped:
                return refusal(self.stopped), None
            # The run budget stops a run once it has spent it, as the harness loop does; the campaign
            # ledger is checked against the request's worst case.
            if self.run_budget is not None and self.spent >= self.run_budget:
                self.stopped = "run_budget"
                return refusal(self.stopped), None
            try:
                reserved = self.ledger.check(worst)
            except accounting.BudgetExceeded:
                self.stopped = "aborted_global_cap"
                return refusal(self.stopped), None
        return None, reserved

    def after_messages(self, sent, status, model, usage, token):
        if self.ledger is None or token is None:
            return
        if status != 200:
            self.ledger.release(token)
            return
        cost = self.pricing.cost(self.priced_model(model), usage or {})
        with self.budget_lock:
            self.spent += cost
        self.ledger.record(dict(self.entry, model=self.priced_model(model), cost_usd=cost), reserved=token)


def refusal(reason):
    """A non-retryable API error, so Claude Code stops instead of retrying into the cap."""
    return (400, {"type": "error", "error": {"type": "invalid_request_error",
                                             "message": "benchmark %s reached" % reason}})


def anchor_rid(args, task, arm, rep, pins):
    key = "anchor|%s|%s|%s|%s|%d|%s|%s|%s" % (args.campaign, args.model, task["id"], arm, rep,
                                              pins["justless_sha256"], pins["harness_tree"], pins["manifest_hash"])
    return hashlib.sha256(key.encode()).hexdigest()[:16]


def anchor_once(args, task, arm, rep, ledger, pricing, upstream, real_key, results_path, fake=None, pins=None):
    import run as run_module
    pins = pins or {}
    rid = anchor_rid(args, task, arm, rep, pins)
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
        run_module.install_pinned(Path(pins["binary_path"]), jl / "justless", pins["justless_sha256"])
        allow_exec.append(jl / "justless")
    env = workspace.tool_env(repo.resolve(), home, bindir, target, jl, tmp=run_dir / "tmp")
    deny = sandbox.default_denies(config, [Path(args.home), Path(args.runroot)])
    cc_dir = Path(args.cc_binary).parent
    hidden = sandbox.cargo_git_denies(config.CARGO_HOME, run_module.own_repos(task), snap / "repo")
    text = sandbox.profile(run_dir, deny_read=deny, allow_read=sandbox.default_allows(config) + [cc_dir],
                           deny_after=hidden, write_regexes=sandbox.cargo_write_regexes(config.CARGO_HOME),
                           allow_exec=allow_exec + [Path(args.cc_binary)])
    profile_path, profile_hash = sandbox.write_profile(run_dir, text)

    def runner(argv, timeout):
        proc = subprocess.run(sandbox.wrap(argv, profile_path), cwd=str(repo), env=env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
        return proc.returncode, proc.stdout.decode("utf-8", "replace")

    if jl:
        runner([str(jl / "justless"), "init", "--hooks", "--agent", "claude"], 120)
    workspace.normalize_mtimes(repo)
    workspace.git_task_history(repo, task, env=workspace.git_env(repo, home))
    if tasks_module.is_edit(task) and task["language"] == "rust":
        runner(["cargo", "test", "--offline", "--no-run"] + task["oracle"].get("cargo_scope", []), 3600)
    reference = workspace.tree_files(repo)
    log = run_dir / "proxy.jsonl"
    run_budget = getattr(args, "run_cap_usd", None) or config.MODELS[args.model]["run_budget"]
    proxy = KeyProxy(upstream, log, real_key, ledger, pricing, args.model, run_budget,
                     {"run_id": rid, "attempt": 1, "campaign": args.campaign, "phase": "anchor", "arm": arm}).start()
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
    requests = [r for r in requests if not r.get("refused")]
    totals = {"requests": len(requests), "cost_usd": 0.0, "input_tokens": 0, "cache_creation_input_tokens": 0,
              "cache_read_input_tokens": 0, "output_tokens": 0, "prompt_tokens": 0, "wall_s": round(wall, 3)}
    for row in requests:
        usage = row.get("usage") or {}
        if row.get("status") == 200:
            totals["cost_usd"] += pricing.cost(proxy.priced_model(row.get("model")), usage)
        for key in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"):
            totals[key] += int(usage.get(key) or 0)
        totals["prompt_tokens"] += accounting.prompt_tokens(usage)
    # As in the harness, a run that stops on a limit is still graded on its final state; only a campaign-cap
    # abort is left ungraded and re-run on resume.
    grade = None
    if proxy.stopped == "aborted_global_cap":
        stopped_by, mode = "aborted_global_cap", "aborted_global_cap"
    else:
        if timed_out:
            stopped_by = "wall_timeout"
        elif proxy.stopped:
            stopped_by = proxy.stopped
        elif result_event.get("subtype") == "error_max_turns":
            stopped_by = "turn_limit"
        else:
            stopped_by = "end_turn"
        grade = oracles.grade(task, repo, reference, runner, final_text)
        mode = grade["failure_mode"]
    record = {"campaign": args.campaign, "phase": "anchor", "agent": "claude-code", "model": args.model, "arm": arm,
              "task_id": task["id"], "family": task["family"], "repo": task["repo"], "language": task["language"],
              "rep": rep, "run_id": rid, "attempt": 1, "success": bool(grade and grade["success"]),
              "failure_mode": mode, "stopped_by": stopped_by,
              "limit_hit": stopped_by in ("turn_limit", "wall_timeout", "run_budget"), "oracle": grade,
              "totals": totals, "settings": {"max_turns": args.max_turns, "run_budget": run_budget},
              "claude_md_seen": any(r.get("has_claude_md") for r in requests),
              "efforts_sent": sorted({str(r.get("effort")) for r in requests}),
              "models_sent": sorted({str(r.get("model")) for r in requests}),
              "pins": dict({k: v for k, v in pins.items() if k != "binary_path"}, cc_binary=args.cc_binary,
                           sandbox_profile_hash=profile_hash)}
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
    if not args.dry_anchor:
        run_module.refuse_dry_campaign(args.home, args.campaign)
    campaign_dir.mkdir(parents=True, exist_ok=True)
    pricing = accounting.Pricing()
    fake = None
    if args.dry_anchor:
        run_module.mark_dry(campaign_dir)
        fake = fake_api.FakeAPI(0, str(campaign_dir / "fake_requests.jsonl")).start()
    home = args.home
    ledger = accounting.Ledger(
        campaign_dir / "ledger.jsonl", args.budget_usd,
        reserve_usd=config.RESERVE_USD if args.budget_usd >= config.CAMPAIGN_CAP_USD else 0.0,
        use_reserve=args.use_reserve, fake=fake is not None,
        prior_fn=None if fake else (lambda: run_module.paid_spend(home, exclude=args.campaign)),
        global_cap_usd=None if fake else config.CAMPAIGN_CAP_USD)
    pins = {"justless_sha256": "none", "binary_path": None, "harness_tree": config.harness_tree_hash(),
            "harness_commit": run_module.harness_commit(), "manifest_hash": run_module.full_manifest(args),
            "pricing_hash": pricing.hash}
    if args.binary:
        binary, pins["justless_sha256"] = run_module.pin_binary(args.home, args.binary)
        pins["binary_path"] = str(binary)
    elif any(config.ARMS[a]["justless"] for a in args.arms.split(",")):
        raise SystemExit("--binary is required for arms b and d")
    if not args.dry_anchor:
        if args.campaign == "main":
            run_module.require_frozen_manifest(args.home, pins["manifest_hash"])
        run_module.check_pins(campaign_dir, {k: pins[k] for k in ("justless_sha256", "harness_tree", "manifest_hash",
                                                                   "pricing_hash")},
                              {"anchor|%s" % args.model: {"max_turns": args.max_turns}}, args.new_pins)
    done = {}
    for row in run_module.read_rows(campaign_dir / "results.jsonl"):
        if row.get("agent") == "claude-code" and row.get("failure_mode") in run_module.TERMINAL:
            done[row["run_id"]] = row
    if fake is not None:
        upstream, key = fake.base_url, None
    else:
        key = os.environ.get("ANTHROPIC_API_KEY")
        if not key:
            raise SystemExit("ANTHROPIC_API_KEY is required for the anchor")
        upstream = args.api_base
    rows = []
    try:
        for task in tasks_module.ordered(task_list, args.seed):
            for arm in config.arm_order(args.model, task["id"], 1, args.arms.split(","), args.seed):
                if anchor_rid(args, task, arm, 1, pins) in done:
                    continue
                record = anchor_once(args, task, arm, 1, ledger, pricing, upstream, key,
                                     campaign_dir / "results.jsonl", fake, pins)
                rows.append(record)
                print("%-40s %s %-14s stopped_by %-12s cost %.4f claude_md_seen=%s" % (
                    task["id"], arm, record["failure_mode"], record["stopped_by"], record["totals"]["cost_usd"],
                    record["claude_md_seen"]))
                if record["failure_mode"] == "aborted_global_cap":
                    print("campaign cap reached; stopping the anchor")
                    return 3
    finally:
        if fake is not None:
            fake.stop()
    return 0
