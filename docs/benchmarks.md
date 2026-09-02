# Benchmarks

## Three-turn repair

The repair benchmark starts with a failing test, offers source retrieval, applies a single proposed arithmetic edit, and reruns the tests. An independent test checks additional inputs. Each variant uses a fresh fixture. In API mode the model selects files to read or, with Justless, can request a task pack. Tool-call counts include file listing, individual source reads or a pack, the edit, and both test runs. The separate correctness check stays outside the model conversation.

```sh
cargo build --locked
python3 scripts/repair_benchmark.py
# Optional: three paid turns per variant using ANTHROPIC_API_KEY
python3 scripts/repair_benchmark.py --api
```

One live pair using Haiku 5.5 produced the following provider-reported totals across all three turns.

| Tools available | Fix verified | Input tokens | Output tokens | Tool calls | Tool output bytes |
| --- | --- | ---: | ---: | ---: | ---: |
| Plain | Yes | 22,363 | 70 | 5 | 19,047 |
| Justless | Yes | 2,845 | 88 | 7 | 2,365 |

The Justless run used 87.3% fewer input tokens. A rerun on 2026-11-18 with the same model reproduced it: 22,364 input tokens for the plain run and 2,851 with Justless, 87.3% fewer, and both fixes passed the independent check. The model chose three direct source reads in that run and one in the plain run. This pair measures the benefit of compact command output on this fixture. The scripted mode exercises the task-pack path. It uses a fixed edit and makes no model calls.

### Across current Claude models

On 2026-11-18 the same three-turn task ran three times per model with `python3 scripts/repair_benchmark.py --api --model MODEL`. Each model's two variants used identical settings: Haiku 5.5 with thinking disabled, Sonnet 5.5 with `between_tools`, and Opus 5.5 and Fable 5.2 with adaptive thinking at low effort, because those two cannot turn thinking off. Every run produced a fix that passed the independent check.

| Model | Plain input tokens (median) | Justless input tokens (median) | Fewer | Fixes verified |
| --- | ---: | ---: | ---: | ---: |
| Claude Haiku 5.5 | 22,367 | 2,847 | 87.3% | 6 of 6 |
| Claude Sonnet 5.5 | 22,762 | 2,840 | 87.5% | 6 of 6 |
| Claude Opus 5.5 | 22,762 | 2,852 | 87.5% | 6 of 6 |
| Claude Fable 5.2 | 22,762 | 2,846 | 87.5% | 6 of 6 |

Runs varied by under 0.5% within each model. The saving comes from what the tools return on this fixture, so it is nearly the same for every model; a multi-task benchmark with agent-chosen tool calls is in progress.

## Build checks and single-task replay

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

The original single-task benchmark on macOS arm64 with Rust 1.93.0 produced these medians over three local replays per variant. Cargo compilation was warmed before the timed commands.

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

## Progressive reads, edits and tool history

Build with `cargo build --release --locked` and run `python3 scripts/context_benchmark.py`. The local fixture has 451 Python functions. The runner browses the first outline page, filters for the function that needs fixing, expands it, submits one revision-bound replacement and checks the result with an independent Python assertion.

| Source operation | Bytes |
| --- | ---: |
| Whole source file | 20,991 |
| First outline page | 2,068 |
| Filtered JSON outline | 203 |
| Exact function | 60 |
| Edit reference plus replacement | 88 |

The same run generates an outbound request with six build results. Retiring the oldest three reduced the serialized request from 38,306 to 19,952 bytes. Restoring the references reproduced the original JSON values. These measurements count local UTF-8 bytes. Provider token counts and prompt-cache effects depend on the model and client.
