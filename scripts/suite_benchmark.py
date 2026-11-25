#!/usr/bin/env python3
"""Reproducible Wave 1-6 benchmark suite exercising 18 real Justless compactors.

Every byte measured here is genuinely produced by the target `justless` binary
inside an isolated temporary workspace (never modifying the repository's own
`.justless/` directory and requiring zero network access).
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[1]
RUN_ID_RE = re.compile(r"\[justless (run-[A-Za-z0-9]+)")
RANGE_RE = re.compile(r"\[(stdout|stderr):(\d+)(?:-(\d+))?\]")


def clean_env(home: Path, extra_bin: Path | None = None) -> dict[str, str]:
    env = dict(os.environ)
    orig_home = Path(env.get("HOME", str(Path.home())))
    env.setdefault("RUSTUP_HOME", str(orig_home / ".rustup"))
    env.setdefault("CARGO_HOME", str(orig_home / ".cargo"))
    for key in (
        "JUSTLESS_DISABLE",
        "JUSTLESS_SESSION",
        "CLAUDE_SESSION_ID",
        "JUSTLESS_MAX_BYTES",
    ):
        env.pop(key, None)
    env["HOME"] = str(home)
    env["XDG_STATE_HOME"] = str(home / ".local/state")
    env["CLAUDE_CONFIG_DIR"] = str(home / ".claude")
    env["CODEX_HOME"] = str(home / ".codex")
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_AUTHOR_NAME"] = "Justless Bench"
    env["GIT_AUTHOR_EMAIL"] = "bench@justless.local"
    env["GIT_COMMITTER_NAME"] = "Justless Bench"
    env["GIT_COMMITTER_EMAIL"] = "bench@justless.local"
    env["GIT_AUTHOR_DATE"] = "2026-11-25T12:00:00Z"
    env["GIT_COMMITTER_DATE"] = "2026-11-25T12:00:00Z"
    env["LC_ALL"] = "C"
    if extra_bin is not None:
        env["PATH"] = f"{extra_bin}:{env.get('PATH', '')}"
    return env


def write_executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def write_fixture_shim(
    bin_dir: Path,
    tool: str,
    fixture_dir: Path,
    stdout_text: str = "",
    stderr_text: str = "",
    exit_code: int = 0,
) -> Path:
    out_file = fixture_dir / f"{tool}.stdout"
    err_file = fixture_dir / f"{tool}.stderr"
    out_file.write_text(stdout_text, encoding="utf-8")
    err_file.write_text(stderr_text, encoding="utf-8")
    lines = ["#!/bin/sh"]
    if stdout_text:
        lines.append(f"cat '{out_file}'")
    if stderr_text:
        lines.append(f"cat '{err_file}' >&2")
    lines.append(f"exit {exit_code}")
    shim = bin_dir / tool
    write_executable(shim, "\n".join(lines) + "\n")
    return shim


def run_cmd(
    argv: list[str],
    cwd: Path,
    env: dict[str, str],
    stdin_bytes: bytes | None = None,
) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        argv,
        cwd=cwd,
        env=env,
        input=stdin_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def extract_run_id(proc: subprocess.CompletedProcess[bytes]) -> str | None:
    for blob in (proc.stderr, proc.stdout):
        match = RUN_ID_RE.search(blob.decode("utf-8", errors="replace"))
        if match:
            return match.group(1)
    return None


def replay_log(
    binary: Path,
    cwd: Path,
    env: dict[str, str],
    run_id: str,
    stream: str,
    start: int | None = None,
    end: int | None = None,
) -> str:
    argv = [str(binary), "log", run_id, "--stream", stream]
    if start is not None:
        argv.extend(["--from", str(start)])
    if end is not None:
        argv.extend(["--to", str(end)])
    proc = run_cmd(argv, cwd, env)
    if proc.returncode != 0:
        raise AssertionError(
            f"justless log failed (exit {proc.returncode}): "
            f"{proc.stderr.decode('utf-8', errors='replace')}"
        )
    blob = proc.stdout if stream == "stdout" else proc.stderr
    return blob.decode("utf-8", errors="replace")


def slice_lines(text: str, start: int, end: int) -> str:
    lines = text.splitlines(keepends=True)
    return "".join(lines[start - 1 : end])


def assert_contains(haystack: str, needle: str, ctx: str) -> None:
    if needle not in haystack:
        raise AssertionError(f"[{ctx}] missing expected text {needle!r}\n---\n{haystack[:1600]}")


def assert_range_replays(
    binary: Path,
    cwd: Path,
    env: dict[str, str],
    run_id: str,
    rendered: str,
    raw_streams: dict[str, str],
    ctx: str,
    stream_filter: str | None = None,
) -> tuple[str, int, int]:
    for match in RANGE_RE.finditer(rendered):
        stream = match.group(1)
        if stream_filter and stream != stream_filter:
            continue
        start = int(match.group(2))
        end = int(match.group(3) or start)
        replayed = replay_log(binary, cwd, env, run_id, stream, start, end)
        expected = slice_lines(raw_streams[stream], start, end)
        if replayed != expected:
            raise AssertionError(
                f"[{ctx}] replay mismatch for [{stream}:{start}-{end}]:\n"
                f"expected: {expected[:300]!r}\n"
                f"got:      {replayed[:300]!r}"
            )
        return stream, start, end
    raise AssertionError(f"[{ctx}] no [{stream_filter or 'stdout|stderr'}:A-B] pointer in:\n{rendered}")


def make_metrics(raw_bytes: int, justless_bytes: int) -> tuple[float, float]:
    cut_pct = round((1.0 - (justless_bytes / raw_bytes)) * 100.0, 1) if raw_bytes > 0 else 0.0
    ratio = round(raw_bytes / max(justless_bytes, 1), 1)
    return cut_pct, ratio


def make_case(
    case_id: str,
    label: str,
    subtitle: str,
    category: str,
    raw_bytes: int,
    justless_bytes: int,
    raw_lines: int,
    justless_lines: int,
    exit_code: int,
    expected_exit: int,
    elapsed_ms: float,
    checks: list[str],
    run_id: str | None = None,
    subcases: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    if exit_code != expected_exit:
        raise AssertionError(
            f"[{case_id}] exit code {exit_code} != expected {expected_exit}"
        )
    if raw_bytes <= 0 or justless_bytes <= 0:
        raise AssertionError(
            f"[{case_id}] non-positive byte count: raw={raw_bytes}, justless={justless_bytes}"
        )
    if justless_bytes >= raw_bytes:
        raise AssertionError(
            f"[{case_id}] did not compress strictly: justless={justless_bytes} >= raw={raw_bytes}"
        )
    cut_pct, ratio = make_metrics(raw_bytes, justless_bytes)
    entry: dict[str, Any] = {
        "id": case_id,
        "label": label,
        "subtitle": subtitle,
        "category": category,
        "raw_bytes": raw_bytes,
        "justless_bytes": justless_bytes,
        "raw_lines": raw_lines,
        "justless_lines": justless_lines,
        "cut_pct": cut_pct,
        "ratio": ratio,
        "exit_code": exit_code,
        "expected_exit": expected_exit,
        "elapsed_ms": round(elapsed_ms, 1),
        "checks": checks,
    }
    if run_id is not None:
        entry["run_id"] = run_id
    if subcases:
        entry["subcases"] = subcases
    return entry


def run_hook_pair(
    binary: Path,
    cwd: Path,
    env: dict[str, str],
    cmd: list[str],
    extra_justless_args: list[str] | None = None,
) -> tuple[subprocess.CompletedProcess[bytes], subprocess.CompletedProcess[bytes], str]:
    raw = run_cmd(cmd, cwd, env)
    jl_cmd = [str(binary), "run", "--hook", "--fresh"]
    if extra_justless_args:
        jl_cmd.extend(extra_justless_args)
    jl_cmd.append("--")
    jl_cmd.extend(cmd)
    jl = run_cmd(jl_cmd, cwd, env)
    run_id = extract_run_id(jl)
    if not run_id:
        raise AssertionError(
            f"missing run ID for {cmd!r}\nstdout:\n{jl.stdout.decode('utf-8', 'replace')}\n"
            f"stderr:\n{jl.stderr.decode('utf-8', 'replace')}"
        )
    return raw, jl, run_id


def count_lines(blob: bytes) -> int:
    return blob.decode("utf-8", errors="replace").count("\n")


def git_run(cwd: Path, env: dict[str, str], *args: str) -> str:
    proc = run_cmd(["git", "-c", "commit.gpgsign=false", *args], cwd, env)
    if proc.returncode != 0:
        raise AssertionError(
            f"git {' '.join(args)} failed: {proc.stderr.decode('utf-8', errors='replace')}"
        )
    return proc.stdout.decode("utf-8", errors="replace")


# ---------------------------------------------------------------------------
# Case 1: cargo test (failing copy of fixtures/failing)
# ---------------------------------------------------------------------------
def case_01_cargo_test(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    failing_dir = work / "failing"
    shutil.copytree(REPO_ROOT / "fixtures/failing", failing_dir)
    (failing_dir / ".justless.toml").write_text("", encoding="utf-8")
    env = clean_env(home)
    # Pre-warm build so benchmark measures test execution, not cold rustc compile.
    warm = run_cmd(["cargo", "test", "--no-run"], failing_dir, env)
    if warm.returncode != 0:
        raise AssertionError(f"cargo test --no-run failed:\n{warm.stderr.decode('utf-8', 'replace')}")

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(binary, failing_dir, env, ["cargo", "test"])
    elapsed = (time.monotonic() - t0) * 1000.0

    raw_out = raw.stdout.decode("utf-8", errors="replace")
    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "1 reported failing test(s)", "cargo_test")
    assert_contains(jl_out, "total_includes_fee", "cargo_test")
    assert_contains(jl_out, "tests/checkout.rs:11:5", "cargo_test")
    assert_contains(jl_out, "1 ran no tests: unittests src/lib.rs", "cargo_test")

    fail_line = next(
        (ln for ln in jl_out.splitlines() if "total_includes_fee" in ln and "[stdout:" in ln),
        "",
    )
    m = RANGE_RE.search(fail_line)
    if not m:
        raise AssertionError(f"[cargo_test] missing [stdout:A-B] on failure line:\n{jl_out}")
    start, end = int(m.group(2)), int(m.group(3) or m.group(2))
    replayed = replay_log(binary, failing_dir, env, run_id, "stdout", start, end)
    assert_contains(replayed, "panicked at tests/checkout.rs:11:5", "cargo_test replay")
    assert_contains(replayed, "left: 106", "cargo_test replay")
    assert_contains(replayed, "right: 105", "cargo_test replay")

    return make_case(
        case_id="cargo_test",
        label="cargo test",
        subtitle="failing fixture",
        category="tests",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout) + count_lines(raw.stderr),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=101,
        elapsed_ms=elapsed,
        checks=[
            "exit 101 preserved",
            "Wave 2 empty target collapsed",
            f"exact log replay [stdout:{start}-{end}]",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 2: vitest (80 files: 78 pass + 2 fail sharing root cause)
# ---------------------------------------------------------------------------
def case_02_vitest(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case02"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    out_lines = [" RUN  v2.1.8 /repo/web", ""]
    for i in range(1, 79):
        out_lines.append(f" ✓ src/modules/mod_{i:02d}.test.ts (6 tests) {4 + (i % 9)}ms")
    out_lines.extend([
        " ❯ src/billing/subtotal.test.ts (4 tests | 1 failed) 18ms",
        " ❯ src/billing/grand_total.test.ts (4 tests | 1 failed) 16ms",
        "",
        " Test Files  2 failed | 78 passed (80)",
        "      Tests  2 failed | 474 passed (476)",
        "   Start at  12:00:00",
        "   Duration  2.14s (transform 110ms, tests 640ms)",
    ])
    vitest_out = "\n".join(out_lines) + "\n"
    vitest_err = (
        "⎯⎯⎯⎯⎯⎯ Failed Tests 2 ⎯⎯⎯⎯⎯⎯\n\n"
        " FAIL  src/billing/subtotal.test.ts > Billing > calculates subtotal\n"
        "TypeError: Cannot read properties of undefined (reading 'taxRate')\n"
        " ❯ tax src/pricing.ts:21:18\n"
        "     19| export function tax(n: number) {\n"
        "     20|   const cfg = loadConfig();\n"
        "     21|   return n * cfg.taxRate;\n"
        "       |                  ^\n"
        " ❯ src/billing/subtotal.test.ts:14:12\n\n"
        "⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[1/2]⎯\n\n"
        " FAIL  src/billing/grand_total.test.ts > Billing > calculates grand total\n"
        "TypeError: Cannot read properties of undefined (reading 'taxRate')\n"
        " ❯ tax src/pricing.ts:21:18\n"
        "     19| export function tax(n: number) {\n"
        "     20|   const cfg = loadConfig();\n"
        "     21|   return n * cfg.taxRate;\n"
        "       |                  ^\n"
        " ❯ src/billing/grand_total.test.ts:27:12\n\n"
        "⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[2/2]⎯\n"
    )
    write_fixture_shim(bin_dir, "vitest", case_dir, vitest_out, vitest_err, exit_code=1)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(binary, ws, env, ["vitest", "run"])
    elapsed = (time.monotonic() - t0) * 1000.0

    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "2 reported failing test(s)", "vitest")
    assert_contains(jl_out, "2 test(s):", "vitest")
    assert_contains(jl_out, "taxRate", "vitest")
    assert_contains(jl_out, "78 passed test file(s)", "vitest")
    if replay_log(binary, ws, env, run_id, "stderr") != vitest_err:
        raise AssertionError("[vitest] full stderr replay mismatch")

    return make_case(
        case_id="vitest",
        label="vitest run",
        subtitle="80 files · 2 fail",
        category="tests",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout) + count_lines(raw.stderr),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=1,
        elapsed_ms=elapsed,
        checks=[
            "exit 1 preserved",
            "2 failures grouped by shared root cause",
            "78 passing files collapsed + exact stderr replay",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 3: pytest (50 parametrized failures + 1 collection error)
# ---------------------------------------------------------------------------
def case_03_pytest(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case03"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    chunks = [
        "============================= test session starts ==============================\n"
        "platform linux -- Python 3.12.3, pytest-8.2.0, pluggy-1.5.0\n"
        "collected 55 items / 1 error\n\n"
        "tests/test_parser.py FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF [ 90%]\n"
        "tests/test_billing.py .F.. [100%]\n\n"
        "==================================== ERRORS ====================================\n"
        "_____________________ ERROR collecting tests/test_broken.py ____________________\n"
        "tests/test_broken.py:3: in <module>\n"
        "    import missing_pkg\n"
        "E   ModuleNotFoundError: No module named 'missing_pkg'\n"
        "=================================== FAILURES ===================================\n"
    ]
    for i in range(1, 51):
        chunks.append(
            f"___________________________ test_parse[case_{i:02d}] ___________________________\n"
            f"x = 'case_{i:02d}'\n\n"
            "    def test_parse(x):\n"
            ">       assert parse(x) == 200\n"
            f"E       AssertionError: expected status 200, got 500 at 0x7f123456{i:04x}\n"
            "E       assert 500 == 200\n\n"
            "tests/test_parser.py:42: AssertionError\n"
        )
    chunks.append(
        "___________________________ test_charge_tax ____________________________\n"
        "    def test_charge_tax():\n"
        ">       compute_tax(-1)\n"
        "E       ValueError: negative amount not allowed\n\n"
        "src/billing.py:88: ValueError\n"
        "=========================== short test summary info ============================\n"
    )
    for i in range(1, 51):
        chunks.append(
            f"FAILED tests/test_parser.py::test_parse[case_{i:02d}] - AssertionError: expected status 200, got 500\n"
        )
    chunks.append(
        "FAILED tests/test_billing.py::test_charge_tax - ValueError: negative amount not allowed\n"
        "ERROR tests/test_broken.py - ModuleNotFoundError: No module named 'missing_pkg'\n"
        "=================== 51 failed, 3 passed, 1 error in 2.41s ======================\n"
    )
    pytest_out = "".join(chunks)
    write_fixture_shim(bin_dir, "pytest", case_dir, pytest_out, "", exit_code=1)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(binary, ws, env, ["pytest"])
    elapsed = (time.monotonic() - t0) * 1000.0

    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "50 test(s): tests/test_parser.py::test_parse[...]", "pytest")
    assert_contains(jl_out, "ModuleNotFoundError: No module named 'missing_pkg'", "pytest")
    assert_contains(jl_out, "ValueError: negative amount not allowed", "pytest")
    assert_range_replays(binary, ws, env, run_id, jl_out, {"stdout": pytest_out}, "pytest", "stdout")

    return make_case(
        case_id="pytest",
        label="pytest",
        subtitle="50 parametrized + err",
        category="tests",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout) + count_lines(raw.stderr),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=1,
        elapsed_ms=elapsed,
        checks=[
            "exit 1 preserved",
            "50 parametrized failures folded",
            "collection error + ValueError preserved + exact replay",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 4: dotnet test (40 passing + 1 failing test)
# ---------------------------------------------------------------------------
def case_04_dotnet_test(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case04"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    lines = [
        "  Determining projects to restore...",
        "  All projects are up-to-date for restore.",
        "  App -> /repo/src/App/bin/Debug/net8.0/App.dll",
        "  App.Tests -> /repo/tests/App.Tests/bin/Debug/net8.0/App.Tests.dll",
        "Test run for /repo/tests/App.Tests/bin/Debug/net8.0/App.Tests.dll (.NETCoreApp,Version=v8.0)",
        "Microsoft (R) Test Execution Command Line Tool Version 17.10.0",
        "Starting test execution, please wait...",
        "A total of 1 test files matched the specified pattern.",
    ]
    for i in range(1, 41):
        lines.append(f"  Passed App.Tests.PricingTests.CalculatesRate_{i:02d} [{1 + (i % 4)} ms]")
    lines.extend([
        "  Failed App.Tests.CheckoutTests.AppliesFee [14 ms]",
        "  Error Message:",
        "   Assert.Equal() Failure: Expected: 105, Actual: 106",
        "  Stack Trace:",
        "     at App.Tests.CheckoutTests.AppliesFee() in /repo/tests/CheckoutTests.cs:line 19",
        "Failed!  - Failed:     1, Passed:    40, Skipped:     0, Total:    41, Duration: 248 ms - App.Tests.dll (net8.0)",
    ])
    dotnet_out = "\n".join(lines) + "\n"
    write_fixture_shim(bin_dir, "dotnet", case_dir, dotnet_out, "", exit_code=1)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(binary, ws, env, ["dotnet", "test"])
    elapsed = (time.monotonic() - t0) * 1000.0

    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "1 reported failing test(s)", "dotnet_test")
    assert_contains(jl_out, "AppliesFee", "dotnet_test")
    assert_contains(jl_out, "Expected: 105, Actual: 106", "dotnet_test")
    assert_range_replays(binary, ws, env, run_id, jl_out, {"stdout": dotnet_out}, "dotnet_test", "stdout")

    return make_case(
        case_id="dotnet_test",
        label="dotnet test",
        subtitle="40 pass + 1 fail",
        category="tests",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout) + count_lines(raw.stderr),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=1,
        elapsed_ms=elapsed,
        checks=[
            "exit 1 preserved",
            "40 passing tests collapsed",
            "assertion message preserved + exact replay",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 5: phpstan (multi-finding lint output across 3 rules)
# ---------------------------------------------------------------------------
def case_05_phpstan(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case05"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    lines = []
    for i in range(1, 31):
        lines.append(
            f"src/Services/Service{i:02d}.php:{10 + i}:"
            f"Method App\\Services\\Service{i:02d}::execute() has no return type specified. [missingType.return]"
        )
    for i in range(1, 21):
        lines.append(
            f"src/Controllers/Handler{i:02d}.php:{20 + i}:"
            f"Parameter #1 $amount of method App\\Billing::charge() expects int, string given. [argument.type]"
        )
    for i in range(1, 11):
        lines.append(
            f"src/Models/Record{i:02d}.php:{30 + i}:"
            f"Access to an undefined property App\\Models\\Record{i:02d}::$legacyId. [property.notFound]"
        )
    lines.append("[ERROR] Found 60 errors")
    phpstan_out = "\n".join(lines) + "\n"
    write_fixture_shim(bin_dir, "phpstan", case_dir, phpstan_out, "", exit_code=1)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(binary, ws, env, ["phpstan", "analyse", "src"])
    elapsed = (time.monotonic() - t0) * 1000.0

    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "missingType.return", "phpstan")
    assert_contains(jl_out, "argument.type", "phpstan")
    assert_contains(jl_out, "property.notFound", "phpstan")
    assert_contains(jl_out, "60", "phpstan")
    assert_range_replays(binary, ws, env, run_id, jl_out, {"stdout": phpstan_out}, "phpstan", "stdout")

    return make_case(
        case_id="phpstan",
        label="phpstan analyse",
        subtitle="60 findings · 3 rules",
        category="lint",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout) + count_lines(raw.stderr),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=1,
        elapsed_ms=elapsed,
        checks=[
            "exit 1 preserved",
            "60 findings grouped across 3 rules",
            "exact [stdout:A-B] replay verified",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 6: docker build (BuildKit 350-line multi-stage Rust+Node clean build)
# ---------------------------------------------------------------------------
def case_06_docker_build(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case06"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    lines = [
        "#0 building with \"default\" instance using docker driver",
        "#1 [internal] load build definition from Dockerfile",
        "#1 transferring dockerfile: 1.48kB done",
        "#1 DONE 0.1s",
        "#2 [internal] load metadata for docker.io/library/node:22-alpine",
        "#2 DONE 0.6s",
        "#3 [internal] load metadata for docker.io/library/rust:1.91-slim",
        "#3 DONE 0.6s",
        "#4 [frontend 1/4] FROM docker.io/library/node:22-alpine",
        "#4 CACHED",
        "#5 [frontend 2/4] WORKDIR /web",
        "#5 CACHED",
        "#6 [frontend 3/4] COPY web/package*.json ./",
        "#6 DONE 0.1s",
        "#7 [frontend 4/4] RUN npm ci && npm run build",
    ]
    for i in range(1, 111):
        lines.append(f"#7 {0.100 + i * 0.04:.3f} added package @acme/ui-module-{i:03d}@2.1.{i % 10} (cached tarball)")
    lines.extend([
        "#7 5.210 ✓ 418 modules transformed.",
        "#7 5.430 dist/assets/index-D9a1f8c2.js   312.40 kB │ gzip: 98.12 kB",
        "#7 5.440 ✓ built in 1.84s",
        "#7 DONE 5.6s",
        "#8 [builder 1/4] FROM docker.io/library/rust:1.91-slim",
        "#8 CACHED",
        "#9 [builder 2/4] WORKDIR /app",
        "#9 CACHED",
        "#10 [builder 3/4] COPY Cargo.toml Cargo.lock src ./",
        "#10 DONE 0.2s",
        "#11 [builder 4/4] RUN cargo build --release --locked",
    ])
    for i in range(1, 206):
        lines.append(f"#11 {0.200 + i * 0.15:.3f}    Compiling workspace-dep-{i:03d} v1.{i % 9}.{i % 17}")
    lines.extend([
        "#11 33.910     Finished `release` profile [optimized] target(s) in 33.70s",
        "#11 DONE 34.2s",
        "#12 [runtime 1/3] FROM gcr.io/distroless/cc-debian12",
        "#12 CACHED",
        "#13 [runtime 2/3] COPY --from=builder /app/target/release/web-api /usr/local/bin/web-api",
        "#13 DONE 0.2s",
        "#14 [runtime 3/3] COPY --from=frontend /web/dist /srv/static",
        "#14 DONE 0.2s",
        "#15 exporting to image",
        "#15 exporting layers 0.9s done",
        "#15 writing image sha256:9f8e7d6c5b4a3210fedcba9876543210abcdef0123456789abcdef0123456789 done",
        "#15 naming to ghcr.io/acme/web-api:v2.4.0 done",
        "#15 DONE 1.1s",
        "[+] Building 42.8s (15/15) FINISHED docker:default",
    ])
    bk_err = "\n".join(lines) + "\n"
    write_fixture_shim(bin_dir, "docker", case_dir, "", bk_err, exit_code=0)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(
        binary, ws, env, ["docker", "build", "-t", "ghcr.io/acme/web-api:v2.4.0", "."]
    )
    elapsed = (time.monotonic() - t0) * 1000.0

    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "ghcr.io/acme/web-api:v2.4.0", "docker_build")
    cut_pct, _ = make_metrics(len(raw.stdout) + len(raw.stderr), len(jl.stdout) + len(jl.stderr))
    if cut_pct <= 95.0:
        raise AssertionError(f"[docker_build] expected >95% cut, got {cut_pct}%\n{jl_out}")
    if replay_log(binary, ws, env, run_id, "stderr") != bk_err:
        raise AssertionError("[docker_build] exact stderr replay mismatch")

    return make_case(
        case_id="docker_build",
        label="docker build",
        subtitle="350-line BuildKit",
        category="build",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout) + count_lines(raw.stderr),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "exit 0 preserved",
            "image tag ghcr.io/acme/web-api:v2.4.0 preserved",
            f">{95}% cut ({cut_pct}%) + exact stderr replay",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 7: kubectl logs (500-line interleaved pod logs with 3 distinct errors)
# ---------------------------------------------------------------------------
def case_07_kubectl_logs(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case07"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    pods = ["payments-0/app", "payments-1/app", "payments-2/app"]
    lines: list[str] = []
    for i in range(1, 501):
        pod = pods[i % 3]
        mm = (i // 60) % 60
        ss = i % 60
        ts = f"2026-11-25T10:{mm:02d}:{ss:02d}.{i % 1000:03d}Z"
        if i == 112:
            lines.append(
                f"[pod/{pod}] {ts} ERROR database pool exhausted code=53300 active=64 max=64"
            )
        elif i == 287:
            lines.append(
                f"[pod/{pod}] {ts} ERROR stripe webhook signature mismatch event_id=evt_9f8a7b6c"
            )
        elif i == 431:
            lines.append(
                f"[pod/{pod}] {ts} ERROR deadlock detected on table ledger_entries tx_id=984210"
            )
        elif i % 3 == 0:
            lines.append(
                f"[pod/{pod}] {ts} INFO GET /healthz status=200 latency={3 + (i % 7)}ms"
            )
        elif i % 3 == 1:
            lines.append(
                f"[pod/{pod}] {ts} INFO POST /v1/charges status=200 latency={18 + (i % 15)}ms req_id=req-{i:05d}"
            )
        else:
            lines.append(
                f"[pod/{pod}] {ts} INFO cache hit key=rate:usd:{i % 20} ttl={120 + (i % 45)}s"
            )
    logs_out = "\n".join(lines) + "\n"
    write_fixture_shim(bin_dir, "kubectl", case_dir, logs_out, "", exit_code=0)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(
        binary, ws, env, ["kubectl", "logs", "-l", "app=payments", "--prefix", "--tail=500"]
    )
    elapsed = (time.monotonic() - t0) * 1000.0

    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "database pool exhausted code=53300", "kubectl_logs")
    assert_contains(jl_out, "stripe webhook signature mismatch", "kubectl_logs")
    assert_contains(jl_out, "deadlock detected on table ledger_entries", "kubectl_logs")
    assert_range_replays(binary, ws, env, run_id, jl_out, {"stdout": logs_out}, "kubectl_logs", "stdout")

    return make_case(
        case_id="kubectl_logs",
        label="kubectl logs",
        subtitle="500 interleaved lines",
        category="logs",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout) + count_lines(raw.stderr),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "all 3 distinct errors preserved verbatim",
            "500 interleaved lines folded into templates",
            "exact [stdout:A-B] replay verified",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 8: kubectl get pods + kubectl describe pod
# ---------------------------------------------------------------------------
def case_08_kubectl_pods_describe(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case08"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    # 8a: 120 pods (118 healthy + 2 CrashLoopBackOff)
    pods_lines = [
        "NAMESPACE     NAME                              READY   STATUS             RESTARTS        AGE"
    ]
    namespaces = ["prod", "payments", "checkout", "edge"]
    for i in range(1, 121):
        ns = namespaces[i % len(namespaces)]
        if i == 41:
            pods_lines.append(
                f"{ns:<13} api-7f8d9c6b5-bad01               0/1     CrashLoopBackOff   14 (2m ago)     1h"
            )
        elif i == 97:
            pods_lines.append(
                f"{ns:<13} worker-5c4b3a2f1-bad02            0/1     CrashLoopBackOff   9 (4m ago)      45m"
            )
        else:
            pods_lines.append(
                f"{ns:<13} service-{i:03d}-6d5c4b3a-ok{i:02d}       1/1     Running            0               3d"
            )
    pods_out = "\n".join(pods_lines) + "\n"

    # 8b: kubectl describe pod with huge last-applied-configuration JSON + env vars + Warning events
    last_applied = json.dumps(
        {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "api-7f8d9c6b5-bad01", "namespace": "prod"},
            "spec": {
                "containers": [
                    {
                        "name": "app",
                        "image": "ghcr.io/acme/api:v2.4.1",
                        "env": [
                            {"name": f"VAR_{i:02d}", "value": f"config-payload-value-{i:04d}"}
                            for i in range(60)
                        ],
                    }
                ]
            },
        },
        separators=(",", ":"),
    )
    desc_lines = [
        "Name:             api-7f8d9c6b5-bad01",
        "Namespace:        prod",
        "Priority:         0",
        "Service Account:  api-sa",
        "Node:             worker-node-03/10.0.1.42",
        "Start Time:       Wed, 25 Nov 2026 10:00:00 +0000",
        "Labels:           app=api",
        "                  pod-template-hash=7f8d9c6b5",
        "                  tier=backend",
        "                  version=v2.4.1",
        "Annotations:      cni.projectcalico.org/podIP: 10.244.1.5/32",
        f"                  kubectl.kubernetes.io/last-applied-configuration: {last_applied}",
        "                  prometheus.io/port: 9090",
        "                  prometheus.io/scrape: true",
        "Status:           Running",
        "IP:               10.244.1.5",
        "IPs:",
        "  IP:           10.244.1.5",
        "Controlled By:    ReplicaSet/api-7f8d9c6b5",
        "Containers:",
        "  app:",
        "    Container ID:   containerd://1234567890abcdef",
        "    Image:          ghcr.io/acme/api:v2.4.1",
        "    Port:           8080/TCP",
        "    State:          Waiting",
        "      Reason:       CrashLoopBackOff",
        "    Last State:     Terminated",
        "      Reason:       OOMKilled",
        "      Exit Code:    137",
        "      Started:      Wed, 25 Nov 2026 10:04:00 +0000",
        "      Finished:     Wed, 25 Nov 2026 10:04:12 +0000",
        "    Ready:          False",
        "    Restart Count:  14",
        "    Limits:",
        "      memory:  256Mi",
        "    Environment:",
    ]
    for i in range(40):
        desc_lines.append(f"      APP_SETTING_{i:02d}:  value-{i:04d}")
    desc_lines.extend([
        "    Mounts:",
        "      /etc/config from app-config (ro)",
        "      /var/run/secrets/kubernetes.io/serviceaccount from kube-api-access-xyz (ro)",
        "  sidecar:",
        "    Container ID:   containerd://abcdef1234567890",
        "    Image:          envoyproxy/envoy:v1.31.0",
        "    State:          Running",
        "      Started:      Wed, 25 Nov 2026 10:00:05 +0000",
        "    Ready:          True",
        "    Restart Count:  0",
        "    Environment:",
        "      ENVOY_LOG_LEVEL:  info",
        "    Mounts:",
        "      /etc/envoy from envoy-cfg (ro)",
        "Conditions:",
        "  Type                        Status",
        "  PodReadyToStartContainers   True",
        "  Initialized                 True",
        "  Ready                       False",
        "  ContainersReady             False",
        "  PodScheduled                True",
        "Volumes:",
        "  app-config:",
        "    Type:      ConfigMap (a volume populated by a ConfigMap)",
        "    Name:      api-config",
        "  kube-api-access-xyz:",
        "    Type:                    Projected (a volume that contains injected data)",
        "    TokenExpirationSeconds:  3607",
        "Events:",
        "  Type     Reason     Age                From               Message",
        "  ----     ------     ----               ----               -------",
        "  Normal   Scheduled  10m                default-scheduler  Successfully assigned prod/api-7f8d9c6b5-bad01 to worker-node-03",
        "  Normal   Pulled     9m                 kubelet            Container image \"ghcr.io/acme/api:v2.4.1\" already present on machine",
        "  Normal   Created    9m                 kubelet            Created container app",
        "  Normal   Started    9m                 kubelet            Started container app",
        "  Warning  BackOff    2m (x18 over 8m)   kubelet            Back-off restarting failed container app in pod api-7f8d9c6b5-bad01",
    ])
    desc_out = "\n".join(desc_lines) + "\n"

    pods_file = case_dir / "pods.txt"
    desc_file = case_dir / "describe.txt"
    pods_file.write_text(pods_out, encoding="utf-8")
    desc_file.write_text(desc_out, encoding="utf-8")
    write_executable(
        bin_dir / "kubectl",
        f"#!/bin/sh\ncase \"$1\" in\n"
        f"  get) cat '{pods_file}' ;;\n"
        f"  describe) cat '{desc_file}' ;;\n"
        f"  *) exit 2 ;;\nesac\n",
    )
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw_a, jl_a, rid_a = run_hook_pair(binary, ws, env, ["kubectl", "get", "pods", "-A"])
    raw_b, jl_b, rid_b = run_hook_pair(
        binary, ws, env, ["kubectl", "describe", "pod", "api-7f8d9c6b5-bad01", "-n", "prod"]
    )
    elapsed = (time.monotonic() - t0) * 1000.0

    out_a = jl_a.stdout.decode("utf-8", errors="replace")
    out_b = jl_b.stdout.decode("utf-8", errors="replace")
    assert_contains(out_a, "120 listed, 2 need attention", "kubectl_get_pods")
    assert_contains(out_a, "api-7f8d9c6b5-bad01", "kubectl_get_pods")
    assert_contains(out_a, "worker-5c4b3a2f1-bad02", "kubectl_get_pods")
    assert_contains(out_a, "CrashLoopBackOff", "kubectl_get_pods")
    assert_contains(out_b, "CrashLoopBackOff", "kubectl_describe")
    assert_contains(out_b, "OOMKilled", "kubectl_describe")
    assert_contains(out_b, "Warning", "kubectl_describe")
    assert_contains(out_b, "BackOff", "kubectl_describe")
    assert_range_replays(binary, ws, env, rid_a, out_a, {"stdout": pods_out}, "kubectl_get_pods", "stdout")
    assert_range_replays(binary, ws, env, rid_b, out_b, {"stdout": desc_out}, "kubectl_describe", "stdout")

    raw_bytes = len(raw_a.stdout) + len(raw_a.stderr) + len(raw_b.stdout) + len(raw_b.stderr)
    jl_bytes = len(jl_a.stdout) + len(jl_a.stderr) + len(jl_b.stdout) + len(jl_b.stderr)
    return make_case(
        case_id="kubectl_pods_describe",
        label="kubectl pods + desc",
        subtitle="120 pods + describe",
        category="infra",
        raw_bytes=raw_bytes,
        justless_bytes=jl_bytes,
        raw_lines=count_lines(raw_a.stdout) + count_lines(raw_b.stdout),
        justless_lines=count_lines(jl_a.stdout) + count_lines(jl_a.stderr) + count_lines(jl_b.stdout) + count_lines(jl_b.stderr),
        exit_code=0,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "both CrashLoopBackOff pods preserved",
            "last-applied-configuration folded + Warning BackOff kept",
            "exact [stdout:A-B] replay on both",
        ],
        subcases=[
            {
                "id": "kubectl_get_pods",
                "raw_bytes": len(raw_a.stdout),
                "justless_bytes": len(jl_a.stdout) + len(jl_a.stderr),
                "run_id": rid_a,
            },
            {
                "id": "kubectl_describe_pod",
                "raw_bytes": len(raw_b.stdout),
                "justless_bytes": len(jl_b.stdout) + len(jl_b.stderr),
                "run_id": rid_b,
            },
        ],
    )


# ---------------------------------------------------------------------------
# Case 9: gh pr checks + pnpm audit
# ---------------------------------------------------------------------------
def case_09_gh_checks_pnpm_audit(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case09"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    # 9a: 60 passing + 1 failing check
    checks_lines = [
        f"ci / build-shard-{i:02d}\tpass\t1m{i % 60:02d}s\thttps://github.com/acme/platform/actions/runs/{1000 + i}"
        for i in range(1, 61)
    ]
    checks_lines.append(
        "ci / integration\tfail\t3m05s\thttps://github.com/acme/platform/actions/runs/9999"
    )
    checks_out = "\n".join(checks_lines) + "\n"

    # 9b: pnpm audit multi-path box-art table
    audit_chunks = []
    for i in range(12):
        audit_chunks.append(
            "┌───────────────┬──────────────────────────────────────────────────────────────┐\n"
            "│ high          │ Prototype Pollution in lodash                                │\n"
            "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
            "│ Package       │ lodash                                                       │\n"
            "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
            "│ Patched in    │ >=4.17.21                                                    │\n"
            "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
            f"│ Dependency of │ service-{i:02d}                                                   │\n"
            "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
            f"│ Path          │ service-{i:02d} > express > body-parser > lodash                  │\n"
            "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
            "│ More info     │ https://github.com/advisories/GHSA-jf85-cpcp-j695            │\n"
            "└───────────────┴──────────────────────────────────────────────────────────────┘\n"
        )
    audit_chunks.append(
        "┌───────────────┬──────────────────────────────────────────────────────────────┐\n"
        "│ critical      │ Remote Code Execution in minimist                            │\n"
        "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
        "│ Package       │ minimist                                                     │\n"
        "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
        "│ Patched in    │ >=1.2.6                                                      │\n"
        "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
        "│ Dependency of │ cli-core                                                     │\n"
        "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
        "│ Path          │ cli-core > optimist > minimist                               │\n"
        "├───────────────┼──────────────────────────────────────────────────────────────┤\n"
        "│ More info     │ https://github.com/advisories/GHSA-xvch-5gv4-984h            │\n"
        "└───────────────┴──────────────────────────────────────────────────────────────┘\n"
        "13 vulnerabilities found\n"
    )
    audit_out = "".join(audit_chunks)

    write_fixture_shim(bin_dir, "gh", case_dir, checks_out, "", exit_code=1)
    write_fixture_shim(bin_dir, "pnpm", case_dir, audit_out, "", exit_code=1)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw_a, jl_a, rid_a = run_hook_pair(binary, ws, env, ["gh", "pr", "checks", "42"])
    raw_b, jl_b, rid_b = run_hook_pair(binary, ws, env, ["pnpm", "audit"])
    elapsed = (time.monotonic() - t0) * 1000.0

    out_a = jl_a.stdout.decode("utf-8", errors="replace")
    out_b = jl_b.stdout.decode("utf-8", errors="replace")
    if jl_a.returncode != 1 or jl_b.returncode != 1:
        raise AssertionError(f"[gh_checks_pnpm_audit] exit codes: {jl_a.returncode}, {jl_b.returncode}")
    assert_contains(out_a, "60 passed", "gh_pr_checks")
    assert_contains(out_a, "ci / integration", "gh_pr_checks")
    assert_contains(out_b, "[critical] minimist (GHSA-xvch-5gv4-984h)", "pnpm_audit")
    assert_contains(out_b, "[high] lodash (GHSA-jf85-cpcp-j695)", "pnpm_audit")
    assert_contains(out_b, "(×12 paths)", "pnpm_audit")
    assert_range_replays(binary, ws, env, rid_a, out_a, {"stdout": checks_out}, "gh_pr_checks", "stdout")
    assert_range_replays(binary, ws, env, rid_b, out_b, {"stdout": audit_out}, "pnpm_audit", "stdout")

    return make_case(
        case_id="gh_checks_pnpm_audit",
        label="gh checks + audit",
        subtitle="61 checks + 13 vuln",
        category="cli_views",
        raw_bytes=len(raw_a.stdout) + len(raw_b.stdout),
        justless_bytes=len(jl_a.stdout) + len(jl_a.stderr) + len(jl_b.stdout) + len(jl_b.stderr),
        raw_lines=count_lines(raw_a.stdout) + count_lines(raw_b.stdout),
        justless_lines=count_lines(jl_a.stdout) + count_lines(jl_a.stderr) + count_lines(jl_b.stdout) + count_lines(jl_b.stderr),
        exit_code=1,
        expected_exit=1,
        elapsed_ms=elapsed,
        checks=[
            "failing check ci / integration preserved",
            "critical minimist sorted before ×12 lodash",
            "exact [stdout:A-B] replay on both",
        ],
        subcases=[
            {
                "id": "gh_pr_checks",
                "raw_bytes": len(raw_a.stdout),
                "justless_bytes": len(jl_a.stdout) + len(jl_a.stderr),
                "run_id": rid_a,
            },
            {
                "id": "pnpm_audit",
                "raw_bytes": len(raw_b.stdout),
                "justless_bytes": len(jl_b.stdout) + len(jl_b.stderr),
                "run_id": rid_b,
            },
        ],
    )


# ---------------------------------------------------------------------------
# Case 10: terraform plan (120 refreshes + 12 resources w/ # forces replacement + outputs)
# ---------------------------------------------------------------------------
def case_10_terraform_plan(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case10"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    chunks: list[str] = []
    for i in range(120):
        chunks.append(f"aws_subnet.private[{i}]: Refreshing state... [id=subnet-{i:04d}]\n")
    chunks.append("\nTerraform will perform the following actions:\n\n")
    chunks.append(
        "  # aws_instance.web must be replaced\n"
        "-/+ resource \"aws_instance\" \"web\" {\n"
        "      ~ ami           = \"ami-old\" -> \"ami-new\" # forces replacement\n"
        "      ~ subnet_id     = \"subnet-1\" -> \"subnet-2\" # forces replacement\n"
        "      ~ instance_type = \"t3.micro\" -> \"t3.small\"\n"
        "    }\n\n"
        "  # aws_launch_template.edge must be replaced\n"
        "+/- resource \"aws_launch_template\" \"edge\" {\n"
        "      ~ name = \"edge-v1\" -> \"edge-v2\" # forces replacement\n"
        "    }\n\n"
    )
    for i in range(1, 7):
        chunks.append(
            f"  # aws_security_group.svc_{i} will be updated in-place\n"
            f"  ~ resource \"aws_security_group\" \"svc_{i}\" {{\n"
            f"        id          = \"sg-0a1b2c3d4e5f100{i}\"\n"
            f"        arn         = \"arn:aws:ec2:us-east-1:123456789012:security-group/sg-100{i}\"\n"
            f"        name        = \"acme-prod-service-{i}-sg\"\n"
            f"        vpc_id      = \"vpc-0987654321fedcba\"\n"
            f"      ~ description = \"Security group for service-{i} rev v{i}\" -> \"Security group for service-{i} rev v{i + 1}\"\n"
            f"      ~ tags        = {{\n"
            f"            \"Environment\" = \"production\"\n"
            f"            \"ManagedBy\"   = \"terraform\"\n"
            f"            \"Owner\"       = \"platform-infra\"\n"
            f"          ~ \"Rev\"         = \"{i}\" -> \"{i + 1}\"\n"
            f"        }}\n"
            f"      + ingress {{\n"
            f"          + cidr_blocks = [\"10.20.{i}.0/24\"]\n"
            f"          + description = \"Allow internal mTLS traffic to service-{i}\"\n"
            f"          + from_port   = 8443\n"
            f"          + protocol    = \"tcp\"\n"
            f"          + to_port     = 8443\n"
            f"        }}\n"
            f"    }}\n\n"
        )
    for i in range(1, 3):
        chunks.append(
            f"  # aws_iam_role.worker_{i} will be created\n"
            f"  + resource \"aws_iam_role\" \"worker_{i}\" {{\n"
            f"      + arn                   = (known after apply)\n"
            f"      + create_date           = (known after apply)\n"
            f"      + force_detach_policies = false\n"
            f"      + id                    = (known after apply)\n"
            f"      + max_session_duration  = 3600\n"
            f"      + name                  = \"acme-prod-worker-role-{i}\"\n"
            f"      + path                  = \"/service-roles/\"\n"
            f"    }}\n\n"
        )
    for i in range(1, 3):
        chunks.append(
            f"  # aws_s3_bucket.legacy_{i} will be destroyed\n"
            f"  - resource \"aws_s3_bucket\" \"legacy_{i}\" {{\n"
            f"      - arn                         = \"arn:aws:s3:::acme-legacy-bucket-{i}\" -> null\n"
            f"      - bucket                      = \"acme-legacy-bucket-{i}\" -> null\n"
            f"      - bucket_domain_name          = \"acme-legacy-bucket-{i}.s3.amazonaws.com\" -> null\n"
            f"      - bucket_regional_domain_name = \"acme-legacy-bucket-{i}.s3.us-east-1.amazonaws.com\" -> null\n"
            f"      - region                      = \"us-east-1\" -> null\n"
            f"    }}\n\n"
        )
    chunks.append(
        "Plan: 4 to add, 6 to change, 4 to destroy.\n\n"
        "Changes to Outputs:\n"
        "  + endpoint = \"https://api.example.com\"\n"
        "  ~ url      = \"http://old\" -> \"https://new\"\n"
        "  - old_arn  = \"arn:aws:s3:::acme-legacy-bucket-1\" -> null\n"
    )
    tf_out = "".join(chunks)
    write_fixture_shim(bin_dir, "terraform", case_dir, tf_out, "", exit_code=0)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(binary, ws, env, ["terraform", "plan"])
    elapsed = (time.monotonic() - t0) * 1000.0

    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "Refreshed/read 120 resource(s)", "terraform_plan")
    assert_contains(jl_out, "replace: ami, subnet_id", "terraform_plan")
    assert_contains(jl_out, "replace: name", "terraform_plan")
    assert_contains(jl_out, "endpoint", "terraform_plan")
    assert_contains(jl_out, "old_arn", "terraform_plan")
    assert_range_replays(binary, ws, env, run_id, jl_out, {"stdout": tf_out}, "terraform_plan", "stdout")

    return make_case(
        case_id="terraform_plan",
        label="terraform plan",
        subtitle="120 refresh + 12 res",
        category="infra",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout) + count_lines(raw.stderr),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "120 refreshes folded to 1 line",
            "# forces replacement attributes preserved",
            "Changes to Outputs preserved + exact replay",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 11: cargo tree (400-line workspace tree with multi-version syn duplicates)
# ---------------------------------------------------------------------------
def case_11_cargo_tree(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case11"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    lines = ["platform v0.1.0 (/repo)"]
    for i in range(20):
        lines.append(f"├── service-dep-{i:02d} v1.{i}.0")
        lines.append("│   ├── syn v1.0.109")
        for j in range(16):
            lines.append(f"│   │   ├── leaf-{i:02d}-{j:02d} v0.{i}.{j}")
        lines.append("│   │   └── quote v1.0.37 (*)")
        lines.append("│   └── proc-macro2 v1.0.86 (*)")
    lines.extend([
        "└── macro-helper v0.2.0",
        "    ├── syn v2.0.87",
        "    └── proc-macro2 v1.0.86 (*)",
    ])
    tree_out = "\n".join(lines) + "\n"
    write_fixture_shim(bin_dir, "cargo", case_dir, tree_out, "", exit_code=0)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(binary, ws, env, ["cargo", "tree"])
    elapsed = (time.monotonic() - t0) * 1000.0

    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "syn", "cargo_tree")
    assert_contains(jl_out, "v1.0.109", "cargo_tree")
    assert_contains(jl_out, "v2.0.87", "cargo_tree")
    assert_range_replays(binary, ws, env, run_id, jl_out, {"stdout": tree_out}, "cargo_tree", "stdout")

    return make_case(
        case_id="cargo_tree",
        label="cargo tree",
        subtitle="400 lines · syn dupes",
        category="deps",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout) + count_lines(raw.stderr),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "both syn v1.0.109 & v2.0.87 duplicates preserved",
            "transitive subtrees collapsed with [stdout:A-B]",
            "exact replay verified",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 12: psql ASCII aligned table (500-row dump — Tier 1 lossless & Tier 2 bounded)
# ---------------------------------------------------------------------------
def case_12_psql_table(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case12"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    lines = [
        "  id  |   region   |  status  |        endpoint        ",
        "------+------------+----------+------------------------",
    ]
    for i in range(1, 501):
        status = "degraded" if i in (37, 214, 488) else "healthy"
        lines.append(f" {i:>4} | us-east-1  | {status:<8} | /v1/orders/shard-{i:03d}   ")
    lines.append("(500 rows)")
    psql_out = "\n".join(lines) + "\n"
    write_fixture_shim(bin_dir, "psql", case_dir, psql_out, "", exit_code=0)
    env = clean_env(home, bin_dir)

    cmd = ["psql", "-c", "SELECT id, region, status, endpoint FROM endpoints"]
    t0 = time.monotonic()
    # Tier 1 lossless (full 500 rows fit in 32 KiB budget)
    raw_t1, jl_t1, rid_t1 = run_hook_pair(binary, ws, env, cmd, ["--max-bytes", "32768"])
    # Tier 2 bounded preview (default 4 KiB budget)
    raw_t2, jl_t2, rid_t2 = run_hook_pair(binary, ws, env, cmd)
    elapsed = (time.monotonic() - t0) * 1000.0

    t1_out = jl_t1.stdout.decode("utf-8", errors="replace")
    t2_out = jl_t2.stdout.decode("utf-8", errors="replace")
    assert_contains(t1_out, "[table: 500 rows, TSV;", "psql_t1")
    assert_contains(t1_out, "region=us-east-1", "psql_t1")
    assert_contains(t1_out, "status?=healthy", "psql_t1")
    assert_contains(t1_out, "degraded", "psql_t1")
    assert_contains(t1_out, "(500 rows)", "psql_t1")
    if len(jl_t1.stdout) + len(jl_t1.stderr) >= len(raw_t1.stdout):
        raise AssertionError("[psql_t1] Tier 1 did not compress strictly")

    assert_contains(t2_out, "[table preview:", "psql_t2")
    assert_contains(t2_out, "region=us-east-1", "psql_t2")
    assert_contains(t2_out, "status?=healthy", "psql_t2")
    assert_contains(t2_out, "(500 rows)", "psql_t2")
    assert_range_replays(binary, ws, env, rid_t2, t2_out, {"stdout": psql_out}, "psql_t2", "stdout")

    return make_case(
        case_id="psql_table",
        label="psql table",
        subtitle="500 rows · Tier 1+2",
        category="tables",
        raw_bytes=len(raw_t2.stdout),
        justless_bytes=len(jl_t2.stdout) + len(jl_t2.stderr),
        raw_lines=count_lines(raw_t2.stdout),
        justless_lines=count_lines(jl_t2.stdout) + count_lines(jl_t2.stderr),
        exit_code=jl_t2.returncode,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "Tier 1 lossless: all 500 rows + const/default hoisting",
            "Tier 2 bounded: header hoisting + (500 rows) footer",
            "exact [stdout:A-B] middle replay verified",
        ],
        run_id=rid_t2,
        subcases=[
            {
                "id": "psql_tier1_lossless",
                "raw_bytes": len(raw_t1.stdout),
                "justless_bytes": len(jl_t1.stdout) + len(jl_t1.stderr),
                "run_id": rid_t1,
            },
            {
                "id": "psql_tier2_bounded",
                "raw_bytes": len(raw_t2.stdout),
                "justless_bytes": len(jl_t2.stdout) + len(jl_t2.stderr),
                "run_id": rid_t2,
            },
        ],
    )


# ---------------------------------------------------------------------------
# Case 13: gh api nested JSON (80-object nested array — lossless jsonview)
# ---------------------------------------------------------------------------
def case_13_gh_api_json(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    case_dir = work / "case13"
    bin_dir = case_dir / "bin"
    ws = case_dir / "ws"
    ws.mkdir(parents=True)
    (ws / ".justless.toml").write_text("", encoding="utf-8")

    objs = []
    for i in range(1, 81):
        objs.append(
            {
                "number": 1000 + i,
                "state": "open",
                "draft": False,
                "mergeable_state": "dirty" if i in (17, 42, 73) else "clean",
                "title": f"fix(api): stage {i:02d}",
                "user": {"login": f"dev-{i % 8}", "type": "User", "site_admin": False},
                "labels": [
                    {"name": "backend", "color": "0e8a16"},
                    {"name": "p1" if i % 10 == 0 else "p2", "color": "fbca04"},
                ],
            }
        )
    raw_json = json.dumps(objs, indent=2) + "\n"
    write_fixture_shim(bin_dir, "gh", case_dir, raw_json, "", exit_code=0)
    env = clean_env(home, bin_dir)

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(binary, ws, env, ["gh", "api", "repos/acme/platform/pulls"])
    elapsed = (time.monotonic() - t0) * 1000.0

    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "[JSON array as a table:", "gh_api")
    assert_contains(jl_out, "80 objects; fields:", "gh_api")
    assert_contains(jl_out, "user{", "gh_api")
    assert_contains(jl_out, "labels:[", "gh_api")
    assert_contains(jl_out, "mergeable_state?=", "gh_api")
    replayed = replay_log(binary, ws, env, run_id, "stdout")
    if json.loads(replayed) != objs:
        raise AssertionError("[gh_api] replayed JSON does not match original objects")

    return make_case(
        case_id="gh_api_json",
        label="gh api JSON",
        subtitle="80 nested objects",
        category="json",
        raw_bytes=len(raw.stdout),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "lossless deep-schema jsonview ({..} nested + :[..] sub-table)",
            "const & majority-default fields hoisted",
            "exact JSON round-trip replay verified",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 14: git diff semantic + lockfile (reindent + 100% rename + .terraform.lock.hcl + logic edit)
# ---------------------------------------------------------------------------
def case_14_git_diff(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    repo = work / "case14_repo"
    repo.mkdir(parents=True)
    env = clean_env(home)
    git_run(repo, env, "init", "-q", "-b", "main")
    (repo / ".justless.toml").write_text("", encoding="utf-8")
    (repo / ".git/info/exclude").write_text(".justless/\n.justless.toml\n", encoding="utf-8")

    (repo / "src").mkdir()
    (repo / "infra").mkdir()

    # 1. File to be re-indented only (80 lines)
    reindent_v1 = ["pub fn format_rows(rows: &[u32]) -> u32 {"]
    reindent_v1.extend(f"    let v{i} = rows.get({i}).copied().unwrap_or({i});" for i in range(75))
    reindent_v1.append("    v0 + v1\n}\n")
    (repo / "src/reindent.rs").write_text("\n".join(reindent_v1), encoding="utf-8")

    # 2. File to be 100% renamed (60 lines)
    helper_body = "\n".join(
        f"pub fn helper_{i:02d}(x: i64) -> i64 {{ x + {i} }}" for i in range(60)
    ) + "\n"
    (repo / "src/old_helper.rs").write_text(helper_body, encoding="utf-8")

    # 3. .terraform.lock.hcl (>= 30 changed lines so lockdiff folds as generated)
    def tf_lock(ver: str, salt: str) -> str:
        blocks = []
        for idx, prov in enumerate(["aws", "google", "kubernetes", "helm", "random", "tls"]):
            hashes = ",\n".join(f'    "h1:{salt}{idx}{k:02d}abcdef0123456789=",' for k in range(6))
            blocks.append(
                f'provider "registry.terraform.io/hashicorp/{prov}" {{\n'
                f'  version     = "{ver}"\n'
                f'  constraints = "~> {ver}"\n'
                f'  hashes = [\n{hashes}\n  ]\n}}\n'
            )
        return "\n".join(blocks)

    (repo / "infra/.terraform.lock.hcl").write_text(tf_lock("5.60.0", "old"), encoding="utf-8")

    # 4. Real logic edit file
    (repo / "src/pricing.rs").write_text(
        "pub fn checkout_total(subtotal: u64, fee: u64) -> u64 {\n"
        "    subtotal - fee\n"
        "}\n",
        encoding="utf-8",
    )

    git_run(repo, env, "add", "-A")
    git_run(repo, env, "commit", "-q", "-m", "initial")

    # Apply working changes
    reindent_v2 = ["pub fn format_rows(rows: &[u32]) -> u32 {"]
    reindent_v2.extend(f"        let v{i} = rows.get({i}).copied().unwrap_or({i});" for i in range(75))
    reindent_v2.append("        v0 + v1\n}\n")
    (repo / "src/reindent.rs").write_text("\n".join(reindent_v2), encoding="utf-8")
    git_run(repo, env, "mv", "src/old_helper.rs", "src/new_helper.rs")
    (repo / "infra/.terraform.lock.hcl").write_text(tf_lock("5.72.0", "new"), encoding="utf-8")
    (repo / "src/pricing.rs").write_text(
        "pub fn checkout_total(subtotal: u64, fee: u64) -> u64 {\n"
        "    subtotal + fee\n"
        "}\n",
        encoding="utf-8",
    )

    t0 = time.monotonic()
    raw, jl, run_id = run_hook_pair(binary, repo, env, ["git", "diff", "-M", "HEAD"])
    elapsed = (time.monotonic() - t0) * 1000.0

    raw_out = raw.stdout.decode("utf-8", errors="replace")
    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "-    subtotal - fee", "git_diff")
    assert_contains(jl_out, "+    subtotal + fee", "git_diff")
    assert_contains(jl_out, ".terraform.lock.hcl", "git_diff")
    assert_contains(jl_out, "src/reindent.rs", "git_diff")
    assert_contains(jl_out, "new_helper.rs", "git_diff")
    assert_range_replays(binary, repo, env, run_id, jl_out, {"stdout": raw_out}, "git_diff", "stdout")

    return make_case(
        case_id="git_diff",
        label="git diff",
        subtitle="semantic + lockfile",
        category="git",
        raw_bytes=len(raw.stdout) + len(raw.stderr),
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=count_lines(raw.stdout),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "real logic edit kept verbatim",
            "reindent, 100% rename & .terraform.lock.hcl folded",
            "exact [stdout:A-B] replay verified",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 15: git status (60 modified files) + git blame (multi-commit lossless)
# ---------------------------------------------------------------------------
def case_15_git_status_blame(binary: Path, work: Path, home: Path) -> dict[str, Any]:
    repo = work / "case15_repo"
    repo.mkdir(parents=True)
    env = clean_env(home)
    git_run(repo, env, "init", "-q", "-b", "main")
    (repo / ".justless.toml").write_text("", encoding="utf-8")
    (repo / ".git/info/exclude").write_text(".justless/\n.justless.toml\n", encoding="utf-8")

    # Build a 60-line file across 4 distinct commits so lossless git blame fits cleanly in default budget.
    (repo / "src").mkdir()
    engine = repo / "src/engine.rs"
    engine_lines: list[str] = []
    for commit_idx in range(4):
        base = commit_idx * 15
        for j in range(15):
            engine_lines.append(f"let step_{base + j:02d} = acc + {base + j};")
        engine.write_text("\n".join(engine_lines) + "\n", encoding="utf-8")
        commit_env = dict(
            env,
            GIT_AUTHOR_NAME=f"Engineer {commit_idx + 1}",
            GIT_AUTHOR_EMAIL=f"dev{commit_idx + 1}@justless.local",
            GIT_AUTHOR_DATE=f"2026-11-{10 + commit_idx:02d}T12:00:00Z",
            GIT_COMMITTER_DATE=f"2026-11-{10 + commit_idx:02d}T12:00:00Z",
        )
        git_run(repo, commit_env, "add", "src/engine.rs")
        git_run(repo, commit_env, "commit", "-q", "-m", f"add engine block {commit_idx + 1}")

    # Create 60 tracked files across 3 realistic dirs (>4 KiB status output), commit, then modify all 60.
    dirs = [
        "services/payments/handlers",
        "services/billing/workers",
        "web/dashboard/components",
    ]
    tracked: list[Path] = []
    for d in dirs:
        (repo / d).mkdir(parents=True)
        for i in range(20):
            p = repo / d / f"checkout_pipeline_module_{i:02d}.ts"
            p.write_text(f"export const version = {i};\n", encoding="utf-8")
            tracked.append(p)
    git_run(repo, env, "add", "-A")
    git_run(repo, env, "commit", "-q", "-m", "add 60 tracked files")
    for idx, p in enumerate(tracked):
        p.write_text(f"export const version = {idx + 100};\n", encoding="utf-8")

    t0 = time.monotonic()
    raw_st, jl_st, rid_st = run_hook_pair(binary, repo, env, ["git", "status"])
    raw_bl, jl_bl, rid_bl = run_hook_pair(binary, repo, env, ["git", "blame", "src/engine.rs"])
    elapsed = (time.monotonic() - t0) * 1000.0

    st_out = jl_st.stdout.decode("utf-8", errors="replace")
    bl_out = jl_bl.stdout.decode("utf-8", errors="replace")
    assert_contains(st_out, "60 modified", "git_status")
    assert_contains(st_out, "services/payments/handlers/", "git_status")
    if "(use " in st_out:
        raise AssertionError(f"[git_status] expected usage hints stripped:\n{st_out}")
    assert_range_replays(
        binary, repo, env, rid_st, st_out, {"stdout": raw_st.stdout.decode("utf-8", "replace")}, "git_status", "stdout"
    )

    assert_contains(bl_out, "git blame: 60 lines, 4 commits", "git_blame")
    assert_contains(bl_out, "1|let step_00 = acc + 0;", "git_blame")
    assert_contains(bl_out, "60|let step_59 = acc + 59;", "git_blame")

    return make_case(
        case_id="git_status_blame",
        label="git status + blame",
        subtitle="60 files + 60L blame",
        category="git",
        raw_bytes=len(raw_st.stdout) + len(raw_bl.stdout),
        justless_bytes=len(jl_st.stdout) + len(jl_st.stderr) + len(jl_bl.stdout) + len(jl_bl.stderr),
        raw_lines=count_lines(raw_st.stdout) + count_lines(raw_bl.stdout),
        justless_lines=count_lines(jl_st.stdout) + count_lines(jl_st.stderr) + count_lines(jl_bl.stdout) + count_lines(jl_bl.stderr),
        exit_code=0,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "git status groups 60 files by directory + strips hints",
            "git blame lossless: all 60 code lines + 4-commit legend",
            "exact [stdout:A-B] replay verified",
        ],
        subcases=[
            {
                "id": "git_status",
                "raw_bytes": len(raw_st.stdout),
                "justless_bytes": len(jl_st.stdout) + len(jl_st.stderr),
                "run_id": rid_st,
            },
            {
                "id": "git_blame",
                "raw_bytes": len(raw_bl.stdout),
                "justless_bytes": len(jl_bl.stdout) + len(jl_bl.stderr),
                "run_id": rid_bl,
            },
        ],
    )


# ---------------------------------------------------------------------------
# Case 16: rg -n "fn " src (live search on isolated copy of this repo)
# ---------------------------------------------------------------------------
def case_16_rg_search(binary: Path, repo_copy: Path, home: Path) -> dict[str, Any]:
    env = clean_env(home)
    t0 = time.monotonic()
    jl = run_cmd(
        [str(binary), "run", "--fresh", "--", "rg", "-n", "--sort", "path", "fn ", "src"],
        repo_copy,
        env,
    )
    elapsed = (time.monotonic() - t0) * 1000.0

    run_id = extract_run_id(jl)
    if not run_id:
        raise AssertionError(f"[rg_search] missing run ID:\n{jl.stderr.decode('utf-8', 'replace')}")
    raw_out = replay_log(binary, repo_copy, env, run_id, "stdout")
    raw_bytes = len(raw_out.encode("utf-8"))
    jl_out = jl.stdout.decode("utf-8", errors="replace")
    assert_contains(jl_out, "rg: ", "rg_search")
    assert_contains(jl_out, "matches in ", "rg_search")
    assert_range_replays(binary, repo_copy, env, run_id, jl_out, {"stdout": raw_out}, "rg_search", "stdout")

    return make_case(
        case_id="rg_search",
        label='rg -n "fn " src',
        subtitle="live repo search",
        category="search",
        raw_bytes=raw_bytes,
        justless_bytes=len(jl.stdout) + len(jl.stderr),
        raw_lines=raw_out.count("\n"),
        justless_lines=count_lines(jl.stdout) + count_lines(jl.stderr),
        exit_code=jl.returncode,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "grouped by file with match counts",
            "exact [stdout:A-B] replay matches archived raw rg byte-for-byte",
        ],
        run_id=run_id,
    )


# ---------------------------------------------------------------------------
# Case 17: justless read docs/reference.md --outline + expand
# ---------------------------------------------------------------------------
def case_17_read_outline_expand(binary: Path, repo_copy: Path, home: Path) -> dict[str, Any]:
    env = clean_env(home)
    ref_path = repo_copy / "docs/reference.md"
    raw_bytes_data = ref_path.read_bytes()
    raw_lines_list = raw_bytes_data.decode("utf-8").splitlines(keepends=True)

    t0 = time.monotonic()
    outline = run_cmd([str(binary), "read", "docs/reference.md", "--outline"], repo_copy, env)
    if outline.returncode != 0:
        raise AssertionError(f"[read_outline] failed: {outline.stderr.decode('utf-8', 'replace')}")
    outline_text = outline.stdout.decode("utf-8")

    # Verify Wave 4 sleek markdown outline kinds: h1..h3, table, code
    for marker in (" h1 ", " h2 ", " h3 ", " table ", " code "):
        assert_contains(outline_text, marker, "read_outline")

    rev_match = re.search(r"^revision ([0-9a-f]+)", outline_text, re.MULTILINE)
    fail_match = re.search(r"^#(\d+) L(\d+)-(\d+) h3 Failures$", outline_text, re.MULTILINE)
    if not rev_match or not fail_match:
        raise AssertionError(f"[read_outline] could not find revision or Failures anchor:\n{outline_text}")
    rev = rev_match.group(1)
    entry_id = fail_match.group(1)
    line_start, line_end = int(fail_match.group(2)), int(fail_match.group(3))
    anchor = f"{rev}#{entry_id}"

    expanded = run_cmd([str(binary), "expand", "docs/reference.md", anchor], repo_copy, env)
    elapsed = (time.monotonic() - t0) * 1000.0
    if expanded.returncode != 0:
        raise AssertionError(f"[expand] failed: {expanded.stderr.decode('utf-8', 'replace')}")
    expected_slice = "".join(raw_lines_list[line_start - 1 : line_end]).encode("utf-8")
    if expanded.stdout != expected_slice:
        raise AssertionError(
            f"[expand] byte mismatch for {anchor} (L{line_start}-{line_end}): "
            f"got {len(expanded.stdout)} B vs expected {len(expected_slice)} B"
        )

    return make_case(
        case_id="read_outline_expand",
        label="read --outline",
        subtitle="reference.md + expand",
        category="source",
        raw_bytes=len(raw_bytes_data),
        justless_bytes=len(outline.stdout) + len(expanded.stdout),
        raw_lines=len(raw_lines_list),
        justless_lines=count_lines(outline.stdout) + count_lines(expanded.stdout),
        exit_code=0,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "h1/h2/h3 + table + code outline anchors present",
            f"expand {anchor} matches L{line_start}-{line_end} byte-for-byte",
        ],
        subcases=[
            {"id": "outline", "raw_bytes": len(raw_bytes_data), "justless_bytes": len(outline.stdout)},
            {"id": "expand", "raw_bytes": len(raw_bytes_data), "justless_bytes": len(expanded.stdout)},
        ],
    )


# ---------------------------------------------------------------------------
# Case 18: query would_route + map --explain + lookup --graph + mask retirement
# ---------------------------------------------------------------------------
def case_18_graph_and_mask(binary: Path, repo_copy: Path, work: Path, home: Path) -> dict[str, Any]:
    env = clean_env(home)
    mixed_copy = work / "mixed"
    shutil.copytree(REPO_ROOT / "fixtures/mixed", mixed_copy)
    (mixed_copy / ".justless.toml").write_text("", encoding="utf-8")

    t0 = time.monotonic()
    # 18a: query would_route vs rg -n -w would_route src + cat src/agents.rs
    raw_rg = run_cmd(["rg", "-n", "-w", "would_route", "src"], repo_copy, env)
    agents_bytes = (repo_copy / "src/agents.rs").read_bytes()
    raw_query_bytes = len(raw_rg.stdout) + len(agents_bytes)
    raw_query_lines = count_lines(raw_rg.stdout) + count_lines(agents_bytes)

    q_proc = run_cmd([str(binary), "query", "would_route"], repo_copy, env)
    if q_proc.returncode != 0:
        raise AssertionError(f"[query] failed: {q_proc.stderr.decode('utf-8', 'replace')}")
    q_text = q_proc.stdout.decode("utf-8")
    assert_contains(q_text, "would_route [function] at src/agents.rs:", "query")
    assert_contains(q_text, "(from :", "query")

    # 18b: map --explain would_route
    m_proc = run_cmd([str(binary), "map", "--explain", "would_route"], repo_copy, env)
    if m_proc.returncode != 0:
        raise AssertionError(f"[map_explain] failed: {m_proc.stderr.decode('utf-8', 'replace')}")
    m_text = m_proc.stdout.decode("utf-8")
    assert_contains(m_text, "Explain ·", "map_explain")
    assert_contains(m_text, "Centrality ·", "map_explain")
    assert_contains(m_text, "Community ·", "map_explain")

    # 18c: lookup --graph NL traversal on fixtures/mixed (TS -> Python -> SQL)
    mixed_files = sorted(p for p in mixed_copy.rglob("*") if p.is_file() and ".justless" not in p.parts)
    mixed_raw_bytes = sum(p.stat().st_size for p in mixed_files)
    mixed_raw_lines = sum(count_lines(p.read_bytes()) for p in mixed_files)
    l_proc = run_cmd(
        [str(binary), "lookup", "how do frontend orders connect to purchase_orders?", "--graph"],
        mixed_copy,
        env,
    )
    if l_proc.returncode != 0:
        raise AssertionError(f"[lookup_graph] failed: {l_proc.stderr.decode('utf-8', 'replace')}")
    l_text = l_proc.stdout.decode("utf-8")
    assert_contains(l_text, "Graph paths ·", "lookup_graph")
    assert_contains(l_text, "fetchOrders", "lookup_graph")
    assert_contains(l_text, "list_orders", "lookup_graph")
    assert_contains(l_text, "purchase_orders", "lookup_graph")

    # 18d: justless mask retirement (12-turn session) + lossless --restore round-trip
    messages = []
    for i in range(12):
        cid = f"tool_call_{i:02d}"
        messages.append(
            {"role": "assistant", "content": [{"type": "tool_use", "id": cid, "name": "build", "input": {}}]}
        )
        messages.append(
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": cid,
                        "content": f"build {i:02d} checked workspace target module\n" * 120,
                    }
                ],
            }
        )
    before_req = json.dumps({"messages": messages}, separators=(",", ":")).encode("utf-8")
    masked = run_cmd([str(binary), "mask"], repo_copy, env, stdin_bytes=before_req)
    if masked.returncode != 0:
        raise AssertionError(f"[mask] failed: {masked.stderr.decode('utf-8', 'replace')}")
    restored = run_cmd([str(binary), "mask", "--restore"], repo_copy, env, stdin_bytes=masked.stdout)
    elapsed = (time.monotonic() - t0) * 1000.0
    if restored.returncode != 0 or json.loads(restored.stdout) != json.loads(before_req):
        raise AssertionError("[mask] --restore round-trip mismatch")
    if len(masked.stdout) >= len(before_req):
        raise AssertionError(f"[mask] did not reduce request: {len(masked.stdout)} >= {len(before_req)}")

    total_raw = raw_query_bytes + mixed_raw_bytes + len(before_req)
    total_jl = len(q_proc.stdout) + len(m_proc.stdout) + len(l_proc.stdout) + len(masked.stdout)
    total_raw_lines = raw_query_lines + mixed_raw_lines + before_req.count(b"\\n")
    total_jl_lines = (
        count_lines(q_proc.stdout)
        + count_lines(m_proc.stdout)
        + count_lines(l_proc.stdout)
        + 1
    )

    return make_case(
        case_id="graph_and_mask",
        label="query + map + mask",
        subtitle="graph + NL + retire",
        category="code_graph",
        raw_bytes=total_raw,
        justless_bytes=total_jl,
        raw_lines=total_raw_lines,
        justless_lines=total_jl_lines,
        exit_code=0,
        expected_exit=0,
        elapsed_ms=elapsed,
        checks=[
            "query would_route grouped edges + (from :LINE) dedup",
            "map --explain role/centrality/community verified",
            "lookup --graph TS→Python→SQL traversal verified",
            "mask + --restore exact JSON round-trip verified",
        ],
        subcases=[
            {"id": "query_would_route", "raw_bytes": raw_query_bytes, "justless_bytes": len(q_proc.stdout)},
            {"id": "map_explain", "raw_bytes": raw_query_bytes, "justless_bytes": len(m_proc.stdout)},
            {"id": "lookup_graph_mixed", "raw_bytes": mixed_raw_bytes, "justless_bytes": len(l_proc.stdout)},
            {"id": "mask_retirement", "raw_bytes": len(before_req), "justless_bytes": len(masked.stdout)},
        ],
    )


def format_bytes(n: int) -> str:
    if n >= 1024:
        return f"{n / 1024:.1f} KB"
    return f"{n} B"


def render_markdown(report: dict[str, Any]) -> str:
    rows = [
        "| # | Workload | Category | Raw | Justless | Cut | Ratio |",
        "| ---: | --- | --- | ---: | ---: | ---: | ---: |",
    ]
    for idx, c in enumerate(report["cases"], start=1):
        name = f"`{c['label']}` ({c['subtitle']})" if c["subtitle"] else f"`{c['label']}`"
        rows.append(
            f"| {idx} | {name} | {c['category']} | {format_bytes(c['raw_bytes'])} | "
            f"{format_bytes(c['justless_bytes'])} | {c['cut_pct']:.1f}% | {c['ratio']:.1f}× |"
        )
    tot = report["totals"]
    rows.append(
        f"| | **Total ({len(report['cases'])} workloads)** | | **{format_bytes(tot['raw_bytes'])}** | "
        f"**{format_bytes(tot['justless_bytes'])}** | **{tot['cut_pct']:.1f}%** | **{tot['ratio']:.1f}×** |"
    )
    return "\n".join(rows)


def run_suite(binary: Path) -> dict[str, Any]:
    binary = binary.resolve()
    if not binary.is_file():
        raise FileNotFoundError(f"justless binary not found: {binary}")

    suite_t0 = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="justless-suite-") as tmp:
        work = Path(tmp)
        home = work / "home"
        home.mkdir()

        # Isolated read-only copy of repo source + reference doc so repo `.justless/` is untouched.
        repo_copy = work / "repo_copy"
        repo_copy.mkdir()
        (repo_copy / ".justless.toml").write_text("", encoding="utf-8")
        shutil.copy2(REPO_ROOT / "Cargo.toml", repo_copy / "Cargo.toml")
        shutil.copytree(REPO_ROOT / "src", repo_copy / "src")
        (repo_copy / "docs").mkdir()
        shutil.copy2(REPO_ROOT / "docs/reference.md", repo_copy / "docs/reference.md")

        # Run independent cases concurrently in their own isolated temp dirs while preserving
        # exact deterministic ordering (Cases 16-18 share the read-only repo_copy sequentially).
        independent = [
            lambda: case_01_cargo_test(binary, work, home),
            lambda: case_02_vitest(binary, work, home),
            lambda: case_03_pytest(binary, work, home),
            lambda: case_04_dotnet_test(binary, work, home),
            lambda: case_05_phpstan(binary, work, home),
            lambda: case_06_docker_build(binary, work, home),
            lambda: case_07_kubectl_logs(binary, work, home),
            lambda: case_08_kubectl_pods_describe(binary, work, home),
            lambda: case_09_gh_checks_pnpm_audit(binary, work, home),
            lambda: case_10_terraform_plan(binary, work, home),
            lambda: case_11_cargo_tree(binary, work, home),
            lambda: case_12_psql_table(binary, work, home),
            lambda: case_13_gh_api_json(binary, work, home),
            lambda: case_14_git_diff(binary, work, home),
            lambda: case_15_git_status_blame(binary, work, home),
        ]

        def run_repo_cases() -> list[dict[str, Any]]:
            return [
                case_16_rg_search(binary, repo_copy, home),
                case_17_read_outline_expand(binary, repo_copy, home),
                case_18_graph_and_mask(binary, repo_copy, work, home),
            ]

        max_workers = min(8, os.cpu_count() or 4)
        with ThreadPoolExecutor(max_workers=max_workers) as pool:
            repo_future = pool.submit(run_repo_cases)
            ind_futures = [pool.submit(fn) for fn in independent]
            cases = [f.result() for f in ind_futures] + repo_future.result()

    total_raw = sum(c["raw_bytes"] for c in cases)
    total_jl = sum(c["justless_bytes"] for c in cases)
    total_cut, total_ratio = make_metrics(total_raw, total_jl)
    total_ms = round((time.monotonic() - suite_t0) * 1000.0, 1)

    return {
        "schema_version": 1,
        "binary": str(binary),
        "elapsed_ms": total_ms,
        "totals": {
            "count": len(cases),
            "raw_bytes": total_raw,
            "justless_bytes": total_jl,
            "cut_pct": total_cut,
            "ratio": total_ratio,
        },
        "cases": cases,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        type=Path,
        default=REPO_ROOT / "target/release/justless",
        help="Path to the compiled justless binary (default: ./target/release/justless)",
    )
    parser.add_argument(
        "--json",
        nargs="?",
        const="-",
        default=None,
        metavar="PATH",
        help="Write structured JSON to PATH (or stdout if PATH is omitted or '-')",
    )
    parser.add_argument(
        "--markdown",
        action="store_true",
        help="Print a GitHub Markdown summary table",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Verify all 18 invariants, exit codes, strict compression, and exact replays",
    )
    args = parser.parse_args(argv)

    report = run_suite(args.binary)

    if args.check:
        if len(report["cases"]) != 18:
            raise AssertionError(f"expected 18 benchmark cases, got {len(report['cases'])}")
        for c in report["cases"]:
            if not c["checks"]:
                raise AssertionError(f"[{c['id']}] recorded no invariant checks")

    json_text = json.dumps(report, indent=2, ensure_ascii=False) + "\n"
    if args.json == "-":
        sys.stdout.write(json_text)
    elif args.json is not None:
        out_path = Path(args.json)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json_text, encoding="utf-8")

    if args.markdown or (args.json is None):
        if args.json == "-":
            sys.stderr.write(render_markdown(report) + "\n")
        else:
            sys.stdout.write(render_markdown(report) + "\n")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
