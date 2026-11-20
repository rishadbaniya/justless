#!/usr/bin/env python3
"""Measure what Claude Code read again after an outlined read.

An outline saves bytes only if the agent does not then read the same lines.
This reads the Claude Code transcripts of one project and, for every Read and
every shell read the hooks replaced with an outline, adds up the later Reads
and shell reads of the same lines within a few tool calls, and any replay of
the run. Raw reads of more than 150 lines are counted the same way as the
baseline, since an agent also reads parts of a file again after it has seen
all of it. It prints counts and byte totals only, never content.
"""

import argparse
import glob
import json
import os
import re
import statistics
import time

OUTLINE = re.compile(r"revision [0-9a-f]{8,} · (\d+) lines(?: · window L(\d+)-(\d+))?")
NUMBERED = re.compile(r"\s*(\d+)[\t→]")
BUCKETS = [(151, 300), (301, 600), (601, 1200), (1201, None)]
# A shell read the post-run hook compacted ends in a run marker and names the
# lines of each file it stands for.
MARKER = re.compile(r"\[justless (run-[A-Za-z0-9]+) · [^\]\n]* shown\]\s*$")
PIECE = re.compile(r"^(\S+) lines ((?:\d+(?:-\d+)?(?:, )?)+)$", re.M)
SED = re.compile(r"\bsed\s+-n\s+'?(\d+),(\d+)p'?\s+(\S+)")
CAT = re.compile(r"(?:^|[;&|]\s*)cat\s+([^\s;&|<>]+)\s*(?:$|[;&])")


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(part.get("text", "") for part in content if isinstance(part, dict))
    return ""


def shell_read(command, text):
    """The files and lines a Bash result stands for, with whether the hook
    compacted it and the run that replays it: the pieces a compacted read names,
    or for a raw result the one `sed -n 'A,Bp' FILE` or `cat FILE` that printed it."""
    marker = MARKER.search(text[-200:])
    if marker:
        spans = []
        for name, ranges in PIECE.findall(text):
            for part in ranges.split(", "):
                first, _, last = part.partition("-")
                spans.append((os.path.basename(name), int(first), int(last or first)))
        return spans, True, marker.group(1)
    lines = text.count("\n") + 1
    if len(SED.findall(command)) == 1 and not CAT.search(command):
        first, last, name = SED.search(command).groups()
        return [(os.path.basename(name), int(first), min(int(last), int(first) + lines - 1))], False, None
    if len(CAT.findall(command)) == 1 and not SED.search(command):
        return [(os.path.basename(CAT.search(command).group(1)), 1, lines)], False, None
    return [], False, None


def reads(path):
    """Each successful Read or Bash result of a transcript: its place among the
    tool results, bytes, whether it is an outline or asked for the lines whole,
    and the files and lines it stands for."""
    calls, found, index = {}, [], 0
    with open(path, encoding="utf-8", errors="ignore") as handle:
        for line in handle:
            if '"tool_use"' not in line and '"tool_result"' not in line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            parts = (entry.get("message") or {}).get("content")
            if not isinstance(parts, list):
                continue
            for part in parts:
                if not isinstance(part, dict):
                    continue
                if part.get("type") == "tool_use":
                    calls[part.get("id")] = (part.get("name"), part.get("input") or {})
                elif part.get("type") == "tool_result":
                    call = calls.pop(part.get("tool_use_id"), None)
                    index += 1
                    if not call or call[0] not in ("Read", "Bash") or part.get("is_error"):
                        continue
                    text = text_of(part.get("content"))
                    lines = text.count("\n") + 1
                    if call[0] == "Bash":
                        command = call[1].get("command", "")
                        spans, compacted, run = shell_read(command, text)
                        found.append(
                            {
                                "index": index,
                                "tool": "Bash",
                                "command": command,
                                "bytes": len(text.encode()),
                                "lines": lines,
                                "outline": compacted,
                                "whole": False,
                                "spans": spans,
                                "run": run,
                            }
                        )
                        continue
                    outline = OUTLINE.search(text[:160])
                    if outline:
                        first = int(outline.group(2) or 1)
                        last = int(outline.group(3) or outline.group(1))
                    else:
                        numbered = NUMBERED.match(text)
                        first = call[1].get("offset") or (int(numbered.group(1)) if numbered else 1)
                        last = first + lines - 1
                    name = os.path.basename(call[1].get("file_path", ""))
                    found.append(
                        {
                            "index": index,
                            "tool": "Read",
                            "command": "",
                            "bytes": len(text.encode()),
                            "lines": lines,
                            "outline": outline is not None,
                            "whole": bool(call[1].get("allow_large")),
                            "spans": [(name, first, last)],
                            "run": None,
                        }
                    )
    return found


