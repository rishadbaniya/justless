"""The large-output family: its own task set, the callers and one-rule lint oracles, the regression history and
cleanup check, the scripted plain paths and the cost estimate. No private task content is used."""

import json
import os
import shutil
import subprocess
import unittest
from pathlib import Path

import helpers

import accounting
import fake_agent
import gen_large
import gen_tasks
import oracles
import run as run_module
import tasks as tasks_module
import workspace


def answer(value):
    return "Done.\n```answer\n%s\n```" % json.dumps(value)


def callers_task():
    units = [{"file": "src/a.rs", "symbol": "Engine::start", "accepted": ["Engine::start", "start"]},
             {"file": "src/a.rs", "symbol": "helper", "accepted": ["helper"]},
             {"file": "src/b.rs", "symbol": "run", "accepted": ["run"]},
             {"file": "tests/it.rs", "symbol": "starts", "accepted": ["starts"]}]
    return {"id": "large-callers-x-01", "family": "large", "kind": "callers", "repo": "x", "language": "rust",
            "prompt": "p", "setup": {"kind": "none"},
            "oracle": {"recall": 0.9, "precision": 0.8, "symbol": "compute_total", "definition_file": "src/c.rs"},
            "truth": {"required": units,
                      "neutral": [{"file": "src/d.rs", "symbol": "doc_only", "accepted": ["doc_only"]}]}}


def edit_task(kind, **extra):
    task = {"id": "large-%s-x-01" % kind, "family": "large", "kind": kind, "repo": "x", "language": "rust",
            "prompt": "p", "visible_command": "cargo test --offline --no-fail-fast",
            "setup": {"kind": "mutation", "edits": []},
            "oracle": {"test_command": ["cargo", "test", "--offline", "--no-fail-fast"], "mutated_file": "src/lib.rs",
                       "mutated_span": [0, 3], "mutated_fn": "f", "anchor_line": "a < b", "noop_comment": " // c"},
            "controls": {"inverse": [{"file": "src/lib.rs", "old": "a <= b", "new": "a < b"}],
                         "inverse_files": {"src/lib.rs": "fixed"}}}
    task.update(extra)
    return task


class TaskSetTests(unittest.TestCase):
    def test_large_tasks_never_enter_the_main_manifest(self):
        home = helpers.tempdir(self)
        main_task = {"id": "impact-x-01", "family": "impact", "repo": "x", "language": "rust", "prompt": "p",
                     "setup": {"kind": "none"}, "oracle": {}}
        tasks_module.save_private(home, main_task)
        before = tasks_module.manifest_hash(tasks_module.load_private(home))
        tasks_module.save_private(home, callers_task(), "large")
        self.assertEqual([t["id"] for t in tasks_module.load_private(home)], ["impact-x-01"])
        self.assertEqual([t["id"] for t in tasks_module.load_private(home, "large")], ["large-callers-x-01"])
        self.assertEqual(before, tasks_module.manifest_hash(tasks_module.load_private(home)))
        self.assertEqual(tasks_module.private_dir(home, "large"), home / "tasks" / "large")
        args = type("A", (), {"home": home, "task_set": "large", "tasks": "all-private"})()
        self.assertEqual([t["id"] for t in run_module.load_tasks(args)], ["large-callers-x-01"])
        main_args = type("A", (), {"home": home, "tasks": "all-private"})()
        self.assertEqual([t["id"] for t in run_module.load_tasks(main_args)], ["impact-x-01"])
        self.assertNotEqual(run_module.full_manifest(args), run_module.full_manifest(main_args))

    def test_each_set_freezes_its_own_manifest(self):
        home = helpers.tempdir(self)
        tasks_module.save_private(home, callers_task(), "large")
        args = type("A", (), {"home": home, "approve": True, "task_set": "large"})()
        gen_tasks.review(args)
        self.assertTrue((home / "tasks" / "large" / "_manifest.json").exists())
        self.assertFalse((home / "tasks" / "_manifest.json").exists())
        manifest = run_module.full_manifest(type("A", (), {"home": home, "task_set": "large"})())
        run_module.require_frozen_manifest(home, manifest, "large")
        with self.assertRaises(SystemExit):
            run_module.require_frozen_manifest(home, manifest)

    def test_large_set_never_defaults_into_the_main_campaign(self):
        home = helpers.tempdir(self)
        seen = {}
        names = ("cmd_dry_run", "cmd_validate_tasks", "cmd_estimate", "cmd_run")
        original = {n: getattr(run_module, n) for n in names}
        try:
            run_module.cmd_dry_run = lambda args: seen.setdefault("dry-run", args.campaign) and 0
            run_module.cmd_validate_tasks = lambda args: seen.setdefault("validate-tasks", args.campaign) and 0
            run_module.cmd_estimate = lambda args: seen.setdefault("estimate", args.campaign) and 0
            run_module.cmd_run = lambda args, phase="main": seen.setdefault("run", args.campaign) and 0
            for command in ("dry-run", "validate-tasks", "estimate", "run"):
                run_module.main([command, "--home", str(home), "--task-set", "large"])
            with self.assertRaises(SystemExit):
                run_module.main(["run", "--home", str(home), "--task-set", "large", "--campaign", "main"])
        finally:
            for name, value in original.items():
                setattr(run_module, name, value)
        self.assertEqual(seen, {"dry-run": "dry-large", "validate-tasks": "validate-large", "estimate": "large",
                                "run": "large"})

    def test_large_tasks_need_a_kind(self):
        task = callers_task()
        self.assertEqual(tasks_module.kind(task), "callers")
        self.assertFalse(tasks_module.is_edit(task))
        self.assertTrue(tasks_module.is_edit(edit_task("regression")))
        self.assertEqual(tasks_module.kind({"family": "lint"}), "lint")
        with self.assertRaises(tasks_module.TaskError):
            tasks_module.validate(dict(task, kind="unknown"))
        with self.assertRaises(tasks_module.TaskError):
            tasks_module.private_dir("/tmp", "nope")


