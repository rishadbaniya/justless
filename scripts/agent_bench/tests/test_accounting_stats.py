import json
import unittest

import helpers

import accounting
import stats


class PricingTests(unittest.TestCase):
    def setUp(self):
        self.p = accounting.Pricing()

    def test_sonnet_and_opus_math(self):
        usage = {"input_tokens": 1000, "cache_creation_input_tokens": 2000,
                 "cache_creation": {"ephemeral_5m_input_tokens": 2000, "ephemeral_1h_input_tokens": 0},
                 "cache_read_input_tokens": 10000, "output_tokens": 500}
        self.assertAlmostEqual(self.p.cost("claude-sonnet-5-5", usage),
                               (1000 * 2 + 2000 * 2.5 + 10000 * 0.1 + 500 * 10) / 1e6)
        self.assertAlmostEqual(self.p.cost("claude-opus-5-5", usage),
                               (1000 * 4 + 2000 * 5 + 10000 * 0.2 + 500 * 20) / 1e6)

    def test_haiku_tier_counts_cache_reads_and_writes(self):
        at = {"input_tokens": 10, "cache_creation_input_tokens": 50000, "cache_read_input_tokens": 49990,
              "output_tokens": 100}
        over = dict(at, cache_read_input_tokens=49991)
        self.assertAlmostEqual(self.p.cost("claude-haiku-5-5", at),
                               (10 * 0.10 + 50000 * 0.125 + 49990 * 0.01 + 100 * 0.5) / 1e6)
        self.assertAlmostEqual(self.p.cost("claude-haiku-5-5", over),
                               (10 * 0.50 + 50000 * 0.625 + 49991 * 0.05 + 100 * 2.5) / 1e6)

    def test_one_hour_writes_and_repricing(self):
        usage = {"input_tokens": 0, "cache_creation_input_tokens": 1000,
                 "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 1000},
                 "cache_read_input_tokens": 0, "output_tokens": 0}
        self.assertAlmostEqual(self.p.cost("claude-sonnet-5-5", usage), 1000 * 4.0 / 1e6)
        five = dict(usage, cache_creation={"ephemeral_5m_input_tokens": 1000, "ephemeral_1h_input_tokens": 0})
        self.assertAlmostEqual(self.p.reprice_1h("claude-sonnet-5-5", five), 1000 * 4.0 / 1e6)
        self.assertAlmostEqual(self.p.reprice_1h("claude-sonnet-5-5", five, ttl_expiry_rewrite=True), 1000 * 0.1 / 1e6)

    def test_ledger_cap_and_reserve(self):
        path = helpers.tempdir(self) / "ledger.jsonl"
        ledger = accounting.Ledger(path, 10.0, reserve_usd=2.0)
        ledger.record({"cost_usd": 7.5})
        with self.assertRaises(accounting.BudgetExceeded):
            ledger.check(0.6)
        ledger.check(0.4)
        again = accounting.Ledger(path, 10.0, reserve_usd=2.0, use_reserve=True)
        self.assertAlmostEqual(again.spent, 7.5)
        again.check(2.4)

    def test_prepilot_estimate_fits(self):
        result = accounting.estimate(accounting.PREPILOT)
        self.assertTrue(result["fits"])
        pilot, _ = accounting.plan_cost(accounting.PILOT_PLAN, accounting.PREPILOT)
        self.assertLessEqual(pilot, 3.0)


def row(task, rep, arm, ok, cost, mode=None, model="claude-sonnet-5-5"):
    return {"task_id": task, "rep": rep, "arm": arm, "success": ok, "model": model,
            "failure_mode": mode or ("success" if ok else "wrong_fix"), "totals": {"cost_usd": cost}}


