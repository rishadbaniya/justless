import copy
import json
import unittest

import helpers

import accounting
import api as api_module
import cc_profile
import cc_tools
import config
import fake_api
import loop
import workspace


class GuardTests(unittest.TestCase):
    def body(self):
        return {"model": "m", "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
                "tools": [{"name": "Bash"}], "max_tokens": 10,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]}

    def extended(self, body, response):
        nxt = copy.deepcopy(body)
        nxt["messages"].append({"role": "assistant", "content": response})
        nxt["messages"].append({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "x"}]})
        return nxt

    def test_append_only_passes_and_cache_control_moves(self):
        guard = loop.AppendOnlyGuard()
        body = self.body()
        guard.check(body)
        response = [{"type": "thinking", "thinking": "", "signature": "sig"}, {"type": "tool_use", "id": "t", "name": "Bash",
                                                                            "input": {}}]
        guard.record_response(response)
        nxt = self.extended(body, response)
        nxt["messages"][0]["content"][0]["cache_control"] = {"type": "ephemeral"}
        guard.check(nxt)

    def test_edits_are_rejected(self):
        response = [{"type": "thinking", "thinking": "", "signature": "sig"}]
        for mutate in (lambda b: b["system"][0].update(text="changed"),
                       lambda b: b["tools"].append({"name": "Read"}),
                       lambda b: b["messages"][0]["content"][0].update(text="edited"),
                       lambda b: b["messages"][1]["content"].pop(0)):
            guard = loop.AppendOnlyGuard()
            body = self.body()
            guard.check(body)
            guard.record_response(response)
            nxt = self.extended(body, response)
            mutate(nxt)
            with self.assertRaises(loop.HarnessError):
                guard.check(nxt)


class StubClient:
    def __init__(self, responses):
        self.responses = list(responses)
        self.bodies = []
        self.api_key = "sk-ant-stub-000000000000000000000000"

    def create(self, body):
        self.bodies.append(copy.deepcopy(body))
        item = self.responses.pop(0)
        if isinstance(item, Exception):
            raise item
        return item, {"attempts": [{}], "retry_wait_s": 0.0, "latency_s": 0.01, "request_id": "req"}


def message(content, stop="tool_use", usage=None):
    return {"content": content, "stop_reason": stop,
            "usage": usage or {"input_tokens": 100, "output_tokens": 10, "cache_creation_input_tokens": 0,
                               "cache_read_input_tokens": 0}}


class LoopTests(unittest.TestCase):
    def make(self, responses, limits=None, run_budget=1.0, ledger_cap=50.0):
        run_dir = helpers.tempdir(self)
        root = run_dir / "repo"
        root.mkdir()
        env = workspace.tool_env(root, run_dir / "home", workspace.make_toolbin(run_dir / "bin"), run_dir / "target",
                                 tmp=run_dir / "tmp")
        ws = cc_tools.Workspace(root, run_dir, env, None, config.CC_BINARY)
        logs = {k: loop.JsonlWriter(run_dir / ("%s.jsonl" % k)) for k in ("requests", "tools", "attempts")}
        ledger = accounting.Ledger(run_dir / "ledger.jsonl", ledger_cap, reserve_usd=0.0)
        spec = {"campaign": "t", "model": "claude-sonnet-5-5", "effort": "medium", "arm": "a", "run_id": "r",
                "attempt": 1, "nonce": config.nonce("r", 1), "home": str(run_dir / "home"), "prompt": "go",
                "run_budget": run_budget}
        client = StubClient(responses)
        run = loop.Run(spec, client, cc_profile.paraphrase(), ledger, accounting.Pricing(), ws, None, logs,
                       limits=limits, api_key=client.api_key)
        return run, client

    def test_end_turn_and_tool_round_trip(self):
        tool = {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "echo hi"}}
        run, client = self.make([message([tool]), message([{"type": "text", "text": "done"}], "end_turn")])
        run.run()
        self.assertIsNone(run.harness_error)
        self.assertIsNone(run.failure_mode)
        self.assertEqual(run.final_text, "done")
        second = client.bodies[1]["messages"]
        self.assertEqual(second[-1]["content"][0]["content"], "hi")
        self.assertEqual(second[-1]["content"][-1]["cache_control"], {"type": "ephemeral"})
        self.assertNotIn("cache_control", run.messages[-1]["content"][-1])

    def test_stop_conditions(self):
        tool = {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "true"}}
        run, _ = self.make([message([tool])] * 5, limits={"turn_limit": 2})
        run.run()
        self.assertEqual(run.failure_mode, "turn_limit")
        run, _ = self.make([message([], "refusal")])
        run.run()
        self.assertEqual(run.failure_mode, "refusal")
        run, _ = self.make([message([], "max_tokens")])
        run.run()
        self.assertEqual(run.failure_mode, "max_tokens")
        run, _ = self.make([message([], "pause_turn")])
        run.run()
        self.assertEqual(run.harness_error.kind, "stop_reason")

    def test_run_budget_and_global_cap(self):
        tool = {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "true"}}
        big = {"input_tokens": 400000, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
        run, _ = self.make([message([tool], usage=big)] * 3, run_budget=0.5)
        run.run()
        self.assertEqual(run.failure_mode, "run_budget")
        self.assertEqual(run.totals["requests"], 1)
        run, _ = self.make([message([tool], usage=big)] * 3, run_budget=100, ledger_cap=1.0)
        run.run()
        self.assertEqual(run.harness_error.kind, "aborted_global_cap")

    def test_api_400_prefix_mismatch_is_flagged(self):
        error = api_module.APIError(400, {"error": {"message": "messages.2.content.0: Invalid `signature` in `thinking` "
                                                               "block. The block is bound to a different conversation."}})
        run, _ = self.make([error])
        run.run()
        self.assertTrue(run.harness_error.append_only)

    def test_nonce_differs_per_attempt(self):
        self.assertNotEqual(config.nonce("run", 1), config.nonce("run", 2))
        self.assertEqual(len(config.nonce("run", 1)), 16)

    def test_secret_filter_blocks_key(self):
        tool = {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "echo sk-ant-api03-" + "A" * 30}}
        run, _ = self.make([message([tool]), message([], "end_turn")])
        run.run()
        self.assertEqual(run.harness_error.kind, "secret_blocked")


