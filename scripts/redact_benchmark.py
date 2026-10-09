#!/usr/bin/env python3
"""Measure what secret redaction adds to capturing and replaying large output.

Each input is written to a temporary project, captured with `justless run -- cat`
with redaction on and off, then replayed with `log` and `log --unredacted`. Times are
the fastest of several repeats, reported in seconds and MiB/s next to the byte count.
With --cpu they are the CPU time Justless used, user plus system, which other load on
the machine disturbs less than elapsed time.
Sample secrets are generated at runtime.
"""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
import time

MIB = 1024 * 1024
REPO = Path(__file__).resolve().parent.parent


def digest_text(seed, length):
    alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    out, block = [], hashlib.sha256(seed.encode()).digest()
    while len(out) < length:
        out.extend(alphabet[b % 62] for b in block)
        block = hashlib.sha256(block).digest()
    return "".join(out[:length])


def secrets(count):
    """Lines that each hold one secret, in the shapes command output prints them."""
    lines = []
    for i in range(count):
        lines.append([
            f"GITHUB_TOKEN={'gh' + 'p_'}{digest_text(f'gh{i}', 36)}\n",
            f"api_key: {digest_text(f'key{i}', 30)}7q\n",
            f"> Authorization: Bearer {digest_text(f'bearer{i}', 40)}\n",
            f"DATABASE_URL={'postgres'}://app:{digest_text(f'pw{i}', 16)}@db:5432/app\n",
        ][i % 4])
    return lines