class CallersOracleTests(unittest.TestCase):
    def test_recall_precision_and_neutral_mentions(self):
        task = callers_task()
        units = task["truth"]["required"]
        everything = [{"file": u["file"], "symbol": u["symbol"]} for u in units]
        self.assertTrue(oracles.grade_callers(task, answer({"callers": everything}))["success"])
        with_neutral = everything + [{"file": "src/d.rs", "symbol": "doc_only"}]
        self.assertTrue(oracles.grade_callers(task, answer({"callers": with_neutral}))["success"])
        missing = oracles.grade_callers(task, answer({"callers": everything[:3]}))
        self.assertEqual((missing["success"], missing["failure_mode"], missing["recall"]), (False, "partial_answer", 0.75))
        noisy = everything + [{"file": "src/e.rs", "symbol": "other"}, {"file": "src/a.rs", "symbol": "Other::start"}]
        self.assertFalse(oracles.grade_callers(task, answer({"callers": noisy}))["success"])
        self.assertEqual(oracles.grade_callers(task, "no block")["failure_mode"], "no_answer")
        self.assertEqual(oracles.grade_callers(task, answer({"files": ["src/a.rs"]}))["failure_mode"], "no_answer")
        wrong = oracles.grade_callers(task, answer({"callers": [{"file": "src/zz.rs", "symbol": "x"}]}))
        self.assertEqual(wrong["failure_mode"], "wrong_answer")

    def test_tool_printed_forms_succeed(self):
        task = callers_task()
        root = "/tmp/run/repo"
        forms = [{"file": root + "/src/a.rs:12", "symbol": "crate::a::Engine::start"},
                 {"file": "./src/a.rs", "symbol": "src/a.rs::helper"},
                 {"file": "src/b.rs", "symbol": "b::run()"},
                 {"file": "tests/it.rs", "symbol": "fn starts"}]
        self.assertTrue(oracles.grade_callers(task, answer({"callers": forms}), root)["success"])
        self.assertEqual(gen_tasks.truth_checks(task), [])

    def test_grade_dispatches_on_kind(self):
        task = callers_task()
        result = oracles.grade(task, None, None, None, answer({"callers": []}))
        self.assertEqual(result["failure_mode"], "wrong_answer")


