#!/usr/bin/env python3
"""Measure `justless test --affected` against the full suite on mutants.

Copies a Cargo project to a disposable directory, commits it, and for each
mutant inserts `panic!("mutant")` at the start of one non-test function. Each
mutant runs twice from the same warm build: once as `justless test --affected`
and once as `justless test` (the full suite), both with the same cargo command.
It reports missed failures (the full suite failed, the subset passed), how
often the subset ran instead of a fallback, wall time in seconds, and the
bytes each run printed. Nothing is fetched: cargo runs with --offline.

    python3 scripts/affected_benchmark.py --binary ./target/debug/justless
    python3 scripts/affected_benchmark.py --binary ./target/debug/justless \
        --project ../some-crate --limit 20 --no-fallback
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FUNCTION = re.compile(r"^(\s*)(pub(\([^)]*\))?\s+)?(const\s+)?(async\s+)?(unsafe\s+)?fn\s+(\w+)[^;]*\{\s*$")


def run(command: list[str], cwd: Path, check: bool = True) -> subprocess.CompletedProcess:
    result = subprocess.run(command, cwd=cwd, capture_output=True, text=True)
    if check and result.returncode != 0:
        raise SystemExit(f"{' '.join(command)} failed in {cwd}:\n{result.stderr[-2000:]}")
    return result


def copy_project(source: Path, destination: Path) -> None:
    ignore = shutil.ignore_patterns("target", ".justless", ".git")
    shutil.copytree(source, destination, ignore=ignore, dirs_exist_ok=True)
    git = ["git", "-c", "user.name=bench", "-c", "user.email=bench@example.invalid"]
    run(git + ["init", "-q"], destination)
    run(git + ["add", "-A"], destination)
    run(git + ["commit", "-qm", "baseline"], destination)


def mutants(project: Path, limit: int | None) -> list[tuple[str, int, str]]:
    """(file, line after which to insert, function name) for non-test functions."""
    found = []
    for path in sorted(project.rglob("*.rs")):
        relative = path.relative_to(project).as_posix()
        parts = relative.split("/")
        if "target" in parts or "tests" in parts or "benches" in parts or "examples" in parts:
            continue
        lines = path.read_text().splitlines()
        in_tests = False
        depth_at_tests = None
        depth = 0
        for number, line in enumerate(lines, 1):
            stripped = line.strip()
            if stripped.startswith("#[cfg(test)]"):
                in_tests = True
                depth_at_tests = depth
            match = FUNCTION.match(line)
            previous = lines[number - 2].strip() if number > 1 else ""
            if (
                match
                and not in_tests
                and not (previous.startswith("#[") and "test" in previous)
                and "const fn" not in line
                and not stripped.startswith("//")
            ):
                found.append((relative, number, match.group(7)))
            depth += line.count("{") - line.count("}")
            if in_tests and depth_at_tests is not None and depth <= depth_at_tests and "}" in line:
                in_tests = False
    if limit is not None and len(found) > limit:
        step = len(found) / limit
        found = [found[int(index * step)] for index in range(limit)]
    return found


def apply(project: Path, mutant: tuple) -> None:
    if isinstance(mutant[1], dict):
        # A named edit: replace the first occurrence of `find` with `replace`.
        edit = mutant[1]
        path = project / edit["file"]
        text = path.read_text()
        if edit["find"] not in text:
            raise SystemExit(f"{edit['find']!r} is not in {edit['file']}")
        path.write_text(text.replace(edit["find"], edit["replace"], 1))
        return
    file, line, name = mutant
    path = project / file
    lines = path.read_text().splitlines(keepends=True)
    indent = re.match(r"\s*", lines[line - 1]).group(0) + "    "
    statement = 'let _ = "edit";' if BENIGN else f'panic!("mutant in {name}");'
    lines.insert(line, f"{indent}{statement}\n")
    path.write_text("".join(lines))


# Whether mutants keep behavior (a passing re-check) instead of panicking.
BENIGN = False


def reset(project: Path, cargo: list[str]) -> None:
    run(["git", "checkout", "-q", "--", "."], project)
    run(["git", "clean", "-qfd", "-e", "target", "-e", ".justless"], project)
    # Warm the build at the baseline so both measurements start alike.
    run(cargo + ["--no-run"], project, check=False)


def measure(binary: Path, project: Path, args: list[str]) -> dict:
    started = time.monotonic()
    result = subprocess.run([str(binary), *args], cwd=project, capture_output=True, text=True)
    elapsed = time.monotonic() - started
    line = next(
        (text for text in result.stdout.splitlines() if text.startswith("cargo ·")),
        "",
    )
    mode = line.split(" · ")[1] if " · " in line else "unknown"
    selected = re.search(r"(\d+) of (\d+) indexed tests in (\d+) of (\d+) test targets", line)
    return {
        "status": result.returncode,
        "seconds": round(elapsed, 2),
        "bytes": len(result.stdout.encode()) + len(result.stderr.encode()),
        "mode": mode,
        "selected": int(selected.group(1)) if selected else None,
        "indexed": int(selected.group(2)) if selected else None,
        "targets": int(selected.group(3)) if selected else None,
        "all_targets": int(selected.group(4)) if selected else None,
        "verdict": result.stderr.strip().splitlines()[-2:] if result.stderr.strip() else [],
    }


def median(values: list[float]) -> float | None:
    values = [value for value in values if value is not None]
    return round(statistics.median(values), 2) if values else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/justless")
    parser.add_argument("--project", type=Path, default=ROOT / "fixtures/affected")
    parser.add_argument("--limit", type=int, help="Mutants to sample evenly")
    parser.add_argument("--no-fallback", action="store_true", help="Also measure --no-fallback")
    parser.add_argument(
        "--benign",
        action="store_true",
        help="Insert a no-op statement instead of a panic, so every suite passes",
    )
    parser.add_argument(
        "--edits",
        type=Path,
        help='JSON list of {"name", "file", "find", "replace"} edits to measure instead of mutants',
    )
    parser.add_argument("--json", type=Path, help="Write every measurement here")
    parser.add_argument("--keep", action="store_true", help="Keep the disposable copy")
    options = parser.parse_args()
    global BENIGN
    BENIGN = options.benign
    binary = options.binary.resolve()
    scratch = Path(tempfile.mkdtemp(prefix="affected-bench-"))
    project = scratch / "project"
    copy_project(options.project.resolve(), project)
    cargo = ["cargo", "test", "--offline"]
    reset(project, cargo)
    run([str(binary), "index"], project)
    rows = []
    if options.edits:
        cases = [(edit["name"], edit) for edit in json.loads(options.edits.read_text())]
    else:
        cases = mutants(project, options.limit)
    for mutant in cases:
        if isinstance(mutant[1], dict):
            row = {"mutant": mutant[0]}
        else:
            row = {"mutant": f"{mutant[0]}:{mutant[1]} {mutant[2]}"}
        # --fresh: no comparison with the previous mutant's run in either output.
        variants = [("affected", ["test", "--affected", "--fresh", "--", *cargo])]
        if options.no_fallback:
            variants.append(
                ("no_fallback", ["test", "--affected", "--no-fallback", "--fresh", "--", *cargo])
            )
        variants.append(("full", ["test", "--fresh", "--", *cargo]))
        for name, args in variants:
            reset(project, cargo)
            apply(project, mutant)
            row[name] = measure(binary, project, args)
        row["missed"] = row["full"]["status"] != 0 and row["affected"]["status"] == 0
        if options.no_fallback:
            row["missed_no_fallback"] = row["full"]["status"] != 0 and row["no_fallback"]["status"] == 0
        rows.append(row)
        affected = row["affected"]
        print(
            f"{row['mutant']:<48} {affected['mode']:<24} "
            f"subset {affected['seconds']:>6.2f} s {affected['bytes']:>6} B · "
            f"full {row['full']['seconds']:>6.2f} s {row['full']['bytes']:>6} B"
            f"{' · MISSED' if row['missed'] else ''}",
            flush=True,
        )
    reset(project, cargo)
    subsets = [row for row in rows if row["affected"]["mode"] == "subset"]
    summary = {
        "project": str(options.project),
        "mutants": len(rows),
        "full_suite_failed": sum(row["full"]["status"] != 0 for row in rows),
        "missed_failures": sum(row["missed"] for row in rows),
        "ran_as_subset": len(subsets),
        "fell_back": sum(row["affected"]["mode"] == "full suite" for row in rows),
        "median_seconds_affected": median([row["affected"]["seconds"] for row in rows]),
        "median_seconds_full": median([row["full"]["seconds"] for row in rows]),
        "median_seconds_subset_only": median([row["affected"]["seconds"] for row in subsets]),
        "median_seconds_full_for_subsets": median([row["full"]["seconds"] for row in subsets]),
        "median_bytes_affected": median([row["affected"]["bytes"] for row in rows]),
        "median_bytes_full": median([row["full"]["bytes"] for row in rows]),
        "median_selected_share": median(
            [row["affected"]["selected"] / row["affected"]["indexed"] for row in subsets if row["affected"]["indexed"]]
        ),
        "median_target_share": median(
            [row["affected"]["targets"] / row["affected"]["all_targets"] for row in subsets if row["affected"]["all_targets"]]
        ),
    }
    if options.no_fallback:
        summary["missed_failures_no_fallback"] = sum(row["missed_no_fallback"] for row in rows)
        summary["median_seconds_no_fallback"] = median([row["no_fallback"]["seconds"] for row in rows])
        summary["median_bytes_no_fallback"] = median([row["no_fallback"]["bytes"] for row in rows])
    print(json.dumps(summary, indent=2))
    if options.json:
        options.json.write_text(json.dumps({"summary": summary, "rows": rows}, indent=2))
    if options.keep:
        print(f"kept {project}", file=sys.stderr)
    else:
        shutil.rmtree(scratch, ignore_errors=True)
    return 1 if summary["missed_failures"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
