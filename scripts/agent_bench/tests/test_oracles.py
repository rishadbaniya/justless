import json
import subprocess
import unittest
from pathlib import Path

import helpers

import oracles
import workspace

LIB = '''pub fn clamp_add(a: u32, b: u32, max: u32) -> u32 {
    pick(a + b, max)
}

fn pick(sum: u32, max: u32) -> u32 {
    if sum > max {
        max
    } else {
        sum
    }
}

pub fn double(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps() {
        assert_eq!(clamp_add(5, 6, 10), 10);
        assert_eq!(clamp_add(2, 3, 10), 5);
    }

    #[test]
    fn doubles() {
        assert_eq!(double(4), 8);
    }
}
'''
MUTATED = LIB.replace("if sum > max {", "if sum < max {")


def answer(value):
    return "Done.\n```answer\n%s\n```" % json.dumps(value)


@unittest.skipUnless(helpers.have_cargo(), "cargo not available")
class RustOracleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        import shutil
        import tempfile
        cls.base = Path(tempfile.mkdtemp(prefix="ab-oracle-"))
        cls.pristine = cls.base / "pristine"
        helpers.make_crate(cls.pristine)
        (cls.pristine / "src" / "lib.rs").write_text(LIB)
        cls.target = cls.base / "target"
        cls.env = helpers.cargo_env(cls.pristine, cls.target)
        out = subprocess.run(["cargo", "test", "--offline", "--no-fail-fast"], cwd=str(cls.pristine), env=cls.env,
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT).stdout.decode()
        cls.baseline = oracles.parse_tests(out)
        cls.addClassCleanup(shutil.rmtree, str(cls.base), True)

    def setUp(self):
        self.root = helpers.tempdir(self) / "repo"
        workspace.clone_tree(self.pristine, self.root)
        (self.root / "src" / "lib.rs").write_text(MUTATED)
        self.reference = workspace.tree_files(self.root)
        self.task = {"id": "bugfix-demo-01", "family": "bugfix", "language": "rust",
                     "oracle": {"test_command": ["cargo", "test", "--offline", "--no-fail-fast"],
                                "mutated_file": "src/lib.rs", "mutated_span": [4, 10]},
                     "baseline": {"test_names": self.baseline, "flaky": []},
                     "controls": {"inverse_files": {"src/lib.rs": LIB}}}

    def runner(self, argv, timeout):
        env = dict(self.env)
        proc = subprocess.run(argv, cwd=str(self.root), env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                              timeout=timeout)
        return proc.returncode, proc.stdout.decode()

    def grade(self):
        return oracles.grade(self.task, self.root, self.reference, self.runner, "")

    def write(self, rel, text):
        (self.root / rel).write_text(text)

    def test_baseline_names(self):
        self.assertEqual(self.baseline.get("demo_crate@src/lib.rs::tests::clamps"), "ok")
        self.assertIn("integration@tests/integration.rs::integration_clamp", self.baseline)

    def test_correct_fix_passes(self):
        self.write("src/lib.rs", LIB)
        result = self.grade()
        self.assertTrue(result["success"], result)
        self.assertTrue(result["details"]["exact_revert"])

    def test_wrong_fix_fails(self):
        self.write("src/lib.rs", MUTATED.replace("x * 2", "x * 2 + 0"))
        self.assertEqual(self.grade()["failure_mode"], "wrong_fix")

    def test_editing_tests_dir_is_tamper(self):
        self.write("src/lib.rs", LIB)
        self.write("tests/integration.rs", helpers.CRATE_TEST.replace("1, 1, 1), 1", "1, 1, 1), 2"))
        self.assertEqual(self.grade()["failure_mode"], "test_tamper")

    def test_editing_inline_test_module_is_tamper(self):
        self.write("src/lib.rs", MUTATED.replace("assert_eq!(clamp_add(5, 6, 10), 10);", ""))
        self.assertEqual(self.grade()["failure_mode"], "test_tamper")

    def test_adding_ignore_is_tamper(self):
        self.write("src/lib.rs", MUTATED.replace("    #[test]\n    fn clamps", "    #[test]\n    #[ignore]\n    fn clamps"))
        self.assertEqual(self.grade()["failure_mode"], "test_tamper")

    def test_deleted_test_is_missing_from_name_set(self):
        results = dict(self.baseline)
        results.pop("demo_crate@src/lib.rs::tests::doubles")
        self.assertEqual(oracles.missing_names(self.baseline, results, set()),
                         ["demo_crate@src/lib.rs::tests::doubles"])

    def test_cargo_test_false_is_tamper(self):
        self.write("src/lib.rs", LIB)
        self.write("Cargo.toml", (self.root / "Cargo.toml").read_text() + "\n[lib]\ntest = false\n")
        self.assertEqual(self.grade()["failure_mode"], "test_tamper")

    def test_fix_elsewhere_is_untouched_site(self):
        bypass = MUTATED.replace("    pick(a + b, max)\n", "    let sum = a + b;\n    if sum > max { max } else { sum }\n")
        self.write("src/lib.rs", bypass)
        self.assertEqual(self.grade()["failure_mode"], "untouched_site")

    def test_compile_error_is_build_broken(self):
        self.write("src/lib.rs", MUTATED.replace("x * 2", "x * \"2\""))
        self.assertEqual(self.grade()["failure_mode"], "build_broken")

    def test_cargo_lock_churn_is_ignored(self):
        self.write("src/lib.rs", LIB)
        self.write("Cargo.lock", "# churn\n")
        changes = oracles.tree_changes(self.root, self.reference)
        self.assertNotIn("Cargo.lock", changes)
        self.assertTrue(self.grade()["success"])

    def test_allow_attribute_is_lint_suppressed(self):
        task = {"family": "lint", "language": "rust", "oracle": {}, "baseline": {"test_names": {}, "flaky": []}}
        self.write("src/lib.rs", "#![allow(clippy::all)]\n" + MUTATED)
        result = oracles.grade(task, self.root, self.reference, lambda a, t: (0, ""), "")
        self.assertEqual(result["failure_mode"], "lint_suppressed")


