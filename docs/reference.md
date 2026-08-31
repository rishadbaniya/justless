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
| `justless map` | Areas, most connected symbols, communities, cross-area links, entry points and test locations, each with a file and line |
| `justless affected SYMBOL --depth 3` | Dependents grouped by distance, then the tests that reach the target and a command to run them. Also takes a file path or `--changes`. |
| `justless find TEXT` | Literal search across indexed text, with exact file and line matches. Stops at `--limit` matches, 20 by default, and says when more exist. |
| `justless context --changes` | Current changes against `HEAD`, with nearby symbols and tests where available. Includes untracked files. A new file or a range across several symbols names each symbol, then shows each in source order with the members it contains, such as a class's methods. A pure deletion shows where the lines were, their text, any symbol removed whole and the surviving symbol that lost them, without pinning them on the line before. A deleted file is marked deleted. Needs a Git repository. |
| `justless context --changes --staged` | Staged ranges for files that currently match the staged source |
| `justless context --at src/lib.rs:8` | Enclosing symbol, relevant calls, and source around a line or range. A range across several symbols shows each of them. A line past the end of the file is an error. |
| `justless code src/lib.rs --from 1 --to 8` | A numbered excerpt of original source. A range that starts past the end of the file is an error, and one that runs past it ends at the last line with a note. |
| `justless read FILE --outline` | Revision, signatures and line ranges. Small files are returned verbatim when called without outline options. |
| `justless expand FILE SELECTOR` | Exact bytes for a symbol, entry, section or inclusive line range |
| `justless edit FILE SELECTOR --revision REV` | Apply the replacement supplied on stdin after verifying the snapshot |
| `justless mask` / `justless recall ID` | Retire older tool outputs from an outbound JSON request or retrieve the original result |
| `justless pack "What breaks if I rename purchase_orders?"` | A budgeted selection of definitions, graph links, source lines, working-tree changes, and relevant recorded failures |
| `justless history "TimeoutError" --oldest --limit 1` | The matching log line from the earliest completed run among archived commands in this project. Omit the text to list recent runs. |
| `justless resume` | Recent command outcomes, inspected source ranges, and files in the last pack. Stale source is marked. |
| `justless stats --days 7 --json` | Bytes kept out of context with an estimated token count, a daily timeline, savings by command, full-log reopenings, repeated reads and commands passed through by hooks |
| `justless discover --days 30 --json` | Shell commands Claude Code ran in this workspace, split into those routed through Justless, supported commands that were not routed, supported tools in a form the hooks pass through, and unsupported commands, with output sizes |
| `justless mcp` | A stdio MCP server for the read-only commands. `--install CLIENT` adds it to a client's project config. |

## Indexing and languages

Queries refresh the index automatically. Justless compares file identity and high-resolution timestamps where available, hashes files that need closer inspection, reuses parsed facts for unchanged content, and removes deleted files. In a Git checkout it uses tracked and unignored files. Files under version-control and dependency directories such as `node_modules`, `.venv` and `__pycache__` are left out, as are untracked files under `target`. Add patterns to `.justlessignore` for repo-specific exclusions. Files larger than `max_file_bytes` and files or directories Justless cannot read are skipped without stopping the index. `justless index` lists them on stderr, except binary files. A `find` that finds nothing names them, and a `query` that finds nothing names the skipped source files and unreadable paths. A file name that contains a control character, such as a newline, is printed as a quoted string with escapes.

Structured navigation supports Rust, JavaScript, TypeScript and TSX, Python, Go, Java, Kotlin, Scala, C, C++, C#, Swift, Dart, Ruby, PHP, Lua, Zig, Bash, SQL, and HCL, including Terraform. The available symbols and relationships vary by language. Calls across implicit imports, framework discovery, dynamic dispatch, and macro expansion can require source verification. Use `find` for any indexed UTF-8 text file. When source has changed since a recorded run, Justless preserves the original log location and flags the mismatch.

Related tests come from each ecosystem's conventions: `#[test]` functions in Rust, `test_` functions in Python, `Test` functions in Go, `@Test` annotations in Java, Kotlin and Scala, XCTest `test…` methods and `@Test` functions in Swift, and Zig `test` blocks. Test calls such as `test("…")` in Scala, Dart and Lua become test entries named by their quoted description, such as `main::"totals items"`. Zig test blocks are named the same way. Calls between Kotlin, Scala and Java resolve as possible links. Members of a Swift extension or a Scala companion object belong to the extended type, as in `InvoiceService::reset`, and a Dart unnamed constructor is `Type::new`.

A definition's line range includes its decorators, attributes and export keyword, as in `read --outline`.

Rust references to a type include enum variant uses and patterns such as `Status::Open`, associated items such as `Widget::new()`, trait impls, generic bounds, where clauses, `dyn` and `impl` trait uses. Glob imports such as `use super::*` resolve through the items the parent module imports. Definitions that share a qualified name, such as cfg-gated twins or an inherent method and a trait method of the same name, each receive the calls as possible relationships. `query`, `path` and `context --symbol` accept `NAME@FILE:LINE` to select one of them, and the ambiguity error prints that selector for each twin. An ambiguity error lists 12 candidates and counts the rest.

