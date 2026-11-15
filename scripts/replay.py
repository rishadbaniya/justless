#!/usr/bin/env python3
"""Replay Claude Code Bash results through the post-command hook."""

import argparse
import json
import os
import subprocess
import sys
from collections import Counter


def entries(project):
    for root, _, files in os.walk(project):
        for name in sorted(files):
            if not name.endswith(".jsonl"):
                continue
            path = os.path.join(root, name)
            with open(path, encoding="utf-8", errors="ignore") as handle:
                for line in handle:
                    try:
                        yield json.loads(line)
                    except json.JSONDecodeError:
                        continue


def results(project):
    uses = {}
    for entry in entries(project):
        content = (entry.get("message") or {}).get("content")
        if not isinstance(content, list):
            continue
        cwd = entry.get("cwd") or ""
        session = entry.get("sessionId") or ""
        for part in content:
            if not isinstance(part, dict):
                continue
            if part.get("type") == "tool_use" and part.get("name") == "Bash":
                uses[part.get("id")] = (part.get("input") or {}).get("command", "")
            elif part.get("type") == "tool_result":
                response = entry.get("toolUseResult")
                if isinstance(response, dict) and "stdout" in response:
                    yield uses.get(part.get("tool_use_id")), response, cwd, session


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", default=os.getcwd())
    parser.add_argument("--project", default=None)
    parser.add_argument("--binary", default="./target/debug/justless")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--json", action="store_true")
    options = parser.parse_args()

    root = os.path.abspath(options.root)
    slug = options.project or root.replace(os.sep, "-")
    project = os.path.expanduser(os.path.join("~/.claude/projects", slug))
    if not os.path.isdir(project):
        parser.error(f"no transcripts for {slug}")

    counts = Counter()
    reasons = Counter()
    shapes = Counter()
    before = after = 0
    for command, response, cwd, session in results(project):
        if command is None:
            continue
        output = response.get("stdout") or ""
        size = len(output.encode())
        counts["results"] += 1
        counts["bytes_total"] += size
        if size <= 4096:
            reasons["small"] += 1
            continue
        if "[justless run-" in output:
            reasons["already"] += 1
            continue
        if not os.path.isdir(cwd):
            reasons["missing-cwd"] += 1
            continue
        if options.limit and counts["eligible"] >= options.limit:
            break
        counts["eligible"] += 1
        payload = {
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "tool_response": response,
            "cwd": cwd,
            "session_id": session,
        }
        try:
            run = subprocess.run(
                [os.path.abspath(options.binary), "hook-claude", "after-bash"],
                input=json.dumps(payload).encode(),
                cwd=cwd,
                capture_output=True,
                timeout=20,
            )
            reply = json.loads(run.stdout or b"{}")
        except Exception:
            reasons["error"] += 1
            continue
        updated = (reply.get("hookSpecificOutput") or {}).get("updatedToolOutput") or {}
        if "stdout" not in updated:
            reasons["passed"] += 1
            continue
        new = len(updated["stdout"].encode())
        counts["compacted"] += 1
        before += size
        after += new
        shapes[
            "list"
            if any(c in command for c in ";\n")
            else "pipe"
            if "|" in command
            else "single"
        ] += 1

    saved = before - after
    percent = (saved * 100 / before) if before else 0
    summary = {
        "project": slug,
        "results": counts["results"],
        "bytes_total": counts["bytes_total"],
        "eligible": counts["eligible"],
        "compacted": counts["compacted"],
        "bytes_before": before,
        "bytes_after": after,
        "bytes_saved": saved,
        "percent": round(percent, 1),
        "shapes": dict(shapes),
        "reasons": dict(reasons),
    }
    if options.json:
        print(json.dumps(summary, indent=2))
    else:
        print(
            f"{summary['project']}: {summary['results']} Bash results, {summary['bytes_total']} bytes"
        )
        print(
            f"eligible {summary['eligible']}, compacted {summary['compacted']}, saved {saved} bytes ({percent:.1f}%)"
        )
        print(f"shapes {summary['shapes']}")
        print(f"reasons {summary['reasons']}")


if __name__ == "__main__":
    sys.exit(main())
