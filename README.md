<p align="center">
  <img src="assets/logo.png" alt="Justless logo" width="140">
</p>

<h1 align="center">justless</h1>

<p align="center">
  <strong>Your agent reads less. It knows more.</strong><br>
  Compact by default. Exact on demand.
</p>

<p align="center">
  <a href="#install">Install</a> · <a href="docs/reference.md">Reference</a> · <a href="docs/examples.md">Examples</a> · <a href="docs/benchmarks.md">Benchmarks</a>
</p>

Justless is a local CLI that helps coding agents use fewer tokens when running commands and exploring a codebase.

## Features

- **Progressive reads.** Browse signatures and line ranges, then expand one symbol or Markdown section to its exact source.
- **Verified edits.** Send just the replacement for a symbol or line range. Preview a diff or apply a same-file batch atomically. Stale revisions are rejected.
- **Recoverable tool history.** Keep recent results in context and retire older ones to local storage with OpenCode v1 or an API integration. Recover any archived result by ID.
- **Task packs.** Give `justless pack` a question and a budget. It gathers relevant source lines, callers, tests, and recent failures in one response.
- **Cross-language navigation.** Trace a TypeScript request through a Python handler to a SQL table. Each link points to a source line, and inferred links are marked as possible.
- **Repository map and change impact.** `justless map` shows a new repository's areas, hubs, linked communities, cross-language links, entry points and tests in one budgeted report. `justless affected` lists what depends on a symbol, file or your current changes, hop by hop, and the tests to run.
- **Search across code, docs, and config.** Find a literal in a runbook or an unfamiliar file type. The index refreshes after edits and reuses unchanged files.
- **Grouped test failures.** See which tests reported the same error, then jump from the captured diagnostic to the assertion or function that produced it.
- **Grouped lint findings.** Compiler and linter output from Clippy, Ruff, mypy, ESLint, tsc, ShellCheck, golangci-lint and others collapses to one line per rule with counts and locations. Errors keep their full detail.
- **Rerun comparisons.** Run the same command after an edit to see which failures or diff hunks appeared or disappeared. Identical output can collapse to a single line.
- **Exact log replay.** Every captured run has an ID. Search its full output or replay a specific range of stdout or stderr, byte for byte.
- **Context that survives compaction.** Claude Code and Codex hooks carry recent run IDs, outcomes and inspected files across compaction, so agents recover logs instead of rerunning commands.
- **Cross-session handoffs.** Search earlier runs for an error, or use `justless resume` to see what the last session inspected and which files have changed since.
- **MCP server.** Cursor, Gemini CLI, VS Code and other MCP clients can call the read-only navigation commands as tools with `justless mcp`. `justless mcp --install cursor` adds it to the project's client config.
- **Savings you can see.** `justless stats` shows bytes and estimated tokens saved by day and by command, and how often agents reopened full logs. `justless discover` finds commands agents ran without Justless in Claude Code transcripts.

On one three-turn repair task, Claude used **87.3% fewer input tokens** with Justless. Both runs produced a fix that passed independent tests. [Benchmark and tool-call counts](docs/benchmarks.md#three-turn-repair)

## Install

### 1. Install the binary (once per machine)

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- --cli-only
```

It downloads a verified prebuilt binary when a release exists for your platform, and otherwise builds with Cargo (Rust 1.91+ and a C compiler). The binary goes to `~/.cargo/bin`, which must be on your `PATH` so agent hooks can find `justless`. From a checkout you can run `cargo install --locked --path .` instead.

### 2. Turn it on in a project

Run this inside each project, naming the agents you use:

```sh
cd your-project
justless init --hooks --agent claude --agent codex
```

| Agent | Flag | What it writes | Then |
| --- | --- | --- | --- |
| Claude Code | `--agent claude` | `.claude/settings.json`, `CLAUDE.md` | Start a new `claude` session |
| Codex | `--agent codex` | `.codex/hooks.json`, `AGENTS.md` | Run `/hooks` in Codex and trust them |
| Cursor | `--agent cursor` | `.cursor/hooks.json`, `AGENTS.md` | Start a new agent chat |
| Gemini CLI | `--agent gemini` | `.gemini/settings.json`, `GEMINI.md` | Start a new session |
| GitHub Copilot CLI | `--agent copilot` | `.github/hooks/justless.json`, `AGENTS.md` | Start a new session |
| Antigravity CLI | `--agent agy` | `.agents/hooks.json`, `AGENTS.md` | Start a new session |
| OpenCode | `--agent opencode` | `.opencode/plugins/justless.js`, `AGENTS.md` | Restart OpenCode |

Setup keeps your existing instructions and settings, and running it again is safe. It also adds `.justless/` to `.gitignore`.

Shortcut: running the installer from inside a project does both steps and sets up every agent it finds on your `PATH`.

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh                  # detect agents
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- claude codex   # pick agents
```

### 3. Check that it works

```sh
justless run -- git status   # ends with a line like: [justless run-…] exit 0 · …
```

In an agent session, ask it to run your tests. The tool output should end with a `[justless run-…]` line. Later, `justless stats` shows how much output was kept out of context, and `justless discover` lists commands that still bypassed Justless.

### Optional: MCP tools

Clients without Justless hooks, such as VS Code, can use the read-only navigation tools over MCP:

```sh
justless mcp --install vscode   # also: claude, cursor, gemini, codex
```

### Update or remove

To update, run the install command again, then rerun `justless init --hooks --agent …` in your projects to pick up new hooks. To remove Justless from a project, delete the section between `<!-- justless:begin -->` and `<!-- justless:end -->` in the instruction files, the `justless` hook entries in the agent config files listed above, and the `.justless/` directory.

[All install options](docs/reference.md#install-and-configure-agents)

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