class LexerTests(unittest.TestCase):
    def test_mask_keeps_length_and_hides_strings(self):
        text = 'let s = "a { b"; // c }\nlet r = r#"x"#; let c = \'{\'; let l: &\'a str;'
        masked = oracles.rust_mask(text)
        self.assertEqual(len(masked), len(text))
        self.assertNotIn("{", masked)
        self.assertIn("&'a str", masked)

    def test_test_regions_and_functions(self):
        regions = oracles.rust_test_regions(LIB)
        self.assertTrue(any(start <= 16 <= end for start, end in regions))
        names = {f[1] for f in oracles.rust_functions(LIB)}
        self.assertTrue({"clamp_add", "pick", "double"} <= names)

    def test_impl_qualification(self):
        text = "struct A;\nimpl A {\n    fn go(&self) -> u32 { 1 }\n}\nimpl<T> Trait for B<T> {\n    fn run(&self) {}\n}\n"
        names = {f[1] for f in oracles.rust_functions(text)}
        self.assertIn("A::go", names)
        self.assertIn("B::run", names)

    def test_parse_tests_attributes_binaries(self):
        out = ("     Running unittests src/lib.rs (/t/debug/deps/demo-0123456789abcdef)\n"
               "test tests::a ... ok\ntest tests::b ... FAILED\n"
               "     Running tests/it.rs (/t/debug/deps/it-fedcba9876543210)\ntest c ... ignored\n"
               "   Doc-tests demo\ntest src/lib.rs - f (line 3) ... ok\n")
        parsed = oracles.parse_tests(out)
        self.assertEqual(parsed["demo@src/lib.rs::tests::a"], "ok")
        self.assertEqual(parsed["demo@src/lib.rs::tests::b"], "FAILED")
        self.assertEqual(parsed["it@tests/it.rs::c"], "ignored")
        self.assertEqual(parsed["doc:demo::src/lib.rs - f (line 3)"], "ok")


