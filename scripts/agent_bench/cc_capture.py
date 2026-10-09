"""Drive real Claude Code against the local fake API and record what it sends.

No paid call is made: Claude Code runs with a dummy key, ANTHROPIC_BASE_URL pointing at
fake_api on 127.0.0.1, nonessential traffic disabled, an isolated HOME, and a sandbox
profile that denies every outbound connection except 127.0.0.1.

Scripted sessions on a fixture project exercise each parity item. The output, written
to $AGENT_BENCH_HOME/cc_capture and never committed (it contains Claude Code's prompt
text), is used by the harness for the system prompt and tool definitions, and by
parity_diff for byte-level comparison of tool results.
"""

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

import config
import fake_api
import workspace

RECORDER = r'''import json, sys, time
data = sys.stdin.read()
try:
    value = json.loads(data)
except ValueError:
    value = {"unparsed": data}
with open(sys.argv[1], "a") as handle:
    handle.write(json.dumps({"t": time.time(), "input": value}) + "\n")
'''

BAD_HOOK = r'''import json, sys
value = json.loads(sys.stdin.read() or "{}")
path = (value.get("tool_input") or {}).get("file_path", "")
if path.endswith("hookbad.txt"):
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PostToolUse",
          "updatedToolOutput": {"bogus": True}}}))
elif path.endswith("hookgood.txt"):
    out = value["tool_response"]
    out["file"]["content"] = "replaced by test hook"
    out["file"]["numLines"] = 1
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PostToolUse",
          "updatedToolOutput": out, "additionalContext": "context from test hook"}}))
else:
    print("{}")
'''


def lines_file(path, count, width=50, prefix="line"):
    with open(path, "w") as handle:
        for i in range(1, count + 1):
            text = "%s %05d " % (prefix, i)
            handle.write(text + "x" * (width - len(text) - 1) + "\n")


def sized_file(path, size, prefix="row"):
    """A file of exactly `size` bytes made of numbered 50-byte lines."""
    with open(path, "w") as handle:
        written = 0
        i = 0
        while written < size:
            i += 1
            text = "%s %05d " % (prefix, i)
            line = text + "y" * (49 - len(text)) + "\n"
            line = line[: size - written]
            handle.write(line)
            written += len(line)


def rust_file(path, functions=80):
    with open(path, "w") as handle:
        handle.write("//! Generated module with many small functions.\n\n")
        for i in range(functions):
            handle.write("/// Adds %d to the input.\npub fn add_%03d(value: u64) -> u64 {\n"
                         "    let step = %d;\n    value.wrapping_add(step)\n}\n\n" % (i, i, i))


def build_project(root, justless_binary=None):
    """The capture fixture: fixtures/failing plus generated files, as a fresh git repo."""
    if root.exists():
        shutil.rmtree(root)
    shutil.copytree(config.REPO / "fixtures" / "failing", root,
                    ignore=shutil.ignore_patterns("target", ".justless", "Cargo.lock"))
    sized_file(root / "out9k.txt", 9000)
    sized_file(root / "out12k.txt", 12000)
    sized_file(root / "out29k.txt", 29000)
    sized_file(root / "out31k.txt", 31000)
    sized_file(root / "out40k.txt", 40000)
    lines_file(root / "lines3000.txt", 3000)
    sized_file(root / "mid_150k.txt", 158000, "mid")
    sized_file(root / "part_150k.txt", 158000, "part")
    sized_file(root / "big_310k.txt", 310000, "big")
    lines_file(root / "lines2500.txt", 2500, prefix="ofs")
    (root / "empty.txt").write_text("")
    (root / "notes.txt").write_text("first note\n")
    (root / "notes2.txt").write_text("second note\n")
    (root / "hookbad.txt").write_text("original content of hookbad\n")
    (root / "hookgood.txt").write_text("original content of hookgood\n")
    (root / "dup.txt").write_text("same\nsame\n")
    rust_file(root / "src" / "big.rs")
    with open(root / "src" / "lib.rs", "a") as handle:
        handle.write("pub mod big;\n")
    (root / "AGENTS.md").write_text("# Agents\n\nImported project conventions: keep functions small.\n")
    (root / "CLAUDE.md").write_text("# Project\n\nSee @AGENTS.md for conventions.\n")
    workspace.normalize_mtimes(root)
    workspace.git_baseline(root, env=workspace.git_env(root))


