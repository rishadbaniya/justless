"""Build a pristine corpus repo's warm target and record its baseline test names (3 runs, flaky scan).

Usage: python3 baseline_build.py REPO [--runs 3]
Writes <store>/pristine/REPO/{target, baseline.json}. Never runs Justless.
"""

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import config  # noqa: E402
import oracles  # noqa: E402
import workspace  # noqa: E402

WORKSPACE_REPOS = {"prediction_market_workspace"}


def test_command(repo, package=None):
    command = ["cargo", "test", "--offline", "--no-fail-fast"]
    if package:
        command += ["-p", package]
    elif repo in WORKSPACE_REPOS:
        command.append("--workspace")
    return command


def env_for(root, target):
    return workspace.tool_env(root, Path(root).parent / "build-home", config.HOME / "toolbin", target)


def run(command, root, target, timeout=7200):
    started = time.time()
    proc = subprocess.run(command, cwd=str(root), env=env_for(root, target), stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
    return proc.returncode, proc.stdout.decode("utf-8", "replace"), time.time() - started


def baseline(repo, root, target, runs=3, package=None):
    command = test_command(repo, package)
    code, out, seconds = run(command[:3] + ["--no-run"] + command[4:], root, target)
    report = {"repo": repo, "package": package, "build_s": round(seconds, 1), "build_exit": code, "runs": []}
    if code != 0:
        report["build_tail"] = out[-2000:]
        return report
    statuses = []
    for _ in range(runs):
        code, out, seconds = run(command, root, target)
        results = oracles.parse_tests(out)
        statuses.append(results)
        report["runs"].append({"exit": code, "seconds": round(seconds, 1), "tests": len(results),
                               "failed": sorted(n for n, s in results.items() if s == "FAILED")})
    names = set()
    for results in statuses:
        names |= set(results)
    report["flaky"] = sorted(n for n in names if len({r.get(n) for r in statuses}) > 1)
    report["test_names"] = statuses[0] if statuses else {}
    report["test_command"] = command
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--package")
    args = parser.parse_args(argv)
    base = config.HOME / "pristine" / args.repo
    workspace.make_toolbin(config.HOME / "toolbin")
    report = baseline(args.repo, base / "repo", base / "target", args.runs, args.package)
    name = "baseline.json" if not args.package else "baseline_%s.json" % args.package
    (base / name).write_text(json.dumps(report, indent=1, sort_keys=True))
    print(json.dumps({k: v for k, v in report.items() if k != "test_names"}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