class CallerTruthTests(unittest.TestCase):
    def setUp(self):
        if not Path("/opt/homebrew/bin/rg").exists() and not shutil.which("rg"):
            self.skipTest("rg is not installed")
        self.root = helpers.tempdir(self)
        (self.root / "src").mkdir()
        (self.root / "src" / "lib.rs").write_text(
            "pub fn compute_total(a: u32) -> u32 {\n    a + 1\n}\n\n"
            "pub struct Engine;\n\nimpl Engine {\n    pub fn start(&self) -> u32 {\n        compute_total(1)\n    }\n}\n\n"
            "// compute_total is documented here\nfn helper() -> u32 {\n    let f = compute_total;\n    f(2)\n}\n\n"
            "fn quiet() {\n    // calls compute_total in a comment only\n}\n\n"
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn totals() {\n        assert_eq!(super::compute_total(1), 2);\n    }\n}\n")
        (self.root / "src" / "other.rs").write_text("use crate::compute_total;\n\npub fn run() -> u32 {\n"
                                                    "    compute_total(3)\n}\n")

    def test_units_are_enclosing_functions_with_comment_mentions_neutral(self):
        required, neutral, method_calls = gen_large.caller_units(self.root, "compute_total", "src/lib.rs")
        self.assertEqual([(u["file"], u["symbol"]) for u in required],
                         [("src/lib.rs", "Engine::start"), ("src/lib.rs", "helper"), ("src/lib.rs", "totals"),
                          ("src/other.rs", "run")])
        self.assertEqual([(u["file"], u["symbol"]) for u in neutral], [("src/lib.rs", "quiet")])
        self.assertEqual(method_calls, 0)
        self.assertTrue(gen_large.free_function(self.root, "src/lib.rs", "compute_total"))
        self.assertFalse(gen_large.free_function(self.root, "src/lib.rs", "start"))

    def test_method_call_mentions_are_counted(self):
        (self.root / "src" / "more.rs").write_text("fn x(v: &V) -> u32 {\n    v.compute_total()\n}\n")
        _, _, method_calls = gen_large.caller_units(self.root, "compute_total", "src/lib.rs")
        self.assertEqual(method_calls, 1)


class LintRuleOracleTests(unittest.TestCase):
    @staticmethod
    def clippy_json(items):
        lines = []
        for code, path, line in items:
            lines.append(json.dumps({"reason": "compiler-message", "message": {
                "level": "warning", "message": "m %s %d" % (code, line), "code": {"code": code},
                "spans": [{"file_name": path, "line_start": line}]}}))
        return "\n".join(lines) + "\n"

    def task(self):
        return {"oracle": {"rule": "clippy::needless_return",
                           "start_counts": {"clippy::needless_return": 2, "clippy::len_zero": 3}}}

    def test_rule_must_go_and_nothing_may_grow(self):
        task = self.task()
        remaining = self.clippy_json([("clippy::needless_return", "a.rs", 1), ("clippy::len_zero", "a.rs", 5)])
        self.assertEqual(oracles.grade_lint_rule(task, remaining, 0, {})["failure_mode"], "lint_remaining")
        grown = self.clippy_json([("clippy::len_zero", "a.rs", n) for n in range(4)])
        verdict = oracles.grade_lint_rule(task, grown, 0, {})
        self.assertEqual(verdict["failure_mode"], "new_warnings")
        self.assertEqual(verdict["details"]["grown"], {"clippy::len_zero": [3, 4]})
        fewer = self.clippy_json([("clippy::len_zero", "a.rs", 5)])
        self.assertIsNone(oracles.grade_lint_rule(task, fewer, 0, {}))
        broken = "error[E0425]: cannot find value\nerror: could not compile `x`\n"
        self.assertEqual(oracles.grade_lint_rule(task, broken, 101, {})["failure_mode"], "build_broken")


