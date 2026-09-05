<p align="center">
  <img src="assets/logo.png" alt="Justless logo" width="140">
</p>

<h1 align="center">justless</h1>

<p align="center">
  <strong>Your agent reads less. It knows more.</strong><br>
  Compact by default. Exact on demand.
</p>

<p align="center">
  <a href="#install">Install</a> · <a href="docs/reference.md">Reference</a> · <a href="docs/examples.md">Examples</a> · <a href="docs/benchmarks.md">Benchmarks</a> · <a href="CHANGELOG.md">Changelog</a>
</p>

**justless** is a local Rust CLI that sits between your coding agent and its tools. It intercepts shell commands through agent hooks and parses their output into compact, structured results. It answers code questions from an incremental **code graph** of symbols, calls, imports, HTTP routes and SQL tables, so agents stop reading whole files. Every byte it cuts is archived locally and one command away.

> **🔒 100% local. Zero network calls.** justless runs entirely on your machine, between your agent and your shell. It makes no network requests of any kind: no telemetry, no analytics, no cloud service, no LLM, no account. Your code and command output never leave your computer.

## How it cuts tokens

- **⚡ Compact Output.** `justless run` parses each tool's output: test failures grouped by their reported diagnostic (cargo, pytest, Go, Jest/Vitest), lint and compiler findings grouped by rule ID with `file:line` (rustc, Clippy, Ruff, mypy, ESLint, tsc and more), search matches grouped by file and paths by directory. Anything else is clipped to a 4 KB preview built from the head, the tail and a section of error and warning lines.
- **🎯 Exact Recall.** Every stream is archived byte for byte under `.justless/runs/<id>` with a SHA-256 digest. `justless log ID --from N --to M` replays exact line ranges, and a rerun with identical digests and exit status collapses to `Output unchanged since run-…`.
- **🔬 Surgical Reads.** Outlines come from syn for Rust and Tree-sitter for 20 more languages (TypeScript and TSX count separately), with revision-bound row IDs. `expand` returns the exact byte span of one symbol, and `edit` writes only the replacement after verifying the file revision.
- **🗺️ Code Map and Impact.** An incremental index of symbols, calls, imports, HTTP routes and SQL relations, refreshed by file timestamp and content hash. `pack` ranks passages by term rarity and follows links up to three hops; `affected` walks reverse edges to the tests that reach a change and prints the command to run them.
- **🧹 Context Hygiene.** Hook-routed runs add a footer only when output was clipped or failed. Compaction hooks carry run IDs, outcomes and inspected ranges into the next turn.

### Measured

| What the agent ran | Raw output | With justless |
| --- | ---: | ---: |
| 77 real commands across four Rust and Python repositories | 1,171 KB | **115 KB** (90% smaller) |
| `git diff` against older revisions | 274 KB | 13.5 KB |
| `find .` | 267 KB | 9.4 KB |
| `grep -rn` | 200 KB | 16.4 KB |
| `cargo check`, successful builds in three repositories | 26.5 KB | 0.5 KB |
| `cargo test` with one failing test, in [`fixtures/failing`](fixtures/failing) | 18,038 B | 700 B |
| `cargo clippy -- -W clippy::pedantic` on this repository, 142 warnings | 79,563 B | 3,916 B |
| One 21 KB source file: outline, then one symbol | 21 KB | 2.1 KB, then 60 B |
| Three-turn repair task, provider-reported input tokens, median of 3 runs: Claude Haiku 5.5 | 22,367 | **2,847** (87.3% fewer) |
| Same task: Claude Sonnet 5.5 | 22,762 | **2,840** (87.5% fewer) |
| Same task: Claude Opus 5.5 | 22,762 | **2,852** (87.5% fewer) |
| Same task: Claude Fable 5.2 | 22,762 | **2,846** (87.5% fewer) |

