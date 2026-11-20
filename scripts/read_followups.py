#!/usr/bin/env python3
"""Measure what Claude Code read again after an outlined Read.

An outline saves bytes only if the agent does not then read the same lines.
This reads the Claude Code transcripts of one project and, for every Read the
post-read hook replaced with an outline, adds up the later Reads of the same
lines within a few tool calls. Raw Reads of more than 150 lines are counted the
same way as the baseline, since an agent also reads parts of a file again after
it has seen all of it. It prints counts and byte totals only, never content.
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


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(part.get("text", "") for part in content if isinstance(part, dict))
    return ""


def reads(path):
    """Each successful Read of a transcript: its place among the tool results,
    file, bytes, whether it is an outline or asked for the lines whole, and the
    lines it stands for."""
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
                    if not call or call[0] != "Read" or part.get("is_error"):
                        continue
                    text = text_of(part.get("content"))
                    lines = text.count("\n") + 1
                    outline = OUTLINE.search(text[:160])
                    if outline:
                        first = int(outline.group(2) or 1)
                        last = int(outline.group(3) or outline.group(1))
                    else:
                        numbered = NUMBERED.match(text)
                        first = call[1].get("offset") or (int(numbered.group(1)) if numbered else 1)
                        last = first + lines - 1
                    found.append(
                        {
                            "index": index,
                            "file": call[1].get("file_path", ""),
                            "bytes": len(text.encode()),
                            "lines": lines,
                            "outline": outline is not None,
                            "whole": bool(call[1].get("allow_large")),
                            "first": first,
                            "last": last,
                        }
                    )
    return found


def followed(found, within):
    """For each outline, and each raw Read of more than 150 lines and 4 KB, the
    later Reads of the same file within `within` tool results: how many, the
    lines and bytes of them that fall inside the first Read's lines, and their
    bytes per line."""
    rows = []
    for at, first in enumerate(found):
        span = first["last"] - first["first"] + 1
        wide = span > 150 and first["bytes"] > 4096 and not first["whole"]
        if not (first["outline"] or wide):
            continue
        row = {"outline": first["outline"], "span": span, "bytes": first["bytes"]}
        row.update(calls=0, lines=0, again=0.0, whole=False, density=[])
        for later in found[at + 1 :]:
            if later["index"] - first["index"] > within:
                break
            if later["file"] != first["file"] or later["outline"]:
                continue
            low, high = max(first["first"], later["first"]), min(first["last"], later["last"])
            if high < low:
                continue
            share = (high - low + 1) / max(1, later["last"] - later["first"] + 1)
            row["calls"] += 1
            row["lines"] += high - low + 1
            row["again"] += later["bytes"] * share
            row["whole"] = row["whole"] or later["whole"]
            row["density"].append(later["bytes"] / max(1, later["lines"]))
        rows.append(row)
    return rows


def table(title, rows, per_line):
    print(f"{title} (n={len(rows)})")
    for low, high in BUCKETS:
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
    rows = []
    for path in glob.glob(os.path.join(project, "**", "*.jsonl"), recursive=True):
        if os.path.getmtime(path) >= since and "/memory/" not in path:
            rows.extend(followed(reads(path), options.calls))
    outlined = [row for row in rows if row["outline"]]
    raw = [row for row in rows if not row["outline"]]
    per_line = statistics.median(row["bytes"] / row["span"] for row in raw) if raw else 45.0
    print(
        f"{slug}: last {options.days} days, Reads of the same lines within "
        f"{options.calls} tool results; a raw Read over 150 lines runs {per_line:.0f} bytes per line"
    )
    table("Outlined Reads", outlined, per_line)
    table("Raw Reads over 150 lines", raw, per_line)
    print(
        "The last column is every byte the agent received for those lines against one raw "
        "Read of them. Raw bytes for outlined lines are estimated."
    )


if __name__ == "__main__":
    main()
