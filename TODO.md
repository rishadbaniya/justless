# TODO

Open work only. Finished work is in [CHANGELOG.md](CHANGELOG.md) and the Git history; ideas that were measured and closed, and what not to build, are in [docs/decisions.md](docs/decisions.md).

## Next

### Large-output benchmark
- **Why.** The main campaign (Sonnet, 248 runs, $10.46) found no demonstrated saving on the existing 31 small tasks (−1.5% [−15.4%, +7.6%], success 98.4% both); they read about 5.6 KB of tool output per run. Pilot, preparation and that result are in docs/benchmarks.md.
- **Done 2026-11-20.** The `large` task set exists (8 tasks from prediction_market_workspace, 59–192 KB of tool output per plain run), passed validation and is frozen (manifest `7185d3845e8aeb68`); see "Large-output tasks" in docs/benchmarks.md.
- **Next.** The paid run, which needs the owner's go-ahead: `run --task-set large --campaign large --model claude-sonnet-5-5 --arms a,b,c,d --reps 3 --ce-trigger 30000 --token-limit 3000000 --budget-usd 35` (96 runs, about $19 expected), then `report --task-set large --campaign large`, and record the result in docs/benchmarks.md and the README.

### Read shapes that still reach the agent whole
- **Why.** Reads and `grep` of files outside the workspace, and the variables a line sets, landed on 2026-11-20: in 30 days of this project's Claude Code transcripts, 332 more results (3.4 MB; MB = 10⁶ bytes here) now qualify for the post-run hook. `discover` still classifies 773 results over 4 KB (6.5 MB) as not supported.
- **Next, by size.** Read lists with a glob, such as `cat notes/*.md` (98 results, 0.8 MB). `for` loops over literal words that only read (part of 88 results, 0.7 MB). Reads piped into `grep`, `sort` or `wc` (53 results, 0.5 MB). `awk` range reads (53 results, 0.5 MB; closed in docs/decisions.md at 0.10 MB, so remeasure first). `ls`, `wc` and `pwd` in a folder outside the workspace (about 0.1 MB).
- **Owner's call.** Output of Python and heredoc scripts (206 results, 1.7 MB) gets only the floor by design; clipping it is the closed last row of docs/decisions.md.
- **Check.** These outside results share the `read batch` label in `stats`, so watch how often those runs are reopened in full now that scratch notes and logs get an excerpt.
