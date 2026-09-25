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
| `justless lookup "where are retries configured?"` | The few symbols, Markdown sections and configuration blocks that best answer a question, each with its line range, name and best-matching lines. `--logs` also ranks recent captured run output. |
| `justless context --changes` | Current changes against `HEAD`, with nearby symbols and tests where available. Includes untracked files. A new file or a range across several symbols names each symbol, then shows each in source order with the members it contains, such as a class's methods. A pure deletion shows where the lines were, their text, any symbol removed whole and the surviving symbol that lost them, without pinning them on the line before. A deleted file is marked deleted. Needs a Git repository. |
| `justless context --changes --staged` | Staged ranges for files that currently match the staged source |
| `justless context --at src/lib.rs:8` | Enclosing symbol, relevant calls, and source around a line or range. A range across several symbols shows each of them. A line past the end of the file is an error. |
| `justless code src/lib.rs --from 1 --to 8` | A numbered excerpt of original source. A range that starts past the end of the file is an error, and one that runs past it ends at the last line with a note. |
| `justless read FILE --outline` | Revision, signatures and line ranges. Small files are returned verbatim when called without outline options. |
| `justless expand FILE SELECTOR` | Exact bytes for a symbol, entry, section or inclusive line range |
| `justless edit FILE SELECTOR --revision REV` | Apply the replacement supplied on stdin after verifying the snapshot |
| `justless mask` / `justless recall ID` | Retire older tool outputs from an outbound JSON request or retrieve the original result |
| `justless wrap claude [ARGS…]` | Claude Code through a local proxy that asks the API to clear old tool results in rare, large steps and archives tool results |
| `justless proxy --port N` | The same proxy for any Anthropic Messages client |
| `justless recall ID` / `justless recall --list TEXT` | The exact original of an archived tool result by its recall or tool_use ID, or archived results whose tool or call matches TEXT. `--from N --to N` and `--grep TEXT` print part of one. |
| `justless pack "What breaks if I rename purchase_orders?"` | A budgeted selection of definitions, graph links, source lines, working-tree changes, and relevant recorded failures |
| `justless history "TimeoutError" --oldest --limit 1` | The matching log line from the earliest completed run among archived commands in this project. Omit the text to list recent runs. |
| `justless resume` | Recent command outcomes, inspected source ranges, and files in the last pack. Stale source is marked. |
| `justless stats --days 7 --json` | Bytes kept out of context with an estimated token count, a daily timeline, savings by command, full-log reopenings, repeated reads and commands passed through by hooks |
| `justless statusline` | One line for Claude Code's status line, such as `justless · ≈41.2k tokens saved this session · 23 runs · 4 reads`, from the session JSON Claude Code passes on stdin. `justless init --statusline` installs it. |
| `justless discover --days 30 --json` | Shell commands Claude Code ran in this workspace, split into those routed through Justless, supported commands that were not routed, supported tools in a form the hooks pass through, and unsupported commands, with output sizes |
| `justless mcp` | A stdio MCP server for the read-only commands. `--install CLIENT` adds it to a client's project config. |
| `justless doctor --json` | Whether this project's setup works: the `justless` on `PATH`, each agent's CLI, hooks and instructions compared with what `init` writes, MCP entries and `.justless/`, with a fix for each problem. Exits 1 when something needs fixing. [Details](#check-the-setup) |
| `justless init --remove --dry-run` | Each file that removing Justless from the project would change, and how. Drop `--dry-run` to make the changes. `--agent NAME` limits them to some clients, and `--purge` also deletes `.justless/`. |

## Indexing and languages

Queries refresh the index automatically. Justless compares file identity and high-resolution timestamps where available, hashes files that need closer inspection, reuses parsed facts for unchanged content, and removes deleted files. In a Git checkout it uses tracked and unignored files. Files under version-control and dependency directories such as `node_modules`, `.venv` and `__pycache__` are left out, as are untracked files under `target`. Add patterns to `.justlessignore` for repo-specific exclusions. Files larger than `max_file_bytes` and files or directories Justless cannot read are skipped without stopping the index. `justless index` lists them on stderr, except binary files. A `find` that finds nothing names them, and a `query` that finds nothing names the skipped source files and unreadable paths. A file name that contains a control character, such as a newline, is printed as a quoted string with escapes.

Structured navigation supports Rust, JavaScript, TypeScript and TSX, Python, Go, Java, Kotlin, Scala, C, C++, C#, Swift, Dart, Ruby, PHP, Lua, Zig, Bash, SQL, and HCL, including Terraform. The available symbols and relationships vary by language. Calls across implicit imports, framework discovery, dynamic dispatch, and macro expansion can require source verification. Use `find` for any indexed UTF-8 text file. When source has changed since a recorded run, Justless preserves the original log location and flags the mismatch.

Related tests come from each ecosystem's conventions: `#[test]` functions in Rust, `test_` functions in Python, `Test` functions in Go, `@Test` annotations in Java, Kotlin and Scala, XCTest `test…` methods and `@Test` functions in Swift, and Zig `test` blocks. Test calls such as `test("…")` in Scala, Dart and Lua become test entries named by their quoted description, such as `main::"totals items"`. Zig test blocks are named the same way. Calls between Kotlin, Scala and Java resolve as possible links. Members of a Swift extension or a Scala companion object belong to the extended type, as in `InvoiceService::reset`, and a Dart unnamed constructor is `Type::new`.

A definition's line range includes its decorators, attributes and export keyword, as in `read --outline`.

Rust references to a type include enum variant uses and patterns such as `Status::Open`, associated items such as `Widget::new()`, trait impls, generic bounds, where clauses, `dyn` and `impl` trait uses. Glob imports such as `use super::*` resolve through the items the parent module imports. A path into another crate of the repository resolves through the `Cargo.toml` dependency that names it, renamed or not, when that dependency has a `path`, directly, through `[workspace.dependencies]` or in a platform-specific `[target.….dependencies]` table. When the workspace root manifest is outside the indexed root, a `name.workspace = true` dependency reaches the one indexed library with that name. Integration tests, examples and binaries reach their own package's library by name. A registry or git dependency stays external even when a workspace crate shares its package name, so a test that compares a crate with its published release links only the calls to the local one. An import such as `use upstream_ctrlc as ctrlc;` shadows the crate it renames over, in its own module and in modules that glob-import it with `use super::*`. Definitions that share a qualified name, such as cfg-gated twins or an inherent method and a trait method of the same name, each receive the calls as possible relationships. A method call whose receiver type is unknown, such as `thread::Builder::new().name(…)`, is a possible call to a method of that name only when it is the one such method in the calling package and the workspace packages it depends on through path dependencies, directly or through others. A method of a test, example, bench, binary or build script counts only for calls in that same target. A method is not inferred to call itself: a call through `self` resolves directly, and the rest mostly forward to another type, as `System.alloc(layout)` does inside `alloc`, though recursion through another value, such as `child.visit()` inside `visit`, is missed too. Within the reachable methods, a name that a standard library type also has, such as `join` or `len`, can still produce a wrong possible call. `query`, `path` and `context --symbol` accept `NAME@FILE:LINE` to select one of them, and the ambiguity error prints that selector for each twin. An ambiguity error lists 12 candidates and counts the rest.

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

`justless mask` reads a complete outbound request from stdin and writes JSON to stdout. It recognizes Anthropic Messages tool results, OpenAI Chat Completions tool messages, Responses API function outputs, and OpenCode v1 tool parts. By default it keeps the newest three results and considers older results of at least 1,024 serialized bytes. Controls include `--keep N`, `--min-bytes N`, `--trigger-bytes N`, repeatable `--pin CALL_ID` and `--keep-tool NAME`. The trigger applies to the combined size of eligible older results. Provider-marked errors, attachments, multimodal results and annotated text blocks stay intact. Requests for Claude models that tie thinking to earlier turns are left unchanged; see [Request-boundary context management](#request-boundary-context-management).

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

`pack` runs locally. It matches identifiers and file paths, ranks passages by how common their terms are in the repository, and follows incoming and outgoing graph edges up to three hops. A definition ranks by how many of the task's words its name covers, after light stemming such as `updated` to `update`, with rarer words counting more. A name the task spells as code, such as `internal_event`, `OrderBook` or a single-word task, ranks first. Modules and files rank below definitions with the same name. It considers up to 96 candidate text files and 25 recent runs, reading files whose name holds a task word first. A passage's terms are weighted by the share of files read that actually contain them. A line matches a task word anywhere, or an inflection of it as a whole word or identifier part, so `hooks` finds `hook-codex` and `set_hook`, and `released` finds `releasing`, but `notes` does not find `not` or `Nothing`. Each passage shows up to three distinct lines among those that match about as well as its best, preferring the ones with the most words, and is headed by its best line. Text outside any definition, such as configuration or documentation, ranks with the definitions: a file that has no definitions counts its file name like a definition name, and matching text adds no more than it adds to a definition. Such a file is about the task words in its name and those that recur on at least three of its lines and one line in sixteen, except words its directory already names. It also leads a definition when it is about a word rarer in the repository than any word the definition's name or matching text covers, unless the task spells that definition's name as code or a recorded failure points at it. So `.codex/hooks.json`, or the same registration in `.codex/settings.json`, leads for "what hooks are configured and running" ahead of tests named for `running`, which most files say, while a long paragraph that mentions many task words once each still follows the code. Working-tree changes boost relevant symbols. Inferred links carry a `possible` label. If a source digest differs from the one stored with a failure, the pack marks the old location as stale. A later successful run of the same command supersedes its failure.

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

Full output is retained locally under `.justless/runs/`, including output short enough to display in full. The archive keeps the streams separately, with byte-for-byte replay for each. Justless creates `.justless/` with its own `.gitignore`, so `git status`, `git add -A` and `context --changes` never pick up archived output. Keep the directory private if a command prints secrets. The CLI runs offline after installation, except for the opt-in `justless proxy` and `justless wrap claude`, which forward your agent's own API requests. `--raw` skips recording.

Reading the index takes no lock, so concurrent queries over unchanged sources run in parallel. Saving a refreshed index, recording a run baseline and applying edits take a project lock under `.justless/`. When another Justless process holds it, Justless says so on stderr after a second and gives up after 30 seconds, or after the number of seconds in `JUSTLESS_LOCK_TIMEOUT`. A query that gives up still answers from the files it read and leaves the saved index unchanged, and a run that gives up records no baseline.

## MCP server

`justless mcp` serves the read-only commands as MCP tools over stdio. Use it with clients that have no Justless hooks, or when an agent should call navigation tools instead of shell commands. Each tool runs the matching command against the project root and returns its output as text. A failed command or an invalid argument comes back as a tool error that the agent can correct. `run`, `edit`, `mask` and `init` are not exposed, so every change still goes through the client's own permission prompts.

The server uses `--root` when given. Otherwise it finds the project the way other commands do, searching upward for `.git`, `.justless` or a project marker. The search starts from `CLAUDE_PROJECT_DIR`, which Claude Code sets to its launch directory, or else from the directory the client starts the server in. A server started in a subdirectory therefore shares the project's state with hooks and commands. The tool list is kept under 8 KB because clients send it with every model request. Arguments are fully validated when a tool is called.

| Tool | Command | Arguments |
| --- | --- | --- |
| `query` | `query SYMBOL` | `symbol`, `view`, `budget` |
| `path` | `path FROM TO` | `from`, `to`, `budget` |
| `find` | `find TEXT` | `text`, `ignore_case`, `limit`, `budget` |
| `lookup` | `lookup QUESTION` | `question`, `limit`, `budget` |
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

File arguments must be workspace-relative, and unknown arguments are rejected. The server resolves the project root once at startup. Clients limit the size of a tool result, so the `log` tool returns at most 32 KiB of replayed output per call, with or without a line range. A longer replay ends on a line boundary with a note naming the stream and the `from` line of the next page. A single line longer than a page is cut, and the note gives the shell command that replays it whole. Queries refresh the index under `.justless/`, and log replays are counted in `stats`. Source files are never modified. The `.justless/` directory holds its own `.gitignore` containing `*`, so Git never lists that local state, even in a checkout or nested repository that does not ignore it.

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

Claude Code's post-read hook can replace a successful whole text read with the smaller outline. Explicit ranges, truncated reads and non-text results pass through. The built-in Read executes before the hook, preserving the read-before-edit workflow. Every shell hook can rewrite a simple single-file `cat` for a large workspace text file. Piped commands and multi-file reads pass through. In shell, Python, JavaScript, TypeScript, Ruby, PHP, Lua and SQL files and in Kotlin or Scala scripts, top-level code runs outside any definition. When up to 20 lines of code are in no outline row, such as a Python `__main__` block, the Read hook lists them verbatim with their line numbers after the rows. A file with more, such as a deploy script made of commands, keeps its full read, and a `cat` is rewritten only when every line of code is in a row. Blank lines, comments, docstrings and statements that only bind modules or names do not count: JavaScript and TypeScript `require` bindings, export lists, re-exports and `module.exports` set to names, and Python `from __future__` imports and `if TYPE_CHECKING:`. Agents can invoke `read`, `expand` and `edit` directly in any client. [Claude Code hook output](https://code.claude.com/docs/en/hooks#posttooluse-decision-control)

Compaction replaces earlier tool results with a summary. In Claude Code, the pre-compaction hook asks the summary to keep recent run IDs with their commands and exit statuses, inspected file ranges and recall IDs word for word, and to note what the runs the task depends on showed. After compaction, Claude Code and Codex start the next turn with the same bounded handoff as `justless resume`. Its introduction points the agent to `justless context --run`, `justless search` and ranged `justless log` replays instead of rerunning commands, since replaying a whole log can cost more than the rerun. The handoff runs synchronously, stays within 1,800 bytes including its introduction, and is skipped when the project has no recorded activity. Antigravity CLI and OpenCode do not provide a post-compaction hook. Cursor, Gemini CLI and Copilot only notify hooks before compaction and do not run one afterward. Run `justless init --hooks` again to add these hooks to an existing setup.

Install automatic history retirement with `justless init --hooks --agent opencode` in an OpenCode v1 project. Its plugin transforms the outgoing message view before each model request. The session keeps its original results. For the other clients, automatic history retirement requires a separate integration at the request boundary. [OpenCode v1 message transform](https://github.com/anomalyco/opencode/blob/v1.2.27/packages/opencode/src/session/prompt.ts#L621-L640)


Open a new session in the trusted workspace. In Codex, review and trust the project hooks with `/hooks`. Cursor runs project hooks only in trusted workspaces, and Gemini CLI warns before it runs a new or changed project hook. The session-start hook refreshes the repo index. Before supported shell commands run, the command hook routes them through Justless. Each client handles permissions for the rewritten command.

The command hook routes searches and file listings in their display formats: `rg`, `grep -r` or `grep` over several files, `git grep`, `ag`, `ack`, `find`, `fd`, `tree`, `git ls-files`, and `ls` with `-l` or `-R`. A plain `ls`, single-file `grep`, and machine-readable or side-effect options such as `--json`, `-print0`, `-0`, `--null`, `-z` and `find -delete` pass through. Claude Code's shell replaces `grep` with a bundled ugrep that honors `.gitignore` and skips binary files, so the Claude Code hook leaves `grep` to that shell and routes `rg`, `git grep`, `find` and the listing commands. Commands that change files, such as `find -delete` or `find -exec`, always pass through.

The command hook also rewrites command lists joined by `&&`, `||` and `;`. Each supported command is wrapped in place, and other commands, operators and spacing are kept, so `cargo fmt && cargo test` becomes `cargo fmt && justless run --hook -- cargo test`. A plain `cd DIR` is allowed when `DIR` has no `..`, `~` or leading `/` and, with symlinks resolved, names an existing directory inside the workspace. Any other change of directory, such as a bare `cd`, `cd -P DIR`, `cd -`, `builtin cd`, `pushd`, `popd`, `source` or `eval`, passes the whole list through, so a wrapped command never runs or records its output outside the workspace. Expansions, pipes, redirections, subshells, background jobs, comments and multi-line scripts pass through whole, and so do commands the client starts in the background: Claude Code's `run_in_background`, Gemini CLI's `is_background` and Antigravity's `RunPersistent`.

Codex approves a rewritten command without asking, so its hook rewrites a list only when every command in it is supported and each one runs a local tool with a conventional target. Package runners that may download the tool or install packages first, `npx`, `bunx`, `uvx`, `pnpx`, `pnpm`, `yarn` and `bun dlx`, and `uv`, `hatch`, `pdm` and `rye run`, pass through to Codex's own approval, including behind another prefix such as `python -m`. So do `make` and `ninja` targets other than `all`, `build`, `test`, `check` and `clean`, `cmake --build` with another `--target`, Maven goals other than `clean` and the phases up to `verify`, Gradle tasks other than `build`, `test`, `check`, `assemble` and `clean`, build options outside a short display list, such as `make` variables like `VERBOSE=1`, `make -C DIR`, Maven profiles and Gradle properties given with `-P`, `dotnet` targets, properties and package sources, `cargo --config`, `go test -exec`, `mypy --install-types`, and any argument with a path outside the working tree, including one after an `=`, `:` or `@` inside the argument, such as `make -C /elsewhere`, `pytest --basetemp=/tmp/x` or `pytest --override-ini=log_file=/tmp/x`. Runners of tools the project already installed, `pnpm`, `yarn` and `bun exec`, `bundle exec`, `poetry run` and `pipenv run`, are judged by the tool they run. Cursor's hook also answers a rewrite with `permission: "allow"`, so it applies the same checks. Cursor and Copilot do not document whether they ask again for a rewritten command, so their hooks also rewrite a list only when every command in it is supported. Gemini CLI runs the hook before its policy check and confirmation, so like Claude Code it checks the rewritten list. [Codex hooks](https://learn.chatgpt.com/docs/hooks) · [Claude Code hooks](https://code.claude.com/docs/en/hooks) · [Antigravity CLI hooks](https://antigravity.google/docs/hooks?tab=cli) · [Cursor hooks](https://cursor.com/docs/hooks) · [Gemini CLI hooks](https://geminicli.com/docs/hooks/reference/) · [Copilot hooks](https://docs.github.com/en/copilot/reference/hooks-reference)

Cursor, Gemini CLI and Copilot can block a tool call when its hook fails or exits with status 2, so their handlers answer any error with an empty decision and exit successfully. The Copilot hook command ends in `|| true` because Copilot also loads `.github/hooks/` in cloud agent sessions, where Justless may not be installed. Cursor and Copilot CLI also run Claude Code hooks from `.claude/settings.json` by default. Copilot CLI passes a shell call to those hooks as `Bash`, so in a project configured for both Claude Code and Copilot, the Claude Code handler also runs. The rule that only lists of supported commands are rewritten applies to the Copilot handler alone. A command that already runs through `justless run` passes through unchanged, so it is not wrapped twice. [Cursor third-party hooks](https://cursor.com/docs/reference/third-party-hooks)

`justless init` defaults to Codex. Omit `--hooks` to install only the selected client's project instructions and add `.justless/` to `.gitignore`. The `init-codex` alias is also supported. When an instructions file is a symlink to a file inside the project, such as `CLAUDE.md -> AGENTS.md`, setup edits that file once and keeps the link. A link that leaves the project is skipped with a warning, and the hooks are still installed. Setup checks every file before it creates `.justless/`, so a failed setup leaves the project unchanged.

Run `justless run -- cargo test`, `justless run -- git diff`, or `justless run -- rg -n pattern src`. For interactive commands, live streaming, or pipelines that consume raw machine-readable output, use `justless run --raw -- …` or call the command directly. For shell expressions, pass the shell explicitly, as in `justless run -- sh -c 'cargo test && git diff'`.

### Claude Code status line

`justless init --statusline` adds `"statusLine": {"type": "command", "command": "justless statusline"}` to the project's `.claude/settings.local.json` and keeps its other settings. Claude Code runs the command after each assistant message and shows a line such as `justless · ≈41.2k tokens saved this session · 23 runs · 4 reads`, or `justless · ready` before anything was saved. The line counts what the Claude Code hooks route and replace, so setup says so when they are not installed; `init --hooks --agent claude --statusline` sets up both. Without `--hooks` or `--agent`, the flag writes no agent instructions or hooks. It is not part of `--hooks`, because a status line takes a row of Claude Code's interface and hides some of its footer hints.

The status line goes in the local file because it is personal: Claude Code applies that file to you in this project only. A `statusLine` in the committed `.claude/settings.json` would replace every teammate's own status line and stay blank where `justless` is not on `PATH`. When setup creates the local file in a Git repository that does not ignore it yet, it adds `/.claude/settings.local.json` to `.gitignore`. To share the line with the team anyway, move the key to `.claude/settings.json`. Delete the key to turn it off. In a Git worktree, Claude Code also reads the local file of the main checkout, and a status line set there wins.

Setup never replaces a status line. When `.claude/settings.local.json`, `.claude/settings.json` or your user settings (`~/.claude/settings.json`, or the one in `CLAUDE_CONFIG_DIR`) already set one, it leaves every file unchanged and prints a command that shows both lines from the same session data, with your command on its own line between the braces:

```sh
input=$(cat); printf '%s' "$input" | {
YOUR_COMMAND
}; printf '%s' "$input" | justless statusline
```

For a status line in the local file it suggests that command for the same file. For one in the shared or user settings it suggests a whole `statusLine` for the local file, with your `padding`, `refreshInterval` and other fields kept, so teammates and other projects keep only their own line. Setup reads every settings file before it writes anything. It stops with an error when one of them is not valid JSON, since that file might set a status line, and when the project's settings file is your user settings file, as in the home directory.

The line counts runs that the hooks routed through `justless run` and that finished after the session began, and the Read, Grep and Glob results that the hooks replaced for this session ID, which they log in `.justless/sessions/`. It reads the projects around the session's current and launch directories, and nested repositories where the session recorded runs or replacements, which are listed for the session in the launch project, so the total stays when the session leaves one. Saved bytes are the raw output minus the preview, footer or outline the agent received, and tokens are estimated as bytes ÷ 4, as in `stats`. The session begins at the first timestamp in its transcript, else when the transcript was created, else `cost.total_duration_ms` before now. Run records do not name their session, so two sessions open in one project at the same time count each other's runs, and a resumed session also counts runs made in the project while it was closed.

The command only reads. It shortens the line to fit `COLUMNS`, and it exits 0 with a line for any input, printing `justless` alone when it cannot open the project. It checks every run record in those projects on each refresh, so it slows as the archive grows. With a release build on an Apple M4 Pro, the median was 20 ms with 600 records, 300 of them in the session, 45 ms with 5,000 records, 1,000 of them in the session, and 53 ms with 20,000 older records. About 10 ms of each is process start. [Claude Code status line](https://code.claude.com/docs/en/statusline) · [Settings files and precedence](https://code.claude.com/docs/en/settings)

## Ranked lookup

`justless lookup QUESTION` lists the few places most likely to answer a question about the repository, such as where a setting is configured or which function handles a case. Each result names a file and line range, the symbol, section or block it covers, and its best-matching lines. Use `find` when you know the exact text and `query` when you know the symbol.

```sh
justless lookup "where are retries configured?"
justless lookup RetryPolicy --limit 3
justless lookup "rollback steps" --json --budget 4000
justless lookup "why did the upload fail" --logs
```

Quote the question, because `?` and `*` are wildcards in most shells. A question that starts with `-` needs `--` before it, as in `justless lookup -- "-v flag handling"`.

On the `fixtures/lookup` sample project:

```text
$ justless lookup "where are retries configured?"
src/net/retry.rs:5-9 struct RetryPolicy
  5 | /// How many times a failed download is retried and how long to wait between attempts.
  6 | pub struct RetryPolicy {
web/client.ts:12-14 method DownloadQueue::retry
  12 | retry(url: string): void {
src/net/retry.rs:1-3 file retry
  1 | //! Retry policy for outbound HTTP calls.
docs/operations.md:9-13 section Retries
  9 | ## Retries
  11 | Downloads are retried five times with exponential backoff.
config/app.toml:5-7 text retry
  5 | [retry]
scripts/fetch.py:5-12 function download
  6 | """Download a file, retrying on failure."""
tests/test_fetch.py:4-8 test test_download_retries
  4 | def test_download_retries(monkeypatch):
7 of 9 ranked · matched retries · no match configured
Next: justless expand src/net/retry.rs L5-9
```

Code is split along the symbols the index finds, so each function, method, class or struct is one result, and lines outside any symbol form file sections. A symbol's range includes the doc comments and decorators directly above it. Markdown is split at headings. Configuration and other text is split at `[section]` headers and every 60 lines. Lockfiles, CSV, minified files and other data files are matched only by name, path and first line, and rank low. Use `find` for their contents.

Words match across naming styles and word forms. `parseHttpHeader`, `parse_http_header` and `parse-http-header` share the words parse, http and header, and the first two also match as one identifier. "retries", "retrying" and "retry" match each other. A match in a symbol's name counts most, then its signature, then its doc comments or the section's parent headings, then its body. Results that match more of the question's words rank higher. Under each result, the first line shown has the rarest question words the header does not already show, and the second adds words the first lacks. Lines longer than 160 bytes are clipped around the words they show. Tests and data files rank lower unless the question mentions tests. Common words such as "where", "how" and "the" are ignored. Text written without spaces, such as Chinese or Japanese, matches only whole runs, so use `find` for it.

At most two results come from one file. Files with identical contents give one result, and its header names the other files after `same in`, which JSON lists as `same`. `--limit` sets the number of results, 8 by default, and `--budget` caps the output. A result that does not fit is left out whole and counted, and `[context budget reached]` follows the counts. The top result always comes first: at a budget too small for it, the word lists shorten and then the closing command is dropped, and a top result too large on its own keeps just its header line. The last lines say how many sections matched, which of your words matched and which did not, and give a command for the top result. That command is `expand` when the whole result fits the budget, and otherwise a 40-line `code` excerpt around its best lines. `--json` returns the same results with scores and line excerpts, and `omitted` counts the results the budget left out. At very small budgets it also shortens the echoed question and word lists, and `words_omitted` counts the words left out.

`--logs` also ranks 40-line windows of output from the 200 most recent runs captured by `justless run`. Log windows are ranked apart from source, with their own statistics. Up to three of them come first and take no places from source results, and the closing command replays the top one with `justless log`, narrowed to the lines shown when the whole window would not fit the budget. A log result names its run, stream, lines and exit status. The MCP `lookup` tool does not rank logs. Use `history` there.

The lookup index is kept in `.justless/lookup/`. It uses the same files, ignore rules, size limit and change detection as the repository index, and parses only files that changed since the last lookup. Edits go to a small second segment that is folded into the main one once it grows. The first lookup in a large repository builds the index and says so on stderr. If the index cannot be saved, lookup still answers and says so on stderr. A damaged index is rebuilt. A file its parser cannot handle is split like plain text, with a note on stderr. Deleting `.justless/lookup/` is always safe.

## Request-boundary context management

`justless proxy` sits between an agent and the Claude API on 127.0.0.1. It adds one request parameter that asks the API to replace old tool results with a short placeholder before the model reads the prompt. It also archives every tool result it relays and records what the API cleared. It never changes the system prompt, the tools or the messages. On Claude Opus 5.5, Sonnet 5.5, Haiku 5.5, and Fable and Mythos 5.1 and later, each thinking block is tied to the exact conversation before it, so a conversation that went through the proxy stays valid when the proxy is stopped, restarted or skipped. The request still carries every old result. What shrinks is what the model reads and what is billed and cached.

### Start

```sh
justless wrap claude                      # Claude Code through a proxy on a free port
justless wrap --keep 20 claude --resume   # proxy options before `claude`, Claude Code's after it
justless proxy --port 8787                # standalone, for any Anthropic Messages client
```

`wrap` starts the proxy, launches `claude` from `PATH` with `ANTHROPIC_BASE_URL` pointing at it, and prints one line before the session and one after. Every argument after `claude` reaches Claude Code unchanged, `--` and `--root` included. `justless proxy` prints its address and an `export` line for Claude Code that also sets the two variables `wrap` sets (see [Claude Code](#claude-code)), and stops on Ctrl-C. Both forward to `$ANTHROPIC_BASE_URL` when it is set, else to `https://api.anthropic.com`. `--upstream URL` overrides that for `proxy`, and a path prefix such as `https://gateway.example/anthropic` is kept. The archive and logs go to the project's `.justless/`, so run `recall` from another directory with `--root`.

### What a request carries

A conversation is identified by its first tool call. Once its boundary covers at least four calls, every request in that conversation carries a clearing edit, after any edit the client sent:

```json
"context_management": {"edits": [
  {"type": "clear_thinking_20251015", "keep": "all"},
  {"type": "clear_tool_uses_20250919",
   "trigger": {"type": "tool_uses", "value": 61},
   "keep": {"type": "tool_uses", "value": 12},
   "clear_at_least": {"type": "input_tokens", "value": 20000}}
]}
```

This request has 64 tool calls and a boundary after call 52. `keep` counts the calls after the boundary and `trigger` sits three below the total, so both grow as calls are appended and the same 52 results stay cleared until the boundary moves. `exclude_tools` and `clear_tool_inputs` are never sent, so every call stays visible. The `anthropic-beta` header gains `context-management-2025-06-27` unless a value already names it. Every other byte of the body and every other header value is forwarded as sent.

The main conversation, subagents and workflows can move the boundary. Token counts, compaction requests, side requests and forks reuse it without moving it, so they read the same cached prefix. Claude Code labels its requests when `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` is set, and unlabeled requests count as the main conversation.

Requests pass through unchanged when the model is not a Claude model or is a Claude 3 model, when they use a message thread or the `compaction` parameter, and when the client sends its own tool-result clearing or compaction edit. A conversation the client clears itself is left to the client from then on.

### When old results are cleared

Nothing is cleared while the estimated prompt is under `--floor` (100,000 tokens), and a boundary never covers fewer than four calls. The newest `--keep` calls (12) are never cleared. A move must newly clear at least `--clear-at-least` estimated tokens (20,000), which is also sent to the API as `clear_at_least`.

A move changes the prompt early in the messages. The prompt cache looks back at most 20 blocks from each breakpoint, so after a move the whole messages layer is written to the cache again. The proxy moves the boundary in two cases:

- **The cache has expired.** The request writes everything again anyway, and clearing only shrinks that write. The prompt cache is kept per model, but the boundary is shared by the whole conversation, so the cache counts as expired only when every model that ran in the conversation has been idle for longer than the TTL plus 15 seconds, or for `--cold-after` seconds. The TTL is 5 minutes, or 1 hour when a `cache_control` asks for it. This is the common case.
- **The move pays back on a warm cache.** The extra cache write must be repaid by cheaper cache reads within `--payback` requests (12). On Opus and Sonnet 5.5, that needs the newly cleared results to be at least 2/3 of the messages layer. When several models ran within the window, the one with the cheapest cache reads sets the bar, so switching models with `/model`, or a fallback request, never rewrites the other model's warm cache for less.

A conversation the proxy sees for the first time, such as a session started without it and resumed under `wrap`, counts as warm: wrongly treating a live cache as expired would rewrite it for nothing, while the other mistake only postpones a clear. Token counts never change this.

| Models | 5-minute cache | 1-hour cache |
| --- | --- | --- |
| Fable and Mythos 5.1 and 5.2 | 80% | 87% |
| Opus and Sonnet 5.5 | 67% | 76% |
| Others, Haiku 5.5 included | 49% | 61% |

For example, Opus 5.5 costs $4 per million input tokens, $5 per million written to a 5-minute cache and $0.20 per million read from it. Take a conversation with 64 calls and 250K tokens of messages, 168K of them in the results of calls 1 to 52. Rewriting the remaining 82K costs about $0.39 once, and each later request then reads 168K fewer cached tokens, $0.034 less. That repays the move after about 12 requests, so the boundary moves while the cache is warm. Later, at 90 calls, clearing calls 53 to 78 would remove 60K of 170K tokens, too little for a warm move. After a 20-minute pause the cache has expired, and the boundary moves for free.

With an API key, Claude Code's conversation cache lasts 5 minutes, so clears follow pauses longer than that. With a Claude subscription it lasts an hour, so clears are rare, and Claude Code's own compaction after an hour idle often comes first. Clearing mainly buys room in the context window. Anthropic measured that clearing a few calls on every request, the API's default shape, cost more than it saved, which is why the proxy clears in rare, large steps.

### Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--keep N` | 12 | The newest N tool calls are never cleared. |
| `--floor TOKENS` | 100000 | No clearing while the estimated prompt is smaller. |
| `--clear-at-least TOKENS` | 20000 | Minimum estimated tokens one move clears. Also sent to the API. |
| `--payback N` | 12 | A move on a warm cache must repay its cache write within N requests. 0 turns warm moves off. |
| `--cold-after SECONDS` | Cache TTL + 15 | Idle time after which the cache counts as expired. 0 turns these moves off. |
| `--no-clear` | | Archive and record only. |
| `--no-archive` | | Do not archive tool results. |

Token figures are estimates of UTF-8 bytes divided by four, with 1,600 tokens per image. Boundaries and idle times are kept in `.justless/proxy/conversations.json`, so a restarted proxy keeps clearing the same calls.

### Recover a cleared result

```console
$ justless recall --list cargo
toolu_01HxV9mTq3K8c2YdL5aZ7wPe  Bash  cargo test --locked  48.2 KB  14m ago
toolu_01Qa2nB7rT4uX9kLm3cV8sDf  Bash  cargo build  22.1 KB  31m ago  error
2 of 418 archived tool results match "cargo". Print one with `justless recall --tool-use ID`.
$ justless recall --tool-use toolu_01HxV9mTq3K8c2YdL5aZ7wPe
```

`--list` matches TEXT against the tool name, the call summary in each row and the call's input, ignoring case, so a Grep pattern, a glob or a search query finds its call. Summaries read `PATTERN in PATH` for Grep and Glob and `FILE:OFFSET+LIMIT` for a partial Read. It lists the newest first, 20 by default (`--limit N`). `justless recall ID` also accepts a tool_use ID, and so does the MCP `recall` tool. It prints text exactly as the tool returned it and notes other blocks, such as images. With `--json` it prints the original content value. `--from N` and `--to N` print the exact bytes of a line range, and `--grep TEXT` prints the numbered lines that contain TEXT, up to `--limit`, so a cleared log does not have to come back whole. The first copy the proxy saw is kept, and only results from requests the API accepted are archived, so a request rejected for its credentials adds nothing. The index is in `.justless/tool-uses/` and the content in `.justless/observations/`. Archived results can contain secrets from tool output, and each index entry keeps up to 512 bytes of the call's input, such as a command or the text an edit wrote.

### Failures

A body the proxy cannot parse or plan is forwarded unchanged, and so is a body over 64 MiB, streamed with its declared length. A body that ends before its `Content-Length`, or a malformed chunked body, is answered with a 400 and never forwarded. `--upstream` and `ANTHROPIC_BASE_URL` cannot carry credentials in the URL, because the proxy prints and logs it. When the API rejects the clearing edit itself with a 400 or 422 that names it, the proxy sends the original request once more, relays that response, and leaves the edit off for that model until it restarts. Every other response, errors included, is relayed unchanged and never retried. An unreachable upstream returns a 502 with an error in the API's format. Compressed responses are relayed but not counted in `stats`.

### Credentials and local files

Header values are forwarded byte for byte and never stored. `.justless/proxy/` holds `conversations.json`, a ledger with one line per request in `requests-YYYY-MM.jsonl`, and `proxy.log` with starts, stops, fallbacks and errors. The ledger records the model, status, decision and the API's usage figures. A request that could have cleared more records why the boundary held: `below-floor`, `below-clear-at-least` or `below-payback`. `justless proxy --verbose` prints the same decision per request, with the payback share reached and the share needed. None of these files holds request content or header values. The only identifiers kept are Claude Code's session, agent and request-class labels. The proxy listens on 127.0.0.1 only. Requests with an `Origin` header, which browsers send, are relayed but never planned or archived. Another user of the same machine with their own API key could still add results to the archive through the port, so use `--no-archive` on shared hosts.

### Claude Code

`wrap` works with an API key, an auth token or a claude.ai login, and the account's usual limits apply. It sets `CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING=1` and `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` unless they are already set, because Claude Code turns both off behind a custom base URL. With them on, a session sends the same Messages requests with or without the proxy, so it can be resumed either way. Remote Control is not available behind a custom base URL, and neither is the cache keepalive Claude Code 2.1.327 sends for some subscribers while a session is idle, so a 1-hour cache can expire sooner under `wrap`. When `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` is set, `wrap` adds `127.0.0.1` and `localhost` to `NO_PROXY` for Claude Code, so its requests reach the local proxy directly.

`wrap` refuses to start when `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`, `CLAUDE_CODE_USE_MANTLE` or `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST` is set, inside a session that already runs through the proxy, and on Windows, where `justless proxy` with `ANTHROPIC_BASE_URL` works instead. It warns when `~/.claude/settings.json`, `.claude/settings.json` or `.claude/settings.local.json` sets `ANTHROPIC_BASE_URL` or a `CLAUDE_CODE_USE_*` variable, because Claude Code then bypasses the proxy. If no request reaches the proxy, the closing line says so. With `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` set, `wrap` archives and records without clearing. When Claude Code clears tool results in a conversation itself, the proxy leaves that conversation alone and the closing line counts it.

Ctrl-C reaches Claude Code, and `wrap` keeps the proxy running until Claude Code exits, then exits with its status. Stopping only the `wrap` process, for example with `kill`, leaves Claude Code without its proxy.

### Codex

Codex is not supported. It uses the OpenAI Responses API, including a WebSocket transport, and manages its own context. That API has no equivalent request parameter for the proxy to add.

### Thinking and history edits

`justless mask` and the OpenCode plugin rewrite earlier tool results. On Claude models that tie thinking to the conversation, that invalidates every later thinking block, so they now return those requests byte for byte:

| Request | Model | `mask` |
| --- | --- | --- |
| Messages | Claude 3 and 4 models, Opus, Sonnet, Fable and Mythos 5, Mythos Preview | Retires results as before |
| Messages | Every other model, including Opus, Sonnet and Haiku 5.5, Fable and Mythos 5.1 and later, and unknown names | Unchanged |
| Chat Completions, Responses or OpenCode | A Claude model | The same rule as Messages |
| Chat Completions, Responses or OpenCode | Another model | Retires results as before |
| OpenCode | None given with `--model` | Unchanged |
| Messages, Chat Completions or Responses | None | Retires results, unless the request carries thinking or reasoning items |

The model is `--model ID` when given, else the request's `model`. Provider prefixes and suffixes such as `openrouter/anthropic/`, `us.anthropic.`, `databricks-`, `anthropic:`, `@20251101`, `-v1:0` and `[1m]` are ignored, and any other name that mentions Claude counts as a Claude model. `--stats` reports why a request was held, and `--restore` always expands references. The OpenCode plugin passes the session's selected model. Run `justless init --hooks --agent opencode` again to update an installed plugin, which leaves every request unchanged until then.

### Stats

`justless stats` adds a request-boundary section after its other output:

```text
Request boundary (justless proxy), last 7 day(s)
  1,204 requests · clearing requested on 388 · 2 conversations left to Claude Code's own clearing · 0 refusals
  412 old tool results marked for clearing (6.1 MB, about 1.5M tokens estimated) in 4 clears: 3 on an expired cache, 1 on a warm cache (81.2K tokens written to cache on that request, reported by the API)
  2.91M input tokens not sent to the model, summed over requests as reported by the API
  7 reads repeated a cleared call with no file change in between
  2,113 tool results archived (31.0 MB). `justless recall --list TEXT` finds one.
```

Results marked for clearing, their bytes and their estimated tokens are the proxy's plan, counted once per result when its boundary moved. Input tokens not sent are the API's own figures, summed over every request that carried the edit, and a request where the API cleared a different number of calls than planned is listed separately, here and in the closing line of `wrap`. A repeated read is a Read, Grep, Glob, WebFetch, WebSearch or NotebookRead call with the same input as a cleared one and no edit of the file, or for searches no edit at all, in between: a sign that the model needed that result back. Test and build reruns are not counted. `--json` adds a `proxy` object with the same counts, the API's usage totals, moves split into `cold` and `payback`, `mismatches` where the API cleared a different number of calls than planned, and `unobserved` compressed responses. It is `null` when the proxy never ran in the project.

## Check the setup

`justless doctor` checks the project it runs in, or the one `--root` names. Each line starts with `✓` for a pass, `!` for something worth a look, `✗` for something that needs fixing, or `·` for information. Every `✗` line names its fix, and a `!` line does when there is something to change: a command to run from the same directory, or an edit to make by hand. The report ends with the fixes for every `✗`: commands first, then the `init` runs combined into one command, then the edits by hand. Doctor plans each `init` it prints the way `init` does, so an agent whose files setup cannot read, such as a hook file with comments, is left out of the command and gets its own edit by hand. Doctor exits 1 when any `✗` remains. `--json` returns the same checks with `status`, `message` and `fix`, plus the counts behind them.

- **Binary.** The running version and path, and whether `justless` on `PATH` is the same file. Hooks and MCP entries call `justless` by name, so a missing one, or a copy with another version, is a `✗`. Shadowed copies are listed, not removed. An agent starts hooks with its own `PATH`, which can differ from your shell's.
- **Agents.** Each client's CLI on `PATH`, found by the command names the installer looks for. Doctor does not run them, so unlike the installer it cannot tell whether OpenCode is v1, which the plugin needs, and it leaves OpenCode out of the `init` command it suggests. A client with Justless hooks, or with a Justless section in `CLAUDE.md`, `GEMINI.md` or `AGENTS.md` and no hooks, has its hook file compared with what `init --hooks` writes now. Running `init` again adds a missing hook, such as a compaction hook added after setup, and replaces an older instruction block. Setup keeps an existing entry as it is, so a changed matcher, a Copilot command without `|| true` or another client's handler is fixed by hand, and doctor names the file and key. A changed timeout is a warning, and so is a changed matcher that still selects what setup's matcher selects, such as `Bash` for `^Bash$`. A hook that calls justless by a path, such as `/opt/bin/justless hook-claude before-command`, counts as present. Setup looks only for the bare `justless` command and would add a second copy, so for that client doctor suggests `init` without `--hooks` and edits by hand. When `CLAUDE.md` imports a file with a line such as `@AGENTS.md`, the imported file's section counts.
- **MCP.** Each client config that lists a `justless` server, and whether its command and arguments start the server.
- **State.** Whether `.justless/` is a private directory with its own `.gitignore`, whose directories accept new files and whose lock files open for writing, the index size and when it was saved, the number of runs, how many of the newest 500 hooks routed and when the last one ran, the size of archived runs and retired tool results, and whether `.justless.toml` loads. Runs do not record which client routed them, so a client with hooks gets a "has not routed a command" note only while no hook has routed or passed through any command.

Doctor only reads. It does not refresh the index, take a lock, create `.justless/` or start an agent's CLI. It runs another `justless` only to ask its version. Outside a project, where neither the directory nor a parent has `.git`, `.justless/` or a project file such as `Cargo.toml` or `package.json`, or in the home directory, it checks the binary and the agents on `PATH` and prints the `init` command for the clients it found.

## Remove Justless from a project

`justless init --remove` undoes `init` and `mcp --install`. Without `--agent` it covers every client: the section between the Justless markers in `CLAUDE.md`, `AGENTS.md` and `GEMINI.md`, each hook handler that runs `justless hook-…`, a Claude Code `statusLine` that runs `justless`, the `justless` server in each MCP config and the OpenCode plugin. A handler matches however its command spells the program, such as a quoted path, a leading `NAME=value` or a `--root` option. Every other instruction, hook, server and key stays. A file left with nothing else is deleted, along with a directory such as `.github/hooks/` that setup created and that is now empty. A linked instructions file is handled as setup handles it.

Setup records each file it writes, as it was before, in `.justless/setup.json`, with the directories it creates and the clients each `init` sets up. A file that still holds exactly what Justless wrote comes back byte for byte, including a layout, final newline or empty array that setup changed. Any other file, such as one edited since or one in a fresh clone, loses only the Justless entries, and the containers they leave empty, with the rest of its bytes unchanged. Setup writes JSON in a two-space layout, so such a file keeps that layout. Without a record, a client directory that only held deleted files is removed too.

Every file is read and checked first. A config file that never mentions justless is left alone, even when it does not parse, such as a `.vscode/mcp.json` with comments. One that does mention it but does not parse, or an instructions file with incomplete or duplicate markers, stops the removal before anything changes, and the error names the file. Fix it or delete its justless entries by hand, or leave its client out with `--agent`. A hook, status line or server that mentions justless in a form setup does not write, such as a wrapper script, stays, and the output lists it by file and JSON pointer to edit by hand. If a change fails partway, the output lists what was already removed, and running the command again finishes the rest. `--dry-run` prints each change and makes none. A second run reports nothing to remove. `.justless/` and the `/.justless/` line in `.gitignore` stay unless you add `--purge`, which deletes the archived runs, index and history.

`--agent NAME`, repeatable, removes only those clients' hooks, MCP entries and instructions. `--agent copilot` also covers VS Code's `.vscode/mcp.json`. A shared file such as `AGENTS.md` keeps its section while another client that reads it was set up by `init` or still has Justless hooks, its plugin or an MCP entry, and the output says which. `--purge` cannot be combined with `--agent`. In the home directory, or a project whose `.claude` or `.codex` is `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, the client folders hold user settings, so removal runs there only with an explicit `--root`. Start a new agent session afterwards, since clients load hooks when a session starts.