SQL tables and views are linked from the statements that read or write them: `SELECT` and `JOIN` sources, `UPDATE`, `INSERT INTO`, `DELETE FROM`, `ALTER TABLE`, `DROP TABLE` and `CREATE INDEX … ON`. Queries in string literals of the other parsed languages are linked the same way. These links are possible relationships. A schema-qualified table such as `audit.event_log` can also be queried by its short name, `event_log`.

HCL files index top-level blocks by their Terraform address, such as `aws_s3_bucket.logs`, `data.aws_iam_policy_document.read`, `var.region`, `module.vpc` and `output.bucket_arn`. Each `locals` entry is indexed as `local.NAME`. Expressions that use an address become references, so `justless query var.region --view references` lists its uses. References to a block in another file are possible links. An address defined in more than one indexed module is ambiguous, so its references stay unlinked.

## Repository map and change impact

`justless map` orients an agent in a repository from the index alone, without reading source files.

- **Areas** are top-level directories with their file and symbol counts and main languages. A directory that holds more than half of the symbols is split into its subdirectories and files. Each area names its most connected symbol.
- **Hubs** are the symbols with the most distinct dependents (`in`) and dependencies (`out`). Certain links rank first, and a count that includes possible links says how many.
- **Communities** are groups of symbols that link to each other. Label propagation runs over resolved links and each symbol's enclosing definition or file, in a fixed order, so the same index always gives the same groups. Inferred calls are left out, while inferred HTTP routes and SQL relations count. Each group is named after the file that holds most of its symbols and lists its two most connected symbols.
- **Cross-area links** group the links between areas or languages by direction, with a count per relationship and one example. Links between languages come first.
- **Entry points** are `main` functions, HTTP route handlers, and functions with no indexed callers and at least three dependencies, which are often framework or command handlers.
- **Tests** lists the directories with test definitions or test-convention files.

Every row carries a file and line for `code`, `expand` or `query`. Sections take turns within the budget, so a small budget still shows the first rows of each. A section that does not fit is listed by name with its row count.

`justless affected TARGET` walks back from a symbol or a workspace-relative file through calls, references, imports, HTTP routes and SQL relations to the code that depends on it. `--changes` starts from the current Git changes: the innermost definition around each changed range, and any definition whose declaration line changed. Add `--staged` for staged changes. Lines removed without a replacement are handled separately: a symbol removed whole is named from the committed version of the file, and the code that still mentions it becomes the starting point; lines removed inside a surviving symbol change that symbol; anything else is reported as removed with unknown impact rather than attributed to the neighboring line. `context --changes` lists the same deletions. Each dependent appears once, at its shortest distance and through its most certain link, so cycles end. `--depth` sets the number of hops, 3 by default, and the report counts the dependents just past it. A dependent reached through an inferred link is marked possible, as is everything reached through it. A Rust `use` outside any item appears as a file scope, except in the file that defines the symbol.

Tests that reach the target come from the walk and from call chains of up to eight hops, whatever the depth. Test-convention files reached without a detected test definition, such as JavaScript suites, are listed by file. When the layout identifies the runner, the report ends with a command for those tests.

| Language | When | Command |
| --- | --- | --- |
| Rust | A `Cargo.toml` above the test file | `cargo test` filtered by the test's path in its crate. A test in `tests/NAME.rs` adds `--test NAME`, and a nested package adds `--manifest-path`. |
| Python | A function, or a method of a `Test` class, in `test_*.py` or `*_test.py` | `pytest` with the node ID |
| Go | A `Test` function in `*_test.go`, with `go.mod` at the root | `go test ./DIR -run '^(TestA\|TestB)$'` |

A command names at most eight tests. The walk follows the first 256 new dependents of any one symbol, certain links first, and stops after 5,000 dependents. The report says when either cap applies. Within the budget, tests and commands are placed first, then nearer distances.

```sh
justless map --budget 4000
justless affected purchase_orders --depth 2
justless affected src/pricing.rs --json
justless affected --changes
```

With `--json`, both commands return the rows selected for the budget as structured fields, with an `omitted` count per section and a `capped` flag.

## Progressive reads and verified edits

`read`, `expand` and `edit` resolve FILE like a shell path, from the current directory, and fall back to the workspace root when the current directory has no such file. With `--root`, which hooks pass, a bare path is taken from the root first instead. Paths that start with `./` or `../`, and absolute paths inside the workspace, always resolve from the current directory. When a bare path names different files from the two places, stderr shows the spelling for the other one. Output and revisions always use the workspace-relative name. Other commands, including `code` and `context --at`, take workspace-relative paths. Run `justless read FILE --outline` to get a path-bound revision and numbered rows. An entry reference combines the revision and row number, such as `abc…#7`. A Rust row's signature starts at the declaration itself, and so does a TypeScript method's, while the row's line range, `expand` and `edit` still cover the Rust doc comments and attributes or the TypeScript decorators above it. Use `--match TEXT` to search names and signatures, including Rust doc comments and attributes and TypeScript method decorators, `--limit N` to set a page size, and `--after CURSOR` to resume the same search. `--budget` bounds the text outline. `--json` adds structured fields and a `next` cursor. Normal reads return small UTF-8 files verbatim. Outlines use the available language parser, Markdown ATX headings, or 80-line pages. A heading's name leaves out an optional closing run of `#`, so `## Setup ##` is named `Setup`. Rust files are parsed whole, so a file with a syntax error has no symbol rows: its outline names the error and its line, `--json` adds a `parse_error` field, and the outline falls back to pages. Symbol names then fail with the same error, and line ranges still work. Delimiters nested more than 1,024 levels deep, or a run of more than 1,024 consecutive operators, count as a syntax error instead of exhausting the parser's stack. Line-range selectors never invoke the parser.

