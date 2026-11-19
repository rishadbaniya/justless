# TODO

Open work only. Finished work is in [CHANGELOG.md](CHANGELOG.md) and the Git history; ideas that were measured and closed, and what not to build, are in [docs/decisions.md](docs/decisions.md).

## Next

### Cut the test suite
- About 785 tests and roughly 33,500 lines of integration tests, 20,192 of them in `tests/cli.rs`. Cut the test code by about half: merge tests that check the same behaviour into table-driven ones and drop duplicates, while keeping every regression case and the `affected_benchmark.py` result of 0 missed failures. Start once the in-progress branches land, since they all add to `tests/cli.rs`.
- **Progress.** *designed*.

### First release
- **Problem.** No tagged release, so every install compiles from source and needs Rust.
- **Progress.** `release.yml` and the checksum-verified installer are *on main*; no tag yet. Tag `v0.1.0` after the in-progress fixes land.

---

### Paid benchmark pilot
- **Goal.** A defensible end-to-end number: cost per successful task, justless vs plain Claude Code vs Anthropic's server-side context editing, at equal or better success.
- **Progress.** The four-arm harness is *on main* (`scripts/agent_bench`, 118 tests): Claude Code 2.1.327 parity (65 of 68 tool results byte-identical), 32 frozen tasks from the owner's repos, sandboxed runs, a $90 hard cap enforced per request, pre-registered analysis. Offline validation passed (dry runs with 0 mismatches). The task store (about 60 GB) is in the session scratchpad. Spent so far on paid runs: about $1.10 (the earlier repair benchmark).
- **Next.** Move the store to a durable private directory; redo `run.py cc-capture` and `parity-diff` with the current binary (hooks changed since the capture); re-run `prepare --generate` for the one lint task that exposes a source file; `validate-tasks`; `review-tasks --approve`; then the pilot (estimate $2.53, cap $3). Scale up only if the pilot is sane.
