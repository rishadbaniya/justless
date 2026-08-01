# Benchmarks

Run these commands from the root of the Justless checkout.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 scripts/benchmark.py --mode local --iterations 3
python3 scripts/benchmark.py --mode codex
```

The CLI tests cover Rust and Python diagnostics, archive replay, multi-language source links, exact text search, source edits and deletion, Git changes, and Codex hook installation and command rewriting. The local benchmark replays the diagnosis on disposable copies of the synthetic fixture. It measures bytes and elapsed time and estimates tool output tokens as bytes divided by four, rounded up per result. The Codex benchmark runs the same task in two fresh sessions, one using Cargo and direct source reads and one using Justless. It records token usage from completed turns.

On macOS arm64 with Rust 1.93.0, three local replays per variant produced these medians. Cargo compilation was warmed before the timed commands.

| Local replay | Successful runs | Tool output | Estimated output tokens | Elapsed |
| --- | ---: | ---: | ---: | ---: |
| Direct Cargo and source reads | 3/3 | 18,531 bytes | 4,634 | 0.926 s |
| Justless and targeted retrieval | 3/3 | 1,889 bytes | 475 | 0.964 s |

Justless delivered 89.8% fewer bytes in this local replay.

On a separate synthetic corpus of 950 source and text files, a release build indexed from scratch in 0.795 s and refreshed the unchanged index in 0.191 s. A query after one TypeScript edit returned the new definition in 0.211 s.

## Claude API replay

With `ANTHROPIC_API_KEY` set in the environment, run `python3 scripts/benchmark.py --mode claude-api`. Anthropic receives only the synthetic fixture’s pre-collected evidence. The benchmark makes two paid requests with a 160-token output cap and reasoning disabled. It checks the returned JSON against the fixture’s expected answer. [Claude Messages API](https://platform.claude.com/docs/en/api/http/messages)

One calibrated pair with Haiku 5.5 returned these provider-reported counts.

| Evidence | Correct task answer | Input tokens | Output tokens | Total tokens | API elapsed |
| --- | --- | ---: | ---: | ---: | ---: |
| Direct Cargo and source reads | No | 7,499 | 101 | 7,600 | 1.42 s |
| Justless and targeted retrieval | Yes | 1,124 | 98 | 1,222 | 1.17 s |

The Justless replay used 85.0% fewer input tokens. The direct replay identified the test function where the answer required its call target. An earlier pair with default reasoning used the full output cap without returning an answer. The API replay measures one fixture with supplied evidence. Live agent behavior and native Codex hook loading require a separate session.
