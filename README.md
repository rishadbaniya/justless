# Justless

Justless helps coding agents use fewer tokens when running commands and exploring a codebase. It summarizes test failures and diffs, shows what changed on a rerun, and keeps the full output locally. Ask a question about your code to get relevant source lines, callers, and recorded failures in one response.

## Install

From your project directory, run the installer with Rust 1.91 or newer and a C compiler on your machine.

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh
```

It installs Justless and configures project hooks for Codex, Claude Code, and Antigravity CLI (`agy`) when it finds them on your `PATH`. Start a new agent session in the project. In Codex, review and trust the hooks with `/hooks`.

## Use from the terminal

```sh
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
```

[Command reference](docs/reference.md) · [Worked examples](docs/examples.md) · [Benchmarks](docs/benchmarks.md)
