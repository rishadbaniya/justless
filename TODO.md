# TODO

Open work only. Finished work is in [CHANGELOG.md](CHANGELOG.md) and the Git history; ideas that were measured and closed, and what not to build, are in [docs/decisions.md](docs/decisions.md).

## Next

### Paid benchmark pilot
- **Goal.** A defensible end-to-end number: cost per successful task, justless vs plain Claude Code vs Anthropic's server-side context editing, at equal or better success.
- **Progress.** The four-arm harness is *on main* (`scripts/agent_bench`, 118 tests): Claude Code 2.1.327 parity (65 of 68 tool results byte-identical), 32 frozen tasks from the owner's repos, sandboxed runs, a $90 hard cap enforced per request, pre-registered analysis. Offline validation passed (dry runs with 0 mismatches). The task store (about 60 GB) is in the session scratchpad. Spent so far on paid runs: about $1.10 (the earlier repair benchmark).
- **Done 2026-11-19.** Store moved to `~/.local/share/justless-bench`; `cc-capture` and `parity-diff` redone with the current binary (65 verified, 3 documented deviations); 8 snapshots whose `Cargo.lock` predated the toolchain rebuilt; `validate-tasks` 412 runs, 0 mismatches; dry runs and the dry anchor pass; 3 secret-scan hits reviewed as false positives. A first paid pass (`pilot`, $0.074) was voided after the disk filled (33 runs failed with ENOSPC; see its `INCIDENT.md`) and rerun as `pilot2`.
- **Next.** Read `pilot2` and `estimate --campaign pilot2`; `review-tasks --approve`, dropping `lint-prediction_market_workspace-03` (its unmutated file is readable from another repo's Cargo git checkout) with a logged reason; then the main campaign (pre-pilot estimate $74.16, cap $80) after the owner's go-ahead.
