# Justless

Justless gives coding agents compact command output and a source-backed map of a repository. Search text across code, docs, and configuration, trace links across languages, and open the exact source or log lines behind a result.

## Install and use with Codex

Build with Rust 1.91 or newer and a C compiler. Git is needed for Git-aware file discovery and changed-line context.

```sh
cargo install --locked --path .
cd /path/to/your/repo
justless init --hooks
codex
```

`init` (also `init-codex`) installs repo instructions in `AGENTS.md` and ignores generated `.justless/` state in Git. `--hooks` also merges Justless hooks into `.codex/hooks.json`. Start Codex in the trusted project with `justless` on `PATH`, then review and trust the new hooks with `/hooks`. [Codex project instructions](https://learn.chatgpt.com/docs/agent-configuration/agents-md) · [Codex hooks](https://learn.chatgpt.com/docs/hooks)

The session hook warms the repo index in the background. The shell hook routes supported, simple Cargo, Go, npm, pytest, Git, and ripgrep commands through `justless run`. Shell pipelines, redirections, substitutions, watch modes, and known machine-output flags run unchanged. Codex evaluates rewritten commands through its normal sandbox and approval flow. The project instructions also work with the CLI directly.

Run `justless run -- cargo test`, `justless run -- git diff`, or `justless run -- rg -n pattern src`. For interactive commands, live streaming, or pipelines that consume raw machine-readable output, use `justless run --raw -- …` or call the command directly. For shell expressions, pass the shell explicitly, as in `justless run -- sh -c 'cargo test && git diff'`.

## Try a mixed-language repo

From this checkout, after building or installing Justless:

```sh
mixed_repo=$(mktemp -d)
cp -R fixtures/mixed/. "$mixed_repo/"
cd "$mixed_repo"
justless init --hooks
justless index
justless path fetchOrders purchase_orders
justless query frontend/orders.ts --view dependencies
justless find canary-checkout
```

The path follows the literal `/api/orders` from TypeScript to a Python route decorator, then follows `purchase_orders` from an embedded SQL statement to the schema. Each relationship includes its source line. The two cross-language relationships are labeled possible because they are inferred from source literals. The text lookup finds the runbook and an unrecognized config extension.

## Reproduce the failing test demo

From the Justless checkout, copy the fixture to a temporary directory and run the test through Justless. The intentional failure leaves the exit status at `101`.

```sh
cargo build --locked
export PATH="$(pwd)/target/debug:$PATH"
demo_root=$(mktemp -d)
cp -R fixtures/failing/. "$demo_root/"
cd "$demo_root"
justless init-codex
demo_status=0
result=$(justless run --max-bytes 512 -- cargo test --offline 2>&1) || demo_status=$?
printf '%s\n' "$result"
test "$demo_status" -eq 101

run_id=$(printf '%s\n' "$result" | sed -n 's/^\[justless \(run-[[:alnum:]]*\)\].*/\1/p')
justless context --run "$run_id" --budget 2400
justless context --symbol checkout_total
justless query apply_fee --view tests

match=$(justless search "$run_id" 'probe=' --stream stdout)
line=$(printf '%s\n' "$match" | cut -d: -f2)
justless log "$run_id" --stream stdout --from "$line" --to "$line"
```

The run links to `tests/checkout.rs:11`, where `total(100, 5)` returns `106` instead of the expected `105`. The call resolves through an import alias and a re-export to `src/pricing.rs:1-3`. The last command replays the exact middle line `billing-trace probe=only-in-the-full-log`.

## Commands and scope

| Command | Result |
| --- | --- |
| `justless run --max-bytes 4096 -- COMMAND ARG…` | Bounded previews for stdout and stderr, the child exit status, and the run ID |
| `justless search RUN_ID TEXT` | Literal matches with stream and line numbers |
| `justless log RUN_ID --stream stdout --from 10 --to 15` | Exact original bytes for the specified inclusive line range. Omit the range to replay the complete stream. |
| `justless query SYMBOL --view VIEW` | Definition, references, callers, or related tests. Use `all` for the combined view. |
| `justless query SYMBOL --view dependencies` | Outgoing calls, imports, and other indexed relationships |
| `justless path FROM TO` | A short relationship chain with the location and direction of each edge |
| `justless find TEXT` | Literal search across indexed text, with exact file and line matches |
| `justless context --changes` | Current changes against `HEAD`, with nearby symbols and tests where available. Includes untracked files. |
| `justless context --changes --staged` | Staged ranges for files that currently match the staged source |
| `justless context --at src/lib.rs:8` | Enclosing symbol, relevant calls, and source around a line or range |
| `justless code src/lib.rs --from 1 --to 8` | A numbered excerpt of original source |
| `justless stats` | Captured and previewed bytes across runs |

Queries refresh the index automatically. Justless compares file identity and high-resolution timestamps where available, hashes files that need closer inspection, reuses parsed facts for unchanged content, and removes deleted files. In a Git checkout it uses tracked and unignored files. Add patterns to `.justlessignore` for repo-specific exclusions.

Structured navigation supports Rust, JavaScript, TypeScript and TSX, Python, Go, Java, C, C++, C#, Ruby, PHP, Bash, and SQL. The available symbols and relationships vary by language. Calls across implicit imports, framework discovery, dynamic dispatch, and macro expansion can require source verification. Use `find` for any indexed UTF-8 text file. When source has changed since a recorded run, Justless preserves the original log location and flags the mismatch.

The optional `.justless.toml` controls project defaults. The values below are the built-in defaults.

```toml
context_budget = 2400
max_output_bytes = 4096
max_file_bytes = 2097152
```

`query`, `path`, `find`, `context`, and `code` accept `--budget`. The renderer uses one UTF-8 byte per budget unit as a conservative ceiling for byte-based tokenizers. Agent totals can also include the prompt, history, and tool metadata.

Full output is retained locally under `.justless/runs/`, including output short enough to display in full. The archive keeps the streams separately, with byte-for-byte replay for each. Keep the directory private if a command prints secrets. The CLI runs offline after installation, and `--raw` skips recording.

## Build checks and benchmark

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

### Claude API replay

With `ANTHROPIC_API_KEY` set in the environment, run `python3 scripts/benchmark.py --mode claude-api`. Anthropic receives only the synthetic fixture’s pre-collected evidence. The benchmark makes two paid requests with a 160-token output cap and reasoning disabled. It checks the returned JSON against the fixture’s expected answer. [Claude Messages API](https://platform.claude.com/docs/en/api/http/messages)

One calibrated pair with Haiku 5.5 returned these provider-reported counts.

| Evidence | Correct task answer | Input tokens | Output tokens | Total tokens | API elapsed |
| --- | --- | ---: | ---: | ---: | ---: |
| Direct Cargo and source reads | No | 7,499 | 101 | 7,600 | 1.42 s |
| Justless and targeted retrieval | Yes | 1,124 | 98 | 1,222 | 1.17 s |

The Justless replay used 85.0% fewer input tokens. The direct replay identified the test function where the answer required its call target. An earlier pair with default reasoning used the full output cap without returning an answer. The API replay measures one fixture with supplied evidence. Live agent behavior and native Codex hook loading require a separate session.
