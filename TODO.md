# TODO

Open work only. Finished work is in [CHANGELOG.md](CHANGELOG.md) and the Git history; ideas that were measured and closed, and what not to build, are in [docs/decisions.md](docs/decisions.md).

## Next

### Large-output benchmark
- **Why.** The main campaign (Sonnet, 248 runs, $10.46) found no demonstrated saving on the existing 31 small tasks (−1.5% [−15.4%, +7.6%], success 98.4% both); they read about 5.6 KB of tool output per run. Pilot, preparation and that result are in docs/benchmarks.md.
- **Next.** Add a pre-registered task family that needs large tool output (long test logs, workspace lints, wide searches, long diffs), validate it for free, then run a Sonnet a/b cell after the owner's go-ahead.
