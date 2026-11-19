# Changelog

Notable changes to Justless, newest release first, in the style of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## 0.1.0 (unreleased)

### Command output

- `justless run -- COMMAND` keeps the exit status, shows a bounded preview and archives both streams; `log` replays any line range byte for byte.
- `search RUN TEXT` finds text in one run and `history TEXT` in every run, failures first, and both count the matches they leave out.
- Test failures from `cargo test`, `go test` and `pytest` are grouped by diagnostic, each linked to its log lines; JavaScript runners drop passing files.
- Compiler and linter findings are grouped by rule with a count and up to four locations, for rustc, Clippy, Ruff, mypy, ESLint, `tsc`, ShellCheck and more.
- Searches are grouped by file and listings by directory, and searches with context lines report blocks: seven such searches went from 54,940 bytes to 16,045.
- Git diffs, `git show` and `git log -p` are summarized by commit and hunk, and a `Cargo.lock` change names the packages added, removed and updated.
- JSON records become tables and other JSON is shown compact, each verified to decode to the original: 25 `docker ps` JSON lines went from 7.3 KB to 1.4 KB.
- `docker ps` and `kubectl get` count rows and name the ones that need attention; `docker logs` and `kubectl logs` collapse repeated lines.
- Previews fold progress frames, long base64 and hex runs and repeated lines and list failures first; a rerun shows what appeared, changed or disappeared.
- Hook-routed runs add a footer only when output was cut or the command failed: on 13 routine commands it adds 3.7% to the output instead of 29.1%.
- Preview budgets adapt to how often each command family's clipped output is replayed, and the archive stays within `max_run_bytes`, `max_runs` and `max_archive_bytes`.
- Secrets such as cloud keys, tokens and private keys are masked in previews, logs, searches and MCP results; `log --unredacted` returns the original.
- `justless exec` runs a script over large data with `run`'s archive and exit status, and `--intent TEXT` ranks the output lines that match.
- `justless test --affected` runs only the tests the current changes reach, with Cargo, pytest, Jest, Vitest or Go, labeled as a subset, or the full suite with a reason.
- `test --compare REF` separates new failures from those already failing at the merge base, and `--base REF` reviews a branch in `test --affected`, `affected` and `context`.

