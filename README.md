# Justless

Justless helps coding agents use fewer tokens when running commands and exploring a codebase. It shows a short preview of each command’s output and saves the full log locally. Its index lets an agent trace a function across files and languages, then read the source lines it needs.

## Install

Building Justless requires Rust 1.91 or newer and a C compiler.

```sh
cargo install --locked --git https://github.com/rishadbaniya/justless.git
```

## Use with Codex

Run these commands from your project’s root.

```sh
justless init --hooks
codex
```

In Codex, open `/hooks` and trust the project hooks. Justless then indexes the repo at session start and automatically shortens output from supported commands.

## Use from the terminal

```sh
justless run -- git diff       # Preview a diff and keep the full output
justless find "/api/orders"    # Find matching lines in code, docs, and config
```

## How it works

```text
  Codex hook or terminal                    Repository files
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
