"""Replay cc_capture's scripted sessions through the harness and diff against real Claude Code.

Compared per scripted step: tool_result text, is_error, and the mid-conversation system note
that follows it; plus tool names, first-message roles and hook stdin field sets. Paths,
durations, run IDs, cargo hashes and timestamps are normalized. A difference fails the step
unless the label is a documented deviation. Writes $AGENT_BENCH_HOME/cc_capture/parity_report.json.
"""

import argparse
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import accounting  # noqa: E402
import api as api_module  # noqa: E402
import cc_capture  # noqa: E402
import cc_hooks  # noqa: E402
import cc_profile  # noqa: E402
import cc_tools  # noqa: E402
import config  # noqa: E402
import fake_api  # noqa: E402
import loop  # noqa: E402
import sandbox  # noqa: E402
import workspace  # noqa: E402

DOCUMENTED = {
    "bash_background": "harness rejects background Bash in every arm",
    "bash_env": "Claude Code adds CLAUDE_CODE_* and other variables to the shell environment",
    "bash_which": "function source path differs (snapshot file vs harness prelude)",
    "input_invalid": "same message; Claude Code also lists the unexpected parameter first",
}
# ugrep searches with several threads, so match order is not stable between runs.
ORDER_INSENSITIVE = {"hook_grep", "bash_grep_nomatch"}
NOTE_KEYS = ("PARTIAL view", "Environment update", "hasn't heard", "changed on disk", "hook additional context")


def normalize(text, roots):
    if text is None:
        return None
    if not isinstance(text, str):
        text = json.dumps(text)
    for root, name in roots:
        text = text.replace(root, name)
    text = re.sub(r"Full output saved to: \S+", "Full output saved to: {path}", text)
    text = re.sub(r"(thread '[^']*') \(\d+\)", r"\1 ({tid})", text)
    text = re.sub(r" *Blocking waiting for file lock on [^\n]*\n", "", text)
    text = re.sub(r"run-[A-Za-z0-9]{8,}", "run-{id}", text)
    text = re.sub(r"\b\d+\.\d+s\b", "{t}s", text)
    text = re.sub(r"-[0-9a-f]{16}\b", "-{hash}", text)
    text = re.sub(r"revision [0-9a-f]{12,}", "revision {rev}", text)
    text = re.sub(r"'[0-9a-f]{12,}#", "'{rev}#", text)
    return text


def note_kinds(messages):
    out = []
    for message in messages or []:
        content = message.get("content")
        text = content if isinstance(content, str) else "".join(b.get("text", "") for b in content or [])
        out.append(sorted(k for k in NOTE_KEYS if k in text))
    return out


def replay(name, steps, workdir, justless_binary, cc_binary, profile, root_length=None):
    run_dir = workdir / ("replay_run_" + name)
    if root_length:
        # Tool output embeds absolute paths, so byte counts and excerpt cuts depend on the root's length.
        stem = len(str(workdir)) + 1 + len("/repo")
        width = root_length - stem
        if width >= 2:
            run_dir = workdir / (("r" + name + "_" * width)[:width])
    root = run_dir / "repo"
    if run_dir.exists():
        shutil.rmtree(str(run_dir))
    run_dir.mkdir(parents=True)
    cc_capture.build_project(root)
    home = run_dir / "home"
    bindir = workspace.make_toolbin(run_dir / "bin")
    justless_dir = None
    if justless_binary:
        justless_dir = run_dir / "jlbin"
        workspace.install_binary(justless_binary, justless_dir / "justless")
    env = workspace.tool_env(root.resolve(), home, bindir, root / "target", justless_dir, tmp=run_dir / "tmp")
    if justless_binary:
        subprocess.run([str(justless_dir / "justless"), "init", "--hooks", "--agent", "claude"], cwd=str(root), env=env,
                       capture_output=True, check=True)
        bad = workdir / "bad_hook.py"
        bad.write_text(cc_capture.BAD_HOOK)
        settings_path = root / ".claude" / "settings.json"
        settings = json.loads(settings_path.read_text())
        settings["hooks"]["PostToolUse"].append({"matcher": "^Read$", "hooks": [
            {"type": "command", "command": "/usr/bin/python3 %s" % bad, "timeout": 10}]})
        settings_path.write_text(json.dumps(settings, indent=2) + "\n")
        workspace.normalize_mtimes(root)
        workspace.git_commit_all(root, "install", env=workspace.git_env(root))
    fake = fake_api.FakeAPI(0).start()
    nonce = "0" * 16
    fake.register(nonce, fake_api.Script([s for _, s in steps], {"root": str(root.resolve())}))
    client = api_module.Client("sk-ant-fake-parity-key-0000000000000000", base_url=fake.base_url,
                               sleep=lambda s: None)
    tools_ws = cc_tools.Workspace(root, run_dir, env, None, cc_binary, model="claude-sonnet-5-5",
                                  token_counter=lambda text: fake_api.tokens_of(fake_api.canonical(
                                      ["user", text])))
    hooks = None
    context = ""
    if justless_binary:
        hooks = cc_hooks.HookRunner(root.resolve(), env, None, session_id=nonce, transcript_path=home / "t.jsonl")
        context = hooks.session_start()
    logs = {k: loop.JsonlWriter(run_dir / ("%s.jsonl" % k)) for k in ("requests", "tools", "attempts")}
    ledger = accounting.Ledger(run_dir / "ledger.jsonl", 1000.0)
    spec = {"campaign": "parity", "model": "claude-sonnet-5-5", "effort": "medium", "arm": "b" if justless_binary else "a",
            "run_id": "parity-" + name, "attempt": 1, "nonce": nonce, "home": str(home), "prompt": "Follow the scripted steps.",
            "run_budget": 1000.0}
    run = loop.Run(spec, client, profile, ledger, accounting.Pricing(), tools_ws, hooks, logs,
                   limits={"turn_limit": len(steps) + 5, "token_limit": 10 ** 9})
    run.run(context)
    fake.stop()
    results = {}
    uses = []
    for i, message in enumerate(run.messages):
        if message["role"] == "assistant":
            for block in message["content"]:
                if block.get("type") == "tool_use":
                    uses.append(block["id"])
    by_id = {}
    for i, message in enumerate(run.messages):
        if message["role"] != "user" or not isinstance(message["content"], list):
            continue
        for block in message["content"]:
            if block.get("type") == "tool_result":
                following = []
                for later in run.messages[i + 1:]:
                    if later["role"] == "assistant":
                        break
                    following.append(later)
                by_id[block["tool_use_id"]] = {"content": block.get("content"), "is_error": block.get("is_error", False),
                                               "following": following}
    labels = [label for label, _ in steps]
    for label, tool_id in zip(labels, uses):
        results[label] = by_id.get(tool_id, {"missing": True})
    hook_payload_keys = {}
    if hooks:
        hook_payload_keys = {
            "PreToolUse": sorted(dict(hooks.base("PreToolUse"), prompt_id="", permission_mode="", effort={}, tool_name="",
                                      tool_input={}, tool_use_id="")),
            "PostToolUse": sorted(dict(hooks.base("PostToolUse"), prompt_id="", permission_mode="", effort={}, tool_name="",
                                       tool_input={}, tool_response={}, tool_use_id="", duration_ms=0)),
            "SessionStart": sorted(dict(hooks.base("SessionStart"), source="")),
        }
    return {"root": str(root.resolve()), "home": str(home), "run_dir": str(run_dir), "results": results,
            "messages": run.messages, "hook_keys": hook_payload_keys,
            "error": str(run.harness_error) if run.harness_error else None}


