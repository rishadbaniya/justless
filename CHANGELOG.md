# Changelog

User-visible changes to Justless are recorded here. No release has been published yet, so the Unreleased section describes what the `main` branch does today. When a version is tagged, its entries move under that version's heading.

## Unreleased

### Command output

- `justless run -- COMMAND` runs a noninteractive command and keeps its exit status. It shows a bounded preview of stdout and stderr and archives both streams under `.justless/runs/`, up to `max_run_bytes` each. `log` replays any line range byte for byte, `search` finds text in one run, and `history` searches every run in the project.
- Test failures from `cargo test`, `go test` and `pytest` are grouped by their reported diagnostic. Each group links to its lines in the archived log. The JavaScript runners `npm`, `pnpm`, `yarn` and `bun test`, `jest`, `vitest run` and `node --test` drop passing-file lines and keep failures.
- Compiler and linter findings are grouped by rule across both streams, with a count, the first message and up to four locations. Errors keep the detail of their first occurrence. Recognized tools include rustc and Clippy, Ruff, flake8, pylint, mypy, Pyright, ESLint, `tsc`, ShellCheck, golangci-lint, `go vet`, staticcheck, RuboCop, SwiftLint and hadolint. Package runners such as `npx`, `uv run` and `bundle exec` select the same grouping.
- Large search output from `rg`, `git grep`, `ag`, `ack` and recursive `grep` is grouped by file, with match counts and the first matching lines exactly as printed. File listings from `find`, `fd`, `tree`, `git ls-files`, `ls -l` and `ls -R` are grouped by directory. Matches and entries inside dependency and build directories collapse into one line each. Small output stays verbatim.
- Git diffs are summarized by hunk. `docker ps` and `kubectl get` for pods, deployments, statefulsets and nodes count the rows and name the ones that need attention, and `docker logs` and `kubectl logs` collapse repeated lines.
- A rerun of the same command in the same directory and session shows which failures, rules, hunks, files or directories appeared, changed or disappeared. Identical output can collapse to one line.
- Hook-routed runs print a footer only when it adds information: when output was shortened or when the command failed. On 13 routine commands, the earlier footer added 29.1% to the bytes an agent received, and the current one adds 3.7%.
- `[[filters]]` entries in `.justless.toml` omit known noise lines from a command's preview. Lines that look like failures are always kept.
- `run` exits like a shell: 127 for a command that is not found and 126 for one that cannot run. Without a writable state directory the command runs uncaptured after one warning, and once it has started, its output and status come back even if `.justless/` is deleted during the run.
- SIGTERM and SIGHUP sent to Justless reach the command, and an interrupt sent to Justless alone is forwarded after 200 ms. A run whose Justless process was killed, as by a client that gave up on a hook-routed command, stays in `history` as unfinished and `log` replays its output so far, until retention deletes it like any other run.
- A clipped preview lists failures first in `[diagnostics]`, such as a compile error after hundreds of warnings or a Python traceback through its exception, and every excerpt ends on a whole line.
- Each stream of a run stores at most `max_run_bytes`, 256 MiB by default, so a command that reads its own archive, such as `sh -c 'grep -rn x .'`, can no longer fill the disk. The command still runs to completion with its own exit status, and the footer and `log` say where the archive was truncated. On Unix, Justless returns when the command exits, and output that a background process writes later is still stored within the cap. On Windows, `run` waits until such a process closes the output.
- After each run, Justless keeps the newest `max_runs` runs within `max_archive_bytes`, and an unchanged rerun links to the streams of the run it matched instead of storing them again.

- Secrets in command output, such as cloud keys, tokens and private keys, are masked in previews, footers, `log`, `search`, `history`, `lookup --logs` and MCP results. The archive keeps the exact bytes, and `log --unredacted` returns them. Command lines that may carry a credential are not stored.
- `justless exec` runs a script an agent writes over large data, with the same archive and exit status as `run`. Long stdout shows whole head and tail lines and the range left out, and `--intent TEXT` ranks the lines that match it.
- `justless learn` reads recent Claude Code transcripts and proposes project rules from command mistakes agents corrected or repeated, such as an unquoted glob or a tool that is not installed. `--apply` writes them to a managed block in `AGENTS.md` or `CLAUDE.md`. Rules that could carry a credential are held back.

