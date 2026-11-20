# TODO

Open work only. Finished work is in [CHANGELOG.md](CHANGELOG.md) and the Git history; ideas that were measured and closed, and what not to build, are in [docs/decisions.md](docs/decisions.md).

## Next

### Large-output benchmark
- **Why.** The main campaign (Sonnet, 248 runs, $10.46) found no demonstrated saving on the existing 31 small tasks (−1.5% [−15.4%, +7.6%], success 98.4% both); they read about 5.6 KB of tool output per run. Pilot, preparation and that result are in docs/benchmarks.md.
- **Done 2026-11-20.** The `large` task set exists (8 tasks from prediction_market_workspace, 59–192 KB of tool output per plain run), passed validation and is frozen (manifest `7185d3845e8aeb68`); see "Large-output tasks" in docs/benchmarks.md.
- **Next.** The paid run, which needs the owner's go-ahead: `run --task-set large --campaign large --model claude-sonnet-5-5 --arms a,b,c,d --reps 3 --ce-trigger 30000 --token-limit 3000000 --budget-usd 35` (96 runs, about $19 expected), then `report --task-set large --campaign large`, and record the result in docs/benchmarks.md and the README.

### Read shapes the hook does not verify
- **Why.** Reads and `grep` of files outside the workspace, the variables a line sets, globs and awk line ranges are verified since 2026-11-20, and output of scripts, loops and unknown tools is clipped over the preview budget. Since reads of at most 600 lines keep their text in Claude Code, these matter only above that size or for results Claude Code saved to a file. Counting results that merely qualify for a hook overstated their value: on 30 days of this project's transcripts the post-run hook would compact 1,134 results (8.5 MB) with the threshold, against 1,973 (17.4 MB) without it.
- **Next, by size.** Reads piped into `grep` or `sed`, other `awk` programs, and `ls`, `wc` and `pwd` in a folder outside the workspace, each under 0.5 MB in 30 days before the threshold. Loops are not worth unrolling: 1 of 82 loop results would verify as a read list. Measure above 600 lines before building any of them.

### Read thresholds after more sessions
- **Why.** Reads and shell reads of at most 600 lines keep their text since 2026-11-20 (`read_outline_lines`), after `scripts/read_followups.py` showed that smaller outlines were read again two to four times in five for no saving in bytes. The samples were 94 outlined Reads and 100 compacted shell reads from one project, mostly sessions that develop Justless.
- **Next.** Rerun the script once sessions with the new threshold accumulate, here and in prediction_market_workspace, and check the 601 to 1,200 line bucket, which had 16 Reads and 3 shell reads. `stats` counts a `justless log` replay as a reopening, but not a later Read or a narrower `sed` of the same file, so its net figure overstates what a compacted read saved; consider counting those.
