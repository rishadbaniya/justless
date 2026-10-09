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
        """Upper bound for the next request: the whole prompt uncached at the top tier plus max output."""
        tier = self.table["models"][model]["tiers"][-1]
        return (prompt_tokens * tier["input"] + max_tokens * tier["output"]) / MTOK

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


class Ledger:
    """Append-only spend ledger shared by every worker in a campaign directory."""

    def __init__(self, path, cap_usd, reserve_usd=config.RESERVE_USD, use_reserve=False, prior_usd=0.0,
                 global_cap_usd=None):
        """cap_usd bounds this campaign; prior_usd is what other paid campaigns already spent, checked
        together with this one against global_cap_usd (the $90 campaign total, its last $10 behind
        use_reserve)."""
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.cap = float(cap_usd)
        self.reserve = float(reserve_usd)
        self.use_reserve = use_reserve
        self.prior = float(prior_usd)
        self.global_cap = None if global_cap_usd is None else float(global_cap_usd)
        self.lock = threading.Lock()
        self.spent = 0.0
        if self.path.exists():
            with open(self.path) as handle:
                for line in handle:
                    try:
                        self.spent += float(json.loads(line).get("cost_usd") or 0)
                    except ValueError:
                        pass

    @property
    def limit(self):
        return self.cap if self.use_reserve else self.cap - self.reserve

    def check(self, worst_case_usd):
        with self.lock:
            if self.spent + worst_case_usd > self.limit:
                raise BudgetExceeded("aborted_global_cap", "spent %.4f + next %.4f > %.4f" % (
                    self.spent, worst_case_usd, self.limit))
            if self.global_cap is not None:
                limit = self.global_cap if self.use_reserve else self.global_cap - config.RESERVE_USD
                if self.prior + self.spent + worst_case_usd > limit:
                    raise BudgetExceeded("aborted_global_cap", "all campaigns %.4f + next %.4f > %.4f" % (
                        self.prior + self.spent, worst_case_usd, limit))

    def record(self, entry):
        with self.lock:
            self.spent += float(entry.get("cost_usd") or 0)
            entry = dict(entry, ledger_total=round(self.spent, 6), t=time.time())
            with open(self.path, "a") as handle:
                handle.write(json.dumps(entry) + "\n")
                handle.flush()
                os.fsync(handle.fileno())


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


def estimate(per_run, spent=0.0, budget=config.CAMPAIGN_CAP_USD, plan=None):
    """Apply the pre-registered scale orders to a per-run cost table. Returns a dict for printing."""
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
    room = budget - spent
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
    return {"expected_usd": round(expected, 2), "room_usd": round(room, 2), "rows": rows, "applied": applied,
            "fits": expected <= room}