def compare(capture_session, harness, name):
    roots = [(capture_session["root"], "{root}"), (capture_session["home"], "{home}"),
             (harness["root"], "{root}"), (harness["run_dir"], "{run}")]
    items = []
    for label, cc in capture_session["results"].items():
        ours = harness["results"].get(label, {"missing": True})
        if label == "final":
            continue
        a = normalize(cc.get("content"), roots)
        b = normalize(ours.get("content"), roots)
        same_text = a == b
        if not same_text and label in ORDER_INSENSITIVE and a is not None and b is not None:
            same_text = sorted(a.split("\n")) == sorted(b.split("\n"))
        same_error = bool(cc.get("is_error")) == bool(ours.get("is_error"))
        same_notes = note_kinds(cc.get("following")) == note_kinds(ours.get("following"))
        status = "verified" if same_text and same_error and same_notes else (
            "documented_deviation" if label in DOCUMENTED else "differs")
        item = {"session": name, "label": label, "status": status, "text": same_text, "is_error": same_error,
                "notes": same_notes}
        if status != "verified":
            item["cc"] = a or ""
            item["harness"] = b or ""
            item["cc_notes"] = note_kinds(cc.get("following"))
            item["harness_notes"] = note_kinds(ours.get("following"))
            item["reason"] = DOCUMENTED.get(label)
        items.append(item)
    return items


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, default=config.HOME)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--cc-binary", default=config.CC_BINARY)
    args = parser.parse_args(argv)
    capture = json.loads((args.home / "cc_capture" / "capture.json").read_text())
    profile = cc_profile.load(args.home, "capture")
    workdir = args.home / "parity_work"
    workdir.mkdir(parents=True, exist_ok=True)
    report = {"capture_hash": profile.capture_hash, "items": [], "checks": {}}
    for name, steps, binary in (("tools", cc_capture.tools_session(), None),
                                ("hooks", cc_capture.hooks_session(), Path(args.binary).resolve())):
        harness = replay(name, steps, workdir, binary, args.cc_binary, profile,
                         len(capture["sessions"][name]["root"]))
        report["items"] += compare(capture["sessions"][name], harness, name)
        if harness["error"]:
            report["checks"]["%s_harness_error" % name] = harness["error"]
        if binary:
            captured = {}
            for entry in capture["sessions"][name].get("hook_inputs") or []:
                event = entry["input"].get("hook_event_name")
                captured.setdefault(event, sorted(entry["input"]))
            report["checks"]["hook_stdin_fields"] = {
                event: {"cc": captured.get(event), "harness": harness["hook_keys"].get(event),
                        "match": captured.get(event) == harness["hook_keys"].get(event)}
                for event in ("SessionStart", "PreToolUse", "PostToolUse")}
    first = json.loads((args.home / "cc_capture" / "first_request_tools.json").read_text())["body"]
    report["checks"]["tool_names"] = {"cc_subset": [t["name"] for t in first["tools"] if t["name"] in cc_profile.TOOL_NAMES],
                                      "harness": [t["name"] for t in profile.tools()]}
    report["checks"]["first_message_roles"] = {"cc": [m["role"] for m in first["messages"]],
                                               "harness": ["user", "system"]}
    counts = {}
    for item in report["items"]:
        counts[item["status"]] = counts.get(item["status"], 0) + 1
    report["counts"] = counts
    out = args.home / "cc_capture" / "parity_report.json"
    out.write_text(json.dumps(report, indent=1))
    for item in report["items"]:
        if item["status"] != "verified":
            print("%-24s %-22s text=%s err=%s notes=%s" % (item["label"], item["status"], item["text"],
                                                           item["is_error"], item["notes"]))
    print(json.dumps({"counts": counts, "checks": report["checks"]}, indent=1)[:3000])
    print(out)
    return 0 if not counts.get("differs") else 1


if __name__ == "__main__":
    sys.exit(main())