- `justless test --affected` runs only the tests that can reach the current Git changes, with Cargo, pytest, Jest, Vitest or Go, through the same capture as `run`. The result is labeled as a subset in the verdict and in `resume`, `history` and `stats`, and keeps the runner's exit status. It runs the full suite instead, and says why, for lockfiles, build or runner configuration, unindexed or unmentioned files, a changed symbol no test reaches or reaches only through inferred links, or a selection of 60% or more of the tests. `--dry-run` prints the plan, `--why RUN_ID` prints why each test ran, and `justless test` runs the full suite.

- Without `--max-bytes`, each command family's preview budget adapts to the project's own history. It doubles, up to four times `max_output_bytes`, when at least a quarter of five or more clipped runs were reopened in full, and halves, down to half of it, after 20 clipped runs that nobody reopened. Each step is judged only by the runs made since the previous one, and an explicit `--max-bytes` always wins. Set `adaptive_previews = false` in `.justless.toml` to turn it off.

### Code navigation

- A local index covers Rust, JavaScript, TypeScript and TSX, Python, Go, Java, Kotlin, Scala, C, C++, C#, Swift, Dart, Ruby, PHP, Lua, Zig, Bash, SQL and HCL, including Terraform. It refreshes on every query and reuses unchanged files.
- `query`, `path`, `find`, `context` and `code` answer definition, reference, caller, test and dependency questions with a file and line for each result. Links inferred across languages, such as an HTTP route or a SQL table name, are marked as possible.
- `pack` gathers definitions, graph links, source lines, working-tree changes and recent failures for a task within a byte budget.
- `map` summarizes a repository's areas, hubs, communities, cross-area links, entry points and test locations.
- `affected` lists what depends on a symbol, a file or the current Git changes, hop by hop. It names the tests that reach the target and, for Rust, Python and Go layouts, a command that runs them. Deleted code is reported as a deletion rather than as a change to the line next to it.
- `context --run RUN_ID` connects a captured failure to its assertion, its enclosing symbol and the functions it calls.
- `NAME@FILE:LINE` selects one of several definitions that share a name, such as cfg-gated twins.
- Rust paths into another crate of the repository resolve through the `path` its dependency names, directly, through `[workspace.dependencies]` or in a `[target.….dependencies]` table. A registry or renamed dependency that shares a workspace crate's package name, or an import such as `use upstream_ctrlc as ctrlc;`, is no longer linked to the local crate.
- An unreadable or oversized file is skipped rather than failing every query. `index` lists the skipped files, and a query or `find` that finds nothing names them.

### Reads and edits

- `read FILE --outline` returns a revision and numbered signatures with line ranges. `expand` returns the exact bytes of a symbol, Markdown section or line range.
- `edit` replaces or inserts at a symbol or line range only when the file still matches the revision the agent read. A batch of same-file edits is checked as a whole, and a stale, ambiguous or overlapping edit rejects the batch. `--dry-run` shows a diff.
- Without `--root`, `read`, `expand` and `edit` resolve a relative path from the current directory, falling back to the project root. `edit` refuses files the user cannot write and keeps hard links and extended attributes.
- An outline of Rust source with a syntax error names the error and its line.

### Tool history and sessions

- `mask` retires older tool results in an outbound request to `[justless recall ID]` references, and `recall` restores the original value. The OpenCode v1 plugin applies this automatically before each model request.
- Claude Code and Codex hooks carry recent run IDs, outcomes and inspected file ranges across compaction, within 1,800 bytes. `resume` shows the same handoff from the terminal. Runs are named by their command, such as `git status` or `cargo test`.

### Agent setup