def corpus():
    """The repository's own history and text files: code, docs, JSON and lockfiles."""
    parts = []
    log = subprocess.run(["git", "-C", str(REPO), "log", "-p", "--all"],
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    parts.append(log.stdout)
    files = subprocess.run(["git", "-C", str(REPO), "ls-files"], stdout=subprocess.PIPE,
                           check=True).stdout.decode().split()
    for name in files:
        path = REPO / name
        if path.suffix in {".rs", ".md", ".toml", ".lock", ".json", ".py", ".sh", ".js",
                           ".ts", ".sql", ".yml", ".yaml"} and path.is_file():
            parts.append(path.read_bytes())
    return b"".join(parts)


def write_mix(path, size, injected):
    base = corpus()
    lines = [line.encode() for line in secrets(injected)]
    gap = max(size // (len(lines) + 1), 1)
    inserted = 0
    with open(path, "wb") as out:
        written, next_secret = 0, gap
        while written < size:
            # Copies of the corpus, with a secret line between copies every `gap` bytes.
            chunk = base[: min(size - written, next_secret - written if lines else size)]
            out.write(chunk)
            written += len(chunk)
            if lines and written >= next_secret:
                line = lines.pop()
                out.write(line)
                written += len(line)
                inserted += 1
                next_secret += gap
    return inserted


def write_dense(path, size):
    """Every line holds a trigger word, none holds a secret."""
    templates = [
        "let token_count = tokenizer.count(&input); // token budget {i}\n",
        "password policy: minimum length 12, rotation every {i} days\n",
        "warning: unused variable `secret` in fn rotate_{i}\n",
        "api_key rotation scheduled (attempt {i}) bearer refresh pending\n",
        '{{"token_count": {i}, "private_key_path": null, "credential": "none"}}\n',
        "fetching https://example.com/v1/items?page={i} with Authorization header\n",
    ]
    with open(path, "wb") as out:
        written, i = 0, 0
        while written < size:
            block = "".join(t.format(i=i + n) for n, t in enumerate(templates * 64)).encode()
            out.write(block)
            written += len(block)
            i += len(templates) * 64
    return 0


def write_line(path, size, secret):
    """One line of base64 with no newline in it, optionally holding one secret."""
    block = base64.b64encode(hashlib.sha256(b"line").digest() * 3072)
    middle = f" token={digest_text('long-line', 28)}7q ".encode() if secret else b""
    with open(path, "wb") as out:
        half = size // 2
        written = 0
        while written < half:
            out.write(block)
            written += len(block)
        out.write(middle)
        while written < size:
            out.write(block)
            written += len(block)
        out.write(b"\n")
    return 1 if secret else 0


def child_cpu():
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return usage.ru_utime + usage.ru_stime


CLOCK = time.perf_counter


def timed(args, cwd, repeat):
    best, result = None, None
    for _ in range(repeat):
        start = CLOCK()
        result = subprocess.run(args, cwd=cwd, stdout=subprocess.DEVNULL,
                                stderr=subprocess.PIPE)
        elapsed = CLOCK() - start
        best = elapsed if best is None else min(best, elapsed)
    return best, result


def clear_runs(project):
    runs = project / ".justless" / "runs"
    if runs.exists():
        for run in runs.iterdir():
            for item in run.iterdir():
                item.unlink()
            run.rmdir()


def latest(project):
    runs = sorted((project / ".justless" / "runs").iterdir(), key=lambda p: p.stat().st_mtime)
    return runs[-1]


def measure(binary, baseline, project, path, repeat):
    size = path.stat().st_size
    row = {"bytes": size}
    for mode, settings in (("off", "redact = false\n"), ("on", "")):
        (project / ".justless.toml").write_text(settings)
        clear_runs(project)
        seconds, result = timed([binary, "run", "--fresh", "--", "cat", str(path)], project,
                                repeat)
        if result.returncode != 0:
            sys.exit(result.stderr.decode())
        row[f"run_{mode}_s"] = seconds
        record = json.loads((latest(project) / "record.json").read_text())
        if mode == "on":
            row["secrets"] = record["redaction"]["count"]
            run_id = latest(project).name
            row["log_s"], _ = timed([binary, "log", run_id, "--stream", "stdout"], project,
                                    repeat)
            row["log_unredacted_s"], _ = timed(
                [binary, "log", run_id, "--stream", "stdout", "--unredacted"], project, repeat)
    if baseline:
        (project / ".justless.toml").write_text("")
        clear_runs(project)
        row["run_baseline_s"], _ = timed([baseline, "run", "--fresh", "--", "cat", str(path)],
                                         project, repeat)
    clear_runs(project)
    row["overhead"] = row["run_on_s"] / row["run_off_s"] - 1
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--baseline", type=Path,
                        help="a Justless build without redaction, timed on the same inputs")
    parser.add_argument("--size-mb", type=int, default=200)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--inputs", nargs="+", choices=["mix", "dense", "line", "line+secret"],
                        help="measure only these inputs (default all; --check needs all)")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--cpu", action="store_true",
                        help="report CPU time instead of elapsed time")
    parser.add_argument("--check", action="store_true",
                        help="fail when redaction overhead or the clean fast path regresses")
    args = parser.parse_args()
    if args.cpu:
        global CLOCK
        CLOCK = child_cpu
    binary = str(args.binary.resolve())
    baseline = str(args.baseline.resolve()) if args.baseline else None
    size = args.size_mb * MIB
    inputs = [
        ("mix", lambda p: write_mix(p, size, 24)),
        ("dense", lambda p: write_dense(p, size)),
        ("line", lambda p: write_line(p, size, False)),
        ("line+secret", lambda p: write_line(p, size, True)),
    ]
    if args.inputs:
        if args.check:
            parser.error("--check measures every input")
        inputs = [(name, write) for name, write in inputs if name in args.inputs]
    rows = {}
    with tempfile.TemporaryDirectory(prefix="justless-redact-") as temp:
        project = Path(temp) / "project"
        project.mkdir()
        for name, write in inputs:
            path = Path(temp) / f"{name}.txt"
            expected = write(path)
            row = measure(binary, baseline, project, path, args.repeat)
            row["expected_secrets"] = expected
            rows[name] = row
            path.unlink()
    if args.json:
        print(json.dumps(rows, indent=2))
    else:
        print(f"{'input':<12} {'bytes':>11} {'secrets':>7} {'run off':>9} {'run on':>9} "
              f"{'MiB/s':>7} {'overhead':>8} {'log':>7} {'log raw':>7}"
              + (f" {'baseline':>9}" if baseline else ""))
        for name, row in rows.items():
            mib = row["bytes"] / MIB
            print(f"{name:<12} {row['bytes']:>11} {row['secrets']:>7} "
                  f"{row['run_off_s']:>8.2f}s {row['run_on_s']:>8.2f}s "
                  f"{mib / row['run_on_s']:>7.0f} {row['overhead']:>+7.0%} "
                  f"{row['log_s']:>6.2f}s {row['log_unredacted_s']:>6.2f}s"
                  + (f" {row['run_baseline_s']:>8.2f}s" if baseline else ""))
    if args.check:
        failures = []
        for name, row in rows.items():
            if row["secrets"] != row["expected_secrets"]:
                failures.append(f"{name}: found {row['secrets']} secrets, "
                                f"expected {row['expected_secrets']}")
        for name, limit in (("mix", 0.5), ("dense", 1.5)):
            if rows[name]["overhead"] > limit:
                failures.append(f"{name}: redaction adds {rows[name]['overhead']:.0%}, "
                                f"over the {limit:.0%} bound")
        dense = rows["dense"]
        if dense["log_s"] > 3 * dense["log_unredacted_s"] + 0.25:
            failures.append("dense: replaying a clean run missed the exact-copy fast path")
        if failures:
            sys.exit("\n".join(failures))


if __name__ == "__main__":
    main()
