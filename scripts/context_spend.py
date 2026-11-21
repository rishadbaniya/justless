#!/usr/bin/env python3
"""Measure where the prompt tokens of Claude Code sessions go.

Every request resends the whole conversation, so a tool result is paid for
once when it is written to the prompt cache and again on each later request
that reads it. This reads the Claude Code transcripts of one project and
splits what each request sent between what the session started with, tool
results by tool, what the model wrote and everything else, priced at API list
prices. It then puts three figures on the hooks: what they kept out of the
prompt, what clipping every large Bash and Read result would have kept out,
and how much output one extra tool call costs. It prints token and byte totals
only, never content.

A shortened result counts for what Claude Code would have shown without the
hooks, which is never more than the largest whole result of that tool in the
same transcripts: Claude Code clips a command's output itself. A command that
names `justless` is not counted for the hooks, since in this repository those
are mostly tests of the tool on made-up output.
"""

import argparse
import glob
import json
import os
import re
import statistics
import time
from collections import Counter

# Dollars per million tokens at API list prices in November 2026: input,
# output, cache read. A cache write costs 1.25 times input for five minutes and
# twice input for an hour. A subscription is billed differently, so the dollar
# figures compare parts of the spend and are not a bill.
RATES = {
    "claude-fable-5-2": (8.0, 40.0, 0.20),
    "claude-opus-5-5": (4.0, 20.0, 0.20),
    "claude-sonnet-5-5": (2.0, 10.0, 0.10),
    "claude-haiku-5-5": (0.10, 0.50, 0.01),
}
CLIP = 4096
# The two footers a clipped run ends in, each with the bytes the command printed.
SHOWN = re.compile(r"\[justless (run-\w+) · ([\d.]+) (B|KB|MB) of ([\d.]+) (B|KB|MB) shown")
BYTES = re.compile(r"\[justless (run-\w+)\] exit \d+ · (\d+) of (\d+) output bytes")
OUTLINE = re.compile(r"revision [0-9a-f]{8,} · (\d+) lines(?: · window L(\d+)-(\d+))?")
RETRIEVAL = re.compile(r"(?:^|[;&|]\s*)justless (?:log|search|recall|expand|context --run)\b")
RUN = re.compile(r"\brun-\w+")
# Kept-out bytes of one clipped result, as the upper edge of each row.
BUCKETS = [(4000, "under 4 KB"), (16000, "4 to 16 KB"), (None, "16 KB or more")]
UNIT = {"B": 1, "KB": 1000, "MB": 1000000}
LISTS = ("break even", "sizes", "shortened items")
ADVICE = "advice the advisor returned"
OTHER = "reminders, attachments and framing"


def size(content):
    """Bytes of text in a message's content."""
    if isinstance(content, str):
        return len(content.encode())
    return sum(
        len(part.get("text", "").encode())
        for part in (content if isinstance(content, list) else [])
        if isinstance(part, dict) and part.get("type") == "text"
    )


def text_of(content):
    if isinstance(content, str):
        return content
    return "".join(part.get("text", "") for part in content or [] if isinstance(part, dict))


def tokens_of(usage):
    """What one pass of a request sent: tokens in all, and its price in input tokens."""
    written = usage.get("cache_creation") or {}
    hour = written.get("ephemeral_1h_input_tokens", 0)
    short = usage.get("cache_creation_input_tokens", 0) - hour
    fresh = usage.get("input_tokens", 0)
    return {
        "input": fresh,
        "read": usage.get("cache_read_input_tokens", 0),
        "written": fresh + short + hour,
        "write cost": fresh + short * 1.25 + hour * 2,
        "out": usage.get("output_tokens", 0),
    }


def result(name, call, content):
    """One tool result: its bytes and, for a Bash or Read result the hooks
    shortened, what it stood for: the bytes a run printed, or the lines an
    outline replaced. `settle` turns those into bytes kept out."""
    item = {"kind": name or "other tool", "bytes": size(content), "saved": 0, "over": 0, "how": None}
    if name not in ("Bash", "Read"):
        return item
    text = text_of(content)
    # A footer counts only as the last line: tests of the tool print footers too.
    last = text.rstrip("\n").rsplit("\n", 1)[-1]
    shown, printed = SHOWN.match(last), BYTES.match(last)
    outline = OUTLINE.search(text[:160]) if name == "Read" else None
    if shown or printed:
        item["how"] = "named" if "justless" in call.get("command", "") else "clipped"
        item["run"] = (shown or printed).group(1)
        item["stood for"] = int(float(shown.group(4)) * UNIT[shown.group(5)]) if shown else int(printed.group(3))
    elif outline:
        first = int(outline.group(2) or 1)
        last = int(outline.group(3) or outline.group(1))
        item["how"] = "outline"
        item["lines"] = last - first + 1
    elif name == "Read":
        item["lines"] = text.count("\n") + 1
    return item


