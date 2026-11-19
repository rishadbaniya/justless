#!/usr/bin/env python3
"""Measure bytes per token by content type with Anthropic's token-counting endpoint.

`justless stats` estimates tokens from bytes with one constant per content type:
code, JSON, prose and logs. This script derives those constants. It is run by
hand, once per tokenizer change, and never by the tests:

    export ANTHROPIC_API_KEY=...        # read from the environment, never printed
    python3 scripts/token_calibration.py [--model claude-sonnet-5-5] [--json out.json]

or in two steps, to read every sample before anything is sent:

    python3 scripts/token_calibration.py --collect samples.json
    python3 scripts/token_calibration.py --samples samples.json --json out.json

`--counts FILE` takes the endpoint's responses from another client instead, one
JSON object per line: the count for "." first, then each sample in order, as in

    { echo '{"model":"claude-sonnet-5-5","messages":[{"role":"user","content":"."}]}'
      jq -c '.samples[] | {model: "claude-sonnet-5-5", messages: [{role: "user", content: .text}]}' samples.json
    } | while IFS= read -r body; do
      curl -sS --retry 6 --retry-all-errors https://api.anthropic.com/v1/messages/count_tokens \
        -H "x-api-key: $ANTHROPIC_API_KEY" \
        -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' -d "$body"; echo
    done > counts.jsonl
    python3 scripts/token_calibration.py --samples samples.json --counts counts.jsonl --json out.json

It sends only this repository's own text to `POST /v1/messages/count_tokens`,
which is free and counts without generating:

- code: tracked source files (Rust, Python, TypeScript, shell, SQL and the other
  fixture languages) and the patches of the last ten commits under src/ and tests/;
- JSON: tracked .json files, `cargo metadata` and `cargo check --message-format=json`
  on a copy of fixtures/failing;
- prose: tracked Markdown;
- logs: the real tool output kept under fixtures/ as .txt, and the output of
  `cargo test` and `cargo clippy` on a copy of fixtures/failing, this crate's unit
  tests, `git log --stat` without author lines, `cargo tree` and `ls -lRn`.

Command output has the checkout, temporary and home directories replaced by fixed
placeholders. No session transcript, `.justless/` archive or file outside the
repository is read. Each text is cut at line ends into samples of at most
--chunk bytes, at most --chunks samples per source and --cap bytes per type.

A request's count includes a fixed overhead for the message wrapper, measured as
the count for "." minus one and subtracted from every sample. Each type's constant
is the pooled ratio: the sum of UTF-8 bytes over the sum of tokens. Run with
CARGO_TARGET_DIR set to reuse a build; `cargo test --bin justless` builds the unit tests.
"""

import argparse
import datetime
import hashlib
import json
import os
import pathlib
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
ENDPOINT = "https://api.anthropic.com/v1/messages/count_tokens"
CODE = {"rs", "py", "ts", "tsx", "js", "sh", "sql", "go", "rb", "swift", "scala", "kt", "dart", "zig", "lua",
        "jl", "groovy", "m", "h", "ex", "exs", "tf", "vue", "svelte", "astro", "ps1", "psm1", "gradle", "toml",
        "yml"}
TYPES = ("code", "json", "prose", "logs")


def git(*args):
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=True).stdout


def tracked(predicate):
    for name in sorted(git("ls-files").splitlines()):
        path = ROOT / name
        if predicate(name) and path.is_file():
            try:
                yield name, path.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue


def scrub(text, *paths):
    """Replace machine-specific directories with placeholders."""
    for path, placeholder in paths:
        if path:
            # Tools print resolved paths, such as /private/var/... for /var/...
            for form in sorted({str(path), str(pathlib.Path(path).resolve())}, key=len, reverse=True):
                text = text.replace(form, placeholder)
    home = os.environ.get("HOME")
    return text.replace(home, "/home/user") if home else text


def output(args, cwd, *paths):
    done = subprocess.run(args, cwd=cwd, capture_output=True, text=True, errors="replace")
    return scrub(done.stdout + done.stderr, (ROOT, "/work/justless"), *paths)


