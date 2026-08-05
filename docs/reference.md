# Justless reference

## Commands

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
| `justless pack "What breaks if I rename purchase_orders?"` | A budgeted selection of definitions, graph links, source lines, working-tree changes, and relevant recorded failures |
| `justless history "TimeoutError" --oldest --limit 1` | First recorded matching log line across archived commands in this project. Omit the text to list recent runs. |
| `justless resume` | Recent command outcomes, inspected source ranges, and files in the last pack. Stale source is marked. |
| `justless stats` | Raw and preview bytes, full-log recalls by command family, repeated reads and commands passed through by hooks |

## Indexing and languages

Queries refresh the index automatically. Justless compares file identity and high-resolution timestamps where available, hashes files that need closer inspection, reuses parsed facts for unchanged content, and removes deleted files. In a Git checkout it uses tracked and unignored files. Add patterns to `.justlessignore` for repo-specific exclusions.

Structured navigation supports Rust, JavaScript, TypeScript and TSX, Python, Go, Java, C, C++, C#, Ruby, PHP, Bash, and SQL. The available symbols and relationships vary by language. Calls across implicit imports, framework discovery, dynamic dispatch, and macro expansion can require source verification. Use `find` for any indexed UTF-8 text file. When source has changed since a recorded run, Justless preserves the original log location and flags the mismatch.

## Task packs and run history

`pack` runs locally. It matches identifiers and file paths, ranks passages by how common their terms are in the repository, and follows incoming and outgoing graph edges up to three hops. It considers up to 96 candidate text files and 25 recent runs. Working-tree changes boost relevant symbols. Inferred links carry a `possible` label. If a source digest differs from the one stored with a failure, the pack marks the old location as stale. A later successful run of the same command supersedes its failure.

`run` groups recognized test failures by their reported diagnostic and summarizes Git patch hunks with file names, hunk headers, and original stdout line ranges. The command still runs every time. For a matching command, directory, session, and preview budget, byte-identical streams and an identical exit status can produce an unchanged notice. Changed test reports and patches show additions and removals against the previous run. A removed failure means it disappeared from the report. Use the exit status to check the overall result. Unrecognized output uses the ordinary preview.

Use `justless run --fresh -- COMMAND` for a fresh preview. Use `--session NAME` or `JUSTLESS_SESSION` to choose a comparison session from the terminal. Project hooks scope comparisons to the client's session identifier when supplied. Captured runs remain searchable across sessions. A marker such as `stdout:8-15` expands with `justless log RUN_ID --stream stdout --from 8 --to 15`. A range marked `prior stdout` belongs to the comparison run.

`history` searches stored streams locally and stops when it reaches the requested match count or byte budget. `resume` covers Justless activity, including the most recent 32 source ranges and files used in the last pack. `stats` counts full-stream reads separately from range, search, and run-context requests. Hook pass-through counts cover shell calls observed by the installed hooks. They record command families without arguments. Preview-byte totals exclude footer text and subsequent lookups. Provider token counts are measured separately by the benchmark.

## Configuration

The optional `.justless.toml` controls project defaults. The values below are the built-in defaults.

```toml
context_budget = 2400
max_output_bytes = 4096
max_file_bytes = 2097152
```

`pack`, `history`, `resume`, `query`, `path`, `find`, `context`, and `code` accept `--budget`. The renderer uses one UTF-8 byte per budget unit as a conservative ceiling for byte-based tokenizers. Agent totals can also include the prompt, history, and tool metadata.

Full output is retained locally under `.justless/runs/`, including output short enough to display in full. The archive keeps the streams separately, with byte-for-byte replay for each. Keep the directory private if a command prints secrets. The CLI runs offline after installation, and `--raw` skips recording.

## Install and configure agents

Run the installer from the project where you want hooks. With no arguments it detects installed clients on `PATH`. You can also choose clients explicitly or install only the CLI. From a Justless checkout you can use `sh /path/to/justless/install.sh` with the same arguments.

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- claude agy
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- --cli-only
```

The installer builds with Cargo. The binary goes in `$CARGO_INSTALL_ROOT/bin`, defaulting to `$CARGO_HOME/bin` or `~/.cargo/bin`. Put that directory on the agent's `PATH`. Once the binary is installed, configure another project from its root with `justless init --hooks --agent codex --agent claude --agent agy`. Select only the clients you use. Running setup again preserves existing instructions, settings, and hooks.

| Client | Instructions | Project hooks |
| --- | --- | --- |
| Codex | `AGENTS.md` | `.codex/hooks.json` |
| Claude Code | `CLAUDE.md` | `.claude/settings.json` |
| Antigravity CLI (`agy`) | `AGENTS.md` | `.agents/hooks.json` |

Open a new session in the trusted workspace. In Codex, review and trust the project hooks with `/hooks`. The session-start hook refreshes the repo index. Before supported shell commands run, the command hook routes them through Justless. Each client handles permissions for the rewritten command. [Codex hooks](https://learn.chatgpt.com/docs/hooks) · [Claude Code hooks](https://code.claude.com/docs/en/hooks) · [Antigravity CLI hooks](https://antigravity.google/docs/hooks?tab=cli)

`justless init` defaults to Codex. Omit `--hooks` to install only the selected client's project instructions and add `.justless/` to `.gitignore`. The `init-codex` alias is also supported.

Run `justless run -- cargo test`, `justless run -- git diff`, or `justless run -- rg -n pattern src`. For interactive commands, live streaming, or pipelines that consume raw machine-readable output, use `justless run --raw -- …` or call the command directly. For shell expressions, pass the shell explicitly, as in `justless run -- sh -c 'cargo test && git diff'`.
