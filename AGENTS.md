# Working on Justless

Justless is a local Rust CLI that reduces the context coding agents spend on commands, source reads, edits and old tool results. It serves repositories in many languages. A compact result should give the agent enough evidence to act and a way to retrieve the original when it needs more.

## Start in a fresh checkout

Check `git status -sb` and `git log -5 --oneline` for current work. The README has the architecture diagram. The command reference describes the shipped behavior and client support. [README](README.md) · [Command reference](docs/reference.md) · [Worked examples](docs/examples.md)

From the repo root, build with Rust 1.91 or newer and a C compiler. Use the checkout binary so you exercise the code you are changing.

```sh
cargo build --locked
./target/debug/justless read src/main.rs --outline
./target/debug/justless --root fixtures/mixed query purchase_orders
```

On a returning checkout, `./target/debug/justless resume` shows recent runs and inspected source. That history lives in the gitignored `.justless/` directory. A fresh clone gets the implementation and guides from Git and builds its own index on first query.

## Where to change a behavior

| If the task concerns | Start here | Contract to preserve |
| --- | --- | --- |
| CLI arguments, root selection, local state | `src/main.rs`, `src/workspace.rs` | Honor `--root` and project settings. Resolve source paths within the workspace. |
| Command previews and reruns | `src/capture.rs`, `src/summary.rs`, `src/history.rs` | Preserve the child exit status and exact stdout/stderr replay. Compare reruns within their command, directory and session. |
| Search, relationships and task packs | `src/rust_syntax.rs`, `src/polyglot.rs` → `src/index.rs` → `src/context.rs`, `src/pack.rs` | Rust uses syn and other structured languages use Tree-sitter. Keep plain-text search available for unfamiliar formats and mark inferred links. Check cache invalidation when changing extracted facts. |
| Outlines, expansion and edits | `src/source.rs` and the two parsers | Expansion returns the exact selected bytes. Edits verify the path-bound revision. A stale or overlapping edit rejects the whole same-file batch. |
| Tool history retirement | `src/observations.rs`, `integrations/opencode.js` | Archive a result before replacing it. Recall must reproduce its original value. OpenCode v1 automates this on outbound requests. Other API clients can call `mask` at the request boundary. |
| MCP server and client config | `src/mcp.rs` | Stdout carries only JSON-RPC messages, so tools run the CLI in a child process. Expose only read-only commands. Client setup merges into existing config without touching other keys. |
| Agent setup and hooks | `install.sh`, `src/agents.rs` | Preserve existing project instructions and settings across repeated installs. Codex, Claude Code and Antigravity have separate shell hook contracts. Claude Code also supports native read outlines. |

The instruction template in `src/agents.rs` is installed into **consumer repositories**. Edit that template to change what their agents see. Edit this file for guidance about developing Justless itself.

## Verify a change

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`tests/cli.rs` exercises the binary, including command capture, navigation and hooks. `tests/progressive.rs` covers exact spans, stale and concurrent edits, and observation recovery. `fixtures/failing` is deliberately broken so the tests can diagnose and repair disposable copies. `fixtures/mixed` exercises links across TypeScript, Python and SQL.

For a context-size change, run `python3 scripts/context_benchmark.py --binary ./target/debug/justless` after building. It applies a verified repair and round-trips a masked request locally. Report byte measurements as bytes. Provider token counts come from the separate API benchmarks. [Benchmark methods and results](docs/benchmarks.md)

Use Conventional Commits such as `feat(source): support section edits`. Keep the README focused on the goal, architecture and quick start. Put detailed flags and examples in `docs/`, and describe features in Justless’s own terms.