def command_sources():
    """(type, label, text) for command output produced from this checkout."""
    sources = []
    scratch = pathlib.Path(tempfile.mkdtemp(prefix="justless-calibration-"))
    try:
        failing = scratch / "failing"
        shutil.copytree(ROOT / "fixtures" / "failing", failing)
        env_target = os.environ.get("CARGO_TARGET_DIR")
        place = ((scratch, "/tmp/work"), (env_target, "/work/target"))
        cargo = ["cargo"]
        sources.append(("json", "cargo metadata", output(cargo + ["metadata", "--format-version", "1", "--locked", "--offline"],
                                                         ROOT, *place)))
        sources.append(("json", "cargo check --message-format=json (fixtures/failing)",
                        output(cargo + ["check", "--all-targets", "--message-format=json"], failing, *place)))
        sources.append(("logs", "cargo test (fixtures/failing)", output(cargo + ["test"], failing, *place)))
        sources.append(("logs", "cargo clippy (fixtures/failing)",
                        output(cargo + ["clippy", "--all-targets"], failing, *place)))
        sources.append(("logs", "cargo test --bin justless",
                        output(cargo + ["test", "--locked", "--bin", "justless"], ROOT, *place)))
        sources.append(("logs", "cargo tree", output(cargo + ["tree", "--locked", "--offline"], ROOT, *place)))
        sources.append(("logs", "git log --stat", output(["git", "log", "--stat", "-n", "60", "--format=commit %H%nDate:   %ad%n%n%w(0,4,4)%B"],
                                                         ROOT, *place)))
        sources.append(("logs", "ls -lRn src fixtures", output(["ls", "-lRn", "src", "fixtures"], ROOT, *place)))
        for sha in git("log", "-n", "10", "--format=%H", "--", "src", "tests").split():
            sources.append(("code", "git show %s -- src tests" % sha[:7],
                            output(["git", "show", "--format=", "-p", sha, "--", "src", "tests"], ROOT, *place)))
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    return sources


def file_sources():
    def ext(name):
        return name.rsplit(".", 1)[-1].lower() if "." in name.rsplit("/", 1)[-1] else ""

    sources = [("code", name, text) for name, text in tracked(lambda n: ext(n) in CODE)]
    sources += [("json", name, text) for name, text in tracked(lambda n: ext(n) == "json")]
    sources += [("prose", name, text) for name, text in tracked(lambda n: ext(n) == "md")]
    sources += [("logs", name, text) for name, text in tracked(lambda n: n.startswith("fixtures/") and ext(n) == "txt")]
    return sources


def chunks(text, limit):
    """Pieces of at most `limit` UTF-8 bytes, cut after a newline where possible."""
    data = text.encode("utf-8")
    start = 0
    while start < len(data):
        end = min(start + limit, len(data))
        if end < len(data):
            cut = data.rfind(b"\n", start, end)
            if cut > start:
                end = cut + 1
        while end < len(data) and (data[end] & 0xC0) == 0x80:
            end -= 1
        piece = data[start:end].decode("utf-8")
        if piece.strip():
            yield piece
        start = end


class Counter:
    def __init__(self, key, model):
        self.key = key
        self.model = model
        self.requests = 0

    def count(self, text):
        body = json.dumps({"model": self.model, "messages": [{"role": "user", "content": text}]}).encode()
        headers = {"x-api-key": self.key, "anthropic-version": "2023-06-01", "content-type": "application/json"}
        for attempt in range(8):
            request = urllib.request.Request(ENDPOINT, data=body, headers=headers, method="POST")
            try:
                with urllib.request.urlopen(request, timeout=60) as response:
                    self.requests += 1
                    return int(json.load(response)["input_tokens"])
            except urllib.error.HTTPError as error:
                detail = error.read().decode("utf-8", "replace").replace(self.key, "[REDACTED]")[:300]
                if error.code not in (429, 500, 502, 503, 504, 529) or attempt == 7:
                    raise SystemExit("count_tokens refused: HTTP %s %s" % (error.code, detail))
                wait = float(error.headers.get("retry-after") or 2 ** attempt)
            except (urllib.error.URLError, TimeoutError) as error:
                if attempt == 7:
                    raise SystemExit("count_tokens unreachable: %s" % error)
                wait = 2 ** attempt
            time.sleep(min(wait, 60))
        raise SystemExit("count_tokens failed")


class Recorded:
    """Counts another client fetched, in the order they are asked for."""

    key = None

    def __init__(self, path):
        self.lines = iter(path.read_text().splitlines())
        self.requests = 0

    def count(self, text):
        line = next(self.lines, None)
        if line is None:
            raise SystemExit("--counts has fewer responses than samples")
        response = json.loads(line)
        if "input_tokens" not in response:
            raise SystemExit("count_tokens refused: %s" % json.dumps(response.get("error", response))[:300])
        self.requests += 1
        return int(response["input_tokens"])


