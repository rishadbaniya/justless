"""Markdown report and summary.json for a campaign. Usage: report.py --campaign NAME [--dry]."""

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import accounting  # noqa: E402
import config  # noqa: E402
import stats  # noqa: E402
import tasks as tasks_module  # noqa: E402

FAMILIES = ["bugfix", "lint", "location", "impact", "large"]
PRIMARY_MODEL = "claude-sonnet-5-5"
CE_MODEL = "claude-haiku-5-5"
FAILURE_MODES = ["success", "wrong_fix", "untouched_site", "build_broken", "test_tamper", "lint_remaining",
                 "lint_suppressed", "wrong_answer", "partial_answer", "no_answer", "turn_limit", "token_limit",
                 "run_budget", "wall_timeout", "max_tokens", "refusal", "harness_unrecoverable", "harness_error",
                 "aborted_global_cap"]


def read_jsonl(path):
    if not Path(path).exists():
        return []
    out = []
    with open(path) as handle:
        for line in handle:
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    return out


def money(x):
    return "n/a" if x is None else "$%.4f" % x


def num(x):
    return "n/a" if x is None else "{:,}".format(int(round(x)))


def pct(x):
    return "n/a" if x is None else "%.1f%%" % (100.0 * x)


def ci(pair, fmt=pct):
    if not pair or pair[0] is None:
        return "[n/a]"
    return "[%s, %s]" % (fmt(pair[0]), fmt(pair[1]))


def median_iqr(values):
    values = sorted(v for v in values if v is not None)
    if not values:
        return "n/a"
    q = stats.percentile
    return "%s [%s–%s]" % (num(q(values, 0.5)), num(q(values, 0.25)), num(q(values, 0.75)))


def median_iqr_money(values):
    values = sorted(v for v in values if v is not None)
    if not values:
        return "n/a"
    q = stats.percentile
    return "%s [%s–%s]" % (money(q(values, 0.5)), money(q(values, 0.25)), money(q(values, 0.75)))


def final_rows(rows):
    """The latest record per run_id, then one record per (agent, model, effort, arm, task, rep, pins): a later
    record of the same block and pins replaces an earlier one instead of being counted twice."""
    latest = {}
    for row in rows:
        key = row.get("run_id")
        if key is None:
            continue
        if key not in latest or row.get("attempt", 1) >= latest[key].get("attempt", 1):
            latest[key] = row
    blocks = {}
    for row in latest.values():
        key = (row.get("agent") or "harness", row.get("model"), row.get("effort_setting") or row.get("effort"),
               row.get("arm"), row.get("task_id"), row.get("rep"), stats.pin_key(row))
        blocks[key] = row
    return list(blocks.values())


def harness_only(rows):
    """Rows of harness runs that ended in a graded or terminal state: no anchor runs, no harness errors, cap
    aborts or infrastructure pauses (those are re-run and reported in section 1)."""
    return [r for r in rows if (r.get("agent") or "harness") == "harness"
            and r.get("failure_mode") not in stats.EXCLUDED_MODES]


def first_request_check(rows, pairs_of):
    """Compare the saved first request of the two arms of every block outside the regions arms may differ in
    (the documented first-request check, run on paid runs)."""
    import dry_checks
    checked = 0
    failures = []
    for (x_arm, y_arm), clusters in pairs_of.items():
        allow_ce = config.ARMS[x_arm]["ce"] != config.ARMS[y_arm]["ce"]
        for task, blocks in clusters.items():
            for x, y in blocks:
                bodies = []
                for row in (x, y):
                    path = Path(row.get("transcript") or "").parent / "first_request.json"
                    bodies.append(json.loads(path.read_text()) if row.get("transcript") and path.exists() else None)
                if None in bodies:
                    continue
                checked += 1
                problems = dry_checks.compare_pair(bodies[0], bodies[1], allow_ce)
                if problems:
                    failures.append("%s %s rep %s %s/%s: %s" % (x.get("model"), task, x.get("rep"), x_arm, y_arm,
                                                                "; ".join(problems)))
    return checked, failures


