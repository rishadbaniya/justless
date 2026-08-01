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
| `justless stats` | Captured and previewed bytes across runs |

## Indexing and languages

Queries refresh the index automatically. Justless compares file identity and high-resolution timestamps where available, hashes files that need closer inspection, reuses parsed facts for unchanged content, and removes deleted files. In a Git checkout it uses tracked and unignored files. Add patterns to `.justlessignore` for repo-specific exclusions.

Structured navigation supports Rust, JavaScript, TypeScript and TSX, Python, Go, Java, C, C++, C#, Ruby, PHP, Bash, and SQL. The available symbols and relationships vary by language. Calls across implicit imports, framework discovery, dynamic dispatch, and macro expansion can require source verification. Use `find` for any indexed UTF-8 text file. When source has changed since a recorded run, Justless preserves the original log location and flags the mismatch.

## Configuration

The optional `.justless.toml` controls project defaults. The values below are the built-in defaults.

```toml
context_budget = 2400
max_output_bytes = 4096
max_file_bytes = 2097152
```

`query`, `path`, `find`, `context`, and `code` accept `--budget`. The renderer uses one UTF-8 byte per budget unit as a conservative ceiling for byte-based tokenizers. Agent totals can also include the prompt, history, and tool metadata.

Full output is retained locally under `.justless/runs/`, including output short enough to display in full. The archive keeps the streams separately, with byte-for-byte replay for each. Keep the directory private if a command prints secrets. The CLI runs offline after installation, and `--raw` skips recording.

## Codex integration

`init` (also `init-codex`) installs repo instructions in `AGENTS.md` and ignores generated `.justless/` state in Git. `--hooks` also merges Justless hooks into `.codex/hooks.json`. Start Codex in the trusted project with `justless` on `PATH`, then review and trust the new hooks with `/hooks`. [Codex project instructions](https://learn.chatgpt.com/docs/agent-configuration/agents-md) · [Codex hooks](https://learn.chatgpt.com/docs/hooks)

The session hook warms the repo index in the background. The shell hook routes supported, simple Cargo, Go, npm, pytest, Git, and ripgrep commands through `justless run`. Shell pipelines, redirections, substitutions, watch modes, and known machine-output flags run unchanged. Codex evaluates rewritten commands through its normal sandbox and approval flow. The project instructions also work with the CLI directly.

Run `justless run -- cargo test`, `justless run -- git diff`, or `justless run -- rg -n pattern src`. For interactive commands, live streaming, or pipelines that consume raw machine-readable output, use `justless run --raw -- …` or call the command directly. For shell expressions, pass the shell explicitly, as in `justless run -- sh -c 'cargo test && git diff'`.
