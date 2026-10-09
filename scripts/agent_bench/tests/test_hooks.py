import json
import subprocess
import unittest

import helpers

import cc_hooks
import cc_tools
import config
import workspace


@unittest.skipUnless(helpers.BINARY.exists(), "build Justless first (cargo build --release --locked)")
class HookTests(unittest.TestCase):
    def setUp(self):
        self.run_dir = helpers.tempdir(self)
        self.root = self.run_dir / "repo"
        self.root.mkdir()
        (self.root / "src").mkdir()
        body = "".join("/// Adds %d.\npub fn add_%03d(v: u64) -> u64 {\n    v + %d\n}\n\n" % (i, i, i) for i in range(80))
        (self.root / "src" / "big.rs").write_text(body)
        (self.root / "src" / "small.rs").write_text("pub fn one() -> u8 { 1 }\n")
        bindir = workspace.make_toolbin(self.run_dir / "bin")
        jl = self.run_dir / "jlbin"
        workspace.install_binary(helpers.BINARY, jl / "justless")
        self.env = workspace.tool_env(self.root, self.run_dir / "home", bindir, self.run_dir / "target", jl,
                                      tmp=self.run_dir / "tmp")
        workspace.git_baseline(self.root)
        subprocess.run([str(jl / "justless"), "init", "--hooks", "--agent", "claude"], cwd=str(self.root), env=self.env,
                       capture_output=True, check=True)
        self.ws = cc_tools.Workspace(self.root, self.run_dir, self.env, None, config.CC_BINARY)
        self.hooks = cc_hooks.HookRunner(self.root, self.env, None, session_id="s1",
                                         transcript_path=self.run_dir / "t.jsonl")

    def test_session_start_context(self):
        context = self.hooks.session_start()
        self.assertIn("Justless indexed", context)

    def test_bash_rewrite_executes_but_transcript_keeps_original(self):
        original = {"command": "git diff", "description": "diff"}
        executed, notes, blocked = self.hooks.pre_tool("Bash", original, "t1")
        self.assertIsNone(blocked)
        self.assertNotEqual(executed["command"], original["command"])
        self.assertIn("justless run --hook", executed["command"])
        self.assertEqual(original["command"], "git diff")

    def test_read_outline_replaces_structured_output(self):
        outcome = self.ws.read({"file_path": str(self.root / "src" / "big.rs")})
        before = outcome.text()
        notes = self.hooks.post_tool("Read", {"file_path": str(self.root / "src" / "big.rs")}, outcome, "t2", 1)
        self.assertTrue(outcome.extra.get("hook_replaced"))
        self.assertIn("revision", outcome.text())
        self.assertLess(len(outcome.text()), len(before))
        self.assertTrue(notes[0].startswith("PostToolUse:Read hook additional context: Justless outline."))

    def test_partial_and_small_reads_pass_through(self):
        outcome = self.ws.read({"file_path": str(self.root / "src" / "big.rs"), "offset": 1, "limit": 10})
        self.hooks.post_tool("Read", {"file_path": str(self.root / "src" / "big.rs"), "offset": 1, "limit": 10},
                             outcome, "t3", 1)
        self.assertFalse(outcome.extra.get("hook_replaced"))
        small = self.ws.read({"file_path": str(self.root / "src" / "small.rs")})
        self.hooks.post_tool("Read", {"file_path": str(self.root / "src" / "small.rs")}, small, "t4", 1)
        self.assertFalse(small.extra.get("hook_replaced"))

    def test_schema_invalid_output_falls_back(self):
        script = self.run_dir / "bad.py"
        script.write_text("import json,sys\nsys.stdin.read()\nprint(json.dumps({'hookSpecificOutput': "
                          "{'hookEventName': 'PostToolUse', 'updatedToolOutput': {'bogus': True}}}))\n")
        self.hooks.settings = {"PostToolUse": [{"matcher": "^Read$", "hooks": [
            {"type": "command", "command": "/usr/bin/python3 %s" % script, "timeout": 5}]}]}
        outcome = self.ws.read({"file_path": str(self.root / "src" / "small.rs")})
        before = outcome.text()
        self.hooks.post_tool("Read", {"file_path": str(self.root / "src" / "small.rs")}, outcome, "t5", 1)
        self.assertEqual(outcome.text(), before)
        self.assertTrue(self.hooks.events[-1].schema_rejected)

    def test_timeout_and_garbage_pass_through(self):
        self.hooks.settings = {"PreToolUse": [{"matcher": "^Bash$", "hooks": [
            {"type": "command", "command": "sleep 3", "timeout": 1},
            {"type": "command", "command": "echo '{not json'", "timeout": 5}]}]}
        executed, notes, blocked = self.hooks.pre_tool("Bash", {"command": "ls"}, "t6")
        self.assertEqual(executed, {"command": "ls"})
        self.assertTrue(self.hooks.events[-2].timed_out)
        self.assertEqual(self.hooks.events[-1].error, "unparsable output")

    def test_plain_arm_has_no_hooks(self):
        plain = self.run_dir / "plain"
        plain.mkdir()
        runner = cc_hooks.HookRunner(plain, self.env)
        self.assertFalse(runner.active)
        self.assertEqual(runner.session_start(), "")


if __name__ == "__main__":
    unittest.main()