def repriced_1h(rows, requests, pricing):
    """Rows with cost re-priced at the 1-hour cache-write rate from the logged usage of their own requests."""
    model_of = {(r.get("run_id"), r.get("attempt")): r.get("model") for r in rows}
    by_run = {}
    for q in requests:
        key = (q.get("run_id"), q.get("attempt"))
        if model_of.get(key) in config.MODELS:
            by_run[key] = by_run.get(key, 0.0) + pricing.reprice_1h(model_of[key], q.get("usage") or {})
    out = []
    for row in rows:
        key = (row.get("run_id"), row.get("attempt"))
        if key in by_run:
            row = dict(row, totals=dict(row.get("totals") or {}, cost_usd=by_run[key]))
        out.append(row)
    return out


def arm_table(rows, model):
    lines = ["| Arm | Runs | Success | Cost per success | Cost per run | Prompt tokens | Uncached / write / read "
             "(sum) | Output tokens | Turns | Bash / rewritten / justless typed / search | Read / outlined / partial "
             "/ allow_large | Edit / Write | Tool bytes seen | Wall s | TTL rewrites | Failure modes |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | "
             "---: | --- |"]
    for arm in sorted(config.ARMS):
        cell = [r for r in rows if r.get("model") == model and r.get("arm") == arm]
        if not cell:
            continue
        totals = [r.get("totals") or {} for r in cell]
        succ = sum(1 for r in cell if r.get("success"))
        cost_sum = sum(t.get("cost_usd", 0) for t in totals)
        counts = {}
        for r in cell:
            for k, v in (r.get("tool_counts") or {}).items():
                counts[k] = counts.get(k, 0) + v
        modes = {}
        for r in cell:
            modes[r.get("failure_mode")] = modes.get(r.get("failure_mode"), 0) + 1
        lines.append("| %s (%s) | %d | %d/%d (%s) | %s | %s | %s | %s / %s / %s | %s | %s | %d / %d / %d / %d | "
                     "%d / %d / %d / %d | %d / %d | %s | %s | %d | %s |" % (
                         arm, config.ARMS[arm]["name"], len(cell), succ, len(cell), pct(succ / float(len(cell))),
                         money(cost_sum / succ) if succ else "n/a",
                         median_iqr_money([t.get("cost_usd") for t in totals]),
                         median_iqr([t.get("prompt_tokens") for t in totals]),
                         num(sum(t.get("input_tokens", 0) for t in totals)),
                         num(sum(t.get("cache_creation_input_tokens", 0) for t in totals)),
                         num(sum(t.get("cache_read_input_tokens", 0) for t in totals)),
                         median_iqr([t.get("output_tokens") for t in totals]),
                         median_iqr([t.get("requests") for t in totals]),
                         counts.get("Bash", 0), counts.get("Bash:rewritten", 0), counts.get("Bash:justless_typed", 0),
                         counts.get("Bash:search", 0), counts.get("Read", 0), counts.get("Read:outlined", 0),
                         counts.get("Read:partial_view", 0), counts.get("Read:allow_large", 0),
                         counts.get("Edit", 0), counts.get("Write", 0),
                         median_iqr([t.get("tool_bytes_seen") for t in totals]),
                         median_iqr([t.get("wall_s") for t in totals]),
                         sum(t.get("ttl_expiry_rewrites", 0) for t in totals),
                         ", ".join("%s %d" % (k, v) for k, v in sorted(modes.items()))))
    return lines