def tools_session():
    """Scripted plain-arm session. Each step is (label, step)."""
    T = fake_api.tool_use
    R = "{root}"
    steps = [
        ("bash_valid_29k", T("Bash", {"command": "cat out29k.txt", "description": "Show file"})),
        ("bash_valid_31k", T("Bash", {"command": "cat out31k.txt", "description": "Show file"})),
        ("bash_fail_9k", T("Bash", {"command": "cat out9k.txt; exit 3", "description": "Fail"})),
        ("bash_fail_12k", T("Bash", {"command": "cat out12k.txt; exit 3", "description": "Fail"})),
        ("bash_fail_40k", T("Bash", {"command": "cat out40k.txt; exit 3", "description": "Fail"})),
        ("bash_fail_mixed", T("Bash", {"command": "echo to-stdout; echo to-stderr >&2; exit 2",
                                       "description": "Fail"})),
        ("bash_ok_stderr", T("Bash", {"command": "echo out-line; echo warn-line >&2", "description": "Warn"})),
        ("bash_grep_nomatch", T("Bash", {"command": "grep -rn zzznomatchzzz src", "description": "Search"})),
        ("bash_false", T("Bash", {"command": "false", "description": "Fail"})),
        ("bash_git_diff_exit1", T("Bash", {"command": "echo change >> notes.txt && git diff --exit-code",
                                           "description": "Diff"})),
        ("bash_true_empty", T("Bash", {"command": "true", "description": "Nothing"})),
        ("bash_cd_inside", T("Bash", {"command": "cd src && pwd", "description": "cd"})),
        ("bash_pwd_after_inside", T("Bash", {"command": "pwd", "description": "pwd"})),
        ("bash_cd_outside", T("Bash", {"command": "cd /usr && pwd", "description": "cd"})),
        ("bash_pwd_after_outside", T("Bash", {"command": "pwd", "description": "pwd"})),
        ("bash_cd_fail", T("Bash", {"command": "cd " + R + "/tests && false", "description": "cd"})),
        ("bash_pwd_after_fail", T("Bash", {"command": "pwd", "description": "pwd"})),
        ("bash_cd_root", T("Bash", {"command": "cd " + R + " && pwd", "description": "cd"})),
        ("bash_timeout", T("Bash", {"command": "sleep 4; echo late", "timeout": 1000, "description": "Sleep"})),
        ("bash_background", T("Bash", {"command": "echo bg", "run_in_background": True, "description": "bg"})),
        ("bash_env", T("Bash", {"command": "env | sort | cut -d= -f1 | tr '\\n' ' '", "description": "env"})),
        ("bash_which", T("Bash", {"command": "type grep find rg 2>&1 | head -5", "description": "type"})),
        ("bash_grep_ignored", T("Bash", {"command": "mkdir -p target && echo zzzignored > target/x.txt && "
                                                    "grep -rn zzzignored . ; echo exit=$?", "description": "s"})),
        ("read_small", T("Read", {"file_path": R + "/src/pricing.rs"})),
        ("read_3000", T("Read", {"file_path": R + "/lines3000.txt"})),
        ("read_mid", T("Read", {"file_path": R + "/mid_150k.txt"})),
        ("read_big", T("Read", {"file_path": R + "/big_310k.txt"})),
        ("read_allow_large", T("Read", {"file_path": R + "/mid_150k.txt", "allow_large": True})),
        ("read_offset", T("Read", {"file_path": R + "/lines3000.txt", "offset": 100, "limit": 5})),
        ("read_offset_big", T("Read", {"file_path": R + "/big_310k.txt", "offset": 10, "limit": 3})),
        ("read_past_end", T("Read", {"file_path": R + "/src/pricing.rs", "offset": 500, "limit": 10})),
        ("read_empty", T("Read", {"file_path": R + "/empty.txt"})),
        ("read_missing", T("Read", {"file_path": R + "/nope.txt"})),
        ("read_dir", T("Read", {"file_path": R + "/src"})),
        ("read_relative", T("Read", {"file_path": "src/pricing.rs"})),
        ("read_repeat_small", T("Read", {"file_path": R + "/src/pricing.rs"})),
        ("edit_after_read", T("Edit", {"file_path": R + "/src/pricing.rs",
                                       "old_string": "subtotal + fee + 1", "new_string": "subtotal + fee"})),
        ("edit_unread", T("Edit", {"file_path": R + "/src/obsolete.rs",
                                   "old_string": "    0\n", "new_string": "    1\n"})),
        ("bash_external_change", T("Bash", {"command": "printf 'extra\\n' >> src/obsolete.rs", "description": "c"})),
        ("edit_stale", T("Edit", {"file_path": R + "/src/obsolete.rs",
                                  "old_string": "    1\n", "new_string": "    2\n"})),
        ("edit_not_found", T("Edit", {"file_path": R + "/src/pricing.rs",
                                      "old_string": "no such text", "new_string": "x"})),
        ("edit_multiple", T("Edit", {"file_path": R + "/dup.txt", "old_string": "same", "new_string": "diff"})),
        ("edit_identical", T("Edit", {"file_path": R + "/dup.txt", "old_string": "same", "new_string": "same"})),
        ("edit_replace_all", T("Edit", {"file_path": R + "/dup.txt", "old_string": "same",
                                        "new_string": "diff", "replace_all": True})),
        ("edit_missing_file", T("Edit", {"file_path": R + "/nope.txt", "old_string": "a", "new_string": "b"})),
        ("write_new", T("Write", {"file_path": R + "/created.txt", "content": "created\n"})),
        ("write_unread_existing", T("Write", {"file_path": R + "/notes2.txt", "content": "rewritten\n"})),
        ("read_part_offset", T("Read", {"file_path": R + "/lines2500.txt", "offset": 1, "limit": 10})),
        ("write_after_offset_read", T("Write", {"file_path": R + "/lines2500.txt", "content": "short\n"})),
        ("read_part_partial", T("Read", {"file_path": R + "/part_150k.txt"})),
        ("write_after_partial", T("Write", {"file_path": R + "/part_150k.txt", "content": "short\n"})),
        ("input_invalid", T("Read", {"path": R + "/src/pricing.rs"})),
        ("final", fake_api.final("Done.")),
    ]
    return steps


