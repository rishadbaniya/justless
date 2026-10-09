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


class CargoCacheTests(unittest.TestCase):
    """The Cargo git cache can hold unmutated originals of task files."""

    def fake_cargo_home(self):
        home = helpers.tempdir(self) / "cargo"
        for ident, revisions in (("proj-0123456789abcdef", ["aaaaaaa"]), ("dep-fedcba9876543210", ["1111111", "2222222"]),
                                 ("other-00112233445566aa", ["3333333"])):
            for revision in revisions:
                (home / "git" / "checkouts" / ident / revision).mkdir(parents=True)
            subprocess.run(["git", "init", "-q", "--bare", str(home / "git" / "db" / ident)], check=True)
        return home

    def store_blob(self, db, text):
        subprocess.run(["git", "--git-dir", str(db), "hash-object", "-w", "--stdin"], input=text.encode(),
                       capture_output=True, check=True)

    def test_hides_own_repository_unlocked_revisions_and_unused_databases(self):
        home = self.fake_cargo_home()
        repo = helpers.tempdir(self) / "repo"
        repo.mkdir()
        (repo / "Cargo.lock").write_text('[[package]]\nname = "dep"\nsource = "git+https://example.invalid/org/dep.git'
                                         '?branch=main#1111111aaaabbbbccccddddeeeeffff00001111"\n')
        hidden = {str(p.relative_to(home)) for p in sandbox.cargo_git_denies(home, ["proj"], repo)}
        self.assertIn("git/checkouts/proj-0123456789abcdef", hidden)
        self.assertIn("git/db/proj-0123456789abcdef", hidden)
        self.assertIn("git/checkouts/dep-fedcba9876543210/2222222", hidden)
        self.assertNotIn("git/checkouts/dep-fedcba9876543210/1111111", hidden)
        self.assertNotIn("git/db/dep-fedcba9876543210", hidden)
        self.assertIn("git/db/other-00112233445566aa", hidden)
        (repo / "Cargo.lock").write_text('source = "git+https://example.invalid/org/proj#aaaaaaa1"\n')
        with self.assertRaises(ValueError):
            sandbox.cargo_git_denies(home, ["proj"], repo)

    def test_originals_reachable_in_a_needed_database_are_reported(self):
        home = self.fake_cargo_home()
        repo = helpers.tempdir(self) / "repo"
        repo.mkdir()
        (repo / "Cargo.lock").write_text('source = "git+https://example.invalid/org/dep.git#1111111aaaa"\n')
        self.store_blob(home / "git" / "db" / "dep-fedcba9876543210", "fn original() {}\n")
        self.store_blob(home / "git" / "db" / "proj-0123456789abcdef", "fn hidden_copy() {}\n")
        denies = sandbox.cargo_git_denies(home, ["proj"], repo)
        found = sandbox.reachable_blobs({"src/a.rs": "fn original() {}\n", "src/b.rs": "fn hidden_copy() {}\n",
                                         "src/c.rs": "fn new() {}\n"}, home, denies)
        self.assertEqual(found, {"src/a.rs": ["dep-fedcba9876543210"]})
        task = {"controls": {"inverse_files": {"src/a.rs": "fn original() {}\n"}}}
        self.assertEqual(sandbox.answer_key_exposure(task, home, denies), [("src/a.rs", "dep-fedcba9876543210")])


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