def settle(sessions):
    """Fill in what each shortened result kept out, and what each whole one
    holds past the clip size. Returns the bytes per line of a whole Read and
    the largest whole result of each tool, which bound what a result stood for."""
    largest, lines, read_bytes = {"Bash": 0, "Read": 0}, 0, 0
    items = [item for requests in sessions for request in requests for item in request["after"]]
    for item in items:
        if item["kind"] in largest and item["how"] is None:
            largest[item["kind"]] = max(largest[item["kind"]], item["bytes"])
            if item.get("lines", 0) > 150:
                lines += item["lines"]
                read_bytes += item["bytes"]
    density = read_bytes / lines if lines else 45.0
    for item in items:
        if item["how"] in ("clipped", "named"):
            item["saved"] = max(0, min(item["stood for"], largest["Bash"] or item["stood for"]) - item["bytes"])
        elif item["how"] == "outline":
            whole = min(item["lines"] * density, largest["Read"] or item["lines"] * density)
            item["saved"] = max(0, int(whole) - item["bytes"])
        elif item["kind"] in largest:
            item["over"] = max(0, item["bytes"] - CLIP)
    return density, largest


def load(path, seen, days):
    """The requests of one transcript in order, each with what it sent and the
    results and messages that followed it. Adds each request's UTC day to `days`."""
    requests, calls, ids, outlined, runs, position = [], {}, {}, {}, {}, 0
    with open(path, encoding="utf-8", errors="ignore") as handle:
        for line in handle:
            if '"message"' not in line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            message = entry.get("message")
            if not isinstance(message, dict):
                continue
            parts = message.get("content")
            if entry.get("type") == "assistant":
                usage = message.get("usage")
                if not usage or message.get("model") == "<synthetic>":
                    continue
                key = message.get("id") or entry.get("requestId")
                if key not in ids:
                    if key in seen:
                        continue
                    seen.add(key)
                    days.add(str(entry.get("timestamp") or "")[:10])
                    ids[key] = len(requests)
                    requests.append({"model": message.get("model"), "calls": 0, "extra": 0.0, "after": []})
                request = requests[ids[key]]
                if usage.get("output_tokens", 0) >= request.get("out", 0):
                    # A request that consults the advisor runs the model twice
                    # over the prompt, and the advisor reads the conversation too.
                    passes = [one for one in usage.get("iterations") or [] if isinstance(one, dict)]
                    own = [one for one in passes if one.get("type") == "message"] or [usage]
                    request.update(tokens_of(usage))
                    request["prompt"] = tokens_of(own[0])["read"] + tokens_of(own[0])["written"]
                    request["passes"] = len(own)
                    request["advisor"] = [
                        (one.get("model"), tokens_of(one)) for one in passes if one.get("type") == "advisor_message"
                    ]
                for part in parts if isinstance(parts, list) else []:
                    if isinstance(part, dict) and part.get("type") == "tool_use":
                        request["calls"] += 1
                        calls[part.get("id")] = (ids[key], part.get("name"), part.get("input") or {})
            elif entry.get("type") == "user" and requests:
                results = [
                    part
                    for part in (parts if isinstance(parts, list) else [])
                    if isinstance(part, dict) and part.get("type") == "tool_result"
                ]
                if not results:
                    requests[-1]["after"].append(
                        {"kind": "user", "bytes": size(parts), "saved": 0, "over": 0, "how": None}
                    )
                for part in results:
                    at, name, call = calls.pop(part.get("tool_use_id"), (None, None, {}))
                    position += 1
                    item = result(name, call, part.get("content"))
                    # A call that only fetches what an earlier result left out
                    # is a request the session would not otherwise have made.
                    # It is charged to the result it fetches from, when the
                    # call names that run or reads that file.
                    again, source = False, None
                    if name == "Bash" and RETRIEVAL.search(call.get("command", "")):
                        again = True
                        named = RUN.search(call.get("command", ""))
                        source = runs.get(named.group(0)) if named else None
                    elif name == "Read" and item["how"] is None:
                        seen_at, source = outlined.get(call.get("file_path"), (-99, None))
                        again = position - seen_at <= 20
                    if item["how"] == "outline":
                        outlined[call.get("file_path")] = (position, item)
                    elif item["how"] == "clipped":
                        runs[item["run"]] = item
                    if again and at is not None:
                        requests[at]["extra"] += 1
                        item["again"] = True
                        item["source"] = source if again else None
                    requests[-1]["after"].append(item)
    return requests


