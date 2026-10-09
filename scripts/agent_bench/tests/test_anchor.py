"""The anchor proxy enforces the budget per request and the anchor skips finished runs."""

import json
import unittest

import helpers

import accounting
import api as api_module
import cc_anchor
import fake_api


class AnchorProxyTests(unittest.TestCase):
    def setUp(self):
        self.api = fake_api.FakeAPI(0).start()
        self.addCleanup(self.api.stop)
        self.api.register("default", fake_api.Script([fake_api.final("ok")] * 10))
        self.dir = helpers.tempdir(self)

    def body(self):
        return {"model": "claude-sonnet-5-5", "max_tokens": 1000,
                "system": [{"type": "text", "text": "x" * 4000}],
                "messages": [{"role": "user", "content": [{"type": "text", "text": "go"}]}]}

    def proxy(self, cap, run_budget=None):
        ledger = accounting.Ledger(self.dir / ("ledger-%s.jsonl" % cap), cap, reserve_usd=0.0)
        proxy = cc_anchor.KeyProxy(self.api.base_url, self.dir / "proxy.jsonl", "sk-ant-real-key",
                                   ledger, accounting.Pricing(), "claude-sonnet-5-5", run_budget,
                                   {"run_id": "r", "campaign": "c", "phase": "anchor", "arm": "a"}).start()
        self.addCleanup(proxy.stop)
        client = api_module.Client("sk-ant-dummy", base_url=proxy.base_url, sleep=lambda s: None, max_attempts=1)
        return proxy, ledger, client

    def test_costs_are_recorded_per_response(self):
        proxy, ledger, client = self.proxy(10.0)
        client.create(self.body())
        client.create(self.body())
        entries = [json.loads(l) for l in (self.dir / "ledger-10.0.jsonl").read_text().splitlines()]
        self.assertEqual(len(entries), 2)
        self.assertTrue(all(e["cost_usd"] > 0 and e["phase"] == "anchor" for e in entries))
        self.assertAlmostEqual(proxy.spent, sum(e["cost_usd"] for e in entries))
        self.assertAlmostEqual(ledger.pending, 0.0)

    def test_campaign_cap_refuses_before_forwarding(self):
        worst = accounting.Pricing().worst_case("claude-sonnet-5-5", len(json.dumps(self.body())) // 2, 1000)
        proxy, ledger, client = self.proxy(worst * 1.5)
        ledger.record({"cost_usd": worst})  # what other workers already spent
        before = len(self.api.requests)
        with self.assertRaises(api_module.APIError) as ctx:
            client.create(self.body())
        self.assertEqual(ctx.exception.status, 400)
        self.assertEqual(proxy.stopped, "aborted_global_cap")
        self.assertEqual(len(self.api.requests), before)
        with self.assertRaises(api_module.APIError):
            client.create(self.body())

    def test_run_budget_stops_the_run(self):
        proxy, ledger, client = self.proxy(10.0, run_budget=1e-9)
        client.create(self.body())
        with self.assertRaises(api_module.APIError):
            client.create(self.body())
        self.assertEqual(proxy.stopped, "run_budget")


if __name__ == "__main__":
    unittest.main()