- `.justless/` holds its own `.gitignore`, so archived output stays out of `git status` even in a project that never ran `justless init`.
- `install.sh` installs a checksum-verified prebuilt binary when a release exists for the platform, and otherwise builds from source. Run from a project, it sets up every detected agent. It does not set up hooks when run from the home directory or `/`.
- `justless doctor` checks that `justless` on `PATH` is the running version, finds each agent's CLI, and compares the project's hooks and instruction blocks with what `init` writes now, naming missing hooks, changed matchers and older blocks. It also checks MCP entries and `.justless/`, and shows when hooks last routed a command. Each problem comes with a command to run or an edit to make, and `--json` returns the same checks. It only reads, and it exits 1 while something needs fixing.
- `justless init --hooks` configures Claude Code, Codex, Cursor, Gemini CLI, GitHub Copilot CLI, Antigravity CLI and OpenCode. It keeps existing instructions and settings, and running it again is safe.
- Hooks rewrite supported commands, including inside `&&`, `||` and `;` lists, and leave pipes, redirections and machine-readable output alone. The Codex, Cursor, Copilot and Antigravity hooks pass a list through unchanged when it also contains an unsupported command, and every hook passes through a list that changes directory other than with a plain `cd DIR` inside the workspace. The Claude Code hook leaves `grep` to Claude Code's gitignore-aware shell and passes through commands started with `run_in_background`. A command that starts inside a nested Git repository or submodule is rewritten with `--root`, so its run stays in the workspace archive.
- Codex and Cursor approve a rewritten command without asking, so their hooks rewrite only commands that run a local tool directly, with conventional build and test targets, a short list of display options and no argument outside the working tree. Other commands, such as deploy targets and package-runner downloads, pass through to the client's own approval.
- Hooks route Git revisions that contain a tilde, such as `git diff HEAD~3` and `git log main~5..HEAD`. A `~` passes through only where the shell expands it: at the start of a word or after `=` or `:`.
- The instruction block that `init` writes into `CLAUDE.md`, `AGENTS.md` or `GEMINI.md` is 886 bytes between its markers, down from 1,856, because agents load it with every request. It tells them to run supported commands plainly, to wrap one command in `justless run` when long output arrives raw, and to recover archived output with `search`, a ranged `log` or `context --run` instead of rerunning. Run `justless init` again to replace the block in an existing project. When `CLAUDE.md` links to `AGENTS.md`, `init` edits the linked file once.
- Claude Code's built-in Grep and Glob results are grouped when they are large, and a whole-file Read of a large text file can be replaced by its outline. When a script has top-level code outside every outline row, the Read hook lists up to 20 such lines after the rows, and a script with more keeps its full read.
- `justless init --remove` takes Justless back out of a project: its instruction sections, hooks, status line, MCP entries and OpenCode plugin, for every client or those named with `--agent`. A file nobody edited since setup comes back byte for byte, other files lose only the Justless entries, and only directories setup created are removed. `--dry-run` shows each change first, a malformed file that mentions justless stops the removal before anything changes, entries in a form setup does not write are listed for editing by hand, and `--purge` also deletes `.justless/` and its `.gitignore` line. The home directory needs an explicit `--root`.
- `justless mcp` serves the read-only commands as MCP tools over stdio. `justless mcp --install` adds the server to Claude Code, Cursor, Gemini CLI, VS Code or Codex project config. The `log` tool returns at most 32 KiB per call and names the next page.

### Reporting

- `stats` reports bytes kept out of context by day and by command, with an estimated token count, full-log reopenings and runs whose output grew.
- `statusline` prints the current Claude Code session's estimated savings on one line, from hook-routed runs and replaced Read, Grep and Glob results. `init --statusline` adds it to the project's local Claude Code settings, `.claude/settings.local.json`, and never replaces an existing status line.

- `stats` reports bytes kept out of context by day and by command, with an estimated token count, full-log reopenings, runs whose output grew and each command family's preview budget with the reason for it.
- `discover` reads Claude Code transcripts for the workspace and lists shell commands that bypassed Justless, with labels and byte counts only.