Originals stay recoverable and exit codes matched in every run. The repair task passed independent tests in all 24 runs (4 models, 3 runs, with and without justless); it is one task, and a broader benchmark is in progress. [Methods](docs/benchmarks.md)

The `fixtures/failing` and Clippy rows were measured on 2026-11-18 at commit `bbe3d44` on macOS with Rust 1.93.0, after a first build. Each compares the bytes the command printed on stdout and stderr with the bytes its `justless run --hook` replacement printed, footer included. The grouped lint output is in the [examples](docs/examples.md#group-lint-findings-by-rule).

### One failing test, before and after

[`fixtures/failing`](fixtures/failing) has a test that logs 351 trace lines before its assertion fails. Without justless, `cargo test` prints 380 lines (18,038 bytes), abridged here:

```text
[…]
---- total_includes_fee stdout ----
billing-trace step=000 customer=demo state=ready
billing-trace step=001 customer=demo state=ready
[… 348 more billing-trace lines …]
billing-trace step=349 customer=demo state=ready

thread 'total_includes_fee' (2688701) panicked at tests/checkout.rs:11:5:
assertion `left == right` failed
  left: 106
 right: 105
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
[…]
error: test failed, to rerun pass `--test checkout`
```

Once the tests are built, the hook runs the same command as `justless run --hook … -- cargo test`, and the agent receives these 700 bytes, unabridged:

```text
1 reported failing test(s)
1 test(s): total_includes_fee tests/checkout.rs:11:5: assertion `left == right` failed left: 106 right: 105 [stdout:365-368]
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.00s
     Running unittests src/lib.rs (target/debug/deps/checkout_fixture-4b58a8d3c9315df3)
     Running tests/checkout.rs (target/debug/deps/checkout-08b118bf0c4a4cc2)
error: test failed, to rerun pass `--test checkout`
[justless run-Rh1dgeGqKvnq · 634 B of 17.6 KB shown · exit 101]
```

The exit status is still 101. `[stdout:365-368]` points into the archived log: `justless log run-Rh1dgeGqKvnq --stream stdout --from 365 --to 368` returns those four lines byte for byte, and `justless context --run run-Rh1dgeGqKvnq` opens the assertion and the function it calls. The [examples](docs/examples.md#diagnose-and-fix-a-failing-test) carry on to the fix.

### What it does not do

- **No changed commands.** Same command, arguments, directory, environment and exit code. The one addition is an option that keeps a search out of `.justless/`. Pipes, redirections, interactive flags and machine formats such as `--json` pass through. Output goes to files rather than a terminal, so a tool that adds color or progress bars only on a terminal prints plain text.
- **No network, no LLM, no telemetry.** justless never opens a network connection. Parsing is deterministic and local, and everything it stores stays in `.justless/` in your project. The installer goes online to download a release or build with Cargo. Your agent still sends its context to its model provider; with justless, that context holds the shortened output.
- **No hidden failures.** Anything cut is marked and one command away.

## Install

Run the line for your agent **inside your project folder**. It installs justless and turns it on for that project. It downloads a prebuilt binary when a release exists for your platform and otherwise builds from source, which needs [Rust](https://rustup.rs).

### Claude Code

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- claude
```

Then start a new `claude` session.

### Codex

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- codex
```

Then run `/hooks` in Codex once and trust the hooks.

### Cursor

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- cursor
```

Then start a new agent chat.

### Gemini CLI

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- gemini
```

Then start a new session.

### GitHub Copilot CLI

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- copilot
```

Then start a new session.

### OpenCode

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- opencode
```

Then restart OpenCode.

### Antigravity CLI

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- agy
```

Then start a new session.

### Several agents, or every agent on your machine

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- claude codex   # name the agents you use
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh                      # set up every agent found on your PATH
```

### Other MCP clients, such as VS Code

```sh
justless mcp --install vscode   # also: claude, cursor, gemini, codex
```

This gives the client justless's read-only navigation tools without hooks.

### Check that it works

```sh
justless run -- git status   # ends with a line like: [justless run-…] exit 0 · …
```

In an agent session, run your tests. Compact or failing output ends with a `[justless run-…]` marker. Short output from a command that succeeded arrives unchanged with no marker, and an identical rerun collapses to `Output unchanged since run-…`. `justless history` still lists every run. `justless stats` shows what was saved, and `justless discover` lists commands that still bypassed justless.

### Add another project later

The binary only needs installing once. In any other project, run `justless init --hooks --agent claude` (or `codex`, `cursor`, `gemini`, `copilot`, `opencode`, `agy`). Setup keeps your existing instructions and settings, and running it again is safe.

### Update or remove

To update, run your install line again, then rerun `justless init --hooks --agent …` in your projects to pick up new hooks. To remove justless from a project, delete the section between `<!-- justless:begin -->` and `<!-- justless:end -->` in `CLAUDE.md`, `AGENTS.md` or `GEMINI.md`, the `justless` hook entries in the agent's config file, and the `.justless/` directory. [All install options](docs/reference.md#install-and-configure-agents)

## Features

- **Progressive reads.** Browse signatures and line ranges, then expand one symbol or Markdown section to its exact source.
- **Verified edits.** Send just the replacement for a symbol or line range. Preview a diff or apply a same-file batch atomically. Stale revisions are rejected.
- **Recoverable tool history.** Keep recent results in context and retire older ones to local storage with OpenCode v1 or an API integration. Recover any archived result by ID.
- **Task packs.** Give `justless pack` a question and a budget. It gathers relevant source lines, callers, tests, and recent failures in one response.
- **Cross-language navigation.** Trace a TypeScript request through a Python handler to a SQL table. Each link points to a source line, and inferred links are marked as possible.
- **Repository map and change impact.** `justless map` shows a new repository's areas, hubs, linked communities, cross-language links, entry points and tests in one budgeted report. `justless affected` lists what depends on a symbol, file or your current changes, hop by hop, and the tests to run.
- **Search across code, docs, and config.** Find a literal in a runbook or an unfamiliar file type. The index refreshes after edits and reuses unchanged files.
- **Grouped test failures.** See which tests reported the same error, then jump from the captured diagnostic to the assertion or function that produced it.
- **Grouped lint findings.** Compiler and linter output from Clippy, Ruff, mypy, ESLint, tsc, ShellCheck, golangci-lint and others collapses to one group per rule with counts and locations. Errors keep their full detail.
- **Rerun comparisons.** Run the same command after an edit to see which failures or diff hunks appeared or disappeared. Identical output can collapse to a single line.
- **Exact log replay.** Every captured run has an ID. Search its full output or replay a specific range of stdout or stderr, byte for byte.
- **Context that survives compaction.** Claude Code and Codex hooks carry recent run IDs, outcomes and inspected files across compaction, so agents recover logs instead of rerunning commands.
- **Cross-session handoffs.** Search earlier runs for an error, or use `justless resume` to see what the last session inspected and which files have changed since.
- **MCP server.** Cursor, Gemini CLI, VS Code and other MCP clients can call the read-only navigation commands as tools with `justless mcp`. `justless mcp --install cursor` adds it to the project's client config.
- **Savings you can see.** `justless stats` shows bytes and estimated tokens saved by day and by command, and how often agents reopened full logs. `justless discover` finds commands agents ran without justless in Claude Code transcripts.

On one three-turn repair task, Claude used **87.3% fewer input tokens** with justless. Both runs produced a fix that passed independent tests. [Benchmark and tool-call counts](docs/benchmarks.md#three-turn-repair)

## Use from the terminal

```sh
justless read src/main.rs --outline  # Expand a row with justless expand FILE 'REV#ID'
justless pack "What breaks if I rename purchase_orders?"
justless run -- git diff      # Run again after an edit to see changed hunks
justless resume              # Pick up where the last session left off
```

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/how-it-works-dark.svg">
  <img src="assets/how-it-works-light.svg" alt="Your agent reaches Justless through hooks, MCP or the CLI. Justless turns commands, code lookups and old tool results into compact answers for the context window, archives the full output in .justless, and returns exact bytes on demand with log, expand and recall." width="100%">
</picture>

- **Commands.** Hooks route builds, tests, linters, diffs, searches, file listings and cluster listings through `justless run`. The agent sees grouped failures, rules, hunks, matches by file and paths by directory with a run ID instead of the raw stream. Claude Code's Grep and Glob results are grouped the same way.
- **Code.** A local index of 21 languages answers definition, caller, path and task-pack questions, so the agent reads the lines it needs rather than whole files.
- **History.** Older tool results are retired to an ID, and recent activity is carried across compaction and sessions.
- **Exact on demand.** Everything is archived in `.justless/`. `log`, `expand` and `recall` return the original bytes whenever the summary is not enough.

[Command reference](docs/reference.md) · [Worked examples](docs/examples.md) · [Benchmarks](docs/benchmarks.md)

## How it compares

Two other projects work on the same problem from different angles. Both are worth knowing.

- **[RTK](https://github.com/rtk-ai/rtk)** also rewrites an agent's shell commands through hooks and filters their output. Its README lists more than 100 supported commands and 18 coding tools, far more than justless covers. RTK also offers smart file reads, a savings dashboard, and `rtk recall` for the full output of failed or trimmed commands. A separate optional proxy compresses API traffic. justless covers fewer commands. It archives every run by default, including output it showed in full, and each grouped failure, lint rule, search file or diff hunk names its line range in the log. It also compares reruns and adds a code index, verified edits and handoffs across compaction. Both tools rewrite the same shell commands, so use one of them for command output in a given project.
- **[graphify](https://github.com/Graphify-Labs/graphify)** turns a folder into a knowledge graph that an agent can query instead of grepping. It parses code locally with Tree-sitter in about 40 languages. It can add docs, PDFs and images through a model pass, and video through local transcription. It writes an interactive HTML graph and a report, and offers `query`, `path` and `explain`. It updates the graph with `--update`, `graphify watch` or a Git hook. It does not process command output. justless indexes source in 21 languages and searches other text files literally with `find`. It does not read PDFs, images or video and has no graph viewer. Its `map`, `query`, `path` and `affected` refresh the index from the working tree on every call, with no separate update step, and each result names a file and line.

## FAQ

**Where is the output stored?**
In `.justless/` at the project root. Each run has a directory with its exact `stdout`, `stderr` and a `record.json`. The same directory holds the code index, retired tool results and recent activity. On Unix it is created readable only by your user. It holds its own `.gitignore`, so Git ignores it even before `justless init` adds it to the project's `.gitignore`. Nothing is deleted automatically. You can delete the directory at any time: the index is rebuilt on the next query, and older run IDs stop resolving. Captured output can include secrets that a command printed, so keep it private. `justless run --raw -- COMMAND` runs a command without recording it.

Routed searches skip the archive: `grep` and `ack` get an option that excludes `.justless/`, as do `rg` and `ag` when they search hidden files. A listing such as `find .` can still include the files under `.justless/runs/`.

**How do I turn it off?**
For one command, run `justless run --raw -- COMMAND`. Hooks leave a command that already runs through `justless run` unchanged, and `--raw` streams its output untouched. For a project, remove the hook entries and the instruction section as described in [Update or remove](#update-or-remove), then start a new agent session. To remove it everywhere, do that in each project, then delete the `justless` binary from the install prefix, `~/.cargo/bin` by default.

## Contributing

Build, test and commit conventions are in [CONTRIBUTING.md](CONTRIBUTING.md). Maintainer notes are in [AGENTS.md](AGENTS.md), and user-visible changes are listed in the [changelog](CHANGELOG.md).
