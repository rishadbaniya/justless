"""Pre-registered statistics. Seeded and stdlib-only.

Unit of analysis: a paired (model, task, rep) block. Primary endpoints for arm X vs arm Y:
  E1  delta_s = mean over blocks of (success_X - success_Y)
  E2  S = 1 - [(sum cost_X / sum succ_X) / (sum cost_Y / sum succ_Y)]
CIs come from a task-cluster bootstrap (resample tasks, keep all reps of each).
"""

import math
import random

import config

B_DEFAULT = 10000
NI_MARGIN = -0.10
POINT_FLOOR = -0.05


EXCLUDED_MODES = ("harness_error", "aborted_global_cap", "paused_infra")
PIN_KEYS = ("justless_sha256", "harness_tree", "harness_commit", "capture_hash", "manifest_hash", "pricing_hash")


def pin_key(row):
    """The pins two arms of a block must share: binary, harness, capture, tasks, prices and effort. Rows from
    before harness_tree existed fall back to harness_commit."""
    pins = row.get("pins") or {}
    harness = pins.get("harness_tree") or pins.get("harness_commit")
    return (pins.get("justless_sha256"), harness, pins.get("capture_hash"), pins.get("manifest_hash"),
            pins.get("pricing_hash"), row.get("effort_setting") or row.get("effort"))


def pairs(rows, arm_x, arm_y, model=None, families=None, agent="harness"):
    """{task_id: [(row_x, row_y), ...]} for complete (task, rep) blocks of one agent (the harness by
    default; the real Claude Code anchor is reported separately). Arms pair only within one pin set, so a
    block is never completed by a run from another binary, harness, capture, task manifest or effort."""
    cells = {}
    for row in rows:
        if (row.get("agent") or "harness") != agent:
            continue
        if model and row.get("model") != model:
            continue
        if families and row.get("family") not in families:
            continue
        if row.get("failure_mode") in EXCLUDED_MODES:
            continue
        key = (row["task_id"], row.get("rep"), pin_key(row))
        cells.setdefault(key, {})[row["arm"]] = row
    out = {}
    for (task, _, _), arms in sorted(cells.items(), key=lambda kv: (kv[0][0], kv[0][1] or 0, str(kv[0][2]))):
        if arm_x in arms and arm_y in arms:
            out.setdefault(task, []).append((arms[arm_x], arms[arm_y]))
    return out


def pin_sets(rows, model=None, agent="harness"):
    """{pin_key: number of rows} over one agent's rows (and one model's, if given)."""
    out = {}
    for row in rows:
        if (row.get("agent") or "harness") != agent or (model and row.get("model") != model):
            continue
        if row.get("failure_mode") in EXCLUDED_MODES:
            continue
        key = pin_key(row)
        out[key] = out.get(key, 0) + 1
    return out


def cost(row):
    return float((row.get("totals") or {}).get("cost_usd") or 0.0)


def success(row):
    return 1.0 if row.get("success") else 0.0


def estimates(clusters):
    blocks = [p for task in clusters for p in clusters[task]]
    if not blocks:
        return None
    ds = sum(success(x) - success(y) for x, y in blocks) / len(blocks)
    cx = sum(cost(x) for x, _ in blocks)
    cy = sum(cost(y) for _, y in blocks)
    sx = sum(success(x) for x, _ in blocks)
    sy = sum(success(y) for _, y in blocks)
    s = None
    if sx > 0 and sy > 0 and cy > 0:
        s = 1.0 - (cx / sx) / (cy / sy)
    r = 1.0 - cx / cy if cy > 0 else None
    return {"n_blocks": len(blocks), "n_tasks": len(clusters), "delta_s": ds, "S": s, "R": r,
            "success_x": sx / len(blocks), "success_y": sy / len(blocks),
            "cost_per_success_x": cx / sx if sx else None, "cost_per_success_y": cy / sy if sy else None,
            "cost_per_run_x": cx / len(blocks), "cost_per_run_y": cy / len(blocks)}


def percentile(values, q):
    values = sorted(values)
    if not values:
        return None
    k = (len(values) - 1) * q
    lo = int(math.floor(k))
    hi = int(math.ceil(k))
    if lo == hi:
        return values[lo]
    return values[lo] + (values[hi] - values[lo]) * (k - lo)