class FakeServerTests(unittest.TestCase):
    def test_binding_check_and_cache(self):
        api = fake_api.FakeAPI(0).start()
        self.addCleanup(api.stop)
        api.register("default", fake_api.Script([fake_api.tool_use("Bash", {"command": "ls"}), fake_api.final("ok")]))
        client = api_module.Client("sk-ant-fake", base_url=api.base_url, sleep=lambda s: None, max_attempts=1)
        body = {"model": "claude-haiku-5-5", "max_tokens": 100, "thinking": {"type": "adaptive", "block_binding":
                {"prefix_mismatch_behavior": "error"}},
                "system": [{"type": "text", "text": "x" * 4000, "cache_control": {"type": "ephemeral"}}],
                "messages": [{"role": "user", "content": [{"type": "text", "text": "go", "cache_control": {"type": "ephemeral"}}]}]}
        first, _ = client.create(body)
        self.assertGreater(first["usage"]["cache_creation_input_tokens"], 0)
        good = copy.deepcopy(body)
        good["messages"][0]["content"][0].pop("cache_control")
        good["messages"].append({"role": "assistant", "content": first["content"]})
        good["messages"].append({"role": "user", "content": [{"type": "tool_result", "tool_use_id": first["content"][1]["id"],
                                                              "content": "out", "cache_control": {"type": "ephemeral"}}]})
        second, _ = client.create(good)
        self.assertGreater(second["usage"]["cache_read_input_tokens"], 0)
        bad = copy.deepcopy(good)
        bad["messages"][0]["content"][0]["text"] = "edited"
        with self.assertRaises(api_module.APIError) as ctx:
            client.create(bad)
        self.assertTrue(ctx.exception.append_only_bug)

    def test_context_editing_simulation(self):
        body = {"context_management": {"edits": [{"type": "clear_tool_uses_20250919", "trigger": {"value": 100},
                                                  "keep": {"value": 1}, "clear_at_least": {"value": 10}}]},
                "messages": [{"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": "x" * 2000}]},
                             {"role": "assistant", "content": [{"type": "text", "text": "k"}]},
                             {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "b", "content": "y" * 2000}]}]}
        edited, applied = fake_api.apply_context_edits(body)
        self.assertEqual(applied[0]["cleared_tool_uses"], 1)
        self.assertEqual(edited["messages"][0]["content"][0]["content"], fake_api.CLEARED)
        self.assertEqual(edited["messages"][2]["content"][0]["content"], "y" * 2000)


class CounterAndSecretTests(unittest.TestCase):
    def test_count_tokens_against_fake_and_cache(self):
        import run as run_module
        api = fake_api.FakeAPI(0).start()
        self.addCleanup(api.stop)
        client = api_module.Client("sk-ant-fake", base_url=api.base_url, sleep=lambda s: None, max_attempts=1)
        self.assertEqual(client.count_tokens("claude-haiku-5-5", "x" * 700),
                         fake_api.tokens_of(fake_api.canonical(["user", "x" * 700])))
        cache = helpers.tempdir(self) / "cache.json"
        counter = run_module.make_token_counter(client, "claude-haiku-5-5", cache)
        first = counter("y" * 7000)
        self.assertEqual(api.count_token_calls, 2)
        self.assertEqual(counter("y" * 7000), first)
        self.assertEqual(api.count_token_calls, 2)
        broken = api_module.Client("sk-ant-fake", base_url="http://127.0.0.1:9", sleep=lambda s: None, max_attempts=1)
        bad = run_module.make_token_counter(broken, "claude-haiku-5-5", cache)
        with self.assertRaises((loop.HarnessError, api_module.APIError, OSError)):
            bad("z" * 7000)

    def test_secret_filter_respects_allowlist(self):
        line = "# KEY=\"-----BEGIN RSA PRIVATE KEY-----\""
        self.assertEqual(loop.secret_hits(line, None), ["private_key"])
        self.assertEqual(loop.secret_hits(line, None, {"-----BEGIN RSA PRIVATE KEY-----"}), [])
        self.assertEqual(loop.secret_hits("token sk-ant-real", "sk-ant-real", {"x"}), ["api_key"])
        root = helpers.tempdir(self)
        (root / "secrets.env.example").write_text("a\n" + line + "\n")
        allowed = workspace.allowed_secret_texts(root, ["secrets.env.example:2:private_key"])
        self.assertEqual(allowed, {"-----BEGIN RSA PRIVATE KEY-----"})


if __name__ == "__main__":
    unittest.main()
