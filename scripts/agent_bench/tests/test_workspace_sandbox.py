import os
import subprocess
import unittest
from pathlib import Path

import helpers

import config
import sandbox
import workspace


class WorkspaceTests(unittest.TestCase):
    def test_mtime_normalization_and_baseline_commit(self):
        root = helpers.tempdir(self) / "repo"
        (root / "src").mkdir(parents=True)
        (root / "src" / "a.rs").write_text("fn a() {}\n")
        (root / "b.txt").write_text("b\n")
        workspace.normalize_mtimes(root)
        self.assertLessEqual(workspace.newest_mtime(root), config.MTIME)
        workspace.git_baseline(root)
        env = workspace.git_env(root)
        log = subprocess.run(["git", "log", "--format=%H %ad %an"], cwd=str(root), env=env, capture_output=True,
                             text=True).stdout.strip().split("\n")
        self.assertEqual(len(log), 1)
        self.assertIn("Bench", log[0])
        status = subprocess.run(["git", "status", "--porcelain"], cwd=str(root), env=env, capture_output=True,
                                text=True).stdout
        self.assertEqual(status, "")
        again = helpers.tempdir(self) / "repo"
        workspace.clone_tree(root, again)
        workspace.git_baseline(again)
        other = subprocess.run(["git", "rev-parse", "HEAD"], cwd=str(again), env=env, capture_output=True, text=True)
        first = subprocess.run(["git", "rev-parse", "HEAD"], cwd=str(root), env=env, capture_output=True, text=True)
        self.assertEqual(other.stdout, first.stdout)

    def test_toolbin_excludes_agent_and_justless_binaries(self):
        bindir = workspace.make_toolbin(helpers.tempdir(self) / "bin")
        names = set(os.listdir(str(bindir)))
        self.assertFalse({n for n in names if n.startswith(("justless", "claude", "codex"))})
        if (config.CARGO_HOME / "bin" / "cargo").exists():
            self.assertIn("cargo", names)

    def test_env_allowlist(self):
        os.environ["ANTHROPIC_API_KEY"] = "sk-ant-should-not-leak-000000000000"
        self.addCleanup(os.environ.pop, "ANTHROPIC_API_KEY", None)
        tmp = helpers.tempdir(self)
        env = workspace.tool_env(tmp / "repo", tmp / "home", tmp / "bin", tmp / "target")
        self.assertFalse([k for k in env if k.startswith("ANTHROPIC")])
        self.assertNotIn("justless", env["PATH"])

    def test_secret_scan(self):
        root = helpers.tempdir(self)
        (root / "a.env").write_text("API_KEY = 'abcdefghijklmnopqrstuvwxyz'\n")
        (root / "b.rs").write_text("let x = 1;\n")
        hits = workspace.secret_scan(root)
        self.assertEqual(hits, [("a.env", 1, "generic_secret")])
        self.assertEqual(workspace.secret_scan(root, ["a.env:1:generic_secret"]), [])


@unittest.skipUnless(sandbox.available(), "sandbox-exec not available")
class SandboxTests(unittest.TestCase):
    def test_self_test(self):
        results = sandbox.self_test(config, helpers.tempdir(self) / "sb")
        failed = [r for r in results if not r[1]]
        self.assertEqual(failed, [])

    def test_cargo_and_rg_work_inside(self):
        run_dir = helpers.tempdir(self)
        root = run_dir / "repo"
        helpers.make_crate(root)
        bindir = workspace.make_toolbin(run_dir / "bin")
        env = workspace.tool_env(root, run_dir / "home", bindir, run_dir / "target", tmp=run_dir / "tmp")
        text = sandbox.profile(run_dir, deny_read=sandbox.default_denies(config), allow_read=sandbox.default_allows(config),
                               allow_write=[config.CARGO_HOME])
        prof, _ = sandbox.write_profile(run_dir, text)
        for argv in (["rg", "-n", "clamp_add", "."], ["git", "--version"]):
            proc = subprocess.run(sandbox.wrap(argv, prof), cwd=str(root), env=env, capture_output=True)
            self.assertEqual(proc.returncode, 0, proc.stderr)
        if helpers.have_cargo():
            proc = subprocess.run(sandbox.wrap(["cargo", "test", "--offline", "-q"], prof), cwd=str(root), env=env,
                                  capture_output=True, timeout=600)
            self.assertEqual(proc.returncode, 0, proc.stderr.decode()[-500:])


if __name__ == "__main__":
    unittest.main()
