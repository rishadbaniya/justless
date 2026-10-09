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
| Command profiles and grouped output | `src/filters.rs` → `src/lint.rs`, `src/inventory.rs` | Route only display-oriented invocations; machine formats, watch modes and pipes keep the ordinary preview. A grouped report replaces the preview only when it is smaller and every group links back to its original log lines. Build fixtures from real tool output where possible. |
| Savings reports | `src/history.rs` (`stats`), `src/discover.rs` | Report measured bytes; label token figures as bytes ÷ 4 estimates. `discover` prints command labels and byte counts only, never full command lines or output. |
| Search, relationships and task packs | `src/rust_syntax.rs`, `src/polyglot.rs` → `src/index.rs` → `src/context.rs`, `src/pack.rs` | Rust uses syn and other structured languages use Tree-sitter. Keep plain-text search available for unfamiliar formats and mark inferred links. Check cache invalidation when changing extracted facts. |
| Affected test selection | `src/impact.rs` → `src/selection.rs` → `src/runners.rs` → `src/testrun.rs` | Label every subset and never report it as the suite. Preserve the runner's exit status. Never select a target the base command would not run. Widen names that did not run instead of passing them. Fall back with a stated reason. |
| Outlines, expansion and edits | `src/source.rs` and the two parsers | Expansion returns the exact selected bytes. Edits verify the path-bound revision. A stale or overlapping edit rejects the whole same-file batch. |
| Tool history retirement | `src/observations.rs`, `integrations/opencode.js` | Archive a result before replacing it. Recall must reproduce its original value. OpenCode v1 automates this on outbound requests. Other API clients can call `mask` at the request boundary. |
| MCP server and client config | `src/mcp.rs` | Stdout carries only JSON-RPC messages, so tools run the CLI in a child process. Expose only read-only commands. Client setup merges into existing config without touching other keys. |
| Agent setup and hooks | `install.sh`, `src/agents.rs`, `src/uninstall.rs` | Preserve existing project instructions and settings across repeated installs. Codex, Claude Code, Antigravity, Cursor, Gemini CLI and Copilot have separate shell hook contracts. Claude Code also supports native read outlines. A client that auto-approves rewritten commands only gets command lists made entirely of supported commands. Re-running `init --hooks` adds new hook entries, such as the compaction hooks, to existing installs. `init --remove` must undo every file setup writes, so call `uninstall::remember` for each new setup write before creating its directory. |
| Setup checks | `src/doctor.rs`, the accessors at the end of `src/agents.rs` | `doctor` only reads: no index refresh, lock or `.justless/`. It judges hooks and instruction blocks against `init`'s own output with `init`'s matching, and each fix it prints must clear its problem, so anything `init` leaves alone gets a fix by hand. |
| Claude Code status line | `src/statusline.rs` | Exit 0 with one line for any input and write nothing. `init --statusline` writes only the local settings file, and its `.gitignore` entry, and never replaces a status line from local, project or user settings. Hooks and routed runs list nested projects for their session in the launch project. |
| Prebuilt binaries and releases | `install.sh`, `.github/workflows/`, `packaging/homebrew/` | Install an archive only when its SHA-256 matches `SHA256SUMS`, and abort on a mismatch. Fall back to Cargo when no prebuilt binary fits. Keep the crate compiling on Windows by gating Unix-only APIs with `cfg(unix)`. |

The instruction template in `src/agents.rs` is installed into **consumer repositories**. Edit that template to change what their agents see. Edit this file for guidance about developing Justless itself.

## Verify a change

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

For an installer change, also run `sh scripts/test_install.sh` (and `sh scripts/test_install.sh dash` where available). It installs from local fake releases with a stub Cargo.

`tests/cli.rs` exercises the binary, including command capture, navigation and hooks. `tests/progressive.rs` covers exact spans, stale and concurrent edits, and observation recovery. `fixtures/failing` is deliberately broken so the tests can diagnose and repair disposable copies. `fixtures/mixed` exercises links across TypeScript, Python and SQL. `fixtures/affected` is a three-package Cargo workspace for `test --affected`: a required-features target, a black-box CLI test, a test macro, an embedded data file and untested code.

For a test-selection change, run `python3 scripts/affected_benchmark.py --binary ./target/debug/justless`. It mutates a disposable copy of `fixtures/affected`, runs each mutant as a subset and as the full suite, and exits nonzero on any missed failure.

For a context-size change, run `python3 scripts/context_benchmark.py --binary ./target/debug/justless` after building. It applies a verified repair and round-trips a masked request locally. Report byte measurements as bytes. Provider token counts come from the separate API benchmarks. [Benchmark methods and results](docs/benchmarks.md)

Use Conventional Commits such as `feat(source): support section edits`. Keep the README focused on the goal, architecture and quick start. Put detailed flags and examples in `docs/`, and describe features in Justless’s own terms.

## Cut a release

1. On an up-to-date `main` where `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` pass locally, set `version` in `Cargo.toml` to `X.Y.Z`, run `cargo build` (without `--locked`) to refresh `Cargo.lock`, and commit `chore(release): X.Y.Z`.
2. Tag and push: `git tag vX.Y.Z && git push origin main vX.Y.Z`.
3. `.github/workflows/release.yml` checks that the tag matches `Cargo.toml`, builds the five targets, and publishes a GitHub release with the archives and `SHA256SUMS`. A version with a hyphen, such as `0.3.0-rc.1`, becomes a prerelease, which the installer's latest download skips.
4. For Homebrew, update `url` and `sha256` in `packaging/homebrew/justless.rb` as its header describes and push it to the tap.
