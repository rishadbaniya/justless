"""Pins, binary pinning, the manifest freeze, the capture profile, the pause gate and retries."""

import json
import unittest

import helpers

import api as api_module
import cc_profile
import config
import fake_api
import loop
import run as run_module
import tasks as tasks_module


class PinTests(unittest.TestCase):
    BASE = {"justless_sha256": "aa", "harness_tree": "h1", "capture_hash": "c", "manifest_hash": "m",
            "pricing_hash": "p", "limits": {"turn_limit": 60}, "seed": 1}

    def test_changed_pins_are_refused_and_new_generations_kept_apart(self):
        directory = helpers.tempdir(self)
        cells = {"claude-haiku-5-5|cc-default": {"max_output_tokens": 32000, "ce_trigger": 45000}}
        run_module.check_pins(directory, self.BASE, cells)
        run_module.check_pins(directory, self.BASE, cells)
        with self.assertRaises(SystemExit) as ctx:
            run_module.check_pins(directory, dict(self.BASE, justless_sha256="bb"), cells)
        self.assertIn("justless_sha256", str(ctx.exception))
        with self.assertRaises(SystemExit):
            run_module.check_pins(directory, self.BASE, {"claude-haiku-5-5|cc-default": {"ce_trigger": 30000}})
        # Another model, or an arm set without context editing, adds to the generation instead of refusing.
        run_module.check_pins(directory, self.BASE, {"claude-sonnet-5-5|cc-default": {"max_output_tokens": 32000}})
        run_module.check_pins(directory, {"justless_sha256": "aa", "harness_tree": "h1"}, {"anchor|x": {"t": 1}})
        stored = run_module.check_pins(directory, dict(self.BASE, justless_sha256="bb"), cells, new_pins=True)
        self.assertEqual(len(stored["generations"]), 2)
        self.assertEqual(sorted(stored["generations"][0]["cells"]),
                         ["anchor|x", "claude-haiku-5-5|cc-default", "claude-sonnet-5-5|cc-default"])

    def test_harness_hash_ignores_tests_and_tracks_code(self):
        root = helpers.tempdir(self)
        (root / "tests").mkdir()
        (root / "a.py").write_text("x = 1\n")
        (root / "tests" / "t.py").write_text("t = 1\n")
        first = config.harness_tree_hash(root)
        (root / "tests" / "t.py").write_text("t = 2\n")
        (root / "__pycache__").mkdir()
        (root / "__pycache__" / "a.pyc").write_bytes(b"\0")
        self.assertEqual(config.harness_tree_hash(root), first)
        (root / "a.py").write_text("x = 2\n")
        self.assertNotEqual(config.harness_tree_hash(root), first)

    def test_binary_is_pinned_by_content(self):
        home = helpers.tempdir(self)
        source = home / "justless"
        source.write_bytes(b"build one")
        pinned, sha = run_module.pin_binary(home, source)
        self.assertEqual(sha, config.sha256_file(source))
        source.write_bytes(b"build two")  # a rebuild during the campaign
        self.assertEqual(pinned.read_bytes(), b"build one")
        dest = home / "run" / "justless"
        run_module.install_pinned(pinned, dest, sha)
        with self.assertRaises(RuntimeError):
            run_module.install_pinned(source, home / "run2" / "justless", sha)

    def test_frozen_manifest_is_the_run_id_manifest(self):
        home = helpers.tempdir(self)
        with self.assertRaises(SystemExit):
            run_module.require_frozen_manifest(home, "abc")
        import gen_tasks
        args = type("A", (), {"home": home, "approve": True})()
        gen_tasks.review(args)
        manifest = tasks_module.manifest_hash(tasks_module.load_public() + tasks_module.load_private(home))
        run_module.require_frozen_manifest(home, manifest)
        with self.assertRaises(SystemExit):
            run_module.require_frozen_manifest(home, "changed")

    def test_nonce_is_salted_per_process(self):
        self.assertNotEqual(config.nonce("r", 1, "salt-a"), config.nonce("r", 1, "salt-b"))
        self.assertEqual(config.nonce("r", 1), config.nonce("r", 1, config.PROCESS_SALT))
        self.assertRegex(config.nonce("r", 1), "^[0-9a-f]{16}$")

    def test_seed_changes_the_arm_order(self):
        arms = ["a", "b", "c", "d"]
        orders = {tuple(config.arm_order("m", "task-%d" % i, 1, arms, seed)) for i in range(6) for seed in (1, 2)}
        default = [config.arm_order("m", "task-%d" % i, 1, arms) for i in range(6)]
        self.assertEqual(default, [config.arm_order("m", "task-%d" % i, 1, arms, config.SEED) for i in range(6)])
        self.assertGreater(len(orders), 1)
        self.assertNotEqual([config.arm_order("m", "task-%d" % i, 1, arms, 1) for i in range(6)],
                            [config.arm_order("m", "task-%d" % i, 1, arms, 2) for i in range(6)])

    def test_token_limit_rule(self):
        self.assertEqual(config.token_limit_rule([]), config.TOKEN_LIMIT_FLOOR)
        self.assertEqual(config.token_limit_rule([500000] * 20), config.TOKEN_LIMIT_FLOOR)
        self.assertEqual(config.token_limit_rule([1500000] * 19 + [1520000]), 4600000)