class PythonOracleTests(unittest.TestCase):
    def test_hidden_tests_and_tamper(self):
        root = helpers.tempdir(self)
        (root / "mod.py").write_text("def inc(x):\n    return x + 2\n")
        visible = "import unittest\nfrom mod import inc\nclass T(unittest.TestCase):\n    def test_a(self):\n        self.assertEqual(inc(1), 2)\n"
        hidden = "import unittest\nfrom mod import inc\nclass H(unittest.TestCase):\n    def test_b(self):\n        self.assertEqual(inc(5), 6)\n"
        (root / "bench_tests").mkdir()
        (root / "bench_tests" / "__init__.py").write_text("")
        (root / "bench_tests" / "test_v.py").write_text(visible)
        reference = workspace.tree_files(root)
        task = {"family": "bugfix", "language": "python",
                "oracle": {"test_command": ["python3", "-m", "unittest", "discover", "-s", "bench_tests", "-t", "."],
                           "visible_tests": {"bench_tests/test_v.py": visible},
                           "hidden_tests": {"bench_tests/test_h.py": hidden},
                           "mutated_file": "mod.py", "mutated_span": [0, 1]},
                "controls": {"inverse_files": {"mod.py": "def inc(x):\n    return x + 1\n"}}}

        def runner(argv, timeout):
            proc = subprocess.run(argv, cwd=str(root), stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            return proc.returncode, proc.stdout.decode()

        (root / "mod.py").write_text("def inc(x):\n    if x == 1:\n        return 2\n    return x + 2\n")
        self.assertEqual(oracles.grade(task, root, reference, runner, "")["failure_mode"], "wrong_fix")
        (root / "bench_tests" / "test_h.py").unlink()
        (root / "mod.py").write_text("def inc(x):\n    return x + 1\n")
        self.assertTrue(oracles.grade(task, root, reference, runner, "")["success"])
        (root / "bench_tests" / "test_h.py").unlink()
        (root / "bench_tests" / "test_v.py").write_text("pass\n")
        self.assertEqual(oracles.grade(task, root, reference, runner, "")["failure_mode"], "test_tamper")


class AnswerTests(unittest.TestCase):
    TASK = {"family": "location", "truth": {"file": "src/a.rs", "symbol": "Book::apply",
                                            "accepted_symbols": ["Book::apply", "apply"], "span": [10, 20]},
            "oracle": {}}

    def test_location_normalization(self):
        for symbol in ("Book::apply", "apply", "`Book::apply()`", "Book<T>::apply", "Book.apply"):
            result = oracles.grade_location(self.TASK, answer({"answers": [{"file": "./src/a.rs", "symbol": symbol}]}))
            self.assertTrue(result["success"], symbol)
        result = oracles.grade_location(self.TASK, answer({"answers": [{"file": "/r/src/a.rs", "symbol": "apply"}]}), "/r")
        self.assertTrue(result["success"])

    def test_location_first_answer_only(self):
        result = oracles.grade_location(self.TASK, answer({"answers": [{"file": "src/b.rs", "symbol": "x"},
                                                                       {"file": "src/a.rs", "symbol": "apply"}]}))
        self.assertEqual(result["failure_mode"], "wrong_answer")
        self.assertEqual(oracles.grade_location(self.TASK, "no block")["failure_mode"], "no_answer")

    def test_file_level_truth_uses_line(self):
        task = {"family": "location", "truth": {"file": "init.lua", "symbol": None, "span": [6, 6]}, "oracle": {}}
        self.assertTrue(oracles.grade_location(task, answer({"answers": [{"file": "init.lua", "line": 7}]}))["success"])
        self.assertFalse(oracles.grade_location(task, answer({"answers": [{"file": "init.lua", "line": 40}]}))["success"])

    def test_impact_three_class(self):
        task = {"family": "impact", "truth": {"required": ["a", "b", "c", "d", "e"], "neutral": ["README.md"]},
                "oracle": {"recall": 0.9, "precision": 0.8}}
        full = oracles.grade_impact(task, answer({"files": ["a", "b", "c", "d", "e", "README.md"]}))
        self.assertTrue(full["success"])
        self.assertEqual(full["precision"], 1.0)
        missing = oracles.grade_impact(task, answer({"files": ["a", "b", "c", "d"]}))
        self.assertFalse(missing["success"])
        self.assertEqual(missing["recall"], 0.8)
        wrong = oracles.grade_impact(task, answer({"files": ["a", "b", "c", "d", "e", "x", "y"]}))
        self.assertAlmostEqual(wrong["precision"], 5 / 7.0, places=3)
        self.assertFalse(wrong["success"])


if __name__ == "__main__":
    unittest.main()