Details: [Run output](docs/reference.md#task-packs-and-run-history) · [Affected tests](docs/reference.md#affected-tests) · [Script runner](docs/reference.md#script-runner) · [Secret redaction](docs/reference.md#secret-redaction)

### Code navigation

- A local index of symbols, calls and imports covers Rust, TypeScript, Python, Go, Java, C and C++, SQL, Terraform, Vue components and more, refreshed on every query.
- `query`, `path`, `find`, `context` and `code` answer definition, caller, test and dependency questions with a file and line, marking cross-language links as possible.
- `lookup QUESTION` ranks the few symbols, Markdown sections and config blocks most likely to answer a question; `--logs` adds recent run output.
- `find --path` and `--glob` narrow a literal search, and a cut page names what cut it and prints the command for the next page.
- `pack "TASK"` gathers the definitions, links, source lines, changes and failures a task needs within a byte budget, and `map` summarizes a repository.
- `affected` lists what depends on a symbol, a file or the current changes, hop by hop, with the tests that reach it; `context --run RUN_ID` ties a failure to its code.
- In a session, `context` and `pack` re-read a symbol as only its changed lines, 512 bytes instead of 2,039 for one changed line; `--fresh` shows it whole.

Details: [Indexing and languages](docs/reference.md#indexing-and-languages) · [Map and change impact](docs/reference.md#repository-map-and-change-impact) · [Ranked lookup](docs/reference.md#ranked-lookup)

### Reads and edits

- `read FILE --outline` returns a revision and numbered signatures, and `expand` returns the exact bytes of a symbol, Markdown section or line range.
- `edit` writes a symbol or line range only if the file still matches the revision the agent read; a stale or overlapping edit rejects the whole batch.
- `edit` rejects a batch that would leave a parsing file unparseable, with the parser's line and message; `--allow-parse-errors` writes it anyway.
- Claude Code's Read hook outlines large whole-file Reads and ranged Reads of more than 150 lines: 167 such ranged Reads went from 2,803,765 bytes to 304,590.
- Shell reads of a wide file with `cat`, `sed -n`, `head`, `tail` or `git show REV:path` show an outline and a command that replays the exact bytes.
- Claude Code's post-edit hook tells the model when a native Edit or Write leaves a file that parsed before unparseable.

Details: [Progressive reads and verified edits](docs/reference.md#progressive-reads-and-verified-edits) · [Shell and Read hooks](docs/reference.md#install-and-configure-agents)

### Tool history and sessions

- `mask` retires older tool results in an outbound request to `[justless recall ID]` references and `recall` restores them; OpenCode v1 does this automatically.
- Opt-in `justless wrap claude` runs Claude Code through a local proxy that asks the API to clear old tool results; `justless proxy` serves any Messages client.
- Claude Code and Codex hooks carry recent run IDs, outcomes and inspected file ranges across compaction within 1,800 bytes, and `resume` prints the same handoff.
- `resume` lists the files the agent read or outlined, through Justless, the Read hook or shell reads, and marks them `[source changed]` after an edit.

Details: [Recoverable tool history](docs/reference.md#recoverable-tool-history) · [Request-boundary context management](docs/reference.md#request-boundary-context-management)

### Agent setup

- `install.sh` builds Justless from source with Cargo and, run in a project, sets up every agent it finds on `PATH`.
- `justless init --hooks` sets up Claude Code, Codex, Cursor, Gemini CLI, Copilot CLI, Antigravity CLI and OpenCode with a short instruction block, and is safe to rerun.
- Hooks route supported commands in `&&`, `||` and `;` lists, after `NAME=value` and before a final `2>&1`; Codex and Cursor, which skip approval, route only local tools.
- The Claude Code hook keeps your permission rules in force by judging the original command, and `doctor` warns about rules such as `Bash(justless run *)`.
- While a `Read`, `Edit` or `Write` deny or ask rule exists, the Claude Code hook leaves file-reading commands such as `rg SECRET .env` as written in every permission mode.
- Hooks that approve rewrites, Codex, Cursor and Antigravity, leave commands with options that run other programs or code, such as `rg --pre`, `rg --hostname-bin` or `go vet -vettool`.
- A `.justless.toml` that fails to load makes every hook leave commands as written instead of routing them into a run that stops on the error.
- `exclude_commands` entries match a command's shell words, whatever its spacing or quoting.
- Claude Code's hooks also compact large output the command hook left alone, such as `cargo test 2>&1 | tail -200`, and large Grep and Glob results.
- Clients without a post-command hook route JSON reads such as `gh … --json` and `kubectl get -o json`: a 90,886-byte `gh api` listing showed 2,789 bytes.
- `justless init --global` adds opt-in user-level hooks for Claude Code and Codex that serve every project, unless one sets `global_hooks = false`.
- `exclude_commands` and `JUSTLESS_DISABLE=1` turn hooks off for some commands or all of them; `justless rewrite "COMMAND"` shows what a hook would do and why.
- `justless doctor` checks the binary, each agent's CLI, hooks, instructions, MCP entries and `.justless/`, and prints a fix for every problem.
- `justless mcp` serves the read-only commands over stdio, and `mcp --install` adds it to Claude Code, Cursor, Gemini CLI, VS Code or Codex.
- `justless init --remove` undoes setup and restores untouched files byte for byte; `--dry-run` shows each change and `--purge` also deletes `.justless/`.
- Removing Codex drops the `AGENTS.md` section that `CLAUDE.md` imports unless Claude Code was set up, and `init --remove --global` keeps your own empty hook arrays.
- `justless learn` turns command mistakes agents corrected or repeated into project rules, and `--apply` writes them to `AGENTS.md` or `CLAUDE.md`.

Details: [Install and configure agents](docs/reference.md#install-and-configure-agents) · [MCP server](docs/reference.md#mcp-server) · [Check the setup](docs/reference.md#check-the-setup) · [Learned command rules](docs/reference.md#learned-command-rules)

### Reporting

- `stats` reports bytes kept out of context by day and by command, net of later replays, and `--sessions` adds the token usage Claude Code recorded.
- Token estimates use bytes-per-token ratios measured on Claude's tokenizer for code, JSON, prose and logs; bytes ÷ 4 counted 56% to 70% of the tokens.
- `stats --all` totals every project on this machine from a local registry, with tokens estimated by content type as `stats` estimates them, and `log`, `search`, `context --run` and MCP `log` pages find and count runs made in another project.
- `justless statusline` shows the session's savings in Claude Code's status line; `init --statusline` adds it without replacing an existing one.
- `discover` lists commands Claude Code and Codex ran outside Justless, as labels and byte counts only, and says when permission rules kept them out.

Details: [Run history and stats](docs/reference.md#task-packs-and-run-history) · [Status line](docs/reference.md#claude-code-status-line) · [Projects on this machine](docs/reference.md#projects-on-this-machine)