def shrank(before, after):
    """Whether the conversation was summarized between two requests."""
    return after["prompt"] < before["prompt"] - max(2000, before["prompt"] // 20)


def fit(sessions):
    """Tokens one byte of a tool result adds to the next prompt: the median over
    steps that added one large Bash or Read result and what the model wrote."""
    ratios = []
    for requests in sessions:
        for before, after in zip(requests, requests[1:]):
            items = before["after"]
            if len(items) == 1 and items[0]["kind"] in ("Bash", "Read") and items[0]["bytes"] > 6000:
                if before["passes"] == 1 and not shrank(before, after):
                    ratios.append((after["prompt"] - before["prompt"] - before["out"]) / items[0]["bytes"])
    return statistics.median(ratios), len(ratios)


def account(requests, per_byte, totals):
    """Split what each request sent between where its tokens came from, and
    price what the hooks kept out and could have kept out."""
    ends, start = {}, 0
    for at in range(1, len(requests) + 1):
        if at == len(requests) or shrank(requests[at - 1], requests[at]):
            for inside in range(start, at):
                ends[inside] = at - 1
            start = at
    held, fresh, before = Counter(), Counter(), None
    for at, request in enumerate(requests):
        rate = RATES.get(request["model"])
        if rate is None:
            totals["unpriced"] += 1
            before = None
            continue
        price, out_price, read_price = (value / 1e6 for value in rate)
        write_price = price * request["write cost"] / request["written"] if request["written"] else price * 1.25
        cost = request["write cost"] * price + request["read"] * read_price
        if before is None or shrank(before, request):
            held = Counter({"carried in" if before is None else "kept after a summary": request["prompt"]})
            fresh = Counter()
        else:
            grown = max(0, request["prompt"] - before["prompt"])
            fresh = Counter({"what the model wrote earlier": before["out"]})
            for item in before["after"]:
                label = "user messages" if item["kind"] == "user" else f"{item['kind']} results"
                if item["how"] in ("clipped", "outline"):
                    label = f"{item['kind']} results the hooks shortened"
                fresh[label] += item["bytes"] * per_byte
            total = sum(fresh.values())
            if total > grown:
                fresh = Counter({label: tokens * grown / total for label, tokens in fresh.items()})
            else:
                fresh[ADVICE if before["advisor"] else OTHER] += grown - total
            held.update(fresh)
            total = sum(held.values())
            if total > request["prompt"] > 0:
                held = Counter({label: tokens * request["prompt"] / total for label, tokens in held.items()})
            # What one token added here costs over the rest of the session, and
            # what one more request costs here.
            life = write_price + read_price * (ends[at] - at)
            one_call = request["prompt"] * read_price
            for item in before["after"]:
                tokens = item["bytes"] * per_byte
                if item["kind"] in ("Bash", "Read"):
                    totals["results"] += 1
                    totals["result dollars"] += tokens * life
                    if item["how"] == "named":
                        totals["named"] += 1
                        totals["named bytes"] += item["saved"]
                    elif item["how"]:
                        item["worth"] = item["saved"] * per_byte * life
                        totals["shortened"] += 1
                        totals["saved bytes"] += item["saved"]
                        totals["saved dollars"] += item["worth"]
                        totals["shortened items"].append(item)
                    elif item["over"]:
                        totals["large"] += 1
                        totals["over bytes"] += item["over"]
                        totals["over dollars"] += item["over"] * per_byte * life
                        totals["worth a call"] += item["over"] * per_byte * life > one_call
                        totals["break even"].append(one_call / life / per_byte)
                if item.get("again"):
                    totals["again dollars"] += tokens * life
                    if item.get("source") is not None:
                        source = item["source"]
                        source["fetches"] = source.get("fetches", 0) + 1
                        source["fetched"] = source.get("fetched", 0.0) + tokens * life + cost / max(1, before["calls"])
            if before["calls"] and before["extra"]:
                totals["extra calls"] += before["extra"] / before["calls"]
                totals["extra dollars"] += cost * before["extra"] / before["calls"]
        # New tokens are written to the cache once; everything else is read.
        added = sum(fresh.values())
        new = min(added, request["written"])
        old = max(1.0, sum(held.values()) - new)
        for label, tokens in held.items():
            first = fresh.get(label, 0.0) * new / added if added else 0.0
            totals[("dollars", label)] += first * write_price + (tokens - first) * (cost - new * write_price) / old
            totals[("tokens", label)] += tokens
        totals[("dollars", "writing: thinking, text and tool calls")] += request["out"] * out_price
        advised = 0.0
        for model, sent in request["advisor"]:
            their = RATES.get(model)
            if their:
                advised += (sent["write cost"] * their[0] + sent["read"] * their[2] + sent["out"] * their[1]) / 1e6
                totals["advisor calls"] += 1
                totals["advisor tokens"] += sent["read"] + sent["written"]
                totals["advisor cached"] += sent["read"]
        totals[("dollars", "the advisor reading the conversation")] += advised
        totals["second pass dollars"] += cost * (1 - 1 / request["passes"])
        totals["requests"] += 1
        totals["prompt"] += request["prompt"]
        totals["sent"] += request["read"] + request["written"]
        totals["read"] += request["read"]
        totals["out"] += request["out"]
        totals["dollars"] += cost + request["out"] * out_price + advised
        totals["sizes"].append(request["prompt"])
        before = request


def report(name, files, totals):
    requests, spent = totals["requests"], totals["dollars"]
    print()
    print(
        f"{name}: {files} transcript(s), {requests} requests, "
        f"{statistics.median(totals['sizes']) / 1000:.0f}K prompt tokens in the median request"
    )
    print(
        f"  sent {totals['sent'] / 1e6:.0f}M prompt tokens, {100 * totals['read'] / totals['sent']:.1f}% read from "
        f"cache, and received {totals['out'] / 1e6:.1f}M: ${spent:.0f} at list prices"
    )
    print("  where the dollars went, and the share of the prompt each part held")
    rows = sorted(
        ((key[1], value) for key, value in totals.items() if isinstance(key, tuple) and key[0] == "dollars"),
        key=lambda row: -row[1],
    )
    for label, dollars in rows:
        if dollars < spent / 500:
            continue
        tokens = totals[("tokens", label)]
        held = f"{100 * tokens / totals['prompt']:4.1f}% of the prompt" if tokens else ""
        print(f"    {100 * dollars / spent:5.1f}%  ${dollars:7.0f}  {label:40} {held}")
    if totals["advisor calls"]:
        print(
            f"  the advisor: {totals['advisor calls']} calls read {totals['advisor tokens'] / 1e6:.1f}M tokens, "
            f"{100 * totals['advisor cached'] / totals['advisor tokens']:.0f}% from cache; the model's second "
            f"pass over the prompt after each cost ${totals['second pass dollars']:.0f}, counted in the rows above"
        )
    print("  the hooks")
    print(
        f"    Bash and Read results: {totals['results']} results cost ${totals['result dollars']:.0f}, "
        f"{100 * totals['result dollars'] / spent:.1f}% of the spend"
    )
    fetched = totals["extra dollars"] + totals["again dollars"]
    print(
        f"    kept out: {totals['saved bytes'] / 1e6:.2f} MB over {totals['shortened']} results, "
        f"worth ${totals['saved dollars']:.0f}; {totals['extra calls']:.0f} calls fetched lines again for "
        f"${fetched:.0f} (${totals['extra dollars']:.0f} to resend the prompt, ${totals['again dollars']:.0f} "
        f"for what they printed): net ${totals['saved dollars'] - fetched:.0f}, "
        f"{100 * (totals['saved dollars'] - fetched) / spent:.2f}% of the spend"
    )
    if totals["shortened"]:
        # A fetch is charged to a result only when the call names its run or
        # reads its file, so these rows hold fewer fetches than the line above.
        groups = [("Read outlines of at most 600 lines", lambda item: item["how"] == "outline" and item["lines"] <= 600)]
        groups.append(("Read outlines of more than 600 lines", lambda item: item["how"] == "outline" and item["lines"] > 600))
        floor = 0
        for ceiling, label in BUCKETS:
            groups.append(
                (
                    f"command output, {label} kept out",
                    lambda item, low=floor, high=ceiling: item["how"] == "clipped"
                    and item["saved"] >= low
                    and (high is None or item["saved"] < high),
                )
            )
            floor = ceiling or 0
        print("    by kind of result, with the fetches that name its run or read its file")
        for label, wanted in groups:
            rows = [item for item in totals["shortened items"] if wanted(item)]
            if rows:
                worth = sum(item["worth"] for item in rows)
                back = sum(item.get("fetched", 0.0) for item in rows)
                again = sum(1 for item in rows if item.get("fetches"))
                print(
                    f"      {label:44} {len(rows):4} results  {sum(item['saved'] for item in rows) / 1e6:5.2f} MB  "
                    f"worth ${worth:6.2f}  {again:3} fetched from again for ${back:6.2f}  net ${worth - back:7.2f}"
                )
    if totals["named"]:
        print(
            f"    not counted: {totals['named']} results of commands that name `justless`, "
            f"{totals['named bytes'] / 1e6:.2f} MB by the same rule"
        )
    print(
        f"    left whole: {totals['large']} results over {CLIP} bytes hold {totals['over bytes'] / 1e6:.1f} MB "
        f"past it, worth ${totals['over dollars']:.0f}, {100 * totals['over dollars'] / spent:.1f}% of the "
        "spend, if clipping changed nothing else"
    )
    if totals["break even"]:
        print(
            f"    one extra call costs as much as {statistics.median(totals['break even']) / 1000:.0f} KB of "
            f"output kept to the end of the session (median); {100 * totals['worth a call'] / totals['large']:.0f}% "
            "of those large results hold more than that past the clip size"
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--root", default=os.getcwd(), help="the project whose transcripts to read")
    parser.add_argument("--days", type=int, default=30)
    options = parser.parse_args()

    slug = re.sub(r"[^A-Za-z0-9]", "-", os.path.abspath(options.root))
    base = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    project = os.path.join(base, "projects", slug)
    if not os.path.isdir(project):
        parser.error(f"no transcripts for {slug}")
    since = time.time() - options.days * 86400
    seen, days, sessions = set(), set(), {"main": [], "agents": []}
    for path in sorted(glob.glob(os.path.join(project, "**", "*.jsonl"), recursive=True)):
        if os.path.getmtime(path) >= since and "/memory/" not in path:
            requests = [request for request in load(path, seen, days) if "prompt" in request]
            if requests:
                sessions["agents" if "/subagents/" in path else "main"].append(requests)
    per_byte, steps = fit(sessions["main"] + sessions["agents"])
    density, largest = settle(sessions["main"] + sessions["agents"])
    days.discard("")
    if not days:
        parser.error(f"no requests in the last {options.days} days for {slug}")
    print(f"{slug}: requests on {len(days)} day(s) from {min(days)} to {max(days)} (UTC)")
    print(
        f"A byte of tool result adds {per_byte:.3f} tokens to every later prompt "
        f"({1 / per_byte:.2f} bytes per token, the median of {steps} steps that added one large result)."
    )
    print(
        f"The largest whole result is {largest['Bash'] / 1000:.1f} KB for Bash and {largest['Read'] / 1000:.1f} KB "
        f"for Read, and a whole Read holds {density:.0f} bytes a line: a shortened result counts for no more."
    )
    everything = Counter()
    for key in LISTS:
        everything[key] = []
    for group, name in [("main", "Main sessions"), ("agents", "Subagents")]:
        totals = Counter()
        for key in LISTS:
            totals[key] = []
        for requests in sessions[group]:
            account(requests, per_byte, totals)
        if not totals["requests"]:
            continue
        report(name, len(sessions[group]), totals)
        for key, value in totals.items():
            if isinstance(value, list):
                everything[key].extend(value)
            else:
                everything[key] += value
    report("All sessions", len(sessions["main"]) + len(sessions["agents"]), everything)
    print()
    print(
        "Dollars are API list prices and compare parts of the spend; a subscription is billed "
        "differently. What a result costs is its tokens written to the cache once and read on "
        "every later request until the session ends or is summarized. The figures for the "
        "hooks assume the agent would have done the same with the longer output, and count "
        "as fetching again only `justless` lookups and Reads of a file within 20 results of "
        "its outline. The rows by kind charge a fetch to a result only when the call names "
        "its run or reads its file, so they hold fewer fetches than the line above them."
    )


if __name__ == "__main__":
    main()
