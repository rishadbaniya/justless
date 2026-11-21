# TODO

Open work only. Finished work is in [CHANGELOG.md](CHANGELOG.md) and the Git history; ideas that were measured and closed, and what not to build, are in [docs/decisions.md](docs/decisions.md).

## Next

### Spend the hooks do not reach
- **Why.** On 2026-11-21 `scripts/context_spend.py` priced every request in the owner's Claude Code transcripts of two projects. Bash and Read results are 18% to 25% of the spend at list prices, and the hooks have kept out about 1% of the spend in this repository; clipping every other result to 4 KB would keep out 7.6% to 10.5% before any fetch. The advisor (28% here), the model's own writing (39% to 54%) and subagents (88% here) are larger. Results are in docs/benchmarks.md.
- **Next.** Rerun the script in prediction_market_workspace once the hooks have run there for a week; on 2026-11-21 they had run for a day and shortened 30 results. Decide whether `discover` should print this split, so a user sees what a client setting costs next to what the hooks keep out.

### A benchmark that reaches large output
- **Why.** Both agent campaigns found no demonstrated saving: the main one on small tasks (Sonnet, 248 runs, −1.5% [−15.4%, +7.6%]), and on 2026-11-20 the large-output family (Sonnet, 96 runs, $5.99, −11.5% [−46.0%, +15.1%], success 100% vs 95.8%). In the second the model filtered its own commands and a plain run saw a median of 9.1 KB of tool output where the scripted path saw 59 to 192 KB, so the hooks changed 34 of 343 results. Results are in docs/benchmarks.md.
- **Next.** Decide what would put large output in front of the hooks in a controlled run: long sessions that accumulate context, a model or effort level that reads more, or replaying the owner's own sessions, where Claude Code returns about 37 MB of shell output a week. A rerun of the same family on the current thresholds would likely show the same, since so little output reached Justless.

### Read shapes the hook does not verify
- **Why.** Reads and `grep` of files outside the workspace, the variables a line sets, globs and awk line ranges are verified since 2026-11-20, and output of scripts, loops and unknown tools is clipped over the preview budget. Since reads of at most 600 lines keep their text in Claude Code, these matter only above that size or for results Claude Code saved to a file. Counting results that merely qualify for a hook overstated their value: on this project's transcripts since 2026-11-17 the post-run hook would compact 1,134 results (8.5 MB) with the threshold, against 1,973 (17.4 MB) without it.
- **Next, by size.** Reads piped into `grep` or `sed`, other `awk` programs, and `ls`, `wc` and `pwd` in a folder outside the workspace, each under 0.5 MB in those transcripts before the threshold. Loops are not worth unrolling: 1 of 82 loop results would verify as a read list. Measure above 600 lines before building any of them.

### Read thresholds after more sessions
- **Why.** Reads and shell reads of at most 600 lines keep their text since 2026-11-20 (`read_outline_lines`), after `scripts/read_followups.py` showed that smaller outlines were read again two to four times in five for no saving in bytes. The samples were 94 outlined Reads and 100 compacted shell reads from one project, mostly sessions that develop Justless. On 2026-11-21 `scripts/context_spend.py` priced the same sessions: later reads of the file cost 249% of what the 59 outlines of at most 600 lines kept out, and 36% for the 19 larger ones.
- **Next.** Rerun the script once sessions with the new threshold accumulate, here and in prediction_market_workspace, and check the 601 to 1,200 line bucket, which had 16 Reads and 3 shell reads. `stats` counts a `justless log` replay as a reopening, but not a later Read or a narrower `sed` of the same file, so its net figure overstates what a compacted read saved; consider counting those.