```sh
justless read src/main.rs --match dispatch --outline
justless expand src/main.rs dispatch --json
justless expand src/main.rs L20-35
```

When you already know the name or range, `expand --json` returns the exact source and a ready-to-use edit selector in one call. Use the revision from the read that informed the edit. Symbol expansion uses the parser’s declaration boundaries, which include the declaration's attributes and decorators, such as a TypeScript method's `@Get()`. The returned bytes show exactly what a symbol replacement will overwrite. Line ranges include indentation and any terminating newline. Replacements and insertions use the supplied bytes verbatim, including their line endings. For duplicate names use a qualified name or a numbered entry. A revision-bound reference works directly with both `expand` and `edit`.

```sh
justless expand src/main.rs "${rev}#7"
justless edit src/main.rs "${rev}#7" --replacement-file replacement.rs --dry-run
justless edit src/main.rs "${rev}#7" --replacement-file replacement.rs
```

For insertions, use `--before` or `--after`. Insertions at a symbol or entry are line-aligned when nothing else shares the symbol's lines. `--before` inserts at the start of its first line, ahead of the indentation, and `--after` inserts after the line ending of its last line, or at the end of a file that has no final line ending. A symbol that shares a line with other code keeps its exact boundary. Supply complete lines with their own indentation. An empty replacement deletes the selected span. `--dry-run` prints a Git patch whose headers name the workspace-relative file, so `git apply` can apply it from the workspace root. To apply several changes to one snapshot, pass a JSON array on stdin. Ranges need `--revision` or a revision in the selector.

```sh
justless edit config.ini --batch --revision "$rev" <<'JSON'
[
  {"selector":"L2","replacement":"workers=4\n"},
  {"selector":"L8","mode":"after","replacement":"timeout=30\n"}
]
JSON
```

Justless validates the full batch before writing. Stale revisions, ambiguous selectors and overlapping spans fail the entire batch. Two insertions at the same point overlap too, such as `after` one symbol and `before` the symbol on the next line. A project lock serializes Justless writers. Justless refuses a file the user cannot write, as a shell write would, and rereads the file immediately before writing it. It normally writes a new copy with the same permissions and renames it into place atomically. When a rename would split hard links, change the owner or group, or drop extended attributes, or the directory cannot take a new file, Justless overwrites the file in place instead. Editors that write independently do not acquire the Justless lock.

## Recoverable tool history

`justless mask` reads a complete outbound request from stdin and writes JSON to stdout. It recognizes Anthropic Messages tool results, OpenAI Chat Completions tool messages, Responses API function outputs, and OpenCode v1 tool parts. By default it keeps the newest three results and considers older results of at least 1,024 serialized bytes. Controls include `--keep N`, `--min-bytes N`, `--trigger-bytes N`, repeatable `--pin CALL_ID` and `--keep-tool NAME`. The trigger applies to the combined size of eligible older results. Provider-marked errors, attachments, multimodal results and annotated text blocks stay intact.

A retired result becomes `[justless recall ID]` in the same message and content format. The original value is stored locally in `.justless/observations/` and verified by SHA-256 on recall. The archive can contain sensitive tool output. `justless recall ID` prints the original text: the bytes of a string result, or the text of each part of an all-text result joined without a separator. `--json` returns the typed value. `justless mask --restore` expands references across a request. `--stats` writes a JSON change count and serialized byte delta to stderr. Reapplying the filter to a masked request preserves existing references. A request can contain unpaired UTF-16 surrogate escapes such as `\ud83d`, which JavaScript and Python write for a string cut inside a character. They are written back exactly as they arrived, and a result that contains one is not retired.

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

`pack` runs locally. It matches identifiers and file paths, ranks passages by how common their terms are in the repository, and follows incoming and outgoing graph edges up to three hops. A definition ranks by how many of the task's words its name covers, after light stemming such as `updated` to `update`, with rarer words counting more. A name the task spells as code, such as `internal_event`, `OrderBook` or a single-word task, ranks first. Modules and files rank below definitions with the same name. It considers up to 96 candidate text files and 25 recent runs. Working-tree changes boost relevant symbols. Inferred links carry a `possible` label. If a source digest differs from the one stored with a failure, the pack marks the old location as stale. A later successful run of the same command supersedes its failure.

`run` groups recognized test failures by their reported diagnostic and summarizes Git patch hunks with file names, hunk headers, and original stdout line ranges. The command still runs every time. For a matching command, directory, session, and preview budget, byte-identical streams and an identical exit status can produce an unchanged notice. Changed test reports and patches show additions and removals against the previous run. A removed failure means it disappeared from the report. Use the exit status to check the overall result. Unrecognized output uses the ordinary preview.