def bootstrap(clusters, B=B_DEFAULT, seed=config.SEED, stat=None):
    """Task-cluster bootstrap. Returns CIs for delta_s and S plus the share of undefined S resamples."""
    tasks = sorted(clusters)
    rng = random.Random(seed)
    ds_values = []
    s_values = []
    r_values = []
    undefined = 0
    for _ in range(B):
        sample = {}
        for i in range(len(tasks)):
            task = tasks[rng.randrange(len(tasks))]
            sample["%s#%d" % (task, i)] = clusters[task]
        est = estimates(sample)
        if est is None:
            continue
        ds_values.append(est["delta_s"])
        if est["R"] is not None:
            r_values.append(est["R"])
        if est["S"] is None:
            undefined += 1
        else:
            s_values.append(est["S"])
    out = {"B": B, "delta_s_ci": [percentile(ds_values, 0.025), percentile(ds_values, 0.975)],
           "S_undefined_share": undefined / float(B) if B else 0.0,
           # The pre-registered fallback when S is undefined: the relative saving in cost per run.
           "R_ci": [percentile(r_values, 0.025), percentile(r_values, 0.975)] if r_values else None}
    if out["S_undefined_share"] > 0.01 or not s_values:
        out["S_ci"] = None
    else:
        out["S_ci"] = [percentile(s_values, 0.025), percentile(s_values, 0.975)]
    return out


def cost_endpoint(est, boot):
    """(name, point, ci): S, or, when S is undefined in more than 1% of resamples, the pre-registered fallback R,
    the relative saving in cost per run."""
    if boot.get("S_ci"):
        return "cost per successful task", est.get("S"), boot["S_ci"]
    if boot.get("R_ci") and boot["R_ci"][0] is not None:
        return "cost per run (S undefined)", est.get("R"), boot["R_ci"]
    return "cost", None, None


def decide(est, boot):
    """Decision rule with precedence: WORSE, WIN, CHEAPER-BUT-SUCCESS-UNCERTAIN, NO DEMONSTRATED SAVING.
    The cost endpoint is S, or R (cost per run) when S is undefined."""
    if est is None:
        return "NO DATA", None
    name, point, c_ci = cost_endpoint(est, boot)
    ds_lo, ds_hi = boot["delta_s_ci"]
    if (c_ci and c_ci[1] < 0) or ds_hi < NI_MARGIN:
        verdict = "WORSE"
    elif c_ci and c_ci[0] > 0 and ds_lo > NI_MARGIN and est["delta_s"] >= POINT_FLOOR:
        verdict = "WIN"
    elif c_ci and c_ci[0] > 0:
        verdict = "CHEAPER-BUT-SUCCESS-UNCERTAIN"
    else:
        verdict = "NO DEMONSTRATED SAVING"
    wording = None
    if verdict == "WIN":
        wording = ("Justless reduced %s by %s [%s, %s] while success was not worse by more than 10 percentage "
                   "points (%s [%s, %s])." % (name, pct(point), pct(c_ci[0]), pct(c_ci[1]), pp(est["delta_s"]),
                                              pp(ds_lo), pp(ds_hi)))
    elif verdict == "CHEAPER-BUT-SUCCESS-UNCERTAIN":
        wording = ("Justless reduced %s by %s [%s, %s]. Success was not shown to be within 10 percentage points "
                   "(%s [%s, %s]); at this sample size that is not evidence that success fell." % (
                       name, pct(point), pct(c_ci[0]), pct(c_ci[1]), pp(est["delta_s"]), pp(ds_lo), pp(ds_hi)))
    return verdict, wording


def pct(x):
    return "n/a" if x is None else "%.1f%%" % (100 * x)


def pp(x):
    return "n/a" if x is None else "%+.1f pp" % (100 * x)


def geometric_ratio(clusters, metric, B=2000, seed=config.SEED):
    """Paired geometric-mean ratio X/Y of a positive metric with a cluster-bootstrap CI."""
    def gm(sample):
        logs = []
        for task in sample:
            for x, y in sample[task]:
                a, b = metric(x), metric(y)
                if a and b and a > 0 and b > 0:
                    logs.append(math.log(a / b))
        return math.exp(sum(logs) / len(logs)) if logs else None

    point = gm(clusters)
    tasks = sorted(clusters)
    rng = random.Random(seed)
    values = []
    for _ in range(B):
        sample = {}
        for i in range(len(tasks)):
            t = tasks[rng.randrange(len(tasks))]
            sample["%s#%d" % (t, i)] = clusters[t]
        v = gm(sample)
        if v is not None:
            values.append(v)
    return {"ratio": point, "ci": [percentile(values, 0.025), percentile(values, 0.975)] if values else None}


