# Benchmarks

## Agent benchmark (four arms)

This benchmark measures whether Justless reduces the cost of real coding-agent work without reducing task success. A model works on 32 frozen tasks from the owner's repositories through a reimplementation of Claude Code 2.1.327's default macOS setup over the Messages API. Arms differ only in the Justless install and in Anthropic's server-side context editing. The harness lives in `scripts/agent_bench/`. Its tasks, snapshots, ground truth, hidden tests, transcripts and the Claude Code capture stay outside Git in `$AGENT_BENCH_HOME`.

No result has been recorded yet. The table at the end of this section is filled only from `report.py` output after the paid campaign.

### Arms

| Arm | Justless | Context editing |
| --- | --- | --- |
| a, plain | No | No |
| b, justless | `justless init --hooks --agent claude` before the baseline commit, and the pinned binary on `PATH` | No |
| c, plain + context editing | No | `clear_tool_uses_20250919` |
| d, justless + context editing | As in b | As in c |

Every request in every arm is otherwise identical: model, effort, `max_tokens` (Claude Code's value capped at 32,000), adaptive thinking with `block_binding.prefix_mismatch_behavior: "error"`, the same `anthropic-beta` header, Claude Code's system prompt, its Bash, Read, Edit and Write definitions, and the same first messages. Every run saves its first request. The dry run and the report compare the two arms of each block and flag any other difference. The context-editing trigger T is set by rule from plain-arm pilot data: the median per-run peak prompt, rounded up to 5,000 and clamped to 30,000-100,000, with `keep` 3 tool uses and `clear_at_least` 5,000 tokens. Anthropic describes context editing as a context-window tool rather than a savings lever, so the contrasts involving c and d claim only "server-side clearing at this configuration".

### Claude Code parity

`run.py cc-capture` drives the real Claude Code binary against `fake_api.py` on 127.0.0.1, with a dummy key, nonessential traffic disabled, an isolated `HOME` and a sandbox that denies every other outbound connection. Scripted sessions record what Claude Code sends: its system prompt, tool definitions, cache breakpoints, effort, the first-message layout, hook input fields, and the exact text of each tool result. `run.py parity-diff` replays the same scripts through the harness and compares every tool result, error flag, follow-up system note and hook input. The last capture verified 65 of 68 scripted steps byte for byte after normalizing paths, timings and run IDs. The other three are documented deviations.

The harness reproduces these Claude Code behaviors:

- Bash runs in zsh after Claude Code's shell snapshot, so `grep` and `find` are Claude Code's embedded ugrep and bfs. Streams are merged and trailing whitespace is removed. The working directory persists only after a successful command that stays inside the project.
- A successful command whose output exceeds 30,000 characters is saved to a file and replaced by a 2 KB preview. A failing command shows `Exit code N` and its output; above 10,000 characters it shows the first and last 5,000 characters of the first 30,000. Exit status 1 counts as success for grep, rg, find, diff, test and `git diff` when the last statement of the command starts with that program as written, without an assignment, wrapper or path.
- Read numbers lines, reads 2,000 lines by default, refuses unbounded reads over 256 KB, and pages a file over 25,000 tokens with a PARTIAL-view note. Claude Code counts those tokens with the `count_tokens` endpoint, and so does the harness in paid runs. `allow_large` reads the whole file.
- Edit works on files that were never read. Write is refused only after a PARTIAL view.
- Claude Code adds system notes after tool results: hook context, the PARTIAL-view notice, a working-directory update, a note when a read file changed on disk, and a progress reminder after 5, 10 and 15 tool calls without text. The harness adds the same notes.
- Hooks run from the project's `.claude/settings.json` with Claude Code's input fields. A PreToolUse `updatedInput` replaces the executed command while the transcript keeps the original. A PostToolUse `updatedToolOutput` that does not match the tool's output schema is ignored. SessionStart context joins the environment note of the first request.

Deviations from Claude Code:

- The harness offers only Bash, Read, Edit and Write. Claude Code also offers Agent, Skill, ToolSearch, Workflow and scheduling tools; their text is removed from the captured prompt by one fixed patch whose hash is recorded.
- Requests are not streamed, use `anthropic-version` 2026-10-21 instead of 2023-06-01, and send only the betas the harness implements.
- A `Session: <nonce>` system block, unique per attempt, replaces Claude Code's billing header. Runs never share a prompt cache, so every run pays for its own prefix in every arm.
- Background Bash is rejected in every arm. Claude Code runs it.
- The project CLAUDE.md is included as interactive Claude Code includes it. `claude -p` under an isolated `HOME` did not include it in the offline capture, so the anchor runs record whether it reached the API.
- The PARTIAL-view page size and the progress-reminder trigger are fitted to captured examples and marked approximate in `data/cc_parity.json`.

On macOS and Linux Claude Code has no Grep or Glob tool, so Justless's after-search hook cannot fire in this benchmark. `justless mask` and `recall` are not exercised: rewriting earlier tool results would break the append-only history that preserved thinking requires.

### Tasks and oracles

The 32 tasks come from five of the owner's repositories, eight per family. Generation is seeded (seed 20261118) and never runs Justless. Candidates are sorted and shuffled, and each task records its candidate pool and index. The repository fixtures are used only for dry runs, unit tests and the parity capture.

| Family | Task | Success |
| --- | --- | --- |
| Bug fix | A deterministic operator, constant or argument mutation that makes `cargo test` fail; one Python task with harness-held tests | Every test name that passed at baseline passes, no test or test configuration was edited, and the diff touches the mutated function. Added library code must not switch on `cfg(test)` or `debug_assertions`, so a fix that only exists under test fails; a new `#[cfg(test)] mod` is allowed. Python tasks also pass hidden tests. |
| Lint | Three to five behavior-preserving rewrites in a clippy-clean package that trigger default clippy lints | Clippy reports nothing, no `allow` or `expect` attribute or lint configuration was added, and the baseline tests pass. Comments and strings that mention a lint are not suppressions. |
| Location | A literal from a log, error, panic, SQL migration or Neovim mapping, quoted verbatim in half the questions and paraphrased in the other half | The first answer names the right file and enclosing symbol. Forms that tools print are accepted: an absolute path, a `:line` suffix, module-qualified names such as `crate_a::mod_b::name` or `pkg.module.name`, and a `file.ext::` prefix. A different owning type is still wrong. |
| Impact | A uniquely defined public symbol referenced from 3 to 25 files | Recall of 0.9 or more and precision of 0.8 or more. Files that mention the symbol only in comments, strings or docs are neutral. |

A bug-fix candidate is accepted only if its failure repeats identically, its tests are not flaky over three baseline runs, and a second mutation in the same function also fails them. The repository mix is Rust-heavy: 25 tasks are Rust, 5 Python, 1 a SQL migration and 1 Lua. The planned Lua impact task was replaced by a Rust one because no Lua symbol is referenced from 3 to 25 files. In a Cargo workspace the lint prompt and oracle name the package, because other members carry their own warnings. Results apply to tasks like these on these repositories.

Every impact task can be solved with grep by construction. Its truth is the `rg -w` hits for a uniquely named symbol, less files that mention it only in comments or strings. The impact family therefore measures cost on lexical cases, where semantic navigation cannot change success. Its results apply to uniquely named symbols. Location tasks record whether grep alone solves them, and the report splits them on it.

Each run clones a warm Cargo target built for its task, then deletes the fingerprints of the repository's own crates so they are rebuilt in the run directory. Test binaries embed their crate's absolute path, and without the rebuild they read fixtures from the snapshot.

### Run isolation

Each run clones the task snapshot and its warm Cargo target with APFS clones into a fresh directory, installs the arm, sets every file's time to 2026-01-01, and makes one fixed-date Git commit, so neither `ls -lt`, `find -newer`, Git history nor Justless's change views reveal the mutation. Tools, hooks and oracles run under `sandbox-exec`. The profile denies reading the task store, the corpus, other runs and the user's home outside the Rust toolchain, denies executing any unpinned `justless`, `claude` or `codex`, and denies outbound network other than 127.0.0.1. The tool environment is an allowlist with no `ANTHROPIC_*` variable. The run directory is deleted after grading.

The corpus repositories depend on one another through Git, so Cargo's git cache holds unmutated copies of task files. Each run hides every checkout and database of the task's own repository, and every checkout revision or database its `Cargo.lock` does not use. A repository never depends on itself, and the harness refuses a task whose lock says otherwise. A database that the build needs stays readable. Generation skips files whose original is stored in such a database, and `validate-tasks` lists any task whose original is still reachable under `answer_key_exposure`. One lint task in the big workspace was generated before that check and is listed. Running `prepare --generate --families lint --repos <that repository>` regenerates it, and possibly that repository's other lint tasks. They must then be validated again before the manifest is frozen. Tool calls that name the Cargo git cache are counted as possible answer-key access.

Writes to the Cargo home are limited to Cargo's lock and usage-tracking files, so a run cannot install binaries or change configuration for later runs. Each record notes any change to `~/.cargo/bin` or its configuration. The binary under test is copied into the store under its SHA-256 at the start of a campaign. Every run installs that copy and checks its hash, so a rebuild at the original path cannot change later runs.

### Pre-registered analysis

The unit of analysis is a paired (model, task, repetition) block, with a seeded arm order inside each block. The primary contrast is b against a on Sonnet 5.5 over all 32 tasks, with two endpoints:

- E1, the success difference: Δs is the mean over blocks of success(b) minus success(a).
- E2, the relative saving in cost per successful task: S = 1 − [(Σ cost b / Σ successes b) / (Σ cost a / Σ successes a)].

Confidence intervals come from a task-cluster bootstrap with 10,000 resamples and seed 20261118. If more than 1% of resamples have no success in an arm, S is reported as undefined. The rule then uses R = 1 − (Σ cost b / Σ cost a), the relative saving in cost per run, with its own bootstrap interval, and the report says so. The decision rule applies in this order, and the first match wins. "S" below means R when S is undefined.

1. WORSE: the upper bound of S is below 0, or the upper bound of Δs is below −0.10.
2. WIN: the lower bound of S is above 0, the lower bound of Δs is above −0.10, and the point estimate of Δs is at least −0.05. The result is worded "Justless reduced cost per successful task by S [CI] while success was not worse by more than 10 percentage points (Δs [CI])".
3. CHEAPER-BUT-SUCCESS-UNCERTAIN: the lower bound of S is above 0, but the success conditions of rule 2 fail. The result is worded "Justless reduced cost per successful task by S [CI]. Success was not shown to be within 10 percentage points (Δs [CI]); at this sample size that is not evidence that success fell."
4. NO DEMONSTRATED SAVING: everything else.

The success endpoint is underpowered for its 10-point margin at the planned size. For the primary contrast, 32 tasks × 3 repetitions is 96 blocks. At a planning ICC of 0.3 the success MDE is 16 points with 20% discordant blocks and 11 points with 10%. In a simulation of a tool with no effect on success, with task success rates spread from 0.2 to 0.95, the success conditions of WIN held in 38% of campaigns of this size. The median Δs half-width was 0.104, wider than the margin. With mostly deterministic tasks they held in 80%. A verdict other than WIN can therefore reflect sample size rather than lower success. The report prints the observed Δs half-width next to the 10-point margin, the MDE at the measured discordance and ICC, and the share of simulated no-effect campaigns of the observed size that meet the success conditions. After the pilot, `report --campaign pilot` gives the measured discordance and ICC. If they imply an MDE above 10 points, the lead either moves budget into the primary contrast, for example a fourth or fifth Sonnet repetition funded by fewer Haiku repetitions, or keeps the plan and the rule-3 wording above.

Key secondaries are the edit families (bug fix and lint) and each family on its own; a negative family estimate must be stated with the headline. The context-editing contrasts (d against c, c against a, d against a) are tested only on Haiku 5.5, with Holm-adjusted sign-flip p-values. Opus 5.5 is descriptive.

Every run is graded on its final repository and answer, however it stopped. A run that hits the turn, token, run-budget or wall-time limit, or stops on `max_tokens` or a refusal, records why in `stopped_by`. Its success is the oracle's verdict, so a fix already on disk counts. The token limit is in the unit Justless reduces, so it must rarely bind. The pilot sets it as at least three times the plain arm's 95th-percentile cumulative prompt tokens per run, and never below 3,000,000; `estimate` prints the value. The wall-time limit is a two-hour hang guard that excludes API retry back-off. The report gives each arm's limit-hit rate and the censored contrast with bootstrap intervals.

Sensitivity analyses treat limit hits as censored, reprice cache writes at the 1-hour rate, drop the Python task, and split location tasks by style and by grep-solvability. Harness errors are retried up to three times with a new nonce and then count as failures. Cap aborts and infrastructure pauses do not use up those retries. The campaign pauses when any arm's unrecoverable rate exceeds 2%, or when arms differ by more than 2 points in that rate or in their limit-hit rate. The rule is checked once every arm has 20 graded runs. `run` stops starting new blocks, exits with status 3, and continues after review with `--ignore-gate`. `count_tokens` is retried like any request, and if it still fails the campaign pauses instead of re-rolling the run.

Arms pair only within one set of pins: binary hash, harness tree hash, capture hash, task manifest, pricing and effort. A campaign records its pins in `pins.json` on first start and refuses to resume with different ones. These are the binary, harness, capture, manifest, pricing, limits and seed, and per model the effort, `max_tokens`, run budget and the context-editing trigger. `--new-pins` starts a new generation, and the report never pairs runs across generations. A contrast with more than one pin set is reported as MIXED PINS, and every pin set is listed. The harness hash covers the files under `scripts/agent_bench` except the tests, so commits elsewhere in the repository do not re-key finished runs.

A small anchor run uses the real `claude -p` on 8 tasks in arms a and b through `proxy.py`, which logs usage and swaps a dummy key for the campaign key so the key never enters the sandbox. The fidelity flag is raised if the anchor's b/a cost ratio falls outside the harness's 95% interval for the same tasks, if a hook firing rate differs by more than 2×, or if plain-arm cost per run differs by more than 35%. A raised flag must appear next to the primary result.

### Budget

Prices come from the pricing page as checked on 2026-11-18 and live in `data/pricing.json`. Haiku 5.5 changes tier per request at 100,000 prompt tokens, counting cache reads and writes. Every request first checks that paid spend in all campaigns, plus a worst-case estimate of the request, stays under $80 ($90 with `--use-reserve`). The worst case prices the whole prompt at the dearest cache-write rate. The check reserves that amount until the response is recorded, so parallel workers cannot all pass the same last dollar. It also re-reads the ledger under a file lock, so it sees what other processes in the campaign spent. Dry runs mark every ledger entry as fake, and fake usage never counts as paid spend. Per-run caps are $0.30 for Haiku, $3 for Sonnet and $6 for Opus, with 60 requests, the pilot-set token limit and the two-hour hang guard. The anchor's proxy applies the same campaign check before forwarding each Claude Code request. It records each cost when the response arrives and refuses further requests once the campaign cap or the run's budget is reached.

| Phase | Runs | Expected cost |
| --- | ---: | ---: |
| Pilot: Haiku, 8 tasks × 4 arms; Sonnet, 4 tasks × arms a and b | 40 | about $2.80, capped at $3 |
| Sonnet: 32 tasks × 3 repetitions × arms a and b | 192 | about $47 |
| Haiku: 32 tasks × 4 repetitions × 4 arms | 512 | about $10 |
| Anchor: Sonnet, 8 tasks × arms a and b in real Claude Code | 16 | about $6 |
| Opus: 4 tasks × 3 repetitions × arms a and b | 24 | about $11 |

`run.py estimate` compares plans with the same $80 limit that the ledger enforces, or $90 with `--use-reserve`. After the pilot, it replaces each assumption with the pilot cost and applies the pre-registered scale order: drop Opus, then cut Haiku to 3 repetitions, the anchor to 4 tasks, and Sonnet to 24 tasks. Below $70 it scales up in the order Opus to 8 tasks, a fourth Sonnet repetition, then Sonnet context-editing arms on the edit families. If the 90th percentile at Claude Code's default effort still exceeds the budget, every arm of that model switches to `medium` effort.

### How to run

Set `AGENT_BENCH_HOME` and `AGENT_BENCH_RUNROOT` to durable private directories first; the defaults live in a scratch directory that can be wiped. The Python bug-fix target is described in `$AGENT_BENCH_HOME/private/python_task.json`, outside the repository because it names private code. Paid commands refuse to start until every secret-scan hit in the snapshots is reviewed: `prepare` lists them, and reviewed `path:line:kind` entries go in `$AGENT_BENCH_HOME/secret_allowlist.json`.

Build the binary under test from the commit being measured, then run these from the repository root. Every step before `pilot` is free.

```sh
cargo build --release --locked
B=target/release/justless
python3 -m unittest discover -s scripts/agent_bench/tests -t scripts/agent_bench/tests
python3 scripts/agent_bench/run.py sandbox-check
python3 scripts/agent_bench/run.py cc-capture --binary $B
python3 scripts/agent_bench/run.py parity-diff --binary $B
python3 scripts/agent_bench/run.py prepare --generate
python3 scripts/agent_bench/run.py validate-tasks --tasks all --binary $B
python3 scripts/agent_bench/run.py review-tasks --approve
python3 scripts/agent_bench/run.py dry-run --tasks all-public --arms a,b,c,d --binary $B
python3 scripts/agent_bench/run.py anchor --dry-anchor --tasks all-public --binary $B
python3 scripts/agent_bench/run.py estimate --assume prepilot
```

Dry runs, validation and the dry anchor default to the campaigns `dry`, `validate` and `anchor-dry`. Paid commands refuse any campaign directory that holds dry-run data.

The paid steps need `ANTHROPIC_API_KEY` for a dedicated key and workspace:

```sh
python3 scripts/agent_bench/run.py pilot --campaign pilot --model claude-haiku-5-5 --arms a,b,c,d --per-family 2 --budget-usd 3 --binary $B
python3 scripts/agent_bench/run.py pilot --campaign pilot --model claude-sonnet-5-5 --arms a,b --per-family 1 --budget-usd 3 --binary $B
python3 scripts/agent_bench/run.py reconcile --campaign pilot --provider-usd <amount from the cost report>
python3 scripts/agent_bench/run.py estimate --campaign pilot
python3 scripts/agent_bench/run.py run --campaign main --model claude-sonnet-5-5 --arms a,b --reps 3 --token-limit <L sonnet> --binary $B
python3 scripts/agent_bench/run.py run --campaign main --model claude-haiku-5-5 --arms a,b,c,d --reps 4 --ce-trigger <T> --token-limit <L haiku> --binary $B
python3 scripts/agent_bench/run.py anchor --campaign main --model claude-sonnet-5-5 --per-family 2 --binary $B
python3 scripts/agent_bench/run.py run --campaign main --model claude-opus-5-5 --arms a,b --reps 3 --per-family 1 --token-limit <L opus> --binary $B
python3 scripts/agent_bench/run.py report --campaign main
python3 scripts/agent_bench/run.py export-audit --campaign main
```

`estimate --campaign pilot` prints T, each model's token limit L and the effort rule. Opus has no pilot runs, so its limit is the Sonnet value. The main campaign and its report run only on the task manifest frozen by `review-tasks --approve`.

Runs are resumable. A run with a terminal record is skipped, and the report is written to `$AGENT_BENCH_HOME/reports/<campaign>/`. A resume with different pins is refused, as described above. `run` exits with status 3 when the campaign pauses. `export-audit` writes every successful bug-fix or lint diff that is not an exact revert of the mutation to `reports/<campaign>/audit/` for a blind review. The patches name the task but not the arm, and `audit_key.json` maps them back to runs.

The free steps check the harness before any money is spent. `validate-tasks` runs every task through the exact run-time path with scripted agents. Doing nothing must fail, and the reference fix must succeed in both arms. Answers in the qualified forms that tools print must succeed. Tampering with tests, suppressing lints, a `#[cfg(test)]` twin of the buggy function or a wrong answer must fail with the expected failure mode. `dry-run` also runs an agent that finishes the task and then keeps going until the turn limit stops it, which must still grade as success. It adds a probe agent and then checks the logs: paired first requests differ only in the CLAUDE.md block and the SessionStart context, plain arms run no hook and cannot run `justless`, the planted parent-environment key appears nowhere, `ls -lt` and `find -newer` show no edited file, and the ledger matches costs recomputed from the logged usage. `anchor --dry-anchor` runs real Claude Code through the proxy against the fake API.

### Results

| Model | Contrast | Success | Cost per successful task | Δs [95% CI] | S [95% CI] | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| Sonnet 5.5 | b vs a | not run | not run | not run | not run | not run |

## Earlier single-task illustrations

The results below come from single pairs on a vendor-written fixture. They illustrate the mechanism and are not evidence of task-level savings.

### Three-turn repair (n=1)

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

### Across current Claude models (one task, 3 runs per model)

On 2026-11-18 the same three-turn task ran three times per model with `python3 scripts/repair_benchmark.py --api --model MODEL`. Each model's two variants used identical settings: Haiku 5.5 with thinking disabled, Sonnet 5.5 with `between_tools`, and Opus 5.5 and Fable 5.2 with adaptive thinking at low effort, because those two cannot turn thinking off. Every run produced a fix that passed the independent check.

| Model | Plain input tokens (median) | Justless input tokens (median) | Fewer | Fixes verified |
| --- | ---: | ---: | ---: | ---: |
| Claude Haiku 5.5 | 22,367 | 2,847 | 87.3% | 6 of 6 |
| Claude Sonnet 5.5 | 22,762 | 2,840 | 87.5% | 6 of 6 |
| Claude Opus 5.5 | 22,762 | 2,852 | 87.5% | 6 of 6 |
| Claude Fable 5.2 | 22,762 | 2,846 | 87.5% | 6 of 6 |

Runs varied by under 0.5% within each model. The saving comes from what the tools return on this fixture, so it is nearly the same for every model; a multi-task benchmark with agent-chosen tool calls is in progress.

### Build checks and single-task replay

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

`scripts/replay.py` replays stored Claude Code Bash results through `justless hook-claude after-bash` and reports measured bytes. On the local transcripts it replayed 1,867 results, 164 eligible, 26 compacted, and saved 278,084 of 314,460 bytes (88.4%). The 1,593 results under 4 KiB, 138 pass-through results and 86 results whose directory no longer existed were left alone.

The original single-task benchmark on macOS arm64 with Rust 1.93.0 produced these medians over three local replays per variant. Cargo compilation was warmed before the timed commands.

| Local replay | Successful runs | Tool output | Estimated output tokens | Elapsed |
| --- | ---: | ---: | ---: | ---: |
| Direct Cargo and source reads | 3/3 | 18,531 bytes | 4,634 | 0.926 s |
| Justless and targeted retrieval | 3/3 | 1,889 bytes | 475 | 0.964 s |

Justless delivered 89.8% fewer bytes in this local replay.

On a separate synthetic corpus of 950 source and text files, a release build indexed from scratch in 0.795 s and refreshed the unchanged index in 0.191 s. A query after one TypeScript edit returned the new definition in 0.211 s.

### Claude API replay (n=1)

With `ANTHROPIC_API_KEY` set in the environment, run `python3 scripts/benchmark.py --mode claude-api`. Anthropic receives only the synthetic fixture’s pre-collected evidence. The benchmark makes two paid requests with a 160-token output cap and reasoning disabled. It checks the returned JSON against the fixture’s expected answer. [Claude Messages API](https://platform.claude.com/docs/en/api/http/messages)

One calibrated pair with Haiku 5.5 returned these provider-reported counts.

| Evidence | Correct task answer | Input tokens | Output tokens | Total tokens | API elapsed |
| --- | --- | ---: | ---: | ---: | ---: |
| Direct Cargo and source reads | No | 7,499 | 101 | 7,600 | 1.42 s |
| Justless and targeted retrieval | Yes | 1,124 | 98 | 1,222 | 1.17 s |

The Justless replay used 85.0% fewer input tokens. The direct replay identified the test function where the answer required its call target. An earlier pair with default reasoning used the full output cap without returning an answer. The API replay measures one fixture with supplied evidence. Live agent behavior and native Codex hook loading require a separate session.

### Progressive reads, edits and tool history

Build with `cargo build --release --locked` and run `python3 scripts/context_benchmark.py`. The local fixture has 451 Python functions. The runner browses the first outline page, filters for the function that needs fixing, expands it, submits one revision-bound replacement and checks the result with an independent Python assertion.

| Source operation | Bytes |
| --- | ---: |
| Whole source file | 20,991 |
| First outline page | 2,068 |
| Filtered JSON outline | 203 |
| Exact function | 60 |
| Edit reference plus replacement | 88 |

The same run generates an outbound request with six build results. Retiring the oldest three reduced the serialized request from 38,306 to 19,952 bytes. Restoring the references reproduced the original JSON values. These measurements count local UTF-8 bytes. Provider token counts and prompt-cache effects depend on the model and client.

## Script runner

```sh
cargo build --release --locked
python3 scripts/exec_benchmark.py --binary ./target/release/justless
```

The script generates seeded data locally, runs a short analysis script over each file through `justless exec`, and compares what the agent receives with the input, with `justless run -- cat FILE`, and with the same script run directly. It makes no network or model calls.

| Task | Input | `justless run -- cat` | Script run directly | `justless exec` |
| --- | ---: | ---: | ---: | ---: |
| List 2,176 slow requests, 61 of them payment timeouts | 19,051,254 bytes | 2,121 bytes, no answer | 119,694 bytes | 4,100 bytes, 2 of the 61; with `--intent "payments timeouts"`, 4,044 bytes, 56 of the 61 |
| Summarize an 8,000-issue JSON export | 5,953,169 bytes | 2,143 bytes, no answer | 257 bytes | 257 bytes |
| Count log errors by service and code | 19,051,254 bytes | 2,121 bytes, no answer | 310 bytes | 310 bytes |
| Revenue and refunds by region from a CSV | 4,395,204 bytes | 2,149 bytes, no answer | 186 bytes | 186 bytes |

Each column is everything the agent received, footer included. When a script's output is large, `exec` bounds it: the first row's listing would put 119,694 bytes in context, and `exec` shows whole lines from both ends with the range left out, or with `--intent` the matching lines. When the output fits, `exec` prints exactly what the script prints and adds no footer to a success, so the last three rows match the direct run; there `exec` adds the archive under a run ID and the time limit, and the bytes saved come from writing a script instead of reading the input. `run -- cat` stays bounded, but its head and tail do not contain the answer.

Measured on macOS arm64. Spacing from `uniq -c` differs on Linux, so the log errors row can change by a few bytes.

## Secret redaction

Build with `cargo build --release --locked` and run `python3 scripts/redact_benchmark.py --binary target/release/justless --cpu`. Each 200 MiB input is captured with `run -- cat` with redaction off and on, then replayed with `log` and `log --unredacted`. `--cpu` reports the CPU time Justless used, user plus system, as the fastest of three repeats. Elapsed time also counts waiting for other jobs, so it varies more on a busy machine.

| Input (200 MiB) | Secrets found | `run` without | `run` with | `log` | `log --unredacted` |
| --- | ---: | ---: | ---: | ---: | ---: |
| This repository's history, source, docs and lockfiles, with 24 secrets added | 174 | 3.66 s | 4.36 s | 0.64 s | 0.06 s |
| Every line holding a keyword, no secrets | 0 | 1.73 s | 3.54 s | 0.06 s | 0.06 s |
| One line of base64, no secrets | 0 | 0.56 s | 0.92 s | 0.05 s | 0.05 s |
| One line of base64 holding one secret | 1 | 0.49 s | 0.90 s | 0.42 s | 0.05 s |

The history holds test fixtures that are redacted too, such as passwords in example URLs, so the first input finds more than the 24 secrets added and `--check` reports the difference. These are release-build figures on Apple Silicon while other jobs kept the load average between 60 and 80. Redaction adds about 159 KB (0.3%) to the release binary.

## Affected tests

`scripts/affected_benchmark.py` copies a Cargo project to a disposable directory, commits it, and measures `justless test --affected --fresh -- cargo test --offline` against `justless test --fresh -- cargo test --offline` on the same edit, each from a build warmed at the baseline. `--fresh` keeps either output from comparing itself with the previous mutant's run. Without `--edits` it makes one mutant per non-test function by inserting `panic!` at the start of its body; `--benign` inserts a no-op statement instead, so every suite passes; `--edits FILE` measures named edits. A missed failure is a mutant for which the full suite fails and the subset passes, and the script exits nonzero if there is one. Times are wall-clock seconds including Justless's planning and Cargo's incremental builds; bytes are everything printed on stdout and stderr. Nothing is fetched.

```sh
cargo build --release --locked
python3 scripts/affected_benchmark.py --binary ./target/release/justless --no-fallback
python3 scripts/affected_benchmark.py --binary ./target/release/justless --benign
python3 scripts/affected_benchmark.py --binary ./target/release/justless --project PATH --limit 12
python3 scripts/affected_benchmark.py --binary ./target/release/justless --project . --edits scripts/affected_edits.json --no-fallback
```

Measured on 2026-11-18 on macOS arm64 with Rust 1.93.0 and a release build, after the selection rules for helpers behind `Display`, crate-level and included doc examples, example binaries and unmentioned SQL were added. Single timings vary by about a second. Medians are over the mutants or edits in each row.

| Project and edits | Missed failures | Ran as a subset | Selected tests (median) | Subset time | Full-suite time | Bytes, affected | Bytes, full suite |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `fixtures/affected`, a panic in each of 13 functions | 0 of 12 | 10 of 13 | 10% of 20 | 1.42 s | 0.95 s | 3,409 | 2,222 |
| `fixtures/affected`, a no-op in each of 13 functions | none failed | 10 of 13 | 10% of 20 | 1.38 s | 2.09 s | 1,475 | 2,145 |
| This repository, 4 edits: a `fixtures/failing` fee, a `fixtures/mixed` table name, one test in `tests/progressive.rs`, one `src/` function | 0 of 2 | 2 of 4 | 16% of 143 | 2.27 s | 10.77 s | 1,509 | 1,415 |
| A single-crate Rust library from a private codebase, with 87 unit tests and crates.io dependencies, a panic in 12 sampled functions | 0 of 3 | 2 of 12 | 7% of 87 | 1.48 s | 2.63 s | 1,792 | 1,576 |
| The same library, a no-op in the same 12 functions | none failed | 2 of 12 | 7% of 87 | 1.36 s | 3.24 s | 1,213 | 991 |

The time columns are for the edits that ran as a subset, compared with the full suite on the same edits; the byte columns are medians over every edit in the row, fallbacks included. A fallback costs a full suite plus the planning, which took 0.1 s on `fixtures/affected` and 0.2 s on this repository with a warm index; the fallback rows differ from their full suites by less than the timing noise.

What the rows show:

- **No missed failures.** Every edit that failed the full suite failed the subset, with and without `--no-fallback`. The edits a review found to pass a subset while `cargo test` failed, a helper called inside `write!` in a `Display` impl, a crate-level doc example, a README included as crate docs, a renamed function an example calls and an SQL migration a test reads, are now covered by `tests/cli.rs`.
- **On a small workspace a failing subset is slower and longer.** `cargo test` stops at the first failing test binary, so the full suite of `fixtures/affected` ends after one binary, while the subset runs its selected tests, whole targets and doctests as separate Cargo invocations. A passing edit is where the subset saves time, by not building or running the packages the change does not reach.
- **The gain grows with the suite.** On this repository the subset took 1.6 s against 9.3 s for the SQL edit and 2.9 s against 12.3 s for the edited test, and printed 951 bytes against 1,423 for the passing one.
- **Fallbacks are honest, and frequent where links are inferred.** The `fixtures/failing` fee edit runs the full suite: `apply_fee` is reached only by tests whose body names it, a name match, while the exact links run through the `fixture()` helper that copies the directory. On the single-crate library, 10 of the 12 sampled functions fall back, because no test reaches them or only inferred links and name matches do. With `--no-fallback` the untested functions only build and the rest run a labeled low-confidence subset, in a median 1.02 s against 3.13 s for the full suite, and each of the 3 mutants the full suite caught still failed; that is a measurement on 12 mutants, not a guarantee.
- **A change to a binary's code on this repository runs everything.** Every integration test here runs the `justless` binary, so a `src/` edit reaches all of them and the subset is too broad.

`fixtures/failing` has one test, and `fixtures/mixed` has no test runner, so neither is a meaningful subset benchmark on its own; the `justless` rows above include edits to both, which reach the tests that copy them.

## Assignments and a final `2>&1`

`justless discover --days 365 --json` read this repository's own Claude Code transcripts, 7,935 commands with 19,769,363 bytes of output, before and after the hooks began routing leading `NAME=value` assignments and a final `2>&1`.

| Discover group | Before | After |
| --- | ---: | ---: |
| Supported but not routed | 255 commands, 927,392 bytes | 264 commands, 933,232 bytes |
| Supported tool in a form that passes through | 1,269 commands, 2,042,885 bytes | 1,263 commands, 2,038,905 bytes |

The new forms cover 9 more commands with 5,840 bytes of output. Pipes account for far more: in the same transcripts, 1,959 commands with 2,004,988 bytes end a command in `2>&1` followed by a pipe, such as `cargo test 2>&1 | tail -50`, and 1,948 commands with 3,047,325 bytes start with an assignment and contain a pipe. Those still pass through the command hook; in Claude Code, the post-command hook compacts their output after they run.

On a copy of `fixtures/failing`, `RUST_BACKTRACE=1 cargo test 2>&1` printed 19,255 bytes and exited with status 101. Its hook form, `RUST_BACKTRACE=1 justless run --hook --merge -- cargo test 2>&1`, printed 2,536 bytes with the same status: the head of the output, the failure lines including Cargo's `error: test failed`, the tail and a footer naming the run. Without `--merge`, the test summary and the stderr preview take 914 bytes, but a summary of a merged stream would drop errors the command wrote to stderr, so a merged run keeps its own preview. That costs most on a large passing run: a crate with 300 passing tests printed 8,918 bytes for `cargo test 2>&1`, 2,089 bytes through `--merge`, and 558 bytes as a summary with a separate stderr preview. The byte counts include absolute paths under the scratch directory, so they vary with the checkout location.

## Ranged Read outlines

Local Claude Code transcripts from 2026-11-07 to 2026-11-18 hold 1,234 Reads of workspace files with `offset` or `limit`, 8,482,665 bytes as the model received them, line numbers included. Each window was replayed through the post-read hook only when its file could be recovered exactly: as it is on disk now, or for this repository from any blob version Git still holds, with the window's lines equal at the same position. 217 of the 397 windows over 150 lines could be. The other 180, with 2,601,016 bytes, count unchanged in both columns. Bytes after count the outline as Claude Code numbers it plus the hook's hint.

| Window lines | Reads | Before | After | Replaced |
| --- | ---: | ---: | ---: | ---: |
| at most 150 | 837 | 2,404,780 | 2,404,780 | 0 |
| 151-200 | 97 | 791,883 | 600,221 | 32 |
| 201-300 | 113 | 1,266,381 | 946,085 | 35 |
| 301-500 | 119 | 2,077,388 | 1,132,324 | 61 |
| over 500 | 68 | 1,942,233 | 900,080 | 39 |

The 167 replaced windows went from 2,803,765 to 304,590 bytes, a median of 12% of their size. Of the 50 recovered wide windows that passed through, 45 sat mostly inside one definition and 5 had more rows than the context budget holds. Thresholds of 100, 200 and 300 lines would have replaced 207, 135 and 100 windows and saved 2,676,278, 2,307,513 and 1,987,217 bytes, against 2,499,175 at 150. These figures do not model what the agent read next, such as a narrower Read of the part it needed.

Against Claude Code 2.1.327 and the harness's local fake API, a Read of lines 100-399 of a generated 482-line Rust file reached the model as the outline and the hint. The same Read again returned the outline again, a 150-line Read returned the native lines, and an Edit inside the outlined window succeeded, as it did in the same script without hooks. After an outlined 300-line window and an outlined 400-line window of 35,231 bytes, the same Read with `allow_large: true` returned exactly what Read returned without hooks. `justless expand` printed the 300-line window exactly, but Claude Code returned the larger window's 32.9 KB as a saved file and a preview, so the hint names the Read.
