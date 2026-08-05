# Justless

Justless is a local CLI that helps coding agents use fewer tokens when running commands and exploring a codebase.

## Features

- **Progressive reads.** Browse signatures and line ranges, then expand one symbol or Markdown section to its exact source.
- **Verified edits.** Send just the replacement for a symbol or line range. Preview a diff or apply a same-file batch atomically. Stale revisions are rejected.
- **Recoverable tool history.** Keep recent results in context and retire older ones to local storage with OpenCode v1 or an API integration. Recover any archived result by ID.
- **Task packs.** Give `justless pack` a question and a budget. It gathers relevant source lines, callers, tests, and recent failures in one response.
- **Cross-language navigation.** Trace a TypeScript request through a Python handler to a SQL table. Each link points to a source line, and inferred links are marked as possible.
- **Search across code, docs, and config.** Find a literal in a runbook or an unfamiliar file type. The index refreshes after edits and reuses unchanged files.
- **Grouped test failures.** See which tests reported the same error, then jump from the captured diagnostic to the assertion or function that produced it.
- **Rerun comparisons.** Run the same command after an edit to see which failures or diff hunks appeared or disappeared. Identical output can collapse to a single line.
- **Exact log replay.** Every captured run has an ID. Search its full output or replay a specific range of stdout or stderr, byte for byte.
- **Cross-session handoffs.** Search earlier runs for an error, or use `justless resume` to see what the last session inspected and which files have changed since.
- **Compression stats.** See how many bytes reached the preview, which commands led agents to reopen full logs, and which commands the hooks passed through.

On one three-turn repair task, Claude used **87.3% fewer input tokens** with Justless. Both runs produced a fix that passed independent tests. [Benchmark and tool-call counts](docs/benchmarks.md#three-turn-repair)

## Install

From your project directory, run the installer with Rust 1.91 or newer and a C compiler on your machine.

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh
```

It installs Justless and configures project hooks for Codex, Claude Code, Antigravity CLI (`agy`), and OpenCode v1 when it finds them on your `PATH`. Start a new agent session in the project. In Codex, review and trust the hooks with `/hooks`.

## Use from the terminal

```sh
justless read src/main.rs --outline  # Expand a row with justless expand FILE 'REV#ID'
justless pack "What breaks if I rename purchase_orders?"
justless run -- git diff      # Run again after an edit to see changed hunks
justless resume              # Pick up where the last session left off
```

## How it works

```text
  Agent hook or terminal                    Repository files
            |                                      |
            v                                      v
       justless run                       Index changed files
            |                                      |
            +--> preview + run ID                  v
            |                             .justless/index.json
            v                             symbols, links, text fingerprints
     .justless/runs/                               |
     logs + diagnostics                            +--> find / query / path
            |                                      |
            +-----------> justless pack <----------+
                                |
                                v
                   source lines + linked evidence

  read --> outline + revision --> expand / verified edit
  mask --> .justless/observations/ --> recall
```

[Command reference](docs/reference.md) · [Worked examples](docs/examples.md) · [Benchmarks](docs/benchmarks.md)