class RegressionTests(unittest.TestCase):
    def test_cleanup_share(self):
        root = helpers.tempdir(self)
        (root / "a.rs").write_text("one\nreturn two;\nthree\n")
        contexts = [{"file": "a.rs", "text": "return two;", "line": 2}, {"file": "a.rs", "text": "x == true", "line": 3},
                    {"file": "gone.rs", "text": "y", "line": 1}, {"file": "a.rs", "text": "three", "line": 3}]
        share, lost = oracles.cleanup_preserved(root, contexts)
        self.assertEqual(share, 0.5)
        self.assertEqual(lost, ["a.rs:3", "gone.rs:1"])
        self.assertEqual(oracles.cleanup_preserved(root, []), (1.0, []))

    def test_history_setup_commits_the_cleanup_on_top_of_a_baseline(self):
        if not shutil.which("git"):
            self.skipTest("git is not installed")
        root = helpers.tempdir(self) / "repo"
        root.mkdir()
        original = "fn f(a: u32, b: u32) -> bool {\n    a < b\n}\n\nfn g() -> u32 {\n    1\n}\n"
        edits = tasks_module.with_new_offsets([
            {"file": "lib.rs", "offset": original.index("a < b"), "old": "a < b", "new": "a <= b"},
            {"file": "lib.rs", "offset": original.index("    1\n"), "old": "    1", "new": "    return 1;"}])
        (root / "lib.rs").write_text(original)
        tasks_module.apply_edits(root, edits)
        final = (root / "lib.rs").read_text()
        task = {"setup": {"kind": "history", "edits": edits, "commit_message": "refactor: tidy"}}
        env = workspace.git_env(root, root.parent / "home")
        workspace.git_task_history(root, task, env=env)
        self.assertEqual((root / "lib.rs").read_text(), final)

        def git(*argv):
            return subprocess.run(["git"] + list(argv), cwd=str(root), env=env, capture_output=True, text=True,
                                  check=True).stdout
        self.assertEqual(git("log", "--format=%s").split("\n")[:2], ["refactor: tidy", "baseline"])
        self.assertEqual(git("show", "HEAD~1:lib.rs"), original)
        self.assertEqual(git("status", "--short"), "")
        self.assertEqual(int(os.stat(str(root / "lib.rs")).st_mtime), helpers.config.MTIME)
        plain = helpers.tempdir(self) / "plain"
        plain.mkdir()
        (plain / "x.txt").write_text("x")
        workspace.git_task_history(plain, {"setup": {"kind": "mutation"}}, env=workspace.git_env(plain))
        self.assertEqual(subprocess.run(["git", "rev-list", "--count", "HEAD"], cwd=str(plain), capture_output=True,
                                        text=True).stdout.strip(), "1")


class ScriptedPathTests(unittest.TestCase):
    @staticmethod
    def commands(steps):
        out = []
        for step in steps:
            step = step.get("when_persisted", step)
            for block in step["content"]:
                if block.get("type") == "tool_use":
                    out.append((block["name"], block["input"].get("command") or block["input"].get("file_path")))
        return out

    def test_saved_output_is_read_only_when_claude_code_saved_it(self):
        import fake_api
        steps = fake_agent.correct(edit_task("regression"))
        show = next(b for s in steps for b in s.get("content", []) if b.get("input", {}).get("command") == "git show HEAD")
        self.assertEqual(show["id"], "toolu_large_show_1")
        self.assertIn(("Read", "{persist}/toolu_large_show_1.txt"), self.commands(steps))
        start = next(i for i, s in enumerate(steps) if "when_persisted" in s)

        def body(text):
            return {"messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_large_show_1", "content": text},
                {"type": "text", "text": "<system-reminder>note</system-reminder>"}]}]}

        for text, expected in (("<persisted-output>\nOutput too large", "{persist}/toolu_large_show_1.txt"),
                               ("diff --git a/x b/x", "{root}/src/lib.rs")):
            script = fake_api.Script(steps, {})
            script.index = start
            step = script.next(body(text))
            self.assertEqual(step["content"][-1]["input"]["file_path"], expected)

    def test_each_kind_has_a_plain_path_and_expected_modes(self):
        bug = edit_task("bugfix")
        path = self.commands(fake_agent.correct(bug))
        self.assertEqual(path[0], ("Bash", "cargo test --offline --no-fail-fast"))
        self.assertIn(("Read", "{root}/src/lib.rs"), path)
        lint = edit_task("lint", visible_command="cargo clippy --offline --all-targets",
                         large={"clippy_human_bytes": 50000})
        self.assertIn(("Read", "{persist}/toolu_large_lint_1.txt"), self.commands(fake_agent.correct(lint)))
        callers = self.commands(fake_agent.correct(callers_task()))
        self.assertEqual(callers[0][0], "Bash")
        self.assertEqual(sorted(p for n, p in callers if n == "Read"),
                         ["{root}/src/a.rs", "{root}/src/b.rs", "{root}/tests/it.rs"])
        self.assertIn(("Bash", "git checkout HEAD~1 -- ."), self.commands(fake_agent.wrong(edit_task("regression"))))
        self.assertEqual(fake_agent.expected("wrong", edit_task("regression")), "reverted_cleanup")
        self.assertEqual(fake_agent.expected("noop", callers_task()), "no_answer")
        self.assertTrue(fake_agent.applies("cfgtest", edit_task("regression")))
        self.assertTrue(fake_agent.applies("qualified", callers_task()))
        self.assertFalse(fake_agent.applies("qualified", edit_task("lint")))


