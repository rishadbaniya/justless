# Contributing to Justless

Thanks for helping. This guide covers building, testing and the conventions the project follows. [AGENTS.md](AGENTS.md) has the maintainer notes: where each behavior lives and which contract it must keep. Coding agents working in this repository read it too.

## Build

You need Rust 1.91 or newer and a C compiler, because the Tree-sitter grammars are compiled from C. Python 3 runs the benchmark scripts.

```sh
cargo build --locked
./target/debug/justless read src/main.rs --outline
./target/debug/justless --root fixtures/mixed query purchase_orders
```

Run the binary from `target/debug` so you test the code you are changing, not an installed copy. Justless keeps its local state in `.justless/`, which Git ignores. A fresh clone builds its index on the first query.

## Check a change

Run these before every commit. There is no CI workflow, so these local checks are the gate.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Run the tests on Linux or macOS. Hooks are disabled on Windows and several suites drive POSIX shell tools, but the crate must still compile there, so gate Unix-only APIs with `cfg(unix)`.

| If you change | Also run |
| --- | --- |
| `install.sh` | `sh scripts/test_install.sh`, and `sh scripts/test_install.sh dash` if `dash` is installed. It runs the installer with a stub Cargo, so it needs no network and builds nothing. |
| How much output or source reaches the agent | `python3 scripts/context_benchmark.py --binary ./target/debug/justless` after building. Report the results as bytes. |
| The repair or replay benchmarks | See [Benchmarks](docs/benchmarks.md). The `--api` modes make paid API calls and are optional. |

### Tests and fixtures

- `tests/cli.rs` runs the binary: command capture, navigation and hooks.
- `tests/progressive.rs` covers exact spans, stale and concurrent edits, and recovery of retired tool results.
- `tests/mcp.rs` covers the MCP server.
- `fixtures/failing` is deliberately broken. Tests copy it to a temporary directory to diagnose and repair it, so do not fix it.
- `fixtures/mixed` links TypeScript, Python and SQL. `fixtures/languages` has samples for Kotlin, Swift, Scala, Dart, Terraform, Lua and Zig, and `fixtures/lint` holds recorded linter output.

If you change the facts the index extracts, make sure existing caches are rebuilt. `VERSION` in `src/index.rs` forces a rebuild when it changes.

## Conventions

- **Commits** use [Conventional Commits](https://www.conventionalcommits.org/), usually with a scope, as in `feat(source): support section edits` or `fix(hooks): leave grep to Claude Code's gitignore-aware shell`. Say in the body what changed for the user and why.
- **Exactness.** A compact result must give the agent enough to act and a way back to the original. Preserve the child's exit status and exact stdout and stderr replay. `expand` returns the exact selected bytes. An edit verifies the revision it was based on.
- **Agent instructions.** The text in `src/agents.rs` between `<!-- justless:begin -->` and `<!-- justless:end -->` is installed into other people's repositories, and their agents load it with every model request. Edit it there to change what their agents see, and keep it short: a test in `tests/cli.rs` fails once it passes 1,024 bytes. Edit `AGENTS.md` for guidance about developing Justless itself.
- **Existing setup.** `justless init` and `install.sh` must keep a project's existing instructions and settings, and running them twice must be safe.
- **Documentation.** Keep the README to the goal, the architecture and the quick start. Put flags and details in [docs/reference.md](docs/reference.md) and walkthroughs in [docs/examples.md](docs/examples.md). Describe features in Justless's own terms. Every number in the docs should come from a measurement described next to it, or from [docs/benchmarks.md](docs/benchmarks.md).
- **Changelog.** Add user-visible changes to the Unreleased section of [CHANGELOG.md](CHANGELOG.md).

## Report a problem

Open an issue on GitHub with the command you ran, what you expected and what Justless showed. A small reproducer, such as a copy of a fixture with a change, helps most. Archived logs under `.justless/` can contain secrets that a command printed, so check them before you attach them.