A command you run yourself ends with a footer that names the run, its exit status and how many bytes were shown. Hooks add `--hook`, which prints a footer only when it adds information. When the agent received every byte and the command succeeded, or a search matched nothing, nothing is added. When output was clipped, grouped or collapsed, one line names the run and what was shown, such as `[justless run-7Gq2 · 3.9 KB of 46.6 KB shown]`. A failed command whose output was shown in full ends with `[justless run-7Gq2 · exit 2]`. Every run is archived either way and listed by `history`. `stats` counts the footer as part of what the agent received and reports runs that grew.

Use `justless run --fresh -- COMMAND` for a fresh preview. Use `--session NAME` or `JUSTLESS_SESSION` to choose a comparison session from the terminal. Project hooks scope comparisons to the client's session identifier when supplied. Captured runs remain searchable across sessions. A marker such as `stdout:8-15` expands with `justless log RUN_ID --stream stdout --from 8 --to 15`. A range marked `prior stdout` belongs to the comparison run.

Container and cluster listings use the default table output. `docker ps` and `kubectl get pods`, `deployments`, `statefulsets` and `nodes` report how many rows were listed and name only the rows that need attention, such as unhealthy or exited containers, pods that are not ready or have restarted, and rollouts that are behind. A few healthy names are shown as examples. Each flagged row links to its original line. A failing command, an unrecognized header or a custom output format such as `-o wide` or `--format` keeps the ordinary preview.

JavaScript test runners (`npm`, `pnpm`, `yarn` and `bun test`, `jest`, `vitest run`, `node --test`) drop passing-file lines and keep failures with the lines that follow them. `docker logs` and `kubectl logs` collapse consecutive identical lines. Both report retained, filtered and repeated line counts. Watch, interactive, coverage and machine-readable reporter flags run the command with the ordinary preview.

Compilers and linters report findings grouped by severity and rule across stdout and stderr. Each group shows its severity, rule, count, first message and up to four locations sorted by file and position, with the original log lines of its first occurrence. Errors also show the detail lines of their first occurrence that has any, such as rustc's expected and found types or a TypeScript elaboration. File names may contain spaces, parentheses or a Windows drive letter. rustc lints keep their name when rustc states it only once or under `-D warnings`. `--message-format=short` names no lint, so common rustc lints such as `unused_variables`, `dead_code`, `non_snake_case` and `unreachable_code` are recognized by their wording there, and other findings, including Clippy's, are grouped by message. Recognized output includes rustc and Clippy (`cargo build`, `check` and `clippy`), Ruff, flake8, pylint, mypy, Pyright, ESLint, TypeScript `tsc`, ShellCheck, golangci-lint, `go vet`, staticcheck, RuboCop, SwiftLint and hadolint's default output. pylint's E and F messages count as errors. After the groups come general outcomes: errors without a source location, such as a failed build script or link step, with their last detail lines, the tool's own totals, one combined `could not compile` line that names each failed package and target, and a count of further outcome lines. The header counts errors without a location separately. A stream that produced no finding, such as a crash traceback on stderr, is summarized by its line count, range, first line and last two lines. A rerun compares groups by severity and rule. It lists groups that appeared, changed count, reported different messages at the same count or are no longer reported, and notes when the same findings moved to other lines. A compile error can stop later lint passes, so a rule that is no longer reported is not necessarily fixed. A report that exceeds the preview budget keeps its header, comparison and outcomes, drops error detail lines first and then whole groups with a count. Output with no recognized finding, and machine-readable formats such as `--format json` or `--output-format`, keep the ordinary preview.

Searches and file listings are grouped by file and directory when their stdout exceeds the preview budget. Smaller output stays verbatim, and stderr keeps its own preview. `rg`, `git grep`, `ag`, `ack` and `grep` with `-r` or several files report the total matches and files, then one group per file in order of match count. Each group names its original stdout lines and shows its first matching lines exactly as printed, followed by the number of remaining matches. Files beyond the budget are summarized by directory. Matches inside dependency and build directories, such as `node_modules` or `target`, collapse into one line per directory unless the command names it, and a `Binary file … matches` line counts as a match in its file. Context lines from `-A`, `-B` and `-C` extend a file's line range and are counted separately from matches. `find`, `fd`, `git ls-files`, `tree`, `ls -l`, `ls -R`, `rg --files` and the `-l` file lists of the search tools report exact entry and directory counts, then list each directory with its entry count, its first few names and its original lines. Dependency, build and cache directories such as `node_modules`, `target`, `.git`, `dist`, `build`, `__pycache__` and `.venv` collapse into a count unless the command names them. When the groups do not fit, deeper directories fold into their parents with a count. A rerun lists files or directories whose counts changed. Counts such as `grep -c`, `--json`, NUL-separated output, `--heading`, `find -exec`, `-delete`, `-print0` and `-ls`, and options outside each tool's recognized display set keep the ordinary preview. A search that finds nothing keeps its exit status 1, and task packs do not list search or listing runs as failures. A recursive `grep` or `ack` run through Justless skips `.justless/`, so it never matches archived logs or its own output.

Package runners are recognized, so `npx eslint`, `pnpm exec tsc`, `uv run ruff check`, `poetry run mypy`, `python -m mypy` and `bundle exec rubocop` select the same profile as the tool itself. Hooks also route `make`, `ninja`, `cmake --build`, Gradle, Maven, `dotnet build`/`test` and `swift build`/`test` through the bounded preview.