def collect(args):
    """The samples to count, with the commit they came from. Each type takes every
    source's first sample before any source's second, in an order fixed by a hash of
    the source's name, so the byte cap does not favour names early in the alphabet."""
    by_type = {kind: [] for kind in TYPES}
    for kind, label, text in file_sources() + command_sources():
        pieces = list(chunks(text, args.chunk))[: args.chunks]
        if pieces:
            by_type[kind].append((hashlib.sha256(label.encode()).hexdigest(), label, pieces))
    samples = []
    for kind, sources in by_type.items():
        sources.sort()
        used = 0
        for turn in range(args.chunks):
            for _, label, pieces in sources:
                if turn < len(pieces) and used + len(pieces[turn].encode("utf-8")) <= args.cap:
                    samples.append({"type": kind, "source": label, "text": pieces[turn]})
                    used += len(pieces[turn].encode("utf-8"))
    return {"commit": git("rev-parse", "HEAD").strip(), "samples": samples}


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--model", default="claude-sonnet-5-5")
    parser.add_argument("--chunk", type=int, default=24_000, help="largest sample in bytes")
    parser.add_argument("--chunks", type=int, default=4, help="most samples per source")
    parser.add_argument("--cap", type=int, default=1_500_000, help="most bytes per content type")
    parser.add_argument("--collect", type=pathlib.Path, help="only write the samples here, to inspect before sending")
    parser.add_argument("--samples", type=pathlib.Path, help="count the samples a --collect run wrote")
    parser.add_argument("--counts", type=pathlib.Path, help="count_tokens responses for '.' and each sample")
    parser.add_argument("--json", type=pathlib.Path, help="also write the measurements here")
    args = parser.parse_args()
    if args.collect:
        args.collect.write_text(json.dumps(collect(args)) + "\n")
        return
    corpus = json.loads(args.samples.read_text()) if args.samples else collect(args)
    if args.counts:
        counter = Recorded(args.counts)
    else:
        key = os.environ.get("ANTHROPIC_API_KEY")
        if not key:
            raise SystemExit("ANTHROPIC_API_KEY is unset; the estimates stay at bytes / 4")
        counter = Counter(key, args.model)
    overhead = counter.count(".") - 1
    taken = {kind: [] for kind in TYPES}
    sources = {kind: set() for kind in TYPES}
    for sample in corpus["samples"]:
        kind, label, piece = sample["type"], sample["source"], sample["text"]
        if counter.key and counter.key in piece:
            raise SystemExit("a sample holds the API key; refusing to send it (%s)" % label)
        tokens = counter.count(piece) - overhead
        if tokens > 0:
            taken[kind].append((label, len(piece.encode("utf-8")), tokens))
            sources[kind].add(label)

    rows = {}
    print("model %s · commit %s · overhead %d token(s) per request · %d request(s)"
          % (args.model, corpus["commit"][:7], overhead, counter.requests))
    print("%-6s %8s %8s %10s %9s %12s %20s" % ("type", "sources", "samples", "bytes", "tokens", "bytes/token",
                                               "sample min/med/max"))
    for kind in TYPES:
        samples = taken[kind]
        if not samples:
            continue
        size = sum(s for _, s, _ in samples)
        tokens = sum(t for _, _, t in samples)
        ratios = [s / t for _, s, t in samples]
        rows[kind] = {"sources": len(sources[kind]), "samples": len(samples), "bytes": size, "tokens": tokens,
                      "bytes_per_token": round(size / tokens, 3),
                      "sample_bytes_per_token": [round(min(ratios), 2), round(statistics.median(ratios), 2),
                                                 round(max(ratios), 2)]}
        print("%-6s %8d %8d %10d %9d %12.3f %20s" % (kind, len(sources[kind]), len(samples), size, tokens,
                                                     size / tokens, "/".join("%.2f" % r for r in
                                                                             rows[kind]["sample_bytes_per_token"])))
    total_bytes = sum(r["bytes"] for r in rows.values())
    total_tokens = sum(r["tokens"] for r in rows.values())
    if total_tokens:
        print("all    %8s %8d %10d %9d %12.3f" % ("", sum(r["samples"] for r in rows.values()), total_bytes,
                                                  total_tokens, total_bytes / total_tokens))
    if args.json:
        args.json.write_text(json.dumps({
            "model": args.model, "commit": corpus["commit"], "date": datetime.date.today().isoformat(),
            "overhead_tokens": overhead, "types": rows,
        }, indent=2) + "\n")


if __name__ == "__main__":
    sys.exit(main())