def mcnemar_exact(clusters):
    """Exact two-sided McNemar on block-level success discordance (ignores clustering)."""
    b = c = 0
    for task in clusters:
        for x, y in clusters[task]:
            if success(x) and not success(y):
                b += 1
            elif success(y) and not success(x):
                c += 1
    n = b + c
    if n == 0:
        return {"b": b, "c": c, "p": 1.0}
    k = min(b, c)
    p = sum(math.comb(n, i) for i in range(0, k + 1)) / float(2 ** n) * 2
    return {"b": b, "c": c, "p": min(1.0, p)}


def signflip_p(clusters, statistic="cost", draws=10000, seed=config.SEED):
    """Paired sign-flip permutation p-value, flipping whole task clusters."""
    tasks = sorted(clusters)

    def per_task(task, sign):
        diffs = []
        for x, y in clusters[task]:
            if statistic == "cost":
                a, b = cost(x), cost(y)
                diffs.append(math.log(a / b) if a > 0 and b > 0 else 0.0)
            else:
                diffs.append(success(x) - success(y))
        return sign * sum(diffs) / len(diffs)

    observed = sum(per_task(t, 1) for t in tasks) / len(tasks) if tasks else 0.0
    rng = random.Random(seed)
    extreme = 0
    for _ in range(draws):
        value = sum(per_task(t, 1 if rng.random() < 0.5 else -1) for t in tasks) / len(tasks)
        if abs(value) >= abs(observed) - 1e-12:
            extreme += 1
    return (extreme + 1) / float(draws + 1)


def holm(pvalues):
    """Holm step-down adjustment of {name: p}."""
    items = sorted(pvalues.items(), key=lambda kv: kv[1])
    m = len(items)
    adjusted = {}
    running = 0.0
    for i, (name, p) in enumerate(items):
        running = max(running, min(1.0, (m - i) * p))
        adjusted[name] = running
    return adjusted


LIMIT_MODES = {"turn_limit", "token_limit", "run_budget", "wall_timeout"}


def limit_hit(row):
    """Whether a run stopped on a limit. Runs record stopped_by; older rows carry the limit as failure_mode."""
    if "limit_hit" in row:
        return bool(row["limit_hit"])
    return row.get("stopped_by") in LIMIT_MODES or row.get("failure_mode") in LIMIT_MODES


def censored(clusters):
    """Sensitivity: drop blocks where any arm hit a limit."""
    out = {}
    for task, blocks in clusters.items():
        kept = [(x, y) for x, y in blocks if not limit_hit(x) and not limit_hit(y)]
        if kept:
            out[task] = kept
    return out


def fidelity_flag(anchor_ratio, harness_ci, anchor_rates, harness_rates, anchor_plain_cost, harness_plain_cost):
    reasons = []
    if anchor_ratio is not None and harness_ci and not (harness_ci[0] <= anchor_ratio <= harness_ci[1]):
        reasons.append("anchor b/a cost ratio outside the harness CI")
    for name, value in (anchor_rates or {}).items():
        other = (harness_rates or {}).get(name)
        if value and other and (value / other > 2 or other / value > 2):
            reasons.append("hook rate %s differs more than 2x" % name)
    if anchor_plain_cost and harness_plain_cost:
        if abs(anchor_plain_cost - harness_plain_cost) / harness_plain_cost > 0.35:
            reasons.append("plain-arm cost per run differs more than 35%")
    return bool(reasons), reasons


