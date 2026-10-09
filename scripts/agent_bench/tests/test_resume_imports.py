import ast
import json
import threading
import unittest
from pathlib import Path

import helpers

import config
import loop
import run as run_module

TASKS = [{"id": "location-x-01", "family": "location", "repo": "x", "language": "rust", "prompt": "p",
          "setup": {}, "oracle": {}},
         {"id": "impact-x-01", "family": "impact", "repo": "x", "language": "rust", "prompt": "p",
          "setup": {}, "oracle": {}}]


class FakeSettings:
    def __init__(self, directory):
        self.dir = directory
        self.campaign = "c"
        self.justless_sha = "j"
        self.commit = "h"
        self.profile = type("P", (), {"capture_hash": "cap"})()
        self.logs = {"results": loop.JsonlWriter(directory / "results.jsonl")}
        self.locks = {}

    def task_lock(self, task_id):
        return self.locks.setdefault(task_id, threading.Lock())


class ResumeTests(unittest.TestCase):
    def setUp(self):
        self.dir = helpers.tempdir(self)
        self.settings = FakeSettings(self.dir)
        self.calls = []
        self.original = run_module.run_once
        self.addCleanup(setattr, run_module, "run_once", self.original)

    def fake(self, behavior):
        def run_once(settings, task, model, effort, arm, rep, attempt, manifest, position=None, phase="main"):
            rid = config.run_id(settings.campaign, model, effort, arm, task["id"], rep, manifest,
                                settings.justless_sha, settings.commit, settings.profile.capture_hash)
            self.calls.append((task["id"], arm, attempt))
            mode = behavior(task, arm, attempt)
            if mode == "crash":
                raise KeyboardInterrupt()
            return {"run_id": rid, "attempt": attempt, "task_id": task["id"], "arm": arm, "failure_mode": mode,
                    "success": mode == "success", "family": task["family"], "rep": rep, "model": model}
        run_module.run_once = run_once

    def rows(self):
        return [json.loads(l) for l in open(str(self.dir / "results.jsonl"))]

    def test_crash_then_resume_has_one_terminal_record_per_run(self):
        state = {"n": 0}

        def crash_on_third(task, arm, attempt):
            state["n"] += 1
            return "crash" if state["n"] == 3 else "success"
        self.fake(crash_on_third)
        with self.assertRaises(KeyboardInterrupt):
            run_module.schedule(self.settings, TASKS, ["claude-haiku-5-5"], ["a", "b"], 1, "cc-default", "m", 1)
        self.fake(lambda task, arm, attempt: "success")
        run_module.schedule(self.settings, TASKS, ["claude-haiku-5-5"], ["a", "b"], 1, "cc-default", "m", 1)
        terminal = [r for r in self.rows() if r["failure_mode"] in run_module.TERMINAL]
        self.assertEqual(len(terminal), 4)
        self.assertEqual(len({r["run_id"] for r in terminal}), 4)

    def test_harness_errors_rerun_then_intention_to_treat(self):
        self.fake(lambda task, arm, attempt: "harness_error" if task["id"] == "impact-x-01" and arm == "a" else "success")
        run_module.schedule(self.settings, TASKS, ["claude-haiku-5-5"], ["a", "b"], 1, "cc-default", "m", 1)
        rows = [r for r in self.rows() if r["task_id"] == "impact-x-01" and r["arm"] == "a"]
        self.assertEqual([r["attempt"] for r in rows], [1, 2, 3])
        self.assertEqual(rows[-1]["failure_mode"], "harness_unrecoverable")
        before = len(self.rows())
        run_module.schedule(self.settings, TASKS, ["claude-haiku-5-5"], ["a", "b"], 1, "cc-default", "m", 1)
        self.assertEqual(len(self.rows()), before)

    def test_arm_order_is_seeded_permutation(self):
        order = config.arm_order("claude-haiku-5-5", "t", 1, ["a", "b", "c", "d"])
        self.assertEqual(sorted(order), ["a", "b", "c", "d"])
        self.assertEqual(order, config.arm_order("claude-haiku-5-5", "t", 1, ["a", "b", "c", "d"]))


class ImportTests(unittest.TestCase):
    STDLIB_EXTRA = {"config", "accounting", "api", "cc_capture", "cc_hooks", "cc_profile", "cc_tools", "fake_agent",
                    "fake_api", "gen_tasks", "loop", "oracles", "parity_diff", "report", "sandbox", "stats", "tasks",
                    "workspace", "baseline_build", "proxy", "cc_anchor", "run", "dry_checks"}

    def test_stdlib_only_and_no_justless_in_generators(self):
        import sys
        stdlib = set(sys.builtin_module_names) | {
            "argparse", "ast", "base64", "concurrent", "copy", "datetime", "difflib", "hashlib", "hmac", "http", "io",
            "json", "math", "os", "pathlib", "random", "re", "shlex", "shutil", "signal", "socket", "subprocess", "sys",
            "tempfile", "threading", "time", "tokenize", "urllib", "uuid", "unittest", "select", "socketserver", "ssl",
            "queue", "struct", "collections", "itertools", "functools", "contextlib", "string", "textwrap"}
        for path in sorted(helpers.BENCH.glob("*.py")):
            tree = ast.parse(path.read_text())
            for node in ast.walk(tree):
                names = []
                if isinstance(node, ast.Import):
                    names = [a.name.split(".")[0] for a in node.names]
                elif isinstance(node, ast.ImportFrom) and node.module:
                    names = [node.module.split(".")[0]]
                for name in names:
                    self.assertIn(name, stdlib | self.STDLIB_EXTRA, "%s imports %s" % (path.name, name))
        for name in ("gen_tasks.py", "oracles.py"):
            text = (helpers.BENCH / name).read_text()
            calls = [line for line in text.split("\n") if "subprocess" in line and "justless" in line]
            self.assertEqual(calls, [], name)


if __name__ == "__main__":
    unittest.main()
