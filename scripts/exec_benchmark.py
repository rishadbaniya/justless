#!/usr/bin/env python3
"""Measure bytes that reach context when a script summarizes large local data."""

import argparse
import json
from pathlib import Path
import random
import subprocess
import tempfile

SUMMARIZE_JSON = """import collections, json, sys
issues = json.load(open(sys.argv[1]))
state = collections.Counter(i["state"] for i in issues)
labels = collections.Counter(l["name"] for i in issues for l in i["labels"])
authors = collections.Counter(i["user"]["login"] for i in issues if i["state"] == "open")
oldest = min((i for i in issues if i["state"] == "open"), key=lambda i: i["created_at"])
print(f"{len(issues)} issues: " + ", ".join(f"{k} {v}" for k, v in state.most_common()))
print("labels: " + ", ".join(f"{k} {v}" for k, v in labels.most_common(5)))
print("top open authors: " + ", ".join(f"{k} {v}" for k, v in authors.most_common(5)))
print(f"oldest open: #{oldest['number']} {oldest['title']} ({oldest['created_at']})")
"""
COUNT_ERRORS = """printf 'ERROR lines: %s of %s\\n' "$(grep -c ' ERROR ' "$1")" "$(wc -l < "$1" | tr -d ' ')"
grep ' ERROR ' "$1" | sed -E 's/.* ERROR \\[([a-z_]+)\\] ([a-z]+) .*/\\2 \\1/' | sort | uniq -c | sort -rn | head -8
grep -m1 -n ' ERROR \\[db_deadlock\\]' "$1"
"""
REVENUE_CSV = """import collections, csv, sys
revenue = collections.defaultdict(float); refunds = collections.Counter(); n = 0
for row in csv.DictReader(open(sys.argv[1])):
    n += 1
    if row["status"] == "paid": revenue[row["region"]] += int(row["qty"]) * float(row["unit_price"])
    elif row["status"] == "refunded": refunds[row["region"]] += 1
print(f"{n} orders")
for region in sorted(revenue): print(f"{region:8} paid {revenue[region]:>14,.2f}  refunds {refunds[region]}")
"""
SLOW_REQUESTS = """import re, sys
print("slow requests (>20s) by line")
n = 0
for no, line in enumerate(open(sys.argv[1]), 1):
    m = re.search(r" (\\w+) \\[(\\w+)\\] (\\w+) request_id=(\\d+) took (\\d+)ms", line)
    if m and int(m.group(5)) > 20000:
        n += 1
        print(f"{no}: {m.group(3)} {m.group(2)} {m.group(5)}ms request_id={m.group(4)}")
print(f"total {n} slow error requests")
"""


