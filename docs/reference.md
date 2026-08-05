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
| `justless read FILE --outline` | Revision, signatures and line ranges. Small files are returned verbatim when called without outline options. |
| `justless expand FILE SELECTOR` | Exact bytes for a symbol, entry, section or inclusive line range |
| `justless edit FILE SELECTOR --revision REV` | Apply the replacement supplied on stdin after verifying the snapshot |
| `justless mask` / `justless recall ID` | Retire older tool outputs from an outbound JSON request or retrieve the original result |
| `justless pack "What breaks if I rename purchase_orders?"` | A budgeted selection of definitions, graph links, source lines, working-tree changes, and relevant recorded failures |
| `justless history "TimeoutError" --oldest --limit 1` | First recorded matching log line across archived commands in this project. Omit the text to list recent runs. |
| `justless resume` | Recent command outcomes, inspected source ranges, and files in the last pack. Stale source is marked. |
| `justless stats` | Raw and preview bytes, full-log recalls by command family, repeated reads and commands passed through by hooks |

## Indexing and languages

Queries refresh the index automatically. Justless compares file identity and high-resolution timestamps where available, hashes files that need closer inspection, reuses parsed facts for unchanged content, and removes deleted files. In a Git checkout it uses tracked and unignored files. Add patterns to `.justlessignore` for repo-specific exclusions.

Structured navigation supports Rust, JavaScript, TypeScript and TSX, Python, Go, Java, C, C++, C#, Ruby, PHP, Bash, and SQL. The available symbols and relationships vary by language. Calls across implicit imports, framework discovery, dynamic dispatch, and macro expansion can require source verification. Use `find` for any indexed UTF-8 text file. When source has changed since a recorded run, Justless preserves the original log location and flags the mismatch.

## Progressive reads and verified edits

Commands take workspace-relative paths. Run `justless read FILE --outline` to get a path-bound revision and numbered rows. An entry reference combines the revision and row number, such as `abc…#7`. Use `--match TEXT` to search names and signatures, `--limit N` to set a page size, and `--after CURSOR` to resume the same search. `--budget` bounds the text outline. `--json` adds structured fields and a `next` cursor. Normal reads return small UTF-8 files verbatim. Outlines use the available language parser, Markdown ATX headings, or 80-line pages.

```sh
justless read src/main.rs --match dispatch --outline
justless expand src/main.rs dispatch --json
justless expand src/main.rs L20-35
```

When you already know the name or range, `expand --json` returns the exact source and a ready-to-use edit selector in one call. Use the revision from the read that informed the edit. Symbol expansion uses the parser’s declaration boundaries. The returned bytes show exactly what a symbol replacement will overwrite. Line ranges include indentation and any terminating newline. Replacements and insertions use the supplied bytes verbatim, including their line endings. For duplicate names use a qualified name or a numbered entry. A revision-bound reference works directly with both `expand` and `edit`.

```sh
justless expand src/main.rs "${rev}#7"
justless edit src/main.rs "${rev}#7" --replacement-file replacement.rs --dry-run
justless edit src/main.rs "${rev}#7" --replacement-file replacement.rs
```

For insertions, use `--before` or `--after`. An empty replacement deletes the selected span. `--dry-run` shows a unified diff using Git. To apply several changes to one snapshot, pass a JSON array on stdin. Ranges need `--revision` or a revision in the selector.

```sh
justless edit config.ini --batch --revision "$rev" <<'JSON'
[
  {"selector":"L2","replacement":"workers=4\n"},
  {"selector":"L8","mode":"after","replacement":"timeout=30\n"}
]
JSON
```

Justless validates the full batch before writing. Stale revisions, ambiguous selectors and overlapping spans fail the entire batch. A project lock serializes Justless writers. Justless rereads the file immediately before the atomic rename and preserves its permissions. Editors that write independently do not acquire the Justless lock.

## Recoverable tool history

`justless mask` reads a complete outbound request from stdin and writes JSON to stdout. It recognizes Anthropic Messages tool results, OpenAI Chat Completions tool messages, Responses API function outputs, and OpenCode v1 tool parts. By default it keeps the newest three results and considers older results of at least 1,024 serialized bytes. Controls include `--keep N`, `--min-bytes N`, `--trigger-bytes N`, repeatable `--pin CALL_ID` and `--keep-tool NAME`. The trigger applies to the combined size of eligible older results. Provider-marked errors, attachments, multimodal results and annotated text blocks stay intact.

A retired result becomes `[justless recall ID]` in the same message and content format. The original value is stored locally in `.justless/observations/` and verified by SHA-256 on recall. The archive can contain sensitive tool output. `justless recall ID` returns original string bytes, while `--json` returns the typed value. `justless mask --restore` expands references across a request. `--stats` writes a JSON change count and serialized byte delta to stderr. Reapplying the filter to a masked request preserves existing references.

For an API client, filter a copy of the full request immediately before sending it. The in-memory conversation can keep its original messages. This example falls back to the original request if the filter fails.

```python
import json
import subprocess

def outbound_request(request, project):
    try:
        result = subprocess.run(
            ["justless", "--root", str(project), "mask"],
            input=json.dumps(request).encode(), capture_output=True,
            check=True, timeout=5,
        )
        return json.loads(result.stdout)
    except (OSError, subprocess.SubprocessError, ValueError):
        return request
```

Only outputs present in the outbound payload can be retired. A provider-managed conversation referenced solely by an ID needs a client integration that supplies its history.

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
| OpenCode v1 | `AGENTS.md` | `.opencode/plugins/justless.js` |

Claude Code's post-read hook can replace a successful whole text read with the smaller outline. Explicit ranges, truncated reads and non-text results pass through. The built-in Read executes before the hook, preserving the read-before-edit workflow. Codex, Claude and Antigravity shell hooks can rewrite a simple single-file `cat` for a large workspace text file. Piped commands and multi-file reads pass through. Agents can invoke `read`, `expand` and `edit` directly in any client. [Claude Code hook output](https://code.claude.com/docs/en/hooks#posttooluse-decision-control)

Install automatic history retirement with `justless init --hooks --agent opencode` in an OpenCode v1 project. Its plugin transforms the outgoing message view before each model request. The session keeps its original results. For Codex, Claude Code and Antigravity, automatic history retirement requires a separate integration at the request boundary. [OpenCode v1 message transform](https://github.com/anomalyco/opencode/blob/v1.2.27/packages/opencode/src/session/prompt.ts#L621-L640)


Open a new session in the trusted workspace. In Codex, review and trust the project hooks with `/hooks`. The session-start hook refreshes the repo index. Before supported shell commands run, the command hook routes them through Justless. Each client handles permissions for the rewritten command. [Codex hooks](https://learn.chatgpt.com/docs/hooks) · [Claude Code hooks](https://code.claude.com/docs/en/hooks) · [Antigravity CLI hooks](https://antigravity.google/docs/hooks?tab=cli)

`justless init` defaults to Codex. Omit `--hooks` to install only the selected client's project instructions and add `.justless/` to `.gitignore`. The `init-codex` alias is also supported.

Run `justless run -- cargo test`, `justless run -- git diff`, or `justless run -- rg -n pattern src`. For interactive commands, live streaming, or pipelines that consume raw machine-readable output, use `justless run --raw -- …` or call the command directly. For shell expressions, pass the shell explicitly, as in `justless run -- sh -c 'cargo test && git diff'`.