`history` searches stored streams locally and stops when it reaches the requested match count or byte budget. `history`, `resume`, `stats` and `pack` order runs by the time each command completed. A run record that cannot be read is skipped with a warning that names the run, so the other runs and the compaction handoff stay available. `resume` covers Justless activity, including each recent run's outcome, such as its failing test count or most frequent lint rule, the most recent 32 source ranges and files used in the last pack. They are listed in that order. The runs come first within the budget, and the source ranges and last-pack files share what remains. Up to half of that space is held for the pack, and the ranges may use the rest, so neither list pushes the other out. Source ranges are dropped oldest first. `history` and `resume` name each run by its command label: the program and, for tools with subcommands, the subcommand, such as `git log`, without other arguments. When the output family adds detail, it follows in parentheses, as in `python (pytest)`. Runs recorded by older versions show their family. `stats` counts full-stream reads separately from range, search, and run-context requests. Hook pass-through counts cover shell calls observed by the installed hooks, including each command of a list that a hook leaves whole. They record command families without arguments. Preview-byte totals exclude footer text and subsequent lookups. Token figures are estimates of UTF-8 bytes divided by four, the same convention the benchmark scripts use, and are always shown next to the measured bytes. Provider token counts are measured separately by the benchmark. A high reopen rate for a command family suggests its preview drops evidence agents need.

`discover` reads Claude Code's session transcripts for this workspace from `~/.claude/projects/` or `$CLAUDE_CONFIG_DIR/projects/`, including the transcripts of subagents and workflow subagents, which count toward their parent session. A command counts as routed when a line of its output is a Justless run footer, either the full `[justless run-ID] exit N · …` line or a hook-routed run's bracketed marker such as `[justless run-ID · exit N]`, when it invokes a `justless` binary by any path, or when a bare `cat FILE` returned the outline that the hooks substitute. A command counts as supported only when the Claude Code hook would route it, so a plain `grep` or a list that starts with an absolute `cd` counts as not supported. Supported commands that reached the agent unfiltered usually mean the hooks are missing or the session started before they were installed. A hook-routed command that succeeded and reached the agent verbatim prints no footer, so it also counts as not routed. A supported tool in a form the hooks pass through whole, such as `cargo test 2>&1 | tail`, is counted as passed through (`passed_through` in `--json`). The report prints command labels and byte counts, never full command lines or output. A label is the program name, plus the subcommand for tools that have them, such as `cargo test` or `git log`. Other arguments, such as host or database names, are left out.

## Configuration

The optional `.justless.toml` controls project defaults. The values below are the built-in defaults.

```toml
context_budget = 2400
max_output_bytes = 4096
max_file_bytes = 2097152
```

Add `[[filters]]` entries to omit known noise from a command's preview. `command` is an argument prefix matched against the program name and its leading arguments. The longest matching prefix applies. `omit` is a regular expression tested against each trimmed line. Lines that look like failures, errors, warnings or assertions, and the four lines after them, are always kept. The full stream stays in the log.

```toml
[[filters]]
command = ["make"]
omit = '^make\[\d+\]: (Entering|Leaving) directory'
```

An empty prefix, an invalid expression or one that matches empty text stops the run with an error.

`pack`, `map`, `affected`, `history`, `resume`, `query`, `path`, `find`, `context`, and `code` accept `--budget`. The renderer uses one UTF-8 byte per budget unit as a conservative ceiling for byte-based tokenizers. Agent totals can also include the prompt, history, and tool metadata.

Full output is retained locally under `.justless/runs/`, including output short enough to display in full. The archive keeps the streams separately, with byte-for-byte replay for each. Keep the directory private if a command prints secrets. The CLI runs offline after installation, and `--raw` skips recording.

Reading the index takes no lock, so concurrent queries over unchanged sources run in parallel. Saving a refreshed index, recording a run baseline and applying edits take a project lock under `.justless/`. When another Justless process holds it, Justless says so on stderr after a second and gives up after 30 seconds, or after the number of seconds in `JUSTLESS_LOCK_TIMEOUT`. A query that gives up still answers from the files it read and leaves the saved index unchanged, and a run that gives up records no baseline.

## MCP server

`justless mcp` serves the read-only commands as MCP tools over stdio. Use it with clients that have no Justless hooks, or when an agent should call navigation tools instead of shell commands. Each tool runs the matching command against the project root and returns its output as text. A failed command or an invalid argument comes back as a tool error that the agent can correct. `run`, `edit`, `mask` and `init` are not exposed, so every change still goes through the client's own permission prompts.

The server uses `--root` when given. Otherwise it finds the project the way other commands do, searching upward for `.git`, `.justless` or a project marker. The search starts from `CLAUDE_PROJECT_DIR`, which Claude Code sets to its launch directory, or else from the directory the client starts the server in. A server started in a subdirectory therefore shares the project's state with hooks and commands. The tool list is kept under 8 KB because clients send it with every model request. Arguments are fully validated when a tool is called.

