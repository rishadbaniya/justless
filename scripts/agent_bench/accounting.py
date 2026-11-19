"""Pricing, per-request cost, the campaign ledger, caps and the budget estimator."""

import json
import os
import threading
import time
from pathlib import Path

import config

MTOK = 1_000_000.0


class Pricing:
    def __init__(self, table=None):
        self.table = table or config.pricing()
        self.hash = config.sha256_json(self.table)

    def tier(self, model, prompt_tokens):
        tiers = self.table["models"][model]["tiers"]
        for tier in tiers:
            limit = tier.get("max_prompt_tokens")
            if limit is None or prompt_tokens <= limit:
                return tier
        return tiers[-1]

    def tier_index(self, model, prompt_tokens):
        tiers = self.table["models"][model]["tiers"]
        return tiers.index(self.tier(model, prompt_tokens))

    def cost(self, model, usage):
        """Cost in USD of one response's usage. The Haiku tier uses input + cache write + cache read."""
        uncached = int(usage.get("input_tokens") or 0)
        read = int(usage.get("cache_read_input_tokens") or 0)
        creation = usage.get("cache_creation") or {}
        write_total = int(usage.get("cache_creation_input_tokens") or 0)
        write_1h = int(creation.get("ephemeral_1h_input_tokens") or 0)
        write_5m = int(creation.get("ephemeral_5m_input_tokens") or (write_total - write_1h))
        output = int(usage.get("output_tokens") or 0)
        prompt = uncached + read + write_total
        tier = self.tier(model, prompt)
        return (uncached * tier["input"] + write_5m * tier["cache_write_5m"] + write_1h * tier["cache_write_1h"]
                + read * tier["cache_read"] + output * tier["output"]) / MTOK

    def worst_case(self, model, prompt_tokens, max_tokens):
        """Upper bound for the next request: the whole prompt at the top tier's dearest prompt rate (a fresh
        prefix is billed as a cache write, which costs more than uncached input) plus max output."""
        tier = self.table["models"][model]["tiers"][-1]
        rate = max(tier["input"], tier["cache_write_5m"], tier["cache_write_1h"])
        return (prompt_tokens * rate + max_tokens * tier["output"]) / MTOK

    def reprice_1h(self, model, usage, ttl_expiry_rewrite=False):
        """Sensitivity: price cache writes at the 1-hour rate; a TTL-expiry rewrite becomes a read."""
        usage = dict(usage)
        creation = dict(usage.get("cache_creation") or {})
        total = int(usage.get("cache_creation_input_tokens") or 0)
        if ttl_expiry_rewrite:
            usage["cache_read_input_tokens"] = int(usage.get("cache_read_input_tokens") or 0) + total
            usage["cache_creation_input_tokens"] = 0
            creation = {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
        else:
            creation = {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": total}
        usage["cache_creation"] = creation
        return self.cost(model, usage)


def prompt_tokens(usage):
    return (int(usage.get("input_tokens") or 0) + int(usage.get("cache_creation_input_tokens") or 0)
            + int(usage.get("cache_read_input_tokens") or 0))


class BudgetExceeded(Exception):
    def __init__(self, kind, detail):
        Exception.__init__(self, "%s: %s" % (kind, detail))
        self.kind = kind


def entry_is_fake(entry, directory_fake=False):
    """Whether a ledger entry records fake (dry-run) usage. Entries carry `fake`; older entries are judged by
    their phase or by a dry-run marker in their directory."""
    if "fake" in entry:
        return bool(entry["fake"])
    return entry.get("phase") == "dry-run" or directory_fake


def directory_is_dry(directory):
    directory = Path(directory)
    return (directory / "DRY_RUN").exists() or (directory / "fake_requests.jsonl").exists()


class _FileLock:
    """An exclusive flock on a sidecar file, so processes sharing a campaign ledger see each other's spend."""

    def __init__(self, path):
        self.path = str(path)
        self.handle = None

    def __enter__(self):
        import fcntl
        self.handle = open(self.path, "a")
        fcntl.flock(self.handle.fileno(), fcntl.LOCK_EX)
        return self

    def __exit__(self, *exc):
        import fcntl
        fcntl.flock(self.handle.fileno(), fcntl.LOCK_UN)
        self.handle.close()
        return False


class Ledger:
    """Append-only spend ledger shared by every worker and process in a campaign directory.

    check() reserves a request's worst case until record() or release() settles it, so concurrent workers
    cannot all pass the same last dollar. Each check re-reads entries other processes appended since the last
    read, under an exclusive file lock, and asks prior_fn for other campaigns' paid spend."""

    def __init__(self, path, cap_usd, reserve_usd=config.RESERVE_USD, use_reserve=False, prior_usd=0.0,
                 global_cap_usd=None, fake=False, prior_fn=None):
        """cap_usd bounds this campaign; prior_usd (or prior_fn()) is what other paid campaigns already spent,
        checked together with this one against global_cap_usd (the $90 campaign total, its last $10 behind
        use_reserve). fake marks every entry as dry-run usage that never counts as paid spend."""
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.cap = float(cap_usd)
        self.reserve = float(reserve_usd)
        self.use_reserve = use_reserve
        self.prior = float(prior_usd)
        self.prior_fn = prior_fn
        self.global_cap = None if global_cap_usd is None else float(global_cap_usd)
        self.fake = bool(fake)
        self.lock = threading.RLock()
        self.file_lock = self.path.with_name(self.path.name + ".lock")
        self.spent = 0.0
        self.pending = 0.0
        self.offset = 0
        self._refresh()

    def _refresh(self):
        """Add entries appended since the last read; a partial last line waits for the next read."""
        if not self.path.exists():
            return
        with open(str(self.path), "rb") as handle:
            handle.seek(self.offset)
            data = handle.read()
        end = data.rfind(b"\n")
        if end < 0:
            return
        for line in data[:end + 1].split(b"\n"):
            if not line.strip():
                continue
            try:
                self.spent += float(json.loads(line.decode("utf-8")).get("cost_usd") or 0)
            except ValueError:
                pass
        self.offset += end + 1

    @property
    def limit(self):
        return self.cap if self.use_reserve else self.cap - self.reserve

    @property
    def global_limit(self):
        if self.global_cap is None:
            return None
        return self.global_cap if self.use_reserve else self.global_cap - config.RESERVE_USD

    def check(self, worst_case_usd):
        """Raise BudgetExceeded, or reserve worst_case_usd and return it for record() or release()."""
        with self.lock, _FileLock(self.file_lock):
            self._refresh()
            committed = self.spent + self.pending
            if committed + worst_case_usd > self.limit:
                raise BudgetExceeded("aborted_global_cap", "spent %.4f + pending %.4f + next %.4f > %.4f" % (
                    self.spent, self.pending, worst_case_usd, self.limit))
            if self.global_limit is not None:
                prior = self.prior_fn() if self.prior_fn else self.prior
                if prior + committed + worst_case_usd > self.global_limit:
                    raise BudgetExceeded("aborted_global_cap", "all campaigns %.4f + next %.4f > %.4f" % (
                        prior + committed, worst_case_usd, self.global_limit))
            self.pending += worst_case_usd
            return worst_case_usd

    def release(self, reserved):
        with self.lock:
            self.pending = max(0.0, self.pending - float(reserved or 0))

    def record(self, entry, reserved=0.0):
        with self.lock, _FileLock(self.file_lock):
            self.pending = max(0.0, self.pending - float(reserved or 0))
            self._refresh()
            cost = float(entry.get("cost_usd") or 0)
            entry = dict(entry, fake=self.fake, ledger_total=round(self.spent + cost, 6), t=time.time())
            with open(str(self.path), "a") as handle:
                handle.write(json.dumps(entry) + "\n")
                handle.flush()
                os.fsync(handle.fileno())
            self._refresh()


def paid_spend(home, exclude=None):
    """Spend recorded by paid requests in every campaign of a store. Dry-run usage is fake and never counts,
    whichever directory its ledger is in."""
    total = 0.0
    for path in sorted((Path(home) / "campaigns").glob("*/ledger.jsonl")):
        if path.parent.name == exclude:
            continue
        directory_fake = directory_is_dry(path.parent)
        with open(str(path)) as handle:
            for line in handle:
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                if not entry_is_fake(entry, directory_fake):
                    total += float(entry.get("cost_usd") or 0)
    return total


# Pre-pilot assumptions (median USD per run) from the design.
PREPILOT = {
    "claude-sonnet-5-5": {"bugfix": 0.35, "lint": 0.35, "location": 0.12, "impact": 0.16},
    "claude-haiku-5-5": {"bugfix": 0.025, "lint": 0.025, "location": 0.009, "impact": 0.012},
    "claude-opus-5-5": {"bugfix": 0.70, "lint": 0.70, "location": 0.24, "impact": 0.32},
}

FAMILIES = ["bugfix", "lint", "location", "impact"]


def plan_cost(plan, per_run):
    """plan: list of {model, tasks_per_family, reps, arms}; per_run: model -> family -> usd."""
    total = 0.0
    rows = []
    for cell in plan:
        model = cell["model"]
        cost = 0.0
        runs = 0
        for family in FAMILIES:
            n = cell["tasks_per_family"] * cell["reps"] * len(cell["arms"])
            cost += n * per_run[model][family]
            runs += n
        rows.append(dict(cell, runs=runs, usd=round(cost, 2)))
        total += cost
    return total, rows


MAIN_PLAN = [
    {"name": "sonnet", "model": "claude-sonnet-5-5", "tasks_per_family": 8, "reps": 3, "arms": ["a", "b"]},
    {"name": "haiku", "model": "claude-haiku-5-5", "tasks_per_family": 8, "reps": 4, "arms": ["a", "b", "c", "d"]},
    {"name": "anchor", "model": "claude-sonnet-5-5", "tasks_per_family": 2, "reps": 1, "arms": ["a", "b"],
     "anchor_overhead": 1.6},
    {"name": "opus", "model": "claude-opus-5-5", "tasks_per_family": 1, "reps": 3, "arms": ["a", "b"]},
]
PILOT_PLAN = [
    {"name": "pilot-haiku", "model": "claude-haiku-5-5", "tasks_per_family": 2, "reps": 1, "arms": ["a", "b", "c", "d"]},
    {"name": "pilot-sonnet", "model": "claude-sonnet-5-5", "tasks_per_family": 1, "reps": 1, "arms": ["a", "b"]},
]
SCALE_DOWN = [("opus", {"tasks_per_family": 0}), ("haiku", {"reps": 3}), ("anchor", {"tasks_per_family": 1}),
              ("sonnet", {"tasks_per_family": 6})]
SCALE_UP = [("opus", {"tasks_per_family": 2}), ("sonnet", {"reps": 4}),
            ("sonnet-ce", {"model": "claude-sonnet-5-5", "tasks_per_family": 8, "reps": 3, "arms": ["c", "d"],
                           "families": ["bugfix", "lint"]})]


# The large-output family is its own campaign, registered separately from the 32-task plan above: Sonnet, arms
# a and b, three repetitions of its eight tasks. It is never scaled up or down by the rules above.
LARGE_PLAN = [{"name": "large-sonnet", "model": "claude-sonnet-5-5", "tasks_per_family": 8, "reps": 3,
               "arms": ["a", "b"], "families": ["large"]}]

# Per-run planning model, fitted on 2026-11-19 to the main campaign's 246 graded Sonnet runs: the peak prompt is
# about RUN_MODEL["base"] + RUN_MODEL["per_byte"] x tool bytes seen + RUN_MODEL["per_request"] x requests tokens.
# Each run writes its whole prompt to the cache once (every run has its own session block) and reads the
# cached prefix on every later request, which grows roughly linearly. Summed over those runs the model gave
# $10.55 against $10.35 billed; per run its ratio to the bill had a median of 1.09 (p10 0.73, p90 1.18).
RUN_MODEL = {"base": 7084, "per_byte": 0.3937, "per_request": 402.7, "first_prompt": 6600,
             "output_per_request": 200}
# A model takes more steps and reads more than the minimal scripted path that dry-run measures.
LARGE_MARGIN = {"requests": 1.5, "bytes": 1.25, "p90": 1.5}
# Without a dry-run measurement: every arm priced like the plain path's medians in the 2026-11-19 dry run
# (about 101 KB seen over 8 requests).
LARGE_PREPILOT_PATH = {"a": {"seen": 101000, "requests": 8}}


def run_cost(model, seen_bytes, requests, pricing_table=None):
    """Planning cost in USD of one run that sees `seen_bytes` of tool output over `requests` requests."""
    pricing_table = pricing_table or Pricing()
    peak = RUN_MODEL["base"] + RUN_MODEL["per_byte"] * seen_bytes + RUN_MODEL["per_request"] * requests
    tier = pricing_table.tier(model, peak)
    reads = max(0, requests - 1) * (peak + RUN_MODEL["first_prompt"]) / 2.0
    output = RUN_MODEL["output_per_request"] * requests
    return (peak * tier["cache_write_5m"] + reads * tier["cache_read"] + output * tier["output"]) / 1e6


def estimate_large(paths, spent=0.0, plan=None, use_reserve=False, budget=config.CAMPAIGN_CAP_USD):
    """Expected and conservative cost of the large-family plan. paths: {task_id: {arm: {seen, requests}}} from
    the scripted plain path (dry-run), scaled by LARGE_MARGIN; an arm without a measurement uses arm a's."""
    plan = plan or LARGE_PLAN
    rows = []
    expected = 0.0
    for cell in plan:
        cost = 0.0
        runs = 0
        per_task = {}
        for task_id, arms in sorted(paths.items()):
            task_cost = 0.0
            for arm in cell["arms"]:
                path = arms.get(arm) or arms.get("a")
                task_cost += run_cost(cell["model"], path["seen"] * LARGE_MARGIN["bytes"],
                                      path["requests"] * LARGE_MARGIN["requests"])
            per_task[task_id] = round(task_cost * cell["reps"], 3)
            cost += task_cost * cell["reps"]
            runs += cell["reps"] * len(cell["arms"])
        rows.append({"cell": cell["name"], "model": cell["model"], "runs": runs, "usd": round(cost, 2),
                     "per_task_usd": per_task})
        expected += cost
    limit = enforced_limit(budget, use_reserve)
    room = limit - spent
    conservative = expected * LARGE_MARGIN["p90"]
    return {"expected_usd": round(expected, 2), "conservative_usd": round(conservative, 2),
            "enforced_limit_usd": round(limit, 2), "spent_usd": round(spent, 4), "room_usd": round(room, 2),
            "fits": conservative <= room, "rows": rows, "margin": LARGE_MARGIN, "run_model": RUN_MODEL}


def dry_run_paths(directory):
    """{task_id: {arm: {seen, requests, source}}} from a dry run's results (the correct variant's campaign)."""
    out = {}
    path = Path(directory) / "results.jsonl"
    if not path.exists():
        return out
    with open(str(path)) as handle:
        for line in handle:
            try:
                row = json.loads(line)
            except ValueError:
                continue
            totals = row.get("totals") or {}
            if row.get("family") != "large" or not totals or row.get("failure_mode") != "success":
                continue
            out.setdefault(row["task_id"], {})[row["arm"]] = {
                "seen": totals.get("tool_bytes_seen", 0), "requests": totals.get("requests", 0),
                "source": totals.get("tool_bytes_source")}
    return out


def enforced_limit(budget=config.CAMPAIGN_CAP_USD, use_reserve=False):
    """The total that Ledger.check enforces across campaigns: the cap less the reserve unless it is released."""
    return budget if use_reserve else budget - config.RESERVE_USD


def estimate(per_run, spent=0.0, budget=config.CAMPAIGN_CAP_USD, plan=None, use_reserve=False):
    """Apply the pre-registered scale orders to a per-run cost table against the limit the ledger enforces.
    Returns a dict for printing."""
    import copy
    plan = copy.deepcopy(plan or MAIN_PLAN)

    def total(p):
        t = 0.0
        rows = []
        for cell in p:
            fams = cell.get("families", FAMILIES)
            n_cost = 0.0
            runs = 0
            for family in fams:
                n = cell["tasks_per_family"] * cell["reps"] * len(cell["arms"])
                n_cost += n * per_run[cell["model"]][family] * cell.get("anchor_overhead", 1.0)
                runs += n
            rows.append({"cell": cell["name"], "model": cell["model"], "runs": runs, "usd": round(n_cost, 2)})
            t += n_cost
        return t, rows

    applied = []
    expected, rows = total(plan)
    limit = enforced_limit(budget, use_reserve)
    room = limit - spent
    if expected > room:
        for name, change in SCALE_DOWN:
            for cell in plan:
                if cell["name"] == name:
                    cell.update(change)
            applied.append("down:%s %s" % (name, change))
            expected, rows = total(plan)
            if expected <= room:
                break
    elif expected < 70.0 - spent:
        for name, change in SCALE_UP:
            trial = copy.deepcopy(plan)
            if name == "sonnet-ce":
                trial.append(dict(change, name="sonnet-ce"))
            else:
                for cell in trial:
                    if cell["name"] == name:
                        cell.update(change)
            t, _ = total(trial)
            if t <= 70.0 - spent:
                plan = trial
                applied.append("up:%s" % name)
        expected, rows = total(plan)
    return {"expected_usd": round(expected, 2), "enforced_limit_usd": round(limit, 2), "spent_usd": round(spent, 4),
            "room_usd": round(room, 2), "rows": rows, "applied": applied, "fits": expected <= room,
            "reserve_released": bool(use_reserve)}
