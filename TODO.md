# TODO

Open work only. Finished work is in [CHANGELOG.md](CHANGELOG.md) and the Git history; ideas that were measured and closed, and what not to build, are in [docs/decisions.md](docs/decisions.md).

## Next

### Large-output benchmark
- **Why.** The main campaign (Sonnet, 248 runs, $10.46) found no demonstrated saving on the existing 31 small tasks (−1.5% [−15.4%, +7.6%], success 98.4% both); they read about 5.6 KB of tool output per run. Pilot, preparation and that result are in docs/benchmarks.md.
- **Done 2026-11-20.** The `large` task set exists (8 tasks from prediction_market_workspace, 59–192 KB of tool output per plain run), passed validation and is frozen (manifest `7185d3845e8aeb68`); see "Large-output tasks" in docs/benchmarks.md.
- **Next.** The paid run, which needs the owner's go-ahead: `run --task-set large --campaign large --model claude-sonnet-5-5 --arms a,b,c,d --reps 3 --ce-trigger 30000 --token-limit 3000000 --budget-usd 35` (96 runs, about $19 expected), then `report --task-set large --campaign large`, and record the result in docs/benchmarks.md and the README.

### Read shapes that still reach the agent whole
- **Why.** Reads and `grep` of files outside the workspace, and the variables a line sets, landed on 2026-11-20: in 30 days of this project's Claude Code transcripts, 332 more results (3.4 MB; MB = 10⁶ bytes here) now qualify for the post-run hook. After clipping script output and reading globs, `discover` still classifies about 530 results over 4 KB (4.5 MB) as not supported.
- **Next, by size.** Reads piped into `grep` or `sed`, such as `sed -n 1,200p f | grep x` (53 results, 0.5 MB). Other `awk` programs (31 results, 0.2 MB; plain line ranges are read since 2026-11-20). Supported commands kept whole in pass-through forms, mostly `git diff` and `sed` (201 results, 1.3 MB). `ls`, `wc` and `pwd` in a folder outside the workspace (about 0.1 MB). Loops are not worth unrolling: 1 of 82 loop results would verify as a read list.
- **Done 2026-11-20.** Output of scripts, loops and tools no hook knows is clipped over the preview budget (289 results, 1.7 MB). Of results over 4 KB that did not go through Justless, 77% of the bytes now qualify with globs, up from 69%.
- **Check.** These outside results share the `read batch` label in `stats`, so watch how often those runs are reopened in full now that scratch notes and logs get an excerpt.