| Tool | Command | Arguments |
| --- | --- | --- |
| `query` | `query SYMBOL` | `symbol`, `view`, `budget` |
| `path` | `path FROM TO` | `from`, `to`, `budget` |
| `find` | `find TEXT` | `text`, `ignore_case`, `limit`, `budget` |
| `context` | `context --at` or `--changes` | `at` or `changes` (with optional `staged`), `budget` |
| `code` | `code FILE` | `file`, `from`, `to`, `budget` |
| `read` | `read FILE` | `file`, `outline`, `match`, `limit`, `after`, `budget` |
| `expand` | `expand FILE SELECTOR` | `file`, `selector`, `revision` |
| `pack` | `pack TASK` | `question`, `budget` |
| `map` | `map` | `budget` |
| `affected` | `affected TARGET` or `--changes` | `target` or `changes`, `depth`, `budget` |
| `log` | `log RUN_ID` | `run_id`, `stream`, `from`, `to` |
| `search` | `search RUN_ID TEXT` | `run_id`, `text`, `stream`, `limit` |
| `history` | `history [TEXT]` | `text`, `oldest`, `limit`, `budget` |
| `recall` | `recall ID` | `id` |
| `resume` | `resume` | `budget` |
| `stats` | `stats` | none |

File arguments must be workspace-relative, and unknown arguments are rejected. The server resolves the project root once at startup. Clients limit the size of a tool result, so the `log` tool returns at most 32 KiB of replayed output per call, with or without a line range. A longer replay ends on a line boundary with a note naming the stream and the `from` line of the next page. A single line longer than a page is cut, and the note gives the shell command that replays it whole. Queries refresh the index under `.justless/`, and log replays are counted in `stats`. Source files are never modified. Run `justless init` or add `/.justless/` to `.gitignore` to keep that local state out of Git.

Add the server to a project's client config from the project root. Select one or more clients. `--install` writes to the project found from the current directory, even when `CLAUDE_PROJECT_DIR` is set, and prints the absolute path of each file.

```sh
justless mcp --install claude --install cursor
justless mcp --install gemini vscode codex
```

| Client | Project file | Server entry |
| --- | --- | --- |
| Claude Code | `.mcp.json` under `mcpServers` | `justless mcp` |
| Cursor | `.cursor/mcp.json` under `mcpServers` | `justless mcp --root ${workspaceFolder}` |
| Gemini CLI | `.gemini/settings.json` under `mcpServers` | `justless mcp` |
| VS Code with Copilot | `.vscode/mcp.json` under `servers` | `justless mcp --root ${workspaceFolder}` |
| Codex | `.codex/config.toml` as `[mcp_servers.justless]` | `justless mcp` |

