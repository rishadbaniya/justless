# Justless

Justless helps coding agents use fewer tokens when running commands and exploring a codebase. It shows a short preview of each command’s output and saves the full log locally. Its index lets an agent trace a function across files and languages, then read the source lines it needs.

## Install

From your project directory, run the installer with Rust 1.91 or newer and a C compiler on your machine.

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh
```

It installs Justless and configures project hooks for Codex, Claude Code, and Antigravity CLI (`agy`) when it finds them on your `PATH`. Start a new agent session in the project. In Codex, review and trust the hooks with `/hooks`.

## Use from the terminal

```sh
justless run -- git diff       # Preview a diff and keep the full output
justless find "/api/orders"    # Find matching lines in code, docs, and config
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
            +-----> justless context --run ID <----+
                                |
                                v
                     log excerpt + source lines
```

[Command reference](docs/reference.md) · [Worked examples](docs/examples.md) · [Benchmarks](docs/benchmarks.md)