def discordance_and_icc(clusters):
    """Measured share of discordant blocks and the intra-task correlation of the paired success difference
    (one-way ANOVA estimator, clamped to [0, 1]), for recomputing the MDE after the pilot."""
    blocks = [(t, success(x) - success(y)) for t in clusters for x, y in clusters[t]]
    if not blocks:
        return None, None
    discordant = sum(1 for _, d in blocks if d) / float(len(blocks))
    k = len(clusters)
    n = len(blocks)
    mean = sum(d for _, d in blocks) / float(n)
    groups = {t: [success(x) - success(y) for x, y in clusters[t]] for t in clusters}
    ssb = sum(len(g) * (sum(g) / float(len(g)) - mean) ** 2 for g in groups.values())
    ssw = sum((d - sum(g) / float(len(g))) ** 2 for g in groups.values() for d in g)
    if k < 2 or n <= k:
        return discordant, None
    msb, msw = ssb / (k - 1), ssw / (n - k)
    m = n / float(k)
    denom = msb + (m - 1) * msw
    icc = (msb - msw) / denom if denom > 0 else 0.0
    return discordant, max(0.0, min(1.0, icc))


def null_success_power(clusters, sims=40, B=300, seed=config.SEED):
    """Share of simulated campaigns of this size in which the success conditions of WIN hold for a tool with no
    effect on success: each task keeps its pooled success rate in both arms and blocks are redrawn
    independently. Costs are left out, so this is the success side of the rule only."""
    rng = random.Random(seed)
    rates = {}
    for task, blocks in clusters.items():
        values = [success(r) for pair in blocks for r in pair]
        rates[task] = sum(values) / float(len(values))
    met = 0
    widths = []
    for _ in range(sims):
        sim = {}
        for task, blocks in clusters.items():
            sim[task] = [({"success": rng.random() < rates[task]}, {"success": rng.random() < rates[task]})
                         for _ in blocks]
        est = estimates(sim)
        boot = bootstrap(sim, B=B, seed=rng.randrange(1 << 30))
        lo, hi = boot["delta_s_ci"]
        widths.append((hi - lo) / 2.0)
        if lo > NI_MARGIN and est["delta_s"] >= POINT_FLOOR:
            met += 1
    return {"sims": sims, "B": B, "success_conditions_met": met / float(sims),
            "median_half_width": percentile(widths, 0.5)}


def mde(n_pairs, sd_log_ratio=0.6, icc=0.3, reps=3, discordant=0.2, z_alpha=1.96, z_beta=0.8416):
    """Planning MDE for the relative cost change and the success difference."""
    deff = 1 + (reps - 1) * icc
    n_eff = n_pairs / deff
    cost_mde = 1 - math.exp(-(z_alpha + z_beta) * sd_log_ratio / math.sqrt(n_eff))
    success_mde = (z_alpha + z_beta) * math.sqrt(discordant / n_eff)
    return {"n_pairs": n_pairs, "design_effect": deff, "n_eff": n_eff, "cost_mde": cost_mde,
            "success_mde": success_mde}


def contrast(rows, arm_x, arm_y, model, families=None, B=B_DEFAULT, power=False):
    clusters = pairs(rows, arm_x, arm_y, model, families)
    est = estimates(clusters)
    if est is None:
        return {"arms": [arm_x, arm_y], "model": model, "n_blocks": 0}
    sets = pin_sets([r for r in rows if r.get("arm") in (arm_x, arm_y)], model)
    if len(sets) > 1:
        # Runs from different binaries, harness trees, captures, manifests or efforts are never pooled.
        return {"arms": [arm_x, arm_y], "model": model, "families": families, "estimate": est,
                "verdict": "MIXED PINS", "wording": None, "pin_sets": [list(k) + [n] for k, n in sorted(
                    sets.items(), key=lambda kv: str(kv[0]))]}
    boot = bootstrap(clusters, B)
    verdict, wording = decide(est, boot)
    discordant, icc = discordance_and_icc(clusters)
    lo, hi = boot["delta_s_ci"]
    out = {"arms": [arm_x, arm_y], "model": model, "families": families, "estimate": est, "bootstrap": boot,
           "verdict": verdict, "wording": wording, "mcnemar": mcnemar_exact(clusters),
           "power": {"delta_s_half_width": None if lo is None else (hi - lo) / 2.0, "margin": -NI_MARGIN,
                     "discordant_share": discordant, "icc": icc,
                     "mde": mde(est["n_blocks"], icc=icc if icc is not None else 0.3,
                                reps=max(1.0, est["n_blocks"] / float(est["n_tasks"])),
                                discordant=discordant if discordant else 0.2)}}
    if power:
        out["power"]["null"] = null_success_power(clusters)
    return out