class GeneratorHelperTests(unittest.TestCase):
    def test_known_failures_and_excerpt(self):
        baseline = {"test_names": {"pkg@src/lib.rs::m::t_one": "FAILED", "pkg@src/lib.rs::m::t_two": "ok"},
                    "flaky": []}
        text = gen_large.known_failures(baseline)
        self.assertIn("`m::t_one` in package `pkg`", text)
        self.assertEqual(gen_large.known_failures({"test_names": {"a::b": "ok"}}), "")
        shown = gen_large.render_failure(101, "x" * 7000 + "MIDDLE" + "y" * 28000 + "LATE" + "z" * 1000)
        self.assertNotIn("MIDDLE", shown)
        self.assertNotIn("LATE", shown)
        self.assertTrue(shown.startswith("Exit code 101"))

    def test_test_module_files_are_found(self):
        root = helpers.tempdir(self)
        (root / "src" / "price").mkdir(parents=True)
        (root / "src" / "lib.rs").write_text("pub mod price;\n#[cfg(test)]\nmod lib_tests;\n")
        (root / "src" / "price" / "mod.rs").write_text("#[cfg(test)] mod influence_tests;\npub fn p() {}\n")
        found = gen_large.test_module_files(root)
        self.assertIn("src/lib_tests.rs", found)
        self.assertIn("src/price/influence_tests.rs", found)
        self.assertNotIn("src/price/mod.rs", found)

    def test_pick_spreads_and_spaces(self):
        import random
        cands = [{"file": "f%d.rs" % (i % 3), "offset": i * 50, "lint": "l%d" % (i % 2)} for i in range(30)]
        chosen = gen_large.pick(cands, 6, random.Random(1), 2, key=lambda c: c["lint"])
        self.assertEqual(len(chosen), 6)
        for c in chosen:
            self.assertTrue(gen_large.spaced([o for o in chosen if o is not c], c))
        self.assertLessEqual(max(sum(1 for o in chosen if o["file"] == f) for f in ("f0.rs", "f1.rs", "f2.rs")), 2)


class EstimateTests(unittest.TestCase):
    def test_run_model_and_large_plan(self):
        small = accounting.run_cost("claude-sonnet-5-5", 7500, 6)
        self.assertGreater(small, 0.03)
        self.assertLess(small, 0.07)
        big = accounting.run_cost("claude-sonnet-5-5", 120000, 10)
        self.assertGreater(big, small * 3)
        paths = {"t%d" % i: {"a": {"seen": 100000, "requests": 8}} for i in range(8)}
        out = accounting.estimate_large(paths, spent=70.0)
        self.assertEqual(out["rows"][0]["runs"], 48)
        expected = 8 * 3 * 2 * accounting.run_cost("claude-sonnet-5-5", 125000, 12)
        self.assertAlmostEqual(out["expected_usd"], round(expected, 2), places=2)
        self.assertEqual(out["room_usd"], 10.0)
        self.assertEqual(out["fits"], out["conservative_usd"] <= 10.0)

    def test_dry_run_paths_read_successful_large_runs(self):
        directory = helpers.tempdir(self)
        rows = [{"family": "large", "task_id": "t", "arm": "a", "failure_mode": "success",
                 "totals": {"tool_bytes_seen": 70000, "requests": 7, "tool_bytes_source": 90000}},
                {"family": "large", "task_id": "t", "arm": "b", "failure_mode": "wrong_fix", "totals": {"requests": 1}},
                {"family": "bugfix", "task_id": "u", "arm": "a", "failure_mode": "success", "totals": {"requests": 1}}]
        (directory / "results.jsonl").write_text("\n".join(json.dumps(r) for r in rows) + "\n")
        self.assertEqual(accounting.dry_run_paths(directory),
                         {"t": {"a": {"seen": 70000, "requests": 7, "source": 90000}}})


if __name__ == "__main__":
    unittest.main()