class StatsTests(unittest.TestCase):
    def rows(self):
        out = []
        for t in range(8):
            for rep in (1, 2, 3):
                out.append(row("t%d" % t, rep, "a", True, 1.0))
                out.append(row("t%d" % t, rep, "b", True, 0.5))
        return out

    def test_estimates_toy(self):
        clusters = stats.pairs(self.rows(), "b", "a", "claude-sonnet-5-5")
        est = stats.estimates(clusters)
        self.assertAlmostEqual(est["S"], 0.5)
        self.assertEqual(est["delta_s"], 0.0)

    def test_bootstrap_deterministic_and_decision(self):
        clusters = stats.pairs(self.rows(), "b", "a", "claude-sonnet-5-5")
        one = stats.bootstrap(clusters, B=300)
        two = stats.bootstrap(clusters, B=300)
        self.assertEqual(one, two)
        verdict, wording = stats.decide(stats.estimates(clusters), one)
        self.assertEqual(verdict, "WIN")
        self.assertIn("not worse by more than 10 percentage points", wording)

    def test_decision_precedence(self):
        est = {"delta_s": -0.2, "S": 0.5}
        self.assertEqual(stats.decide(est, {"S_ci": [0.3, 0.6], "delta_s_ci": [-0.3, -0.12]})[0], "WORSE")
        self.assertEqual(stats.decide(dict(est, delta_s=-0.06), {"S_ci": [0.3, 0.6], "delta_s_ci": [-0.09, 0.0]})[0],
                         "CHEAPER-BUT-SUCCESS-UNCERTAIN")
        self.assertEqual(stats.decide(dict(est, delta_s=0.0), {"S_ci": [-0.1, 0.6], "delta_s_ci": [-0.05, 0.05]})[0],
                         "NO DEMONSTRATED SAVING")
        self.assertEqual(stats.decide(dict(est, S=-0.5), {"S_ci": [-0.8, -0.1], "delta_s_ci": [-0.05, 0.05]})[0], "WORSE")

    def test_zero_success_guard(self):
        rows = [row("t%d" % t, 1, arm, False, 1.0) for t in range(4) for arm in "ab"]
        boot = stats.bootstrap(stats.pairs(rows, "b", "a", "claude-sonnet-5-5"), B=100)
        self.assertIsNone(boot["S_ci"])
        self.assertEqual(boot["S_undefined_share"], 1.0)

    def test_holm_and_mcnemar(self):
        adjusted = stats.holm({"x": 0.01, "y": 0.04, "z": 0.03})
        self.assertAlmostEqual(adjusted["x"], 0.03)
        self.assertAlmostEqual(adjusted["z"], 0.06)
        self.assertAlmostEqual(adjusted["y"], 0.06)
        rows = [row("t%d" % t, 1, "a", False, 1) for t in range(6)] + [row("t%d" % t, 1, "b", True, 1) for t in range(6)]
        result = stats.mcnemar_exact(stats.pairs(rows, "b", "a", "claude-sonnet-5-5"))
        self.assertEqual((result["b"], result["c"]), (6, 0))
        self.assertAlmostEqual(result["p"], 2 / 64.0)

    def test_censoring_and_fidelity(self):
        rows = self.rows() + [row("t9", 1, "a", False, 3.0, "turn_limit"), row("t9", 1, "b", True, 1.0)]
        clusters = stats.pairs(rows, "b", "a", "claude-sonnet-5-5")
        self.assertIn("t9", clusters)
        self.assertNotIn("t9", stats.censored(clusters))
        flag, reasons = stats.fidelity_flag(0.5, [0.6, 0.8], {"rewrite": 0.1}, {"rewrite": 0.5}, 1.0, 2.0)
        self.assertTrue(flag)
        self.assertEqual(len(reasons), 3)
        self.assertFalse(stats.fidelity_flag(0.7, [0.6, 0.8], {}, {}, 1.0, 1.1)[0])

    def test_anchor_rows_stay_out_of_harness_contrasts(self):
        rows = self.rows()
        anchor = [dict(row("t0", 1, arm, False, 9.0), agent="claude-code") for arm in "ab"]
        clusters = stats.pairs(rows + anchor, "b", "a", "claude-sonnet-5-5")
        self.assertTrue(all(x.get("agent") is None for x, _ in clusters["t0"]))
        self.assertEqual(len(stats.pairs(rows + anchor, "b", "a", "claude-sonnet-5-5", agent="claude-code")["t0"]), 1)

    def test_mde_planning(self):
        result = stats.mde(96, reps=3)
        self.assertAlmostEqual(result["design_effect"], 1.6)
        self.assertTrue(0.15 < result["cost_mde"] < 0.25)


if __name__ == "__main__":
    unittest.main()
