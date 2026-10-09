"""Markdown report and summary.json for a campaign. Usage: report.py --campaign NAME [--dry]."""

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import config  # noqa: E402
import stats  # noqa: E402

FAMILIES = ["bugfix", "lint", "location", "impact"]
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
    """The terminal record per run_id (latest attempt)."""
    latest = {}
    for row in rows:
        key = row.get("run_id")
        if key is None:
            continue
        if key not in latest or row.get("attempt", 1) >= latest[key].get("attempt", 1):
            latest[key] = row
    return list(latest.values())


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


def contrast_section(rows, x, y, model, families=None, B=stats.B_DEFAULT):
    result = stats.contrast(rows, x, y, model, families, B)
    if not result.get("estimate"):
        return result, ["No complete %s/%s blocks for %s." % (x, y, model)]
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
    if boot["delta_s_ci"][0] is not None and (boot["delta_s_ci"][0] > 0 or boot["delta_s_ci"][1] < 0):
        lines.append("- **The Δs CI excludes 0.**")
    return result, lines


def build(home, campaign, dry=False, B=stats.B_DEFAULT):
    base = Path(home) / "campaigns" / campaign
    rows = final_rows(read_jsonl(base / "results.jsonl"))
    attempts = read_jsonl(base / "results.jsonl")
    requests = read_jsonl(base / "requests.jsonl")
    tools = read_jsonl(base / "tools.jsonl")
    ledger = read_jsonl(base / "ledger.jsonl")
    out = []
    summary = {"campaign": campaign, "dry_run": dry, "runs": len(rows)}
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
        if row.get("failure_mode") in ("harness_error", "harness_unrecoverable"):
            key = (row.get("arm"), row["failure_mode"])
            harness_errors[key] = harness_errors.get(key, 0) + 1
    out += ["## 1. Manifest", ""] + ["- %s: `%s`" % (k, v) for k, v in sorted(pins.items())] + [
        "- runs (terminal records): %d; attempts: %d" % (len(rows), len(attempts)),
        "- harness errors by arm: %s" % (", ".join("%s %s=%d" % (a, m, n) for (a, m), n in sorted(
            harness_errors.items())) or "none"), ""]
    summary["pins"] = pins
    summary["harness_errors"] = {"%s:%s" % k: v for k, v in harness_errors.items()}

    out += ["## 2. Pre-registered primary result (%s, b vs a)" % PRIMARY_MODEL, ""]
    primary, lines = contrast_section(rows, "b", "a", PRIMARY_MODEL, B=B)
    out += lines + [""]
    summary["primary"] = primary
    edit, lines = contrast_section(rows, "b", "a", PRIMARY_MODEL, ["bugfix", "lint"], B=B)
    out += ["### Key secondary: edit families (bug fix + lint)", ""] + lines + [""]
    summary["edit_families"] = edit
    per_family = {}
    negative = []
    for family in FAMILIES:
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
    anchor = [r for r in rows if r.get("agent") == "claude-code"]
    if anchor:
        for arm in ("a", "b"):
            cell = [r for r in anchor if r["arm"] == arm]
            out.append("- anchor arm %s: %d/%d success, cost per run %s" % (
                arm, sum(1 for r in cell if r.get("success")), len(cell),
                money(sum(stats.cost(r) for r in cell) / len(cell) if cell else None)))
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
    clusters = stats.censored(stats.pairs(rows, "b", "a", PRIMARY_MODEL))
    est = stats.estimates(clusters)
    sens["limits_censored"] = est
    out.append("- Limit hits censored: %s" % ("n/a" if not est else "Δs %s, S %s" % (
        stats.pp(est["delta_s"]), pct(est["S"]))))
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
    out.append("")
    summary["sensitivity"] = sens

    out += ["## 8. Per family and repository (exploratory)", "",
            "| Model | Family | Repo | Arm | Runs | Success | Cost per run |", "| --- | --- | --- | --- | ---: | ---: | ---: |"]
    groups = {}
    for r in rows:
        groups.setdefault((r.get("model"), r.get("family"), r.get("repo"), r.get("arm")), []).append(r)
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
    for entry in ledger:
        key = "%s %s %s" % (entry.get("phase"), entry.get("model"), entry.get("arm"))
        by[key] = by.get(key, 0.0) + float(entry.get("cost_usd") or 0)
    for key, value in sorted(by.items()):
        out.append("- %s: %s" % (key, money(value)))
    total = sum(by.values())
    out.append("- total: %s of the %s cap; reserve untouched: %s" % (
        money(total), money(config.CAMPAIGN_CAP_USD), total <= config.CAMPAIGN_CAP_USD - config.RESERVE_USD))
    out.append("")
    summary["spend"] = {"total": total, "by": by}

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
    args = parser.parse_args(argv)
    text, summary = build(args.home, args.campaign, args.dry, args.bootstrap)
    out = Path(args.home) / "reports" / args.campaign
    out.mkdir(parents=True, exist_ok=True)
    (out / "report.md").write_text(text)
    (out / "summary.json").write_text(json.dumps(summary, indent=1, default=str))
    print(out / "report.md")
    return 0


if __name__ == "__main__":
    sys.exit(main())