class GateTests(unittest.TestCase):
    def rows(self, arm, n, limit_hits=0, unrecoverable=0):
        out = []
        for i in range(n):
            mode = "harness_unrecoverable" if i < unrecoverable else "success"
            out.append({"run_id": "%s%d" % (arm, i), "model": "m", "arm": arm, "failure_mode": mode,
                        "limit_hit": unrecoverable <= i < unrecoverable + limit_hits})
        return out

    def test_gate_waits_for_enough_runs_then_pauses(self):
        self.assertEqual(run_module.campaign_gate(self.rows("a", 10, 5) + self.rows("b", 10), "m", ["a", "b"]), [])
        self.assertEqual(run_module.campaign_gate(self.rows("a", 40) + self.rows("b", 40), "m", ["a", "b"]), [])
        reasons = run_module.campaign_gate(self.rows("a", 40, limit_hits=3) + self.rows("b", 40), "m", ["a", "b"])
        self.assertTrue(any("limit-hit" in r for r in reasons))
        reasons = run_module.campaign_gate(self.rows("a", 40, unrecoverable=2) + self.rows("b", 40), "m", ["a", "b"])
        self.assertTrue(any("unrecoverable" in r for r in reasons))


class ProfileTests(unittest.TestCase):
    def capture(self, home, models):
        capture = home / "cc_capture"
        work = home / "cc_capture_work"
        capture.mkdir(parents=True)
        work.mkdir()
        root = "/w/proj_tools"
        env = "# Environment\n - Primary working directory: %s\n\nToday's date is x."
        body = {"system": [{"type": "text", "text": "billing"}, {"type": "text", "text": "You are Claude Code in %s" % root}],
                "tools": [{"name": "Bash"}, {"name": "Agent"}],
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]},
                             {"role": "system", "content": [{"type": "text", "text": env % root}]}]}
        (capture / "capture.json").write_text(json.dumps({"sessions": {"tools": {"root": root, "home": "/w/home"}}}))
        (capture / "first_request_tools.json").write_text(json.dumps({"body": body}))
        for model, effort in models.items():
            mroot = "/w/proj_model_%s" % model
            mbody = {"output_config": {"effort": effort}, "max_tokens": 64000,
                     "messages": [{"role": "user", "content": [{"type": "text", "text": "attr " + model}]},
                                  {"role": "system", "content": [{"type": "text", "text": env % mroot}]}]}
            with open(str(work / ("requests_model_%s.jsonl" % model)), "w") as handle:
                handle.write(json.dumps({"path": "/v1/messages", "body": mbody, "headers": {}}) + "\n")
        return capture, work

    def test_missing_model_capture_is_an_error_and_the_rest_is_frozen(self):
        home = helpers.tempdir(self)
        efforts = {m: "medium" for m in config.MODELS}
        capture, work = self.capture(home, efforts)
        profile = cc_profile.from_capture(home)
        self.assertEqual(profile.effort("claude-sonnet-5-5"), "medium")
        for model in config.MODELS:
            self.assertTrue((capture / ("first_request_model_%s.json" % model)).exists())
        # The store moved without cc_capture_work: the frozen copies keep the same profile and hash.
        for path in work.iterdir():
            path.unlink()
        moved = cc_profile.from_capture(home)
        self.assertEqual(moved.capture_hash, profile.capture_hash)
        (capture / "first_request_model_claude-opus-5-5.json").unlink()
        with self.assertRaises(ValueError):
            cc_profile.from_capture(home)

    def test_capture_hash_follows_the_effective_profile(self):
        one = helpers.tempdir(self)
        two = helpers.tempdir(self)
        self.capture(one, {m: "medium" for m in config.MODELS})
        self.capture(two, dict({m: "medium" for m in config.MODELS}, **{"claude-sonnet-5-5": "high"}))
        a, b = cc_profile.from_capture(one), cc_profile.from_capture(two)
        self.assertNotEqual(a.capture_hash, b.capture_hash)
        self.assertEqual(a.model_pins("claude-sonnet-5-5")["effort"], "medium")
        self.assertEqual(a.model_pins("claude-sonnet-5-5", 1000)["max_tokens"], 1000)


class RetryTests(unittest.TestCase):
    def test_count_tokens_retries_transient_errors(self):
        api = fake_api.FakeAPI(0).start()
        self.addCleanup(api.stop)
        client = api_module.Client("sk-ant-fake", base_url=api.base_url, sleep=lambda s: None, max_attempts=4)
        calls = {"n": 0}
        original = client._post

        def flaky(path, body):
            calls["n"] += 1
            if calls["n"] == 1:
                return 529, {"error": {"message": "overloaded"}}, {}
            if calls["n"] == 2:
                raise OSError("connection reset")
            return original(path, body)
        client._post = flaky
        self.assertGreater(client.count_tokens("claude-haiku-5-5", "x" * 100), 0)
        self.assertEqual(calls["n"], 3)

    def test_persistent_count_tokens_failure_pauses_instead_of_rerolling(self):
        broken = api_module.Client("sk-ant-fake", base_url="http://127.0.0.1:9", sleep=lambda s: None, max_attempts=2)
        counter = run_module.make_token_counter(broken, "claude-haiku-5-5", helpers.tempdir(self) / "cache.json")
        with self.assertRaises(loop.HarnessError) as ctx:
            counter("z" * 7000)
        self.assertEqual(ctx.exception.kind, "infra_pause")
        self.assertIn("paused_infra", run_module.RERUN)


if __name__ == "__main__":
    unittest.main()