def overlap(first, later):
    """How many lines of `first`'s files `later` shows again."""
    return sum(
        max(0, min(last, again_last) - max(start, again_start) + 1)
        for name, start, last in first["spans"]
        for again, again_start, again_last in later["spans"]
        if again == name
    )


def followed(found, within, tool):
    """For each outlined result of `tool`, and each raw one of more than 150
    lines and 4 KB, the later Reads and shell reads within `within` tool results
    that show its lines again: how many, the lines and bytes of them that fall
    inside the first result's lines, and their bytes per line. A `justless log`
    of a compacted read's run counts whole."""
    rows = []
    for at, first in enumerate(found):
        span = sum(last - start + 1 for _, start, last in first["spans"])
        wide = span > 150 and first["bytes"] > 4096 and not first["whole"]
        if first["tool"] != tool or not first["spans"] or not (first["outline"] or wide):
            continue
        row = {"outline": first["outline"], "span": span, "bytes": first["bytes"]}
        row.update(calls=0, lines=0, again=0.0, whole=False, density=[])
        for later in found[at + 1 :]:
            if later["index"] - first["index"] > within:
                break
            if first["run"] and first["run"] in later["command"]:
                shared, share = span, 1.0
            elif later["outline"] or not later["spans"]:
                continue
            else:
                shared = overlap(first, later)
                shown = sum(last - start + 1 for _, start, last in later["spans"])
                share = min(1.0, shared / max(1, shown))
            if not shared:
                continue
            row["calls"] += 1
            row["lines"] += shared
            row["again"] += later["bytes"] * share
            row["whole"] = row["whole"] or later["whole"] or share == 1.0 and shared >= span
            row["density"].append(later["bytes"] / max(1, later["lines"]))
        rows.append(row)
    return rows


def table(title, rows, per_line):
    print(f"{title} (n={len(rows)})")
    for low, high in [(1, 150)] + BUCKETS:
        group = [row for row in rows if row["span"] >= low and (high is None or row["span"] <= high)]
        if not group:
            continue
        count = len(group)
        first = sum(row["bytes"] for row in group)
        again = sum(row["again"] for row in group)
        # An outline stands for lines it did not show: price them at the bytes
        # per line of the Reads that followed it, or of raw Reads in general.
        raw = sum(
            row["span"] * (statistics.median(row["density"]) if row["density"] else per_line)
            if row["outline"]
            else row["bytes"]
            for row in group
        )
        label = f"{low}-{high}" if high else f"{low}+"
        print(
            f"  {label:9} lines  n={count:4}"
            f"  read again {100 * sum(1 for row in group if row['calls']) // count:3}%"
            f" (whole {100 * sum(1 for row in group if row['whole']) // count:2}%)"
            f"  lines again {100 * sum(min(row['lines'], row['span']) for row in group) / sum(row['span'] for row in group):3.0f}%"
            f"  later Reads {sum(row['calls'] for row in group) / count:4.2f}"
            f"  first {first / 1000:7.0f} KB + again {again / 1000:7.0f} KB"
            f" = {100 * (first + again) / raw:4.0f}% of the raw Read"
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--root", default=os.getcwd(), help="the project whose transcripts to read")
    parser.add_argument("--days", type=int, default=30)
    parser.add_argument("--calls", type=int, default=20, help="tool results after a Read that count")
    options = parser.parse_args()

    slug = re.sub(r"[^A-Za-z0-9]", "-", os.path.abspath(options.root))
    base = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    project = os.path.join(base, "projects", slug)
    if not os.path.isdir(project):
        parser.error(f"no transcripts for {slug}")
    since = time.time() - options.days * 86400
    rows = {"Read": [], "Bash": []}
    for path in glob.glob(os.path.join(project, "**", "*.jsonl"), recursive=True):
        if os.path.getmtime(path) >= since and "/memory/" not in path:
            found = reads(path)
            for tool in rows:
                rows[tool].extend(followed(found, options.calls, tool))
    print(
        f"{slug}: last {options.days} days, the same lines shown again within "
        f"{options.calls} tool results"
    )
    for tool, name in [("Read", "Reads"), ("Bash", "shell reads")]:
        outlined = [row for row in rows[tool] if row["outline"]]
        raw = [row for row in rows[tool] if not row["outline"]]
        per_line = statistics.median(row["bytes"] / row["span"] for row in raw) if raw else 45.0
        table(f"Outlined {name}", outlined, per_line)
        table(f"Raw {name} over 150 lines ({per_line:.0f} bytes per line)", raw, per_line)
    print(
        "The last column is every byte the agent received for those lines against one raw "
        "read of them. Raw bytes for outlined lines are estimated. A raw shell read is one "
        "`sed -n 'A,Bp' FILE` or `cat FILE`; a compacted one may be a list of reads."
    )


if __name__ == "__main__":
    main()