def hooks_session():
    """Scripted Justless-arm session."""
    T = fake_api.tool_use
    R = "{root}"
    return [
        ("hook_cargo_test", T("Bash", {"command": "cargo test --offline", "description": "Run tests",
                                       "timeout": 300000})),
        ("hook_git_diff", T("Bash", {"command": "echo more >> notes.txt && git diff", "description": "diff"})),
        ("hook_git_diff_plain", T("Bash", {"command": "git diff", "description": "diff"})),
        ("hook_grep", T("Bash", {"command": "grep -rn checkout_total src", "description": "grep"})),
        ("hook_rg", T("Bash", {"command": "rg -n checkout_total src", "description": "rg"})),
        ("hook_rg_nomatch", T("Bash", {"command": "rg -n zzznomatchzzz src", "description": "rg"})),
        ("hook_git_diff_exit", T("Bash", {"command": "git diff --exit-code", "description": "diff"})),
        ("hook_cat_big", T("Bash", {"command": "cat src/big.rs", "description": "cat"})),
        ("hook_read_big", T("Read", {"file_path": R + "/src/big.rs"})),
        ("hook_read_big_offset", T("Read", {"file_path": R + "/src/big.rs", "offset": 1, "limit": 20})),
        ("hook_read_big_allow_large", T("Read", {"file_path": R + "/src/big.rs", "allow_large": True})),
        ("hook_read_bad", T("Read", {"file_path": R + "/hookbad.txt"})),
        ("hook_read_good", T("Read", {"file_path": R + "/hookgood.txt"})),
        ("hook_edit_after_outline", T("Edit", {"file_path": R + "/src/big.rs",
                                               "old_string": "    let step = 7;\n", "new_string": "    let step = 70;\n"})),
        ("hook_justless_query", T("Bash", {"command": "justless query checkout_total", "description": "query"})),
        ("hook_cargo_fail_long", T("Bash", {"command": "cargo test --offline 2>&1; exit 101",
                                            "description": "tests", "timeout": 300000})),
        ("final", fake_api.final("Done.")),
    ]


def claude_env(home, base_url, path_dirs):
    env = {
        "PATH": ":".join([str(p) for p in path_dirs] + [config.SYSTEM_PATH]),
        "HOME": str(home),
        "TMPDIR": str(home / "tmp"),
        "LANG": "en_US.UTF-8",
        "TERM": "dumb",
        "ANTHROPIC_API_KEY": "sk-ant-dummy-offline-capture",
        "ANTHROPIC_BASE_URL": base_url,
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_AUTOUPDATER": "1",
        "CARGO_HOME": str(config.CARGO_HOME),
        "RUSTUP_HOME": str(config.RUSTUP_HOME),
        "CARGO_NET_OFFLINE": "true",
    }
    (home / "tmp").mkdir(parents=True, exist_ok=True)
    return env