def generate(work):
    rng = random.Random(7)
    labels = ["bug", "enhancement", "docs", "perf", "security", "ci", "refactor", "good first issue"]
    users = [f"user{i:03d}" for i in range(240)]
    words = ["the", "request", "fails", "when", "payload", "is", "large", "retry", "cache", "worker", "queue"]
    titles = ["crash on start", "slow query", "typo in docs", "flaky test", "memory leak",
              "timeout in payments", "update dependency"]
    issues = [{
        "number": n, "title": f"Issue {n}: {rng.choice(titles)}",
        "state": rng.choice(["open", "closed", "closed"]),
        "user": {"login": rng.choice(users), "id": rng.randint(1, 10**7), "type": "User", "site_admin": False},
        "labels": [{"name": l, "color": "ededed", "default": False} for l in rng.sample(labels, rng.randint(0, 3))],
        "comments": rng.randint(0, 40),
        "created_at": f"2026-{rng.randint(1, 10):02d}-{rng.randint(1, 28):02d}T{rng.randint(0, 23):02d}:00:00Z",
        "body": " ".join(rng.choice(words) for _ in range(rng.randint(20, 80))),
    } for n in range(1, 8001)]
    (work / "issues.json").write_text(json.dumps(issues, indent=2))
    services = ["payments", "orders", "auth", "search", "inventory", "email"]
    codes = ["timeout", "conn_reset", "bad_request", "db_deadlock", "rate_limited", "null_field"]
    with open(work / "app.log", "w") as log:
        for i in range(220000):
            level = rng.choices(["INFO", "DEBUG", "WARN", "ERROR"], [70, 20, 7, 3])[0]
            service = rng.choice(services)
            if level == "ERROR":
                message = (f"[{rng.choice(codes)}] {service} request_id={rng.randint(10**8, 10**9)} "
                           f"took {rng.randint(5, 30000)}ms")
            else:
                message = (f"{service} handled GET /api/{service}/{rng.randint(1, 9999)} status=200 "
                           f"in {rng.randint(1, 900)}ms")
            log.write(f"2026-11-{1 + i // 20000:02d}T{(i // 900) % 24:02d}:{(i // 15) % 60:02d}:"
                      f"{i % 60:02d}.{i % 1000:03d}Z {level} {message}\n")
    with open(work / "orders.csv", "w") as orders:
        orders.write("order_id,region,sku,qty,unit_price,status\n")
        for i in range(120000):
            orders.write(f"{i},{rng.choice(['us-east', 'us-west', 'eu', 'apac'])},SKU-{rng.randint(1, 500):04d},"
                         f"{rng.randint(1, 9)},{rng.randint(100, 99999) / 100},"
                         f"{rng.choice(['paid', 'paid', 'refunded', 'pending'])}\n")
    (work / "slow.py").write_text(SLOW_REQUESTS)


def measure(binary, work):
    def call(*args, code=None):
        result = subprocess.run([binary, "--root", str(work), *args], cwd=work, input=code,
                                capture_output=True, timeout=300)
        return result.returncode, result.stdout, result.stderr

    tasks = [
        ("Summarize an 8,000-issue JSON export", "issues.json", "python3", SUMMARIZE_JSON, b"8000 issues:"),
        ("Count log errors by service and code", "app.log", "sh", COUNT_ERRORS, b"ERROR lines:"),
        ("Revenue and refunds by region from a CSV", "orders.csv", "python3", REVENUE_CSV, b"120000 orders"),
    ]
    rows = []
    for name, data, lang, script, answer in tasks:
        status, out, err = call("exec", "--lang", lang, "--file", data, "-", code=script.encode())
        assert status == 0 and out.startswith(answer), (name, out, err)
        _, cat_out, cat_err = call("run", "--", "cat", data)
        rows.append({
            "task": name,
            "input_bytes": (work / data).stat().st_size,
            "run_cat_bytes": len(cat_out) + len(cat_err),
            "run_cat_has_answer": answer in cat_out,
            "exec_bytes": len(out) + len(err),
        })
    relevant = b"payments timeout"
    _, plain, plain_err = call("exec", "--script", "slow.py", "--file", "app.log")
    _, ranked, ranked_err = call("exec", "--script", "slow.py", "--file", "app.log",
                                 "--intent", "payments timeouts")
    assert b"[focus]" in ranked and len(ranked) <= 4096, ranked
    # Count only marked stdout lines: the [focus] header repeats the intent itself.
    ranked_lines = [line for line in ranked.splitlines() if line.startswith(b"stdout:")]
    full = subprocess.run(["python3", "-I", "slow.py", "app.log"], cwd=work, capture_output=True).stdout
    rows.append({
        "task": "Slow requests listing, with and without --intent",
        "script_output_bytes": len(full),
        "relevant_lines_in_output": full.count(relevant),
        "default_preview_bytes": len(plain) + len(plain_err),
        "default_relevant_lines": plain.count(relevant),
        "intent_preview_bytes": len(ranked) + len(ranked_err),
        "intent_relevant_lines": sum(relevant in line for line in ranked_lines),
    })
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="justless")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve()) if "/" in args.binary else args.binary
    with tempfile.TemporaryDirectory() as directory:
        work = Path(directory)
        (work / ".justless.toml").write_text("")
        generate(work)
        print(json.dumps(measure(binary, work), indent=2))


if __name__ == "__main__":
    main()