Setup keeps every other key and server in these files and leaves an existing `justless` entry unchanged. It checks every selected file before writing any of them, so one unreadable file leaves the others untouched, and the error names that file. JSON files containing comments must be edited by hand. Claude Code, Gemini CLI and Codex do not document the directory they start a server in, so their entry relies on it being inside the project. If a client starts it elsewhere, add `--root` with the project path. Codex loads project config only in trusted projects. VS Code plans to remove the Local harness that reads `.vscode/mcp.json` in February 2027. Its Copilot harness reads the root `.mcp.json`, which `--install claude` writes. For other clients, such as Zed or Windsurf, add a stdio server with the command `justless` and the arguments `mcp --root /path/to/project`. [Claude Code](https://code.claude.com/docs/en/mcp) · [Cursor](https://cursor.com/docs/context/mcp) · [Gemini CLI](https://geminicli.com/docs/tools/mcp-server/) · [VS Code](https://code.visualstudio.com/docs/copilot/reference/mcp-configuration) · [Codex](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)

The server implements MCP revision 2026-07-28, where each request declares its protocol version. It also answers the `initialize` handshake from revisions 2024-11-05 through 2025-11-25 for clients that predate it. After a client negotiates 2025-03-26, the only revision with JSON-RPC batches, the server answers a batch array with one array of responses. [MCP specification](https://modelcontextprotocol.io/specification/2026-07-28)

## Install and configure agents

Run the installer from the project where you want hooks. With no arguments it detects installed clients on `PATH`. It finds Cursor through the editor's `cursor` command or the older `cursor-agent` command. The Cursor CLI's `agent` command is too generic to detect, so name `cursor` explicitly if that is the only one installed. You can also choose clients explicitly or install only the CLI. From a Justless checkout you can use `sh /path/to/justless/install.sh` with the same arguments.

```sh
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- claude agy
curl -fsSL https://raw.githubusercontent.com/rishadbaniya/justless/main/install.sh | sh -s -- --cli-only
```

The installer prefers a prebuilt binary. Each [release](https://github.com/rishadbaniya/justless/releases) publishes `justless-VERSION-TARGET.tar.gz` for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin` and `aarch64-apple-darwin`, a `.zip` for `x86_64-pc-windows-msvc`, and a `SHA256SUMS` file. The installer downloads `SHA256SUMS` and the archive for your system with `curl` or `wget`. It checks the archive with `sha256sum`, `shasum` or `openssl`, and confirms that the binary runs before it installs it. If the checksum does not match, it stops without installing anything. The Linux binaries need glibc 2.35 or newer.

The installer builds with Cargo instead when no archive matches the system, the download fails, no checksum tool is available, or the binary does not run here, as on musl-based Linux. Building needs Rust 1.91 or newer and a C compiler. Pass `--from-source` to always build, as in `sh -s -- --from-source claude`. Running `install.sh` from a Justless checkout builds that checkout.

| Variable | Effect |
| --- | --- |
| `JUSTLESS_VERSION` | Install release `X.Y.Z` instead of the latest. A source build uses tag `vX.Y.Z`. |
| `JUSTLESS_RELEASE_BASE_URL` | Download `SHA256SUMS` and archives from this URL instead of the GitHub release, for a mirror or a local `file://` directory. |
| `CARGO_INSTALL_ROOT`, `CARGO_HOME` | Choose the install prefix for both prebuilt and source installs. |

The binary goes in `$CARGO_INSTALL_ROOT/bin`, defaulting to `$CARGO_HOME/bin` or `~/.cargo/bin`. Put that directory on the agent's `PATH`. On Windows, extract `justless.exe` from the release zip to a directory on `PATH`. There the command hooks pass every shell command through unchanged. Once the binary is installed, configure another project from its root with `justless init --hooks --agent codex --agent claude --agent agy`. Select only the clients you use. Running setup again preserves existing instructions, settings, and hooks.

| Client | Instructions | Project hooks |
| --- | --- | --- |
| Codex | `AGENTS.md` | `.codex/hooks.json` |
| Claude Code | `CLAUDE.md` | `.claude/settings.json` |
| Antigravity CLI (`agy`) | `AGENTS.md` | `.agents/hooks.json` |
| OpenCode v1 | `AGENTS.md` | `.opencode/plugins/justless.js` |
| Cursor (`cursor`) | `AGENTS.md` | `.cursor/hooks.json` |
| Gemini CLI (`gemini`) | `GEMINI.md` | `.gemini/settings.json` |
| GitHub Copilot CLI (`copilot`) | `AGENTS.md` | `.github/hooks/justless.json` |

Cursor reads `AGENTS.md` alongside `.cursor/rules`. Gemini CLI reads `GEMINI.md` unless its `context.fileName` setting names other files. Copilot CLI also reads `CLAUDE.md`, `GEMINI.md` and `.github/copilot-instructions.md`. VS Code sessions that use the Copilot harness and Copilot cloud agent sessions load the same `.github/hooks/` file. [Cursor rules](https://cursor.com/docs/rules#agentsmd) · [Gemini CLI context files](https://geminicli.com/docs/cli/gemini-md/) · [Copilot CLI instructions](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-custom-instructions) · [VS Code hooks](https://code.visualstudio.com/docs/agent-customization/hooks)

Claude Code's post-search hook groups a large built-in Grep or Glob result the same way. When the result exceeds the context budget, Grep content is grouped by file, and Grep file lists and Glob results are grouped by directory relative to the session directory. Counts, pagination and Glob's truncation notice are kept. The original text is archived like a retired tool result, and the first line of the grouped result names `justless recall ID` for the exact original. Results without file paths, such as a single-file Grep, count results and small results pass through. Run `justless init --hooks --agent claude` again to add this hook to an existing setup.

Claude Code's post-read hook can replace a successful whole text read with the smaller outline. Explicit ranges, truncated reads and non-text results pass through. The built-in Read executes before the hook, preserving the read-before-edit workflow. Every shell hook can rewrite a simple single-file `cat` for a large workspace text file. Piped commands and multi-file reads pass through. In shell, Python, JavaScript, TypeScript, Ruby, PHP, Lua and SQL files and in Kotlin or Scala scripts, top-level code runs outside any definition, so a file with code that no outline row covers, such as a script's commands or a Python `__main__` block, keeps its full read and its `cat` is not rewritten. Blank lines, comments and docstrings do not count. Agents can invoke `read`, `expand` and `edit` directly in any client. [Claude Code hook output](https://code.claude.com/docs/en/hooks#posttooluse-decision-control)

Compaction replaces earlier tool results with a summary. In Claude Code, the pre-compaction hook asks the summary to keep recent run IDs, exit statuses and inspected file ranges word for word. After compaction, Claude Code and Codex start the next turn with the same bounded handoff as `justless resume`, so the agent can recover exact logs with `justless log` instead of rerunning commands. The handoff runs synchronously, stays within 1,800 bytes including its introduction, and is skipped when the project has no recorded activity. Antigravity CLI and OpenCode do not provide a post-compaction hook. Cursor, Gemini CLI and Copilot only notify hooks before compaction and do not run one afterward. Run `justless init --hooks` again to add these hooks to an existing setup.

Install automatic history retirement with `justless init --hooks --agent opencode` in an OpenCode v1 project. Its plugin transforms the outgoing message view before each model request. The session keeps its original results. For the other clients, automatic history retirement requires a separate integration at the request boundary. [OpenCode v1 message transform](https://github.com/anomalyco/opencode/blob/v1.2.27/packages/opencode/src/session/prompt.ts#L621-L640)


Open a new session in the trusted workspace. In Codex, review and trust the project hooks with `/hooks`. Cursor runs project hooks only in trusted workspaces, and Gemini CLI warns before it runs a new or changed project hook. The session-start hook refreshes the repo index. Before supported shell commands run, the command hook routes them through Justless. Each client handles permissions for the rewritten command.

The command hook routes searches and file listings in their display formats: `rg`, `grep -r` or `grep` over several files, `git grep`, `ag`, `ack`, `find`, `fd`, `tree`, `git ls-files`, and `ls` with `-l` or `-R`. A plain `ls`, single-file `grep`, and machine-readable or side-effect options such as `--json`, `-print0`, `-0`, `--null`, `-z` and `find -delete` pass through. Claude Code's shell replaces `grep` with a bundled ugrep that honors `.gitignore` and skips binary files, so the Claude Code hook leaves `grep` to that shell and routes `rg`, `git grep`, `find` and the listing commands. Commands that change files, such as `find -delete` or `find -exec`, always pass through.

The command hook also rewrites command lists joined by `&&`, `||` and `;`. Each supported command is wrapped in place, and other commands, operators and spacing are kept, so `cargo fmt && cargo test` becomes `cargo fmt && justless run --hook -- cargo test`. A plain `cd DIR` is allowed when `DIR` has no `..`, `~` or leading `/` and, with symlinks resolved, names an existing directory inside the workspace. Any other change of directory, such as a bare `cd`, `cd -P DIR`, `cd -`, `builtin cd`, `pushd`, `popd`, `source` or `eval`, passes the whole list through, so a wrapped command never runs or records its output outside the workspace. Expansions, pipes, redirections, subshells, background jobs, comments and multi-line scripts pass through whole, and so do commands the client starts in the background: Claude Code's `run_in_background`, Gemini CLI's `is_background` and Antigravity's `RunPersistent`.

Codex approves a rewritten command without asking, so its hook rewrites a list only when every command in it is supported and each one runs a local tool with a conventional target. Package runners that may download the tool or install packages first, `npx`, `bunx`, `uvx`, `pnpx`, `pnpm`, `yarn` and `bun dlx`, and `uv`, `hatch`, `pdm` and `rye run`, pass through to Codex's own approval, including behind another prefix such as `python -m`. So do `make` and `ninja` targets other than `all`, `build`, `test`, `check` and `clean`, `cmake --build` with another `--target`, Maven goals other than `clean` and the phases up to `verify`, Gradle tasks other than `build`, `test`, `check`, `assemble` and `clean`, build options outside a short display list, such as `make` variables like `VERBOSE=1`, `make -C DIR`, Maven profiles and Gradle properties given with `-P`, `dotnet` targets, properties and package sources, `cargo --config`, `go test -exec`, `mypy --install-types`, and any argument with a path outside the working tree, including one after an `=`, `:` or `@` inside the argument, such as `make -C /elsewhere`, `pytest --basetemp=/tmp/x` or `pytest --override-ini=log_file=/tmp/x`. Runners of tools the project already installed, `pnpm`, `yarn` and `bun exec`, `bundle exec`, `poetry run` and `pipenv run`, are judged by the tool they run. Cursor's hook also answers a rewrite with `permission: "allow"`, so it applies the same checks. Cursor and Copilot do not document whether they ask again for a rewritten command, so their hooks also rewrite a list only when every command in it is supported. Gemini CLI runs the hook before its policy check and confirmation, so like Claude Code it checks the rewritten list. [Codex hooks](https://learn.chatgpt.com/docs/hooks) · [Claude Code hooks](https://code.claude.com/docs/en/hooks) · [Antigravity CLI hooks](https://antigravity.google/docs/hooks?tab=cli) · [Cursor hooks](https://cursor.com/docs/hooks) · [Gemini CLI hooks](https://geminicli.com/docs/hooks/reference/) · [Copilot hooks](https://docs.github.com/en/copilot/reference/hooks-reference)

Cursor, Gemini CLI and Copilot can block a tool call when its hook fails or exits with status 2, so their handlers answer any error with an empty decision and exit successfully. The Copilot hook command ends in `|| true` because Copilot also loads `.github/hooks/` in cloud agent sessions, where Justless may not be installed. Cursor and Copilot CLI also run Claude Code hooks from `.claude/settings.json` by default. Copilot CLI passes a shell call to those hooks as `Bash`, so in a project configured for both Claude Code and Copilot, the Claude Code handler also runs. The rule that only lists of supported commands are rewritten applies to the Copilot handler alone. A command that already runs through `justless run` passes through unchanged, so it is not wrapped twice. [Cursor third-party hooks](https://cursor.com/docs/reference/third-party-hooks)

`justless init` defaults to Codex. Omit `--hooks` to install only the selected client's project instructions and add `.justless/` to `.gitignore`. The `init-codex` alias is also supported. When an instructions file is a symlink to a file inside the project, such as `CLAUDE.md -> AGENTS.md`, setup edits that file once and keeps the link. A link that leaves the project is skipped with a warning, and the hooks are still installed. Setup checks every file before it creates `.justless/`, so a failed setup leaves the project unchanged.

Run `justless run -- cargo test`, `justless run -- git diff`, or `justless run -- rg -n pattern src`. For interactive commands, live streaming, or pipelines that consume raw machine-readable output, use `justless run --raw -- …` or call the command directly. For shell expressions, pass the shell explicitly, as in `justless run -- sh -c 'cargo test && git diff'`.