def network_profile(path):
    path.write_text("(version 1)\n(allow default)\n(deny network-outbound)\n"
                    "(allow network-outbound (remote ip \"localhost:*\"))\n"
                    "(allow network-outbound (remote unix-socket))\n")
    return path


def run_session(cc_binary, workdir, name, steps, model, justless_binary=None, prompt="Follow the scripted steps."):
    workdir.mkdir(parents=True, exist_ok=True)
    root = workdir / ("proj_" + name)
    home = workdir / ("home_" + name)
    if home.exists():
        shutil.rmtree(home)
    home.mkdir(parents=True)
    build_project(root)
    bindir = workdir / ("bin_" + name)
    workspace.make_toolbin(bindir, justless_binary)
    hooks_log = workdir / ("hooks_%s.jsonl" % name)
    if hooks_log.exists():
        hooks_log.unlink()
    if justless_binary:
        env = workspace.tool_env(root, home, bindir, root / "target")
        subprocess.run([str(bindir / "justless"), "init", "--hooks", "--agent", "claude"], cwd=str(root),
                       env=env, capture_output=True, check=True)
        recorder = workdir / "recorder.py"
        recorder.write_text(RECORDER)
        bad = workdir / "bad_hook.py"
        bad.write_text(BAD_HOOK)
        settings_path = root / ".claude" / "settings.json"
        settings = json.loads(settings_path.read_text())
        hooks = settings.setdefault("hooks", {})
        record = "/usr/bin/python3 %s %s" % (recorder, hooks_log)
        for event in ("SessionStart", "PreToolUse", "PostToolUse"):
            hooks.setdefault(event, []).append({"matcher": ".*" if event != "SessionStart" else "",
                                                "hooks": [{"type": "command", "command": record, "timeout": 10}]})
        hooks["PostToolUse"].append({"matcher": "^Read$", "hooks": [
            {"type": "command", "command": "/usr/bin/python3 %s" % bad, "timeout": 10}]})
        settings_path.write_text(json.dumps(settings, indent=2) + "\n")
        workspace.normalize_mtimes(root)
        workspace.git_commit_all(root, "install", env=workspace.git_env(root))
    log = workdir / ("requests_%s.jsonl" % name)
    if log.exists():
        log.unlink()
    api = fake_api.FakeAPI(0, str(log)).start()
    labels = [label for label, _ in steps]
    api.register("default", fake_api.Script([step for _, step in steps], {"root": str(root)}))
    env = claude_env(home, api.base_url, [bindir])
    profile = network_profile(workdir / "net.sb")
    argv = ["/usr/bin/sandbox-exec", "-f", str(profile), cc_binary, "-p", prompt, "--model", model,
            "--output-format", "stream-json", "--verbose", "--permission-mode", "bypassPermissions",
            "--max-turns", str(len(steps) + 5)]
    started = time.time()
    proc = subprocess.run(argv, cwd=str(root), env=env, stdin=subprocess.DEVNULL, capture_output=True,
                          timeout=900)
    api.stop()
    (workdir / ("stream_%s.jsonl" % name)).write_bytes(proc.stdout)
    (workdir / ("stderr_%s.txt" % name)).write_bytes(proc.stderr)
    return {"name": name, "root": str(root), "home": str(home), "labels": labels, "exit": proc.returncode,
            "seconds": round(time.time() - started, 2), "requests": str(log), "hooks_log": str(hooks_log),
            "count_tokens_calls": api.count_token_calls}


def load_requests(path):
    rows = []
    with open(path) as handle:
        for line in handle:
            row = json.loads(line)
            if row.get("method") == "POST" and row.get("path", "").split("?")[0].endswith("/v1/messages"):
                rows.append(row)
    return rows