def contrast_section(rows, x, y, model, families=None, B=stats.B_DEFAULT, power=False):
    result = stats.contrast(rows, x, y, model, families, B, power=power)
    if not result.get("estimate"):
        return result, ["No complete %s/%s blocks for %s." % (x, y, model)]
    if result["verdict"] == "MIXED PINS":
        return result, ["**MIXED PINS: %s/%s on %s has runs from %d pin sets; no contrast is computed.** Split the "
                        "campaign or re-run under one set of pins. Pin sets (binary, harness, capture, manifest, "
                        "pricing, effort, rows): %s" % (x, y, model, len(result["pin_sets"]),
                                                         json.dumps(result["pin_sets"]))]
    est = result["estimate"]
    boot = result["bootstrap"]
    lines = [
        "| Measure | %s | %s |" % (x, y), "| --- | ---: | ---: |",
        "| Success | %s | %s |" % (pct(est["success_x"]), pct(est["success_y"])),
        "| Cost per successful task | %s | %s |" % (money(est["cost_per_success_x"]), money(est["cost_per_success_y"])),
        "| Cost per run | %s | %s |" % (money(est["cost_per_run_x"]), money(est["cost_per_run_y"])),
        "",
        "- Blocks: %d over %d tasks. Δs = %s %s. S = %s %s%s." % (
            est["n_blocks"], est["n_tasks"], stats.pp(est["delta_s"]), ci(boot["delta_s_ci"], stats.pp),
            pct(est["S"]), ci(boot.get("S_ci")),
            "" if boot.get("S_ci") else " (S undefined in %s of resamples; use cost per run)" % pct(
                boot["S_undefined_share"])),
        "- Verdict under the pre-registered rule: **%s**.%s" % (
            result["verdict"], (" " + result["wording"]) if result["wording"] else ""),
        "- Exact McNemar (ignores clustering): b=%d, c=%d, p=%.3f." % (
            result["mcnemar"]["b"], result["mcnemar"]["c"], result["mcnemar"]["p"]),
    ]
    if not boot.get("S_ci"):
        lines.append("- Cost endpoint: S is undefined, so the rule used R, the saving in cost per run: %s %s." % (
            pct(est.get("R")), ci(boot.get("R_ci"))))
    if boot["delta_s_ci"][0] is not None and (boot["delta_s_ci"][0] > 0 or boot["delta_s_ci"][1] < 0):
        lines.append("- **The Δs CI excludes 0.**")
    power_info = result.get("power") or {}
    if power_info.get("delta_s_half_width") is not None:
        mde = power_info.get("mde") or {}
        line = ("- Power: the Δs CI half-width is %s against the 10-point margin; success MDE %s at the measured "
                "discordance %s and ICC %s." % (
                    stats.pp(power_info["delta_s_half_width"]).lstrip("+"), pct(mde.get("success_mde")),
                    pct(power_info.get("discordant_share")),
                    "n/a" if power_info.get("icc") is None else "%.2f" % power_info["icc"]))
        null = power_info.get("null")
        if null:
            line += (" A tool with no effect on success meets the success conditions of WIN in %s of %d simulated "
                     "campaigns of this size." % (pct(null["success_conditions_met"]), null["sims"]))
        if power_info["delta_s_half_width"] >= 0.10:
            line += " **WIN was out of reach for the success endpoint at this sample size.**"
        lines.append(line)
    return result, lines


