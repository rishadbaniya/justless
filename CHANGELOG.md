# Changelog

User-visible changes to Justless are recorded here. No release has been published yet, so the Unreleased section describes what the `main` branch does today. When a version is tagged, its entries move under that version's heading.

## Unreleased

### Command output

- `justless run -- COMMAND` runs a noninteractive command and keeps its exit status. It shows a bounded preview of stdout and stderr and archives both streams in full under `.justless/runs/`. `log` replays any line range byte for byte, `search` finds text in one run, and `history` searches every run in the project.
- Test failures from `cargo test`, `go test` and `pytest` are grouped by their reported diagnostic. Each group links to its lines in the archived log. The JavaScript runners `npm`, `pnpm`, `yarn` and `bun test`, `jest`, `vitest run` and `node --test` drop passing-file lines and keep failures.
- Compiler and linter findings are grouped by rule across both streams, with a count, the first message and up to four locations. Errors keep the detail of their first occurrence. Recognized tools include rustc and Clippy, Ruff, flake8, pylint, mypy, Pyright, ESLint, `tsc`, ShellCheck, golangci-lint, `go vet`, staticcheck, RuboCop, SwiftLint and hadolint. Package runners such as `npx`, `uv run` and `bundle exec` select the same grouping.
- Large search output from `rg`, `git grep`, `ag`, `ack` and recursive `grep` is grouped by file, with match counts and the first matching lines exactly as printed. File listings from `find`, `fd`, `tree`, `git ls-files`, `ls -l` and `ls -R` are grouped by directory. Matches and entries inside dependency and build directories collapse into one line each. Small output stays verbatim.
- Git diffs are summarized by hunk. `docker ps` and `kubectl get` count the rows and name the ones that need attention, and `docker logs` and `kubectl logs` collapse repeated lines.
- A rerun of the same command in the same directory and session shows which failures, rules, hunks, files or directories appeared, changed or disappeared. Identical output can collapse to one line.
- Hook-routed runs print a footer only when it adds information: when output was shortened or when the command failed. On 13 routine commands, the earlier footer added 29.1% to the bytes an agent received, and the current one adds 3.7%.
- `[[filters]]` entries in `.justless.toml` omit known noise lines from a command's preview. Lines that look like failures are always kept.

### Code navigation

- A local index covers Rust, JavaScript, TypeScript and TSX, Python, Go, Java, Kotlin, Scala, C, C++, C#, Swift, Dart, Ruby, PHP, Lua, Zig, Bash, SQL and HCL, including Terraform. It refreshes on every query and reuses unchanged files.
- `query`, `path`, `find`, `context` and `code` answer definition, reference, caller, test and dependency questions with a file and line for each result. Links inferred across languages, such as an HTTP route or a SQL table name, are marked as possible.
- `pack` gathers definitions, graph links, source lines, working-tree changes and recent failures for a task within a byte budget.
- `map` summarizes a repository's areas, hubs, communities, cross-area links, entry points and test locations.
- `affected` lists what depends on a symbol, a file or the current Git changes, hop by hop. It names the tests that reach the target and, for Rust, Python and Go layouts, a command that runs them. Deleted code is reported as a deletion rather than as a change to the line next to it.
- `context --run RUN_ID` connects a captured failure to its assertion, its enclosing symbol and the functions it calls.

### Reads and edits

- `read FILE --outline` returns a revision and numbered signatures with line ranges. `expand` returns the exact bytes of a symbol, Markdown section or line range.
- `edit` replaces or inserts at a symbol or line range only when the file still matches the revision the agent read. A batch of same-file edits is checked as a whole, and a stale, ambiguous or overlapping edit rejects the batch. `--dry-run` shows a diff.

### Tool history and sessions

- `mask` retires older tool results in an outbound request to `[justless recall ID]` references, and `recall` restores the original value. The OpenCode v1 plugin applies this automatically before each model request.
- Claude Code and Codex hooks carry recent run IDs, outcomes and inspected file ranges across compaction. `resume` shows the same handoff from the terminal.

### Agent setup

- `install.sh` installs a checksum-verified prebuilt binary when a release exists for the platform, and otherwise builds from source. Run from a project, it sets up every detected agent. It does not set up hooks when run from the home directory or `/`.
- `justless init --hooks` configures Claude Code, Codex, Cursor, Gemini CLI, GitHub Copilot CLI, Antigravity CLI and OpenCode. It keeps existing instructions and settings, and running it again is safe.
- Hooks rewrite supported commands inside `&&`, `||` and `;` lists and leave pipes, redirections and machine-readable output alone. The Claude Code hook leaves `grep` to Claude Code's gitignore-aware shell.
- Claude Code's built-in Grep and Glob results are grouped when they are large, and a whole-file Read of a large text file can be replaced by its outline.
- `justless mcp` serves the read-only commands as MCP tools over stdio. `justless mcp --install` adds the server to Claude Code, Cursor, Gemini CLI, VS Code or Codex project config.

### Reporting

- `stats` reports bytes kept out of context by day and by command, with an estimated token count, full-log reopenings and runs whose output grew.
- `discover` reads Claude Code transcripts for the workspace and lists shell commands that bypassed Justless, with labels and byte counts only.