def extract_results(rows, labels):
    """Map each scripted label to the tool_result Claude Code returned for it."""
    by_id = {}
    order = []
    for index, row in enumerate(rows):
        response = row.get("response") or {}
        for block in response.get("content") or []:
            if block.get("type") == "tool_use":
                by_id[block["id"]] = {"request_index": index, "tool": block["name"], "input": block["input"]}
                order.append(block["id"])
    results = {}
    for index, row in enumerate(rows):
        messages = row["body"].get("messages") or []
        for mi, message in enumerate(messages):
            if message.get("role") != "user" or not isinstance(message.get("content"), list):
                continue
            blocks = message["content"]
            for bi, block in enumerate(blocks):
                if block.get("type") != "tool_result" or block["tool_use_id"] not in by_id:
                    continue
                if block["tool_use_id"] in results:
                    continue
                trailing = [b for b in blocks[bi + 1:] if b.get("type") != "tool_result"]
                following = []
                for later in messages[mi + 1:]:
                    if later.get("role") == "assistant":
                        break
                    following.append(later)
                results[block["tool_use_id"]] = dict(by_id[block["tool_use_id"]], content=block.get("content"),
                                                     is_error=block.get("is_error", False), trailing=trailing,
                                                     following=following, message_index=mi)
    out = {}
    for label, tool_id in zip(labels, order):
        out[label] = results.get(tool_id, {"missing": True, "tool": by_id[tool_id]["tool"]})
    return out


def summarize_request(body, headers):
    system = body.get("system")
    return {
        "headers": {k: v for k, v in headers.items() if k.lower().startswith(("anthropic", "x-app", "user-agent"))},
        "keys": sorted(body),
        "model": body.get("model"),
        "max_tokens": body.get("max_tokens"),
        "thinking": body.get("thinking"),
        "output_config": body.get("output_config"),
        "context_management": body.get("context_management"),
        "top_cache_control": body.get("cache_control"),
        "stream": body.get("stream"),
        "tool_names": [t.get("name") for t in body.get("tools") or []],
        "system_blocks": [{"chars": len(b.get("text", "")), "cache_control": b.get("cache_control")}
                          for b in (system if isinstance(system, list) else [])],
        "message_cache_control": [
            [mi, bi, b.get("cache_control")]
            for mi, m in enumerate(body.get("messages") or []) if isinstance(m.get("content"), list)
            for bi, b in enumerate(m["content"]) if isinstance(b, dict) and b.get("cache_control")],
        "message_roles": [m.get("role") for m in body.get("messages") or []],
    }


def capture(cc_binary, justless_binary, out_dir, workdir):
    out_dir.mkdir(parents=True, exist_ok=True)
    report = {"cc_binary": cc_binary, "cc_version": subprocess.run([cc_binary, "--version"], capture_output=True,
                                                                    text=True).stdout.strip(),
              "justless_binary": str(justless_binary), "justless_sha256": config.sha256_file(justless_binary),
              "captured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "sessions": {}, "models": {}}
    sessions = [("tools", tools_session(), None, "claude-sonnet-5-5"),
                ("hooks", hooks_session(), justless_binary, "claude-sonnet-5-5")]
    for name, steps, binary, model in sessions:
        meta = run_session(cc_binary, workdir, name, steps, model, binary)
        rows = load_requests(meta["requests"])
        first = rows[0]["body"] if rows else {}
        meta["first_request"] = summarize_request(first, rows[0]["headers"]) if rows else {}
        meta["all_requests"] = [summarize_request(r["body"], r["headers"]) for r in rows]
        meta["results"] = extract_results(rows, meta["labels"])
        meta["append_only_violations"] = [r["response"]["error"]["message"] for r in rows
                                          if r.get("status") == 400]
        hooks = []
        if Path(meta["hooks_log"]).exists():
            with open(meta["hooks_log"]) as handle:
                hooks = [json.loads(line) for line in handle]
        meta["hook_inputs"] = hooks
        report["sessions"][name] = meta
        if rows:
            with open(out_dir / ("first_request_%s.json" % name), "w") as handle:
                json.dump({"headers": rows[0]["headers"], "body": first}, handle, indent=1)
    for model in config.MODELS:
        meta = run_session(cc_binary, workdir, "model_" + model, [("final", fake_api.final("ok"))], model)
        rows = load_requests(meta["requests"])
        if rows:
            report["models"][model] = summarize_request(rows[0]["body"], rows[0]["headers"])
    with open(out_dir / "capture.json", "w") as handle:
        json.dump(report, handle, indent=1, default=str)
    return report


def main(argv=None):
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cc-binary", default=config.CC_BINARY)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--home", default=str(config.HOME))
    args = parser.parse_args(argv)
    out = Path(args.home) / "cc_capture"
    report = capture(args.cc_binary, Path(args.binary).resolve(), out, Path(args.home) / "cc_capture_work")
    print(json.dumps({k: (v if k != "sessions" else {n: {"exit": s["exit"], "requests": len(s["all_requests"])}
                                                     for n, s in v.items()}) for k, v in report.items()
                      if k != "models"}, indent=1))


if __name__ == "__main__":
    sys.exit(main())