def build(home, campaign, dry=False, B=stats.B_DEFAULT, task_set="main"):
    base = Path(home) / "campaigns" / campaign
    all_rows = final_rows(read_jsonl(base / "results.jsonl"))
    rows = harness_only(all_rows)
    anchor_rows = [r for r in all_rows if r.get("agent") == "claude-code"]
    attempts = read_jsonl(base / "results.jsonl")
    requests = read_jsonl(base / "requests.jsonl")
    tools = read_jsonl(base / "tools.jsonl")
    ledger = read_jsonl(base / "ledger.jsonl")
    out = []
    summary = {"campaign": campaign, "dry_run": dry, "runs": len(rows), "anchor_runs": len(anchor_rows)}
    if dry:
        out += ["> **DRY RUN - fake model, numbers meaningless.**", ""]
    out += ["# Agent benchmark report: %s" % campaign, "",
            "## 0. What was measured", "",
            "The harness reimplements Claude Code 2.1.327's default macOS setup over the Messages API: Bash, Read, "
            "Edit and Write, Claude Code's own system prompt and tool text, its output limits and its hook contract, "
            "parity-tested against real Claude Code. It has no subagents, no Grep or Glob tools and no compaction. "
            "Byte counts are bytes; token counts are provider-reported. `justless mask` and `recall` were not "
            "exercised. Justless's Grep|Glob hook cannot fire in this configuration.", ""]
    pins = (rows[0].get("pins") if rows else {}) or {}
    harness_errors = {}
    for row in attempts:
        if row.get("failure_mode") in ("harness_error", "harness_unrecoverable", "aborted_global_cap",
                                       "paused_infra"):
            key = (row.get("arm"), row["failure_mode"])
            harness_errors[key] = harness_errors.get(key, 0) + 1
    sets = {}
    for row in all_rows:
        key = ((row.get("agent") or "harness"), row.get("model")) + stats.pin_key(row)
        sets[key] = sets.get(key, 0) + 1
    frozen_path = tasks_module.private_dir(home, task_set) / "_manifest.json"
    frozen = json.loads(frozen_path.read_text()).get("hash") if frozen_path.exists() else None
    manifests = sorted({str((r.get("pins") or {}).get("manifest_hash")) for r in all_rows})
    manifest_ok = bool(frozen) and manifests == [frozen]
    if (campaign == "main" or task_set != "main") and not dry and all_rows and not manifest_ok:
        raise SystemExit("campaign main ran on task manifest(s) %s, not the frozen manifest %s; refusing to report"
                         % (", ".join(manifests), frozen))
    out += ["## 1. Manifest", ""] + ["- %s: `%s`" % (k, v) for k, v in sorted(pins.items())] + [
        "- frozen task manifest: `%s`; manifests in this campaign: %s%s" % (
            frozen, ", ".join("`%s`" % m for m in manifests) or "none",
            "" if manifest_ok or not all_rows else " **(differs from the frozen manifest)**"),
        "- pin sets (agent, model, binary, harness, capture, manifest, pricing, effort: runs):"] + [
        "  - %s: %d" % (" / ".join(str(x)[:16] for x in key), n) for key, n in sorted(sets.items(), key=str)] + [
        "- runs (terminal harness records): %d; anchor runs: %d; attempts: %d" % (len(rows), len(anchor_rows),
                                                                                 len(attempts)),
        "- harness errors, cap aborts and pauses by arm: %s" % (", ".join("%s %s=%d" % (a, m, n) for (a, m), n in sorted(
            harness_errors.items())) or "none"), ""]
    summary["pins"] = pins
    summary["pin_sets"] = [list(k) + [n] for k, n in sorted(sets.items(), key=str)]
    summary["frozen_manifest"] = {"frozen": frozen, "campaign": manifests, "ok": manifest_ok}
    summary["harness_errors"] = {"%s:%s" % k: v for k, v in harness_errors.items()}

    out += ["## 2. Pre-registered primary result (%s, b vs a)" % PRIMARY_MODEL, ""]
    primary, lines = contrast_section(rows, "b", "a", PRIMARY_MODEL, B=B, power=True)
    out += lines + [""]
    summary["primary"] = primary
    edit, lines = contrast_section(rows, "b", "a", PRIMARY_MODEL, ["bugfix", "lint"], B=B)
    out += ["### Key secondary: edit families (bug fix + lint)", ""] + lines + [""]
    summary["edit_families"] = edit
    per_family = {}
    negative = []
    for family in [f for f in FAMILIES if any(r.get("family") == f for r in rows)]:
        result, lines = contrast_section(rows, "b", "a", PRIMARY_MODEL, [family], B=min(B, 2000))
        per_family[family] = result
        out += ["### %s" % family, ""] + lines + [""]
        est = result.get("estimate") or {}
        if est.get("S") is not None and est["S"] < 0:
            negative.append(family)
    if negative:
        out += ["**Per-family S point estimate is negative for: %s.**" % ", ".join(negative), ""]
    summary["per_family"] = per_family

    out += ["## 3. Fidelity anchor", ""]
    anchor = [r for r in anchor_rows if r.get("failure_mode") not in stats.EXCLUDED_MODES]
    if anchor:
        out += ["| Arm | Runs | Success | Cost per run | Limit hits | CLAUDE.md seen |", "| --- | ---: | ---: | ---: | ---: | ---: |"]
        for arm in ("a", "b"):
            cell = [r for r in anchor if r["arm"] == arm]
            if cell:
                out.append("| %s | %d | %d/%d | %s | %d | %d |" % (
                    arm, len(cell), sum(1 for r in cell if r.get("success")), len(cell),
                    money(sum(stats.cost(r) for r in cell) / len(cell)), sum(1 for r in cell if stats.limit_hit(r)),
                    sum(1 for r in cell if r.get("claude_md_seen"))))
    else:
        out.append("No anchor runs in this campaign.")
    out.append("")

    out += ["## 4. Context-editing contrasts on %s" % CE_MODEL, ""]
    ce = {}
    pvalues = {}
    for x, y in (("d", "c"), ("c", "a"), ("d", "a")):
        clusters = stats.pairs(rows, x, y, CE_MODEL)
        if clusters:
            pvalues["%s-%s" % (x, y)] = stats.signflip_p(clusters, "cost", draws=min(B, 2000))
    adjusted = stats.holm(pvalues) if pvalues else {}
    for x, y in (("d", "c"), ("c", "a"), ("d", "a")):
        result, lines = contrast_section(rows, x, y, CE_MODEL, B=min(B, 2000))
        ce["%s-%s" % (x, y)] = result
        out += ["### %s vs %s" % (x, y), ""] + lines
        if "%s-%s" % (x, y) in adjusted:
            out.append("- Holm-adjusted sign-flip p (cost): %.4f" % adjusted["%s-%s" % (x, y)])
        out.append("")
    out += ["Anthropic's guidance describes context editing as a context-window tool rather than a savings lever. "
            "Claims hold only for server-side clearing at this configuration.", ""]
    summary["context_editing"] = ce

    out += ["## 5. Per-model arm table", ""]
    for model in config.MODELS:
        if any(r.get("model") == model for r in rows):
            out += ["### %s" % model, ""] + arm_table(rows, model) + [""]

    out += ["## 6. Paired ratios", ""]
    ratios = {}
    for model in config.MODELS:
        for x, y in (("b", "a"), ("d", "c")):
            clusters = stats.pairs(rows, x, y, model)
            if not clusters:
                continue
            for name, metric in (("prompt_tokens", lambda r: (r.get("totals") or {}).get("prompt_tokens")),
                                 ("output_tokens", lambda r: (r.get("totals") or {}).get("output_tokens")),
                                 ("cost_per_run", stats.cost),
                                 ("turns", lambda r: (r.get("totals") or {}).get("requests")),
                                 ("wall_s", lambda r: (r.get("totals") or {}).get("wall_s"))):
                value = stats.geometric_ratio(clusters, metric, B=min(B, 1000))
                ratios["%s %s/%s %s" % (model, x, y, name)] = value
                out.append("- %s %s/%s %s: %s %s" % (model, x, y, name,
                                                    "n/a" if value["ratio"] is None else "%.3f" % value["ratio"],
                                                    ci(value["ci"], lambda v: "%.3f" % v)))
    out.append("")
    summary["ratios"] = ratios

    out += ["## 7. Sensitivity analyses", ""]
    sens = {}
    out.append("- Limit-hit rates (runs stopped by turns, tokens, run budget or wall time; every run is graded on its "
               "final state):")
    limit_rates = {}
    for model in config.MODELS:
        for arm in sorted(config.ARMS):
            cell = [r for r in rows if r.get("model") == model and r.get("arm") == arm]
            if cell:
                hits = sum(1 for r in cell if stats.limit_hit(r))
                limit_rates["%s %s" % (model, arm)] = hits / float(len(cell))
                out.append("  - %s arm %s: %d/%d (%s)" % (model, arm, hits, len(cell), pct(hits / float(len(cell)))))
    sens["limit_hit_rates"] = limit_rates
    clusters = stats.censored(stats.pairs(rows, "b", "a", PRIMARY_MODEL))
    est = stats.estimates(clusters)
    boot = stats.bootstrap(clusters, B=min(B, 2000)) if est else None
    sens["limits_censored"] = {"estimate": est, "bootstrap": boot}
    out.append("- Limit hits censored: %s" % ("n/a" if not est else "Δs %s %s, S %s %s (%d blocks)" % (
        stats.pp(est["delta_s"]), ci(boot["delta_s_ci"], stats.pp), pct(est["S"]), ci(boot.get("S_ci")),
        est["n_blocks"])))
    repriced = repriced_1h(rows, requests, accounting.Pricing())
    est = stats.estimates(stats.pairs(repriced, "b", "a", PRIMARY_MODEL))
    sens["cache_writes_1h"] = est
    out.append("- Cache writes re-priced at the 1-hour rate: %s" % ("n/a" if not est else "S %s" % pct(est["S"])))
    without_py = [r for r in rows if not (r.get("family") == "bugfix" and r.get("language") == "python")]
    est = stats.estimates(stats.pairs(without_py, "b", "a", PRIMARY_MODEL))
    sens["without_python"] = est
    out.append("- Excluding the Python task: %s" % ("n/a" if not est else "Δs %s, S %s" % (
        stats.pp(est["delta_s"]), pct(est["S"]))))
    for style in ("verbatim", "paraphrase"):
        subset = [r for r in rows if r.get("family") != "location" or r.get("style") == style]
        est = stats.estimates(stats.pairs(subset, "b", "a", PRIMARY_MODEL, ["location"]))
        sens["location_" + style] = est
        out.append("- Location %s: %s" % (style, "n/a" if not est else "success b %s vs a %s" % (
            pct(est["success_x"]), pct(est["success_y"]))))
    for solvable in (True, False):
        subset = [r for r in rows if r.get("family") != "location" or bool(r.get("grep_solvable")) == solvable]
        est = stats.estimates(stats.pairs(subset, "b", "a", PRIMARY_MODEL, ["location"]))
        sens["location_grep_solvable_%s" % solvable] = est
        out.append("- Location tasks %s with grep: %s" % ("solvable" if solvable else "not solvable",
                                                          "n/a" if not est else "success b %s vs a %s, S %s" % (
                                                              pct(est["success_x"]), pct(est["success_y"]),
                                                              pct(est["S"]))))
    out.append("- Impact tasks are grep-solvable by construction (uniquely named symbols; truth is the `rg -w` "
               "hits less comment-only mentions), so the impact family measures cost on lexical cases where "
               "semantic navigation cannot change success.")
    out.append("")
    summary["sensitivity"] = sens

    out += ["## 8. Per family and repository (exploratory)", "",
            "| Model | Family | Repo | Arm | Runs | Success | Cost per run |", "| --- | --- | --- | --- | ---: | ---: | ---: |"]
    groups = {}
    for r in rows:
        family = "large/%s" % r.get("kind") if r.get("family") == "large" else r.get("family")
        groups.setdefault((r.get("model"), family, r.get("repo"), r.get("arm")), []).append(r)
    flags = []
    for key in sorted(groups, key=lambda k: tuple(str(x) for x in k)):
        cell = groups[key]
        rate = sum(1 for r in cell if r.get("success")) / float(len(cell))
        out.append("| %s | %s | %s | %s | %d | %s | %s |" % (key + (len(cell), pct(rate),
                                                                   money(sum(stats.cost(r) for r in cell) / len(cell)))))
    for model in config.MODELS:
        for family in FAMILIES:
            plain = [r for r in rows if r.get("model") == model and r.get("family") == family and r.get("arm") == "a"]
            if plain:
                rate = sum(1 for r in plain if r.get("success")) / float(len(plain))
                if rate >= 0.95 or rate <= 0.10:
                    flags.append("%s %s plain success %s (%s)" % (model, family, pct(rate),
                                                                  "ceiling" if rate >= 0.95 else "floor"))
    out += [""] + ["- %s" % f for f in flags] + [""]

    out += ["## 9. Success-difference flags", ""]
    diffs = []
    for model in config.MODELS:
        threshold = 3 if model == CE_MODEL else 2
        for task, blocks in stats.pairs(rows, "b", "a", model).items():
            sb = sum(stats.success(x) for x, _ in blocks)
            sa = sum(stats.success(y) for _, y in blocks)
            if abs(sb - sa) >= threshold:
                diffs.append("%s %s: b %d vs a %d; transcripts %s" % (
                    model, task, sb, sa, ", ".join(x.get("transcript", "") for x, _ in blocks[:2])))
    out += (["- " + d for d in diffs] or ["None."]) + [""]

    out += ["## 9b. First-request check", ""]
    pairs_of = {}
    for model in config.MODELS:
        for x, y in (("b", "a"), ("d", "c"), ("c", "a"), ("d", "a")):
            clusters = stats.pairs(rows, x, y, model)
            if clusters:
                pairs_of.setdefault((x, y), {}).update({"%s %s" % (model, t): v for t, v in clusters.items()})
    checked, failures = first_request_check(rows, pairs_of)
    out.append("- paired blocks checked: %d; differing outside the CLAUDE.md block, the SessionStart context and "
               "context management: %d" % (checked, len(failures)))
    out += ["  - %s" % f for f in failures[:20]] + [""]
    summary["first_request_check"] = {"checked": checked, "failures": failures}

    out += ["## 10. Context-editing activity", ""]
    for arm in ("c", "d"):
        cell = [r for r in rows if r.get("arm") == arm]
        if cell:
            fired = [r for r in cell if (r.get("totals") or {}).get("ce_edits")]
            out.append("- arm %s: fired in %d/%d runs; cleared tool uses %d; cleared input tokens %s" % (
                arm, len(fired), len(cell), sum((r["totals"].get("ce_cleared_tool_uses") or 0) for r in cell),
                num(sum((r["totals"].get("ce_cleared_input_tokens") or 0) for r in cell))))
    out.append("")

    out += ["## 11. Justless diagnostics", ""]
    hooks = {}
    for r in rows:
        for event in r.get("hook_events") or []:
            key = "%s %s" % (event.get("event"), event.get("match"))
            item = hooks.setdefault(key, {"runs": 0, "applied": 0, "timeouts": 0, "errors": 0, "schema_rejected": 0})
            item["runs"] += 1
            item["applied"] += 1 if event.get("applied") else 0
            item["timeouts"] += 1 if event.get("timed_out") else 0
            item["errors"] += 1 if event.get("error") else 0
            item["schema_rejected"] += 1 if event.get("schema_rejected") else 0
    for key, item in sorted(hooks.items()):
        out.append("- hook %s: %s" % (key, json.dumps(item)))
    out.append("- Grep|Glob hook events: %d (the tools are absent on default macOS)" % sum(
        1 for t in tools for h in t.get("hooks") or [] if h.get("match") in ("Grep", "Glob")))
    rewritten_fail = [t for t in tools if t.get("executed_input") and t.get("is_error")]
    out.append("- rewritten commands that rendered as failures: %d" % len(rewritten_fail))
    typed = [t for t in tools if t.get("name") == "Bash" and str((t.get("input") or {}).get("command", ""))
             .strip().startswith("justless")]
    out.append("- Justless commands typed: %d (exit codes %s)" % (len(typed), sorted({t.get("exit_code") for t in typed},
                                                                                     key=str)))
    outlined_large = [t for t in tools if t.get("name") == "Read" and (t.get("input") or {}).get("allow_large")
                      and t.get("kind") == "outlined"]
    out.append("- allow_large reads that were outlined: %d" % len(outlined_large))
    renders = {}
    for t in tools:
        if t.get("render"):
            k = "%s:%s" % (t["name"], t["render"])
            renders[k] = renders.get(k, 0) + 1
    out.append("- renders: %s" % json.dumps(renders, sort_keys=True))
    out.append("- PARTIAL views passed through: %d" % sum(1 for t in tools if t.get("kind") == "partial_view"))
    out.append("- sandbox denials: %d" % sum(t.get("denials") or 0 for t in tools))
    out.append("- tool calls naming the Cargo git cache (possible answer-key access): %d in %d runs; outputs that "
               "mention it: %d" % (sum(1 for t in tools if t.get("cargo_cache_access")),
                                   sum(1 for r in rows if (r.get("totals") or {}).get("cargo_cache_access")),
                                   sum(1 for t in tools if t.get("cargo_cache_mention"))))
    changed = [r for r in rows if r.get("cargo_home_changed")]
    out.append("- runs after which ~/.cargo bin or config changed: %d%s" % (
        len(changed), (" (%s)" % ", ".join(sorted({k for r in changed for k in r["cargo_home_changed"]}))[:300])
        if changed else ""))
    deltas = []
    for model in config.MODELS:
        for task, blocks in stats.pairs(rows, "b", "a", model).items():
            for x, y in blocks:
                deltas.append((stats.cost(x) - stats.cost(y), model, task, x.get("transcript")))
    deltas.sort()
    out.append("- largest Justless wins: %s" % json.dumps([d[:3] for d in deltas[:5]]))
    out.append("- largest Justless losses: %s" % json.dumps([d[:3] for d in deltas[-5:][::-1]]))
    out.append("")
    summary["hooks"] = hooks

    out += ["## 12. Spend", ""]
    by = {}
    directory_fake = accounting.directory_is_dry(base)
    for entry in ledger:
        key = "%s %s %s%s" % (entry.get("phase"), entry.get("model"), entry.get("arm"),
                              " (fake)" if accounting.entry_is_fake(entry, directory_fake) else "")
        by[key] = by.get(key, 0.0) + float(entry.get("cost_usd") or 0)
    for key, value in sorted(by.items()):
        out.append("- %s: %s" % (key, money(value)))
    total = sum(v for k, v in by.items() if not k.endswith("(fake)"))
    fake_total = sum(v for k, v in by.items() if k.endswith("(fake)"))
    out.append("- paid total in this campaign: %s of the %s cap; all campaigns: %s; reserve untouched: %s%s" % (
        money(total), money(config.CAMPAIGN_CAP_USD), money(accounting.paid_spend(home)),
        accounting.paid_spend(home) <= config.CAMPAIGN_CAP_USD - config.RESERVE_USD,
        "; fake usage (never paid): %s" % money(fake_total) if fake_total else ""))
    out.append("")
    summary["spend"] = {"total": total, "fake": fake_total, "by": by}

    out += ["## 13. Deviations", "",
            "- No Agent/subagent, Skill, ToolSearch, Workflow or scheduling tools; their text is patched out of "
            "the captured prompt (patch hash `%s`)." % pins.get("patch_hash"),
            "- Non-streaming requests; anthropic-version %s; only the betas the harness honours." % config.ANTHROPIC_VERSION,
            "- A per-attempt `Session: <nonce>` system block replaces Claude Code's billing header.",
            "- Per-run APFS clones of a per-task warm target.",
            "- Background Bash is rejected in every arm.",
            "- The project CLAUDE.md is included as interactive Claude Code includes it (not observed under "
            "`claude -p` in the offline capture).",
            "- Items marked approximate or open in data/cc_parity.json.", ""]
    summary["requests_logged"] = len(requests)
    summary["input_transformations_nonempty"] = sum(1 for q in requests if q.get("input_transformations"))
    return "\n".join(out) + "\n", summary


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, default=config.HOME)
    parser.add_argument("--campaign", required=True)
    parser.add_argument("--dry", action="store_true")
    parser.add_argument("--bootstrap", type=int, default=stats.B_DEFAULT)
    parser.add_argument("--task-set", default="main", choices=sorted(tasks_module.TASK_SETS))
    args = parser.parse_args(argv)
    text, summary = build(args.home, args.campaign, args.dry, args.bootstrap, args.task_set)
    out = Path(args.home) / "reports" / args.campaign
    out.mkdir(parents=True, exist_ok=True)
    (out / "report.md").write_text(text)
    (out / "summary.json").write_text(json.dumps(summary, indent=1, default=str))
    print(out / "report.md")
    return 0


if __name__ == "__main__":
    sys.exit(main())
