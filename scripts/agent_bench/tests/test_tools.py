import os
import unittest

import helpers

import cc_tools
import config
import workspace


class ToolTests(unittest.TestCase):
    def setUp(self):
        self.run_dir = helpers.tempdir(self)
        self.root = self.run_dir / "repo"
        self.root.mkdir()
        (self.root / "sub").mkdir()
        env = workspace.tool_env(self.root, self.run_dir / "home", workspace.make_toolbin(self.run_dir / "bin"),
                                 self.run_dir / "target", tmp=self.run_dir / "tmp")
        self.ws = cc_tools.Workspace(self.root, self.run_dir, env, None, config.CC_BINARY,
                                     token_counter=cc_tools.estimate_tokens)

    def lines(self, name, size, width=50):
        path = self.root / name
        with open(str(path), "w") as handle:
            written = 0
            i = 0
            while written < size:
                i += 1
                line = ("row %05d " % i).ljust(width - 1, "y") + "\n"
                line = line[:size - written]
                handle.write(line)
                written += len(line)
        return path

    def bash(self, command, **extra):
        tool_input = dict({"command": command}, **extra)
        outcome = self.ws.bash(tool_input, tool_input, "toolu_x")
        return outcome.text(), outcome.is_error, outcome

    def test_valid_inline_and_persisted(self):
        self.lines("a.txt", 29000)
        self.lines("b.txt", 30001 + 1)
        text, error, _ = self.bash("cat a.txt")
        self.assertFalse(error)
        self.assertEqual(len(text), 28999)
        text, error, outcome = self.bash("cat b.txt")
        self.assertTrue(text.startswith("<persisted-output>\nOutput too large (29.3KB). Full output saved to: "))
        self.assertIn("\n\nPreview (first 2KB):\nrow 00001", text)
        self.assertTrue(text.endswith("row 00040 " + "y" * 39 + "\n...\n</persisted-output>"))
        self.assertEqual(outcome.extra["render_meta"]["render"], "persisted")
        saved = text.split("Full output saved to: ")[1].split("\n")[0]
        self.assertEqual(len(open(saved).read()), 30002)

    def test_failure_inline_and_excerpt(self):
        self.lines("f.txt", 9000)
        text, error, _ = self.bash("cat f.txt; exit 3")
        self.assertTrue(error)
        self.assertTrue(text.startswith("Exit code 3\nrow 00001"))
        self.assertEqual(len(text), 9011)
        self.lines("g.txt", 12000)
        text, error, outcome = self.bash("cat g.txt; exit 3")
        self.assertIn("\n\n... [2011 characters truncated] ...\n\n", text)
        self.assertEqual(len(text), 10039)
        self.assertEqual(outcome.extra["render_meta"]["render"], "excerpt")
        self.lines("h.txt", 40000)
        text, _, _ = self.bash("cat h.txt; exit 3")
        self.assertIn("[20011 characters truncated]", text)
        self.assertTrue(text.endswith("row 00600 " + "y" * 39))

    def test_benign_exit_one(self):
        self.assertEqual(self.bash("grep -rn zzznope .")[:2], ("(Bash completed with no output)", False))
        self.assertTrue(cc_tools.benign_exit1("echo x >> f && git diff --exit-code"))
        self.assertFalse(cc_tools.benign_exit1("cargo test"))
        self.assertFalse(cc_tools.benign_exit1("justless run --hook -- cargo test"))
        text, error, _ = self.bash("false")
        self.assertEqual((text, error), ("Exit code 1", True))

    def test_streams_merge_and_trim(self):
        text, error, _ = self.bash("echo a; echo b >&2; echo c; exit 4")
        self.assertEqual(text, "Exit code 4\na\nb\nc")
        self.assertEqual(self.bash("printf 'x\\n\\n\\n'")[0], "x")

    def test_cwd_persistence_and_reset(self):
        text, _, outcome = self.bash("cd sub && pwd")
        self.assertTrue(text.endswith("/repo/sub"))
        self.assertIn("# Environment update", outcome.notes[0])
        self.assertTrue(self.bash("pwd")[0].endswith("/repo/sub"))
        text, _, _ = self.bash("cd /usr && pwd")
        self.assertEqual(text, "/usr\nShell cwd was reset to %s" % self.root)
        self.assertEqual(self.bash("pwd")[0], str(self.root))
        self.bash("cd sub && false")
        self.assertEqual(self.bash("pwd")[0], str(self.root))

    def test_timeout_and_background(self):
        text, error, _ = self.bash("sleep 3", timeout=500)
        self.assertTrue(error)
        self.assertEqual(text, "Exit code 143\nCommand timed out after 0.5s")
        text, error, _ = self.bash("echo hi", run_in_background=True)
        self.assertEqual((text, error), (cc_tools.BACKGROUND_DISABLED, True))

    def test_read_rules(self):
        (self.root / "small.rs").write_text("fn a() {}\nfn b() {}\n")
        out = self.ws.read({"file_path": str(self.root / "small.rs")})
        self.assertEqual(out.text(), "1\tfn a() {}\n2\tfn b() {}\n3\t")
        self.assertEqual(out.structured["file"]["numLines"], out.structured["file"]["totalLines"])
        self.lines("big.txt", 300000)
        out = self.ws.read({"file_path": str(self.root / "big.txt")})
        self.assertTrue(out.is_error)
        self.assertIn("File content (293.0KB) exceeds maximum allowed size (256KB)", out.text())
        self.lines("mid.txt", 100000)
        out = self.ws.read({"file_path": str(self.root / "mid.txt")})
        self.assertEqual(out.kind, "partial_view")
        self.assertIn("[Truncated: PARTIAL view", out.notes[0])
        out = self.ws.read({"file_path": str(self.root / "mid.txt"), "allow_large": True})
        self.assertEqual(out.kind, "allow_large")
        self.assertEqual(out.structured["file"]["numLines"], 2001)
        self.lines("long.txt", 3000 * 50, width=50)
        out = self.ws.read({"file_path": str(self.root / "long.txt"), "offset": 100, "limit": 5})
        self.assertTrue(out.text().startswith("100\trow 00100"))
        self.assertEqual(out.text().count("\n"), 4)
        (self.root / "empty.txt").write_text("")
        self.assertIn("contents are empty", self.ws.read({"file_path": str(self.root / "empty.txt")}).text())
        missing = self.ws.read({"file_path": str(self.root / "nope.txt")})
        self.assertTrue(missing.is_error)
        self.assertIn("File does not exist", missing.text())

    def test_edit_and_write_rules(self):
        path = self.root / "code.rs"
        path.write_text("let a = 1;\nlet b = 1;\n")
        out = self.ws.edit({"file_path": str(path), "old_string": "let a = 1;", "new_string": "let a = 2;"})
        self.assertFalse(out.is_error)
        self.assertEqual(out.text(), "The file %s has been updated successfully." % path)
        path.write_text("let a = 1;\nlet a = 1;\n")
        out = self.ws.edit({"file_path": str(path), "old_string": "let a = 1;", "new_string": "let a = 3;"})
        self.assertIn("Found 2 matches of the string to replace", out.text())
        path.write_text("let a = 2;\nlet b = 1;\nextra\n")
        out = self.ws.edit({"file_path": str(path), "old_string": "let b = 1;", "new_string": "let b = 5;"})
        self.assertFalse(out.is_error)
        self.assertTrue(out.text().endswith(cc_tools.READ_SUFFIX))
        out = self.ws.edit({"file_path": str(path), "old_string": "zzz", "new_string": "y"})
        self.assertTrue(out.text().startswith("<tool_use_error>String to replace not found"))
        self.lines("part.txt", 100000)
        self.ws.read({"file_path": str(self.root / "part.txt")})
        out = self.ws.write({"file_path": str(self.root / "part.txt"), "content": "x"})
        self.assertTrue(out.is_error)
        self.assertIn("File has not been read yet", out.text())
        out = self.ws.write({"file_path": str(self.root / "new.txt"), "content": "x"})
        self.assertTrue(out.text().startswith("File created successfully at: "))

    def test_changed_file_note(self):
        path = self.root / "watched.rs"
        path.write_text("fn a() {}\n")
        self.ws.read({"file_path": str(path)})
        os.utime(str(path), (1, 1))
        path.write_text("fn a() {}\nfn b() {}\n")
        notes = self.ws.changed_files_notes()
        self.assertEqual(len(notes), 1)
        self.assertIn("changed on disk since you last read it", notes[0])
        self.assertIn("2\tfn b() {}", notes[0])
        self.assertEqual(self.ws.changed_files_notes(), [])

    def test_validation_messages(self):
        text = cc_tools.validate("Read", {"path": "/x"})
        self.assertEqual(text, "<tool_use_error>InputValidationError: Read failed due to the following issues:\n"
                               "The required parameter `file_path` is missing\n"
                               "An unexpected parameter `path` was provided</tool_use_error>")
        self.assertIsNone(cc_tools.validate("Bash", {"command": "ls", "timeout": 5}))


if __name__ == "__main__":
    unittest.main()
