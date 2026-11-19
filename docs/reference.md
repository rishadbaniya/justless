# Justless reference

## Commands

| Command | Result |
| --- | --- |
| `justless run --max-bytes 4096 -- COMMAND ARG…` | Bounded previews for stdout and stderr, the child exit status (127 or 126 when the command cannot be found or run), and the run ID. Without `--max-bytes`, each command family's budget adapts to how often agents replayed its clipped streams in full. |
| `justless exec --lang python3 --file data.json CODE` | Runs a short script and returns only what it prints, with its exit status. The full output keeps a run ID. The code can also come from stdin (`-`) or `--script FILE`. |
| `justless exec … --intent TEXT` | Replaces stdout longer than the preview with the lines most relevant to TEXT, each marked for `justless log` |
| `justless search RUN_ID TEXT` | Literal matches with stream and line numbers, up to `--limit`, 20 by default. When the limit cuts, every stream with a match keeps a place, a failed run's failure lines and stderr come first, and a last line counts what each stream left out. |
| `justless log RUN_ID --stream stdout --from 10 --to 15` | Original bytes for the specified inclusive line range, with secrets replaced by markers. `--unredacted` returns the exact bytes. Omit the range to replay the complete stream. |
| `justless query SYMBOL --view VIEW` | Definition, references, callers, or related tests. Use `all` for the combined view. |
| `justless query SYMBOL --view dependencies` | Outgoing calls, imports, and other indexed relationships |
| `justless path FROM TO` | A short relationship chain with the location and direction of each edge |
| `justless map` | Areas, most connected symbols, communities, cross-area links, entry points and test locations, each with a file and line |
| `justless affected SYMBOL --depth 3` | Dependents grouped by distance, then the tests that reach the target and a command to run them. Also takes a file path or `--changes`. |
| `justless test --affected [-- RUNNER ARG…]` | Runs only the tests that can reach the current Git changes, with the project's own runner, through the same capture as `run`. The result is labeled as a subset, keeps the runner's exit status, and falls back to the full suite with a stated reason when the selection cannot be trusted. |
| `justless test [-- RUNNER ARG…]` | Runs the full suite of each detected runner, or of the given command, labeled as the full suite |
| `justless test --compare REF [-- RUNNER ARG…]` | Runs the tests, then reruns only the failing ones at the merge base of `REF` and `HEAD` in a temporary Git worktree and groups them as new, changed or already failing there. Cargo and pytest. Keeps the working tree's exit status. Combines with `--affected`. |
| `justless test --affected --dry-run [--json]` | The selection, the reasons, each test's evidence and the exact commands, without running anything |
| `justless test --why RUN_ID [--test NAME]` | Why a recorded `test` run chose its tests, with changed files whose source has moved since marked `[source changed]` |
| `justless find TEXT --path src --glob '!*_test.rs'` | Literal search across indexed text, with exact file and line matches. `--path` and `--glob` narrow it, and a page cut by `--limit` (20 by default) or the budget ends with a command for the next page. [Details](#narrow-and-page-find) |
| `justless lookup "where are retries configured?"` | The few symbols, Markdown sections and configuration blocks that best answer a question, each with its line range, name and best-matching lines. `--logs` also ranks recent captured run output. |
| `justless context --changes` | Current changes against `HEAD`, with nearby symbols and tests where available. Includes untracked files. A new file or a range across several symbols names each symbol, then shows each in source order with the members it contains, such as a class's methods. A pure deletion shows where the lines were, their text, any symbol removed whole and the surviving symbol that lost them, without pinning them on the line before. A deleted file is marked deleted. Every path Git lists is kept, and a change no line range shows says what changed: `mode 100755 -> 100644`, `binary or no line diff` for a binary or a file marked `-diff`, a symlink's new target, `type changed from file to symlink`, `submodule changed`, a new symlink or nested repository. A staged rename or copy names its source, as `renamed from OLD`; the old path still counts as deleted. `affected --changes` lists the same descriptions. Needs a Git repository. |
| `justless context --changes --staged` | Staged ranges for files that currently match the staged source |
| `justless context --changes --base main` | Changes against the merge base of `main` and `HEAD`: a branch's commits and the working tree on top of them. On a clean tree, plain `--changes` suggests it. |
| `justless context --at src/lib.rs:8` | Enclosing symbol, relevant calls, and source around a line or range. A range across several symbols shows each of them. A line past the end of the file is an error. |
| `justless code src/lib.rs --from 1 --to 8` | A numbered excerpt of original source. A range that starts past the end of the file is an error, and one that runs past it ends at the last line with a note. |
| `justless read FILE --outline` | Revision, signatures and line ranges. Small files are returned verbatim when called without outline options. |
| `justless expand FILE SELECTOR` | Exact bytes for a symbol, entry, section or inclusive line range |
| `justless edit FILE SELECTOR --revision REV` | Apply the replacement supplied on stdin after verifying the snapshot and that the file still parses |
| `justless mask` / `justless recall ID` | Retire older tool outputs from an outbound JSON request or retrieve the original result |
| `justless wrap claude [ARGS…]` | Claude Code through a local proxy that asks the API to clear old tool results in rare, large steps and archives tool results |
| `justless proxy --port N` | The same proxy for any Anthropic Messages client |
| `justless recall ID` / `justless recall --list TEXT` | The exact original of an archived tool result by its recall or tool_use ID, or archived results whose tool or call matches TEXT. `--from N --to N` and `--grep TEXT` print part of one. |
| `justless pack "What breaks if I rename purchase_orders?"` | A budgeted selection of definitions, graph links, source lines, working-tree changes, and relevant recorded failures |
| `justless history "TimeoutError" --oldest --limit 1` | The matching log line from the earliest completed run among archived commands in this project, and a count of the matches left out. Omit the text to list recent runs. |
| `justless resume` | Recent command outcomes, inspected source ranges, and files in the last pack. Stale source is marked. |
| `justless stats --days 7 --json` | Bytes kept out of context at first preview and net of later replays, with estimated token counts, a daily timeline, savings by command, full-log reopenings, each command family's preview budget and why, repeated reads and commands passed through by hooks |
| `justless statusline` | One line for Claude Code's status line, such as `justless · net ≈41.2k tokens saved this session · 23 runs · 4 reads`, from the session JSON Claude Code passes on stdin. `justless init --statusline` installs it. |
| `justless discover --days 30 --json --plan` | Shell commands Claude Code ran in this workspace, split into those routed through Justless, supported commands that were not routed, supported tools in a form the hooks pass through, and unsupported commands, with output sizes. `--plan` ranks the unrouted families by output and marks how much a hook or `run --` would compact now |
| `justless learn --days 30 --json` | Command mistakes agents corrected or keep repeating, as proposed rules with failure, session and thread counts, the failed output they cost, the last date seen, a check against the current tree and the run behind each rule. `--apply` writes them to a managed block in the project instructions. |
| `justless mcp` | A stdio MCP server for the read-only commands. `--install CLIENT` adds it to a client's project config. |
| `justless doctor --json` | Whether this project's setup works: the `justless` on `PATH`, each agent's CLI, hooks and instructions compared with what `init` writes, MCP entries and `.justless/`, with a fix for each problem. Exits 1 when something needs fixing. [Details](#check-the-setup) |
| `justless init --remove --dry-run` | Each file that removing Justless from the project would change, and how. Drop `--dry-run` to make the changes. `--agent NAME` limits them to some clients, and `--purge` also deletes `.justless/`. |

Commands that Justless prints for the agent to run next, such as a `Next:` line, a `justless log RUN_ID` pointer, the `Full suite:` and `Why:` lines or the commands in hook context, reach the same workspace when run as printed from the directory the first command started in. They start with `justless --root ROOT`, with the absolute root, when a plain `justless` started there would find another root, such as after `--root billing` or in a nested repository. `read`, `expand` and `edit` commands also add it whenever that directory is not the root, because they resolve a bare path from the current directory first. Otherwise they start with plain `justless`. A hook-routed run whose marker would need `--root` adds a line such as `[reopen with justless --root /work/app log run-ID]`. The MCP server cannot know where the agent's shell starts, so every command in its tool results, such as lookup's `Next:` line or the `log` paging note, names the root. `--root` may be repeated before the subcommand when every copy names the same directory.

## Indexing and languages

Queries refresh the index automatically. Justless compares file identity and high-resolution timestamps where available, hashes files that need closer inspection, reuses parsed facts for unchanged content, and removes deleted files. In a Git checkout it uses tracked and unignored files. Files under version-control and dependency directories such as `node_modules`, `.venv` and `__pycache__` are left out, as are untracked files under `target`. Add patterns to `.justlessignore` for repo-specific exclusions. Files larger than `max_file_bytes` and files or directories Justless cannot read are skipped without stopping the index. `justless index` lists them on stderr, except binary files. A `find` that finds nothing names them, and a `query` that finds nothing names the skipped source files and unreadable paths. A file name that contains a control character, such as a newline, is printed as a quoted string with escapes.

Structured navigation supports Rust, JavaScript, TypeScript and TSX, Python, Go, Java, Kotlin, Scala, C, C++, C#, Swift, Dart, Ruby, PHP, Lua, Zig, Bash, SQL, and HCL, including Terraform. The available symbols and relationships vary by language. Calls across implicit imports, framework discovery, dynamic dispatch, and macro expansion can require source verification. Use `find` for any indexed UTF-8 text file. When source has changed since a recorded run, Justless preserves the original log location and flags the mismatch.

### Narrow and page `find`

`find` reads every indexed text file unless `--path` or `--glob` narrows it. Both repeat. `--path` names a file or directory and resolves like other path arguments: a bare path comes from the current directory and falls back to the root, or the reverse after `--root`, and paths starting with `./` or `../` always come from the current directory. A path outside the workspace or missing from it is an error. `--glob` (`-g`) works like `rg -g` on workspace-relative paths: a glob includes the files it matches, a leading `!` excludes them, the last matching glob wins, and an excluded directory takes its files with it. A narrowed search starts with the scope it read, such as `[scope: src/, docs/ · glob !*_test.rs · 41 of 1200 indexed files]`. A path with no indexed text files, such as `node_modules/`, or a glob that matches no indexed file gets its own line, and a scope with no files says `No indexed file is in scope.` instead of `No matching indexed text.`

Matches come in index order, one per line, up to `--limit`, 20 by default, and within the budget. A page always shows its first match, cut short if it alone would exceed the budget, so a budget too small for the notice and the next-page command runs over by that much. When matches are left out, the page says which bound stopped it, such as `[--limit 20 reached after 20 of 62 matches in 60 files]` or `[budget reached after 5 of 60 matches in 58 files, before --limit 20]`, and prints a `Next page:` command. That command repeats the search's options with `--after CURSOR` and starts the way other printed commands do, adding `--root` whenever a scoped search's root-relative paths would otherwise resolve elsewhere. The last page of a paged search ends with `[end of 62 matches in 60 files]`. A cursor holds a hash of the search (its text, `-i`, paths and globs, not `--limit` or `--budget`), the file and line the page stopped at, and a hash of that file's indexed content. Using it with another search, after that file changed, or after it left the index is an error that says which, because the lines it points at may have moved. Changes to other files are picked up, and the counts reflect them.

`--json` prints one object with `total` matches, the number of `files` they are in, the position of the `first` match on the page, and `matches` as `file`, `line` and `text`. A narrowed search adds `scope` and any `notes`. A cut page adds `cut` (`limit` or `budget`), `after` and the `next` command. The object stays within the budget, except that it always holds one match, shortened as needed.

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

`justless affected TARGET` walks back from a symbol or a workspace-relative file through calls, references, imports, HTTP routes and SQL relations to the code that depends on it. `--changes` starts from the current Git changes: the innermost definition around each changed range, and any definition whose declaration line changed. Add `--staged` for staged changes, or `--base REF` to start from everything since the merge base of `REF` and `HEAD`, as `context --changes --base REF` reads it. Lines removed without a replacement are handled separately: a symbol removed whole is named from the committed version of the file, and the code that still references it becomes the starting point; lines removed inside a surviving symbol change that symbol; anything else is reported as removed with unknown impact rather than attributed to the neighboring line. To find those references, the index is rebuilt in memory with the removed symbol restored, so a path, import or alias that resolves to it counts, including a file-scope `use`. Rust files that mention the name are read again with the bodies of all macro calls, so a call inside `ensure!` resolves too. A mention that resolves to another definition of the same name, such as a twin in another module, does not count when the name appears once on its line, unless a trait ties the two, as it does an implementation to the removed trait method. Neither does a Rust field, variable binding, plain comment or path into the standard library (unless the workspace defines a module of that name) outside a string literal, since a format string can capture a name. Any other mention of the name is marked possible, as is everything reached through it. Mentions outside every definition, such as documentation or a `macro_rules!` body, are counted in one line and not followed, so a report of no remaining references then covers indexed definitions only. The report also says when the name search stopped at 256 lines. A removed import is handled as removed lines, not as a symbol. `context --changes` lists the same deletions. Each dependent appears once, at its shortest distance and through its most certain link, so cycles end. `--depth` sets the number of hops, 3 by default, and the report counts the dependents just past it. A dependent reached through an inferred link is marked possible, as is everything reached through it. A Rust `use` outside any item appears as a file scope, except in the file that defines the symbol.

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

To run the tests `affected --changes` points at, with fallbacks and labeling, use `justless test --affected`.

## Affected tests

`justless test --affected` runs the tests that can reach the current Git changes with the project's own runner. Every step goes through the same capture as `justless run`, so its output is grouped, archived under a run ID and replayable, and the runner's exit status is the command's. `justless test` without `--affected` runs each runner's full suite. Run it before reporting that tests pass: a subset is always labeled as one, and Justless never reports that all tests pass after a subset.

```sh
justless test --affected                                  # detected runners
justless test --affected -- cargo test --offline          # this command's full suite, narrowed
justless test --affected --dry-run                        # the plan, nothing runs
justless test --affected --base main                      # a branch's commits and the working tree
justless test --affected --no-fallback                    # keep a low-confidence subset
justless test --why run-k2P9xq --test vat                 # why those tests ran
justless test                                             # the full suite
```

`--session`, `--fresh` and `--max-bytes` mean what they mean for `run`, and apply to every step. Without `--`, the runners come from the project files listed below; a project with several, such as a Cargo workspace beside a Python package, runs each in turn.

### How tests are chosen

1. **Changes.** The working tree and untracked files against `HEAD`, as `context --changes` reads them. With `--base REF`, against the merge base of `REF` and `HEAD` instead, so a branch's commits count along with uncommitted edits, and the header names the ref and the merge base, as in `working tree vs main (merge-base 3f2c1ab)`. A ref that names no commit, or shares no history with `HEAD`, is an error. A clean tree without `--base` says so and suggests it, naming `origin`'s default branch or a local `main`, `master`, `trunk` or `develop` when one points elsewhere than `HEAD`. Lines removed without a replacement, deleted files and renamed definitions are handled too: a definition present at the base and gone now is looked up by name, and the code that still names it becomes a starting point. A change with no line diff, such as a mode, a binary body, a symlink or a submodule, is listed with what changed and falls back with `no-line-diff`.
2. **Starting symbols.** The innermost definition around each changed range, and any definition whose declaration line changed. A change outside any definition, such as a `use` line, starts from the definitions in that file that use the names on the changed lines. A trait method that is called implicitly, such as `fmt`, `from` or `eq`, is also traced through its type.
3. **Reach.** Each starting symbol is walked on its own, 64 hops deep, through every indexed relationship, so every changed symbol is accounted for. Inferred links are kept and mark the tests behind them as possible. When the walk reaches an implicitly called trait method, such as a `Display::fmt` whose `write!` calls a changed helper, it continues through that method's type too.
4. **Safety nets.** Tests whose body names a changed symbol are added, which catches calls inside macros the index does not expand. Other code that calls a changed function by name where the index has no link, such as inside a project macro, becomes a possible dependent and is walked in turn. A documentation example that names a changed symbol, or anything the change reaches, such as the public function that calls a changed helper, runs as a doctest wherever it is: in an item's `///` or `/** */` docs, in a file's `//!` docs, in a `#[doc = "..."]` attribute, or in a file the docs include with `#![doc = include_str!("../README.md")]`. Examples are the code blocks rustdoc runs: fenced with backticks or tildes, unless the info string names another language such as `text` or `sh`, or indented four spaces. A method as common as `new` is looked up by its type's name, a name given with `use … as`, such as `pub use inner::price as cost;`, is looked up too, names on changed lines outside any definition, such as an associated constant, count too, and a name too common to search runs its crate's doctests. Changed documentation that holds an example runs every doctest in its file. Documentation Justless cannot read, such as `#[doc = concat!(…)]`, a `cfg_attr` doc or doc comments inside a macro call, runs the crate's doctests unfiltered. A changed file mentioned by path in indexed code, such as an `include_str!` fixture or a directory a test copies, selects the tests and functions that mention it; a path written relative to the file that names it counts, so a changed README included as crate docs runs that crate's doctests. A changed data file, which is anything but Rust, Python, Go or JavaScript code, mentioned at the top level of other code makes that module a starting point, or in Rust falls back with `data-unlinked`; changed code is traced through its own symbols instead. A Rust integration test that runs a binary through `CARGO_BIN_EXE_<bin>` or `cargo_bin("<bin>")` runs whole when the change reaches that binary's package, and so does a test target with more test attributes than indexed tests, which usually means tests a macro generates. A test file reached through a file-level import runs whole, and a pytest file whose text names a changed module, such as `billing.rates` in an import or a `mock.patch` target, runs whole too.
5. **Every change accounted for.** Each changed symbol in a runner's scope must reach at least one test through exact links, and only tests the runner's full suite would run count: a test in a file pytest's `python_files` or `testpaths` leaves out, or in a Cargo target with `test = false` or an unmet `required-features`, satisfies nothing. Code in a binary that an integration test runs whole counts as reached, and code in an example only has to compile, which the plan builds. Otherwise the runner falls back, as listed below. A changed file in a language no detected runner runs, such as SQL or shell, is treated like data: it needs a mention or a link to a test.
6. **Scope.** Each runner turns the selection into its own arguments, then the checks below decide whether the subset stands.

| Runner | Found from | Unit | Command |
| --- | --- | --- | --- |
| Cargo | `Cargo.toml`, read with `cargo metadata --no-deps --offline` | Test name within a target | `cargo test -p PKG --lib --test T -- NAME… --exact`, plus whole targets, doctests and build-only targets in separate steps |
| pytest | `pytest.ini`, `[tool.pytest.ini_options]`, `tox.ini` `[pytest]`, `setup.cfg` `[tool:pytest]`, or test files at the root or in a top-level `tests/` directory. Its `python_files` and `testpaths` decide which files hold tests. | Node ID, or a whole test file | `.venv/bin/python -m pytest` when that exists, else `python3 -m pytest`, followed by node IDs and files |
| Jest, Vitest | A `package.json` that lists `jest` or `vitest`, or their config files | Test file | `node_modules/.bin/jest FILE…` or `node_modules/.bin/vitest run FILE…` |
| Go | `go.mod` | Top-level test function, or a whole package | `go test -v ./PKG… -run '^(TestA\|TestB)$'` for `func TestX(t *testing.T)`, and `go test -v ./PKG…` for packages whose selected tests `-run` is not given, such as testify suite methods, examples and fuzz targets |
| Other | Any other command after `--` | — | The command unchanged, as the full suite |

A command after `--` is the base. Justless adds its package and target flags after the command's own options and its test names after any test arguments that follow `--`, so `--offline`, `--features` and `-- --nocapture` keep working. A command that already selects tests, such as `cargo test -p api`, a test name, `pytest tests/x.py` or `--lf`, runs as given. `npm test`, `pnpm test`, `yarn test` and `bun test` run the `jest` or `vitest` script from `node_modules/.bin` when the script is that tool with plain flags, and otherwise as given.

Cargo steps:

| Step | When | Arguments |
| --- | --- | --- |
| Whole packages | A member `Cargo.toml` or `build.rs` changed, a source file was deleted, a `macro_rules!` body changed, or a proc-macro crate changed. A macro in an integration test file runs only that target. | `-p PKG` for the package and every member that depends on it |
| Selected tests | Tests reached by name | `-p PKG… --lib --bin B --test T -- NAME… --exact`. `--exact` is dropped when an `#[rstest]` or `#[test_case]` test is selected. Changed targets ride along so they compile. |
| Whole targets | Black-box, opaque, custom-harness and file-imported targets, or more than 300 names | `-p PKG… --test T…` |
| Doctests | A reached library item has an example in its documentation, changed documentation holds one, or an example elsewhere names a changed symbol or anything the change reaches | `-p PKG --doc -- PATH…`, where a file's examples are `'src/lib.rs - '` and a method's are `'::total (line '`, since rustdoc names it with its type's generics. Documentation Justless cannot read, or a reached name too common to search, runs `-p PKG --doc` unfiltered, and a filter that matches no doctest reruns its crates' doctests in full. |
| Build only | A changed target has no test step | `--no-run` for those targets, examples included, since `cargo test` compiles every example |

Packages and targets come from `cargo metadata`, not from the index, so `-p` gets each package's name as written. A target with `test = false`, or with `required-features` the command does not enable, is never selected and is listed as skipped like `cargo test` would skip it; an example still builds when a change reaches it. A `harness = false` target never receives test-name filters. A Cargo package nested inside another one that is not a workspace member, such as a test fixture, is never run; its changes reach the tests that mention its path. When a passing selected-tests step never reported a selected name, the name's target runs again whole, with the note `widened: N selected tests reported no result under their test names`. With `cargo test -q`, which prints no names, each target's summary count is checked instead. Cargo stops at the first failing test binary in a step, as `cargo test` does; add `--no-fail-fast` to the command to see every failure.

Go steps add `-v` unless the command has `-v` or `-json`, so the tests that ran are named; a package where a selected test reported nothing, such as `[no tests to run]`, runs again without `-run`. pytest reruns the selected node IDs as whole files when it exits 4, which is how it reports a node ID it cannot find, and runs the full suite when it collects none of the selected tests (exit 5). Either way the wider run's status replaces the rejected selection's, and after exit 5 the verdict and the run record say `Full suite (fallback from --affected: runner-mismatch)`.

### When the full suite runs instead

| Reason | When |
| --- | --- |
| `lockfile` | `Cargo.lock`, `uv.lock`, `poetry.lock`, `Pipfile.lock`, `requirements*.txt`, `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`, `bun.lock` or `go.sum` changed, for the runner it belongs to |
| `workspace-config` | The Cargo workspace manifest with `[workspace]` or `[profile]`, a single-package root manifest, `.cargo/config*`, `rust-toolchain*`, a root `pyproject.toml`, `setup.py` or `setup.cfg`, a JavaScript root `package.json`, `tsconfig*.json`, Babel or SWC configuration, `go.mod` or `go.work` changed |
| `test-config` | `pytest.ini`, `tox.ini`, the root `conftest.py`, a Jest, Vitest or Vite config, or `.env*` changed |
| `configured` | A changed file matches `[test] full_suite_on` |
| `parse-error` | A changed source file does not parse |
| `unindexed` | A changed source file is not indexed, for example because it is larger than `max_file_bytes` |
| `data-unlinked` | A changed file that is not code, or is code in a language no detected runner runs and reaches no test, is not mentioned by indexed source and is not documentation or repository metadata. Also a file used only at the top level of a Rust file. |
| `no-line-diff` | Git lists a changed file with no line diff: a mode change such as a lost executable bit, a tracked file's binary or `-diff` body, a symlink, a submodule, a nested repository or a change of type. Tests that name the file are not enough, so every runner falls back, whatever its root. Documentation and repository metadata, and files matching `[test] ignore`, are listed with what changed and have no test impact. |
| `deleted-module` | A Python, JavaScript, TypeScript or Go source file was deleted. A deleted Rust file runs its package whole instead. |
| `no-tests` | A changed symbol is reached by no test the runner collects. A definition that is new and has no users, or only users in examples, is the exception: it only has to compile. |
| `inferred-only` | Every test that reaches a changed symbol does so through an inferred link or a name match |
| `walk-capped` | A walk or a name search stopped at its cap |
| `too-broad` | The subset reaches 60% or more of the runner's indexed tests |
| `runner-mismatch` | pytest collected none of the selected tests (exit 5); found while running, after which the full suite runs |
| `unknown-runner` | The command is not one Justless can narrow, such as `make test` or `cargo nextest` |
| `runner-facts` | `cargo metadata` failed, or a detected runner is not installed |
| `no-git` | The project is not a Git checkout |

Each reason is printed with its message on the runner line and in `--dry-run`. `--no-fallback` keeps the subset instead and labels it `low-confidence subset (CODES)`, or `build only, low confidence (CODES)` when no test reaches the change. Documentation, `README*`, `CHANGELOG*`, `LICENSE*`, `.gitignore`, `.github/` and agent instruction files that no code mentions have no test impact, and a change to them alone runs nothing.

### Output, exit status and records

The first lines name the base revision, then one line per runner: `cargo · subset · 7 of 20 indexed tests in 4 of 7 test targets`, `cargo · full suite · Cargo.lock changed: …`, `cargo · build only · …` or `cargo · nothing to run · …`. Each step prints `Run: COMMAND` with test names collapsed to a count, then the usual capture preview and footer. The verdict comes last on stderr:

```text
Subset passed (cargo): ran 8 tests reachable from your changes (7 of 20 indexed tests selected, 13 not run); the full suite was not run (1.3 s).
Full suite: justless test -- cargo test --offline · Why: justless test --why run-k2P9xq
```

The `Full suite:` command repeats the runner given after `--`, if any.

A failing subset adds up to three failing tests with the first link of their evidence. Build-only and nothing-ran results say `this is not a test result`, and a build-only result names the targets it built, as in `Build only (cargo): reports --example weekly, reports --lib compiled`. A fallback says `Full suite (fallback from --affected: CODES)`. Go output without `-v` is counted in packages. The exit status is the runner's, or with several steps or runners the first nonzero status in the order they ran; later steps still run, and a step replaced by a wider rerun does not count. A runner that cannot start, such as a missing program (exit 127) or a Python without pytest, keeps its status and is reported as `Runner not available (pytest): No module named pytest; exit 1. Its tests did not run; this is not a test result.` Nothing to run exits 0. Errors before any step runs, such as an unknown run ID or no detected runner, exit 2.

Each step's run directory holds `selection.json` beside the streams: the base revision, the changed files and their digests, the starting symbols, the runner's plan with every selected test, its confidence (`exact`, `possible`, `mention`, `scope`, `changed-test`) and its evidence chain, each step's command, run ID and status, and the verdict with time and bytes. It holds paths and names, never output. `--why RUN_ID` prints it within `--budget`, and `--test NAME` keeps the tests whose name contains `NAME`. `resume`, `history` and `stats` show these runs as `cargo test (affected subset)`, `(affected low-confidence subset)` or `(affected build only)`. A fallback and plain `justless test` record the runner's usual family, such as `cargo test`.

`--dry-run` runs nothing and records no run; it reads the index and runs `cargo metadata`. `--json` prints the same plan as data, identical across runs on the same tree.

Hooks never narrow a test command; `justless test` is something an agent asks for. `test` executes project code, so like `run` it is not an MCP tool; the read-only `affected` tool shows the same reach.

### Failures compared with a base

`justless test --compare REF` separates the failures a change introduced from those already failing on the base branch. It runs the tests in the working tree as usual, the full suite or with `--affected` the subset, then reruns only the tests that failed, by exact name, at the merge base of `REF` and `HEAD`, in a temporary Git worktree under the system temporary directory. The worktree is removed afterwards, also when the base run fails, and one an interrupted comparison left behind is removed by the next comparison in the project. When nothing failed, nothing runs at the base.

```sh
justless test --compare main
justless test --compare main -- cargo test --offline
justless test --affected --base main --compare main   # the branch's affected tests, compared with main
```

The base command prints as `Run at main (merge-base 3f2c1ab), temporary worktree: cargo test --no-fail-fast -- --exact <4 test names>`, and the groups follow the verdict on stderr:

```text
Full suite (cargo): exit 101, 5 tests ran, 4 failed (1.4 s).
Compare with main (merge-base 3f2c1ab): reran 4 tests failing here by exact name at the base, in a temporary worktree; exit 101 (1.1 s).
  New failures, passing at the base (1): tests::breaks
  New failures, no result at the base (added or renamed since?) (1): tests::added
  Changed failures, failing at the base with another message (1): tests::changed
    tests::changed · here: assertion `left == right` failed left: 2 right: 10
    tests::changed · base: assertion `left == right` failed left: 1 right: 10
  Already failing at the base, same message (1): tests::old_bug
```

A test is already failing when it fails at the base with the same normalized message. For Cargo that is the test's block under `failures:` from the panic on, without the thread name and ID, the panic's source location, `note:` lines, hexadecimal addresses and the paths of the two checkouts and target directories, so a failure in a file whose lines moved still matches. For pytest it is the reason on the short summary line. "No result at the base" means the base did not report the test, as for one added or renamed since, or one in a pytest file the base does not have. pytest cuts that reason to the terminal width, 80 columns when its output is captured, so only the part it prints is compared. A test that fails at both with no message on either side, as under `--nocapture`, is listed as `Failing at the base too, no message to compare`. Cargo doctests are listed as not compared, since their names carry line numbers. Tests the change fixed are not listed, because only the failing tests run at the base. Up to eight names are listed per group, and up to three changed failures show both messages.

The base command is the working-tree command with the failing names added. Cargo also gets `--no-fail-fast`, so a failing test binary does not hide the next one, and loses `-q`, so passing tests are named, and any `--target-dir`. It builds in `.justless/compare/cargo-target`. The first comparison builds every dependency there, which takes as long and as much disk as a fresh `cargo test`; later comparisons reuse them and rebuild only the project's own crates. Delete the directory to reclaim the space. It never builds in the working tree's target directory: Cargo would take the base's artifacts for the working tree's, and the next plain `cargo test` would run the base's code. pytest gets `-rA --maxfail=0`, so every outcome is named and no failure stops the run, and `PYTHONPATH` starts with the checkout's runner directory and its `src/`, so an editable install does not import the working tree's code. A node ID whose file does not exist at the base is not passed, since pytest rejects a whole run over one, and one pytest reports as not found is dropped and the rest run again. A relative program path, such as `.venv/bin/pytest`, still names the working tree's program, and an absolute path into the repository, such as a `--manifest-path`, moves into the checkout. The checkout has no untracked or ignored files, such as a `.env` or generated fixtures, and no submodules, and the project's checkout hooks do not run. Comparisons in one project take turns.

The exit status is the working-tree run's, whatever happens at the base. A ref that names no commit or shares no history with `HEAD` exits 2 before anything runs. When the base cannot be checked out, does not build or reports no test result, the comparison says so, as in `the base run reported no test result, exit 101 (error: …); failures are not grouped.` Jest, Vitest, Go and other commands are not compared: the comparison line says `not supported` and nothing runs at the base. Each of the runner's run directories gets `compare.json`: the ref, the merge base, the base command, its status and time, and each test's group with both messages, up to 2 KB each.

## Progressive reads and verified edits

`read`, `expand` and `edit` resolve FILE like a shell path, from the current directory, and fall back to the workspace root when the current directory has no such file. With `--root`, which hooks pass, a bare path is taken from the root first instead. Paths that start with `./` or `../`, and absolute paths inside the workspace, always resolve from the current directory. When a bare path names different files from the two places, stderr shows the spelling for the other one. Output and revisions always use the workspace-relative name. Other commands, including `code` and `context --at`, take workspace-relative paths. Run `justless read FILE --outline` to get a path-bound revision and numbered rows. An entry reference combines the revision and row number, such as `abc…#7`. A Rust row's signature starts at the declaration itself, and so does a TypeScript method's, while the row's line range, `expand` and `edit` still cover the Rust doc comments and attributes or the TypeScript decorators above it. Use `--match TEXT` to search names and signatures, including Rust doc comments and attributes and TypeScript method decorators, `--limit N` to set a page size, and `--after CURSOR` to resume the same search. `--budget` bounds the text outline. `--json` adds structured fields and a `next` cursor. Normal reads return small UTF-8 files verbatim. Outlines use the available language parser, Markdown headings, or 80-line pages. Markdown headings follow CommonMark: an ATX line such as `## Setup`, or a setext heading, paragraph text underlined with `=` for level 1 or `-` for level 2. A heading's name leaves out an optional closing run of `#`, so `## Setup ##` is named `Setup`, and joins the lines of a setext heading with spaces. `#tag` is not a heading, a `---` that underlines no paragraph is a thematic break, and lines in fenced or indented code, HTML blocks, YAML front matter, block quotes and list items start no section. A section runs until the next heading of the same or a higher level, so `expand` and `edit` of one section never reach into the next. Rust files are parsed whole, so a file with a syntax error has no symbol rows: its outline names the error and its line, `--json` adds a `parse_error` field, and the outline falls back to pages. Symbol names then fail with the same error, and line ranges still work. Delimiters nested more than 1,024 levels deep, or a run of more than 1,024 consecutive operators, count as a syntax error instead of exhausting the parser's stack. Line-range selectors resolve without the parser, though `edit` still parses the result to check it.

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

Before writing, `edit` parses the file as the batch would leave it, with the parser its outline uses: syn for Rust, and Tree-sitter for the other indexed languages except SQL, whose grammar rejects most dialects. When the file parsed before the edit and does not after it, how the batch is handled depends on how sure the parser can be. syn follows stable Rust, so any Rust error rejects the batch. A bundled Tree-sitter grammar can lag behind its language and report valid newer syntax as an error, such as TypeScript variance annotations, Python 3.13 type parameter defaults, Kotlin context parameters or C# 14 extension blocks. So a Tree-sitter error rejects the batch only when the grammar found a token missing, such as a `;` or `)`, or the edit changed how the file's brackets pair up, which no version of the language accepts. Brackets are counted from the grammar's punctuation tokens and from text it skipped, so brackets in strings and comments do not count and `${`, `#{` and `\(` count as openers. A rejected batch leaves the file unchanged: `edit` exits 2 and prints the parser's line, column and message, such as `src/lib.rs:1:19: unclosed `{` (Rust parser)`, a few lines around it with a caret under the column, the lines the edit wrote and, for a Tree-sitter language, that the brackets no longer pair up. `--dry-run` prints the patch and gives the same verdict and exit status. `--allow-parse-errors` writes the edit anyway, and stderr notes that the file no longer parses. Any other new error is written, and stderr names it with a warning, such as `justless: warning: box.ts may no longer parse: box.ts:1:25: syntax error at identifier (TypeScript parser).`, followed by why the edit was written. C, C++ and shell files are checked the same way but never rejected, because their grammars reject much working code: in local corpora of working code they reported errors in 43% of `.c`, 67% of `.h`, 45% of `.cc` and 1.1% of `.sh` files, against at most 0.2% for the other checked languages, and a missing token was common in C headers because of macros. A file that already failed to parse is never blocked: after the edit, stderr says whether the file parses now, still fails where it did, fails somewhere else, or has a new error in the text the edit wrote. A valid edit of a parsing file prints exactly what it printed before. `fixtures/parse_check` holds valid code with macros, decorators, templates, embedded HTML and recent syntax in every checked language, and the tests require the check to find no error in it and its brackets to pair up. The texts before and after the edit are parsed at once on two threads, brackets are counted only when the edit left an error, and a Tree-sitter parse that takes over 400 ms skips the check with a warning. On release builds the check adds about 2 ms to an edit of a 27 KB TypeScript file, 14 ms for 450 KB of JavaScript, 21 ms for 690 KB of Python and 37 ms for 690 KB of Rust.

## Recoverable tool history

`justless mask` reads a complete outbound request from stdin and writes JSON to stdout. It recognizes Anthropic Messages tool results, OpenAI Chat Completions tool messages, Responses API function outputs, and OpenCode v1 tool parts. By default it keeps the newest three results and considers older results of at least 1,024 serialized bytes. Controls include `--keep N`, `--min-bytes N`, `--trigger-bytes N`, repeatable `--pin CALL_ID` and `--keep-tool NAME`. The trigger applies to the combined size of eligible older results. Provider-marked errors, attachments, multimodal results and annotated text blocks stay intact. Requests for Claude models that tie thinking to earlier turns are left unchanged; see [Request-boundary context management](#request-boundary-context-management).

A retired result becomes `[justless recall ID]` in the same message and content format. The original value is stored locally in `.justless/observations/` and verified by SHA-256 on recall. The archive can contain sensitive tool output. `justless recall ID` prints the original text: the bytes of a string result, or the text of each part of an all-text result joined without a separator. `--json` returns the typed value. `justless mask --restore` expands references across a request. `--stats` writes a JSON change count and serialized byte delta to stderr. That delta is for one request, and for `--restore` it is negative. A result recalled later comes back into context, and `justless stats` reports what `recall` printed beside it. Reapplying the filter to a masked request preserves existing references. A request can contain unpaired UTF-16 surrogate escapes such as `\ud83d`, which JavaScript and Python write for a string cut inside a character. They are written back exactly as they arrived, and a result that contains one is not retired.

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

`run` groups recognized test failures by their reported diagnostic and summarizes Git patch hunks with file names, hunk headers, and original stdout line ranges. The command still runs every time. For a matching command, directory, session, and preview budget, byte-identical streams and an identical exit status can produce an unchanged notice. Changed test reports and patches show additions and removals against the previous run. A Rust panic's diagnostic is its location and message, so panics at one location with different messages are listed separately, and a changed message counts as a change. Hunks are compared by file, function context and content, so a hunk that only moved because an earlier hunk changed is not reported. A removed failure means it disappeared from the report. Use the exit status to check the overall result. Unrecognized output uses the ordinary preview.

A command you run yourself ends with a footer that names the run, its exit status and how many bytes were shown. Hooks add `--hook`, which prints a footer only when it adds information. When the agent received every byte and the command succeeded, or a search matched nothing, nothing is added. When output was clipped, grouped or collapsed, one line names the run and what was shown, such as `[justless run-7Gq2 · 3.9 KB of 46.6 KB shown]`. A failed command whose output was shown in full ends with `[justless run-7Gq2 · exit 2]`. Hooks add `--merge` for a command that ended in `2>&1`, and the command then writes both streams to one pipe that Justless archives as stdout, so output shown in full is the bytes the command printed, in order. A merged run keeps its own preview rather than a test, patch or inventory summary, which reads stdout alone and would drop what the command wrote to stderr. A linter report still replaces it, as it replaces both streams of any run. A merged search or listing keeps its streams apart, because its grouping reads stdout alone, so when both streams had output its footer always names the run, as in `[justless run-7Gq2 · stderr after stdout · exit 2]`. Every run is archived either way and listed by `history`. `stats` counts the footer as part of what the agent received and reports runs that grew.

The ordinary preview shows a stream verbatim when it fits the budget. A longer stream shows its first lines, a `[diagnostics]` section and its last lines. `[diagnostics]` lists failures first: error lines, compiler errors after a `path:line:col:` prefix with their indented source lines, panics with their messages, Python tracebacks through the exception line, and failed tests. Indented error lines, which can also be fields in a debug dump, come next, then lines that name a source location and warnings. A repeated entry appears once with its count, such as `[×300]`. Lines inside a patch hunk are file content and never count. Every excerpt ends on a whole line. A line cut to fit ends with `…`, and the tail of a line longer than 256 KiB shows the line's real end after a leading `…`. Each run record keeps up to 100 source locations from failures and 100 others, failures first, with up to four log lines of each kind per location, for `context --run`, `pack` and `resume`. A failure keeps its location even when progress output named the same line first.

Without `--max-bytes`, a run's preview budget comes from its command family's history in the project. The evidence is kept per stream. A stream is clipped when its log is larger than the budget, so its preview showed the first and last lines in about half of it. A stream replaced by a summary or grouped report counts as clipped only when the report took at least a third of the budget, as a grouped search cut to fit does. A failing `cargo test` summarized in a few hundred bytes reads the same at any budget. `justless log` records which stream it replayed in full, in the run's `replays.json`, and a replay counts only when it replayed a clipped stream. Replaying the summarized stdout of a `cargo test` whose compiler warnings were clipped on stderr therefore leaves the budget alone. When at least five runs were clipped at a family's current budget and at least a quarter of the latest 20 of them had a clipped stream replayed in full, the budget doubles. It halves after 20 clipped runs that nobody replayed in full. Lowering waits for all 20 because a smaller preview can hide evidence, while a larger one costs only bytes. Above `max_output_bytes`, a stream shown whole that the next step down would clip also counts toward lowering, so a raise wears off once output no longer needs it. At or below `max_output_bytes`, only clipped streams count, since a stream shown whole says nothing about a smaller budget. A budget that was lowered and then raised back needs twice as many runs before it is lowered from there again, up to 160, so an agent that needs the larger preview is not retested every 20 runs. Targeted lookups, such as a line range, `search` or `context --run`, show that the preview led the agent to what it needed, so they count toward neither. Runs reported as unchanged are left out, and so are runs whose preview could not have come from the family's budget. A run that showed more of a stream than the budget allows came from a larger `--max-bytes`, and one that showed a clipped stream in under a quarter of it most likely came from a much smaller one. Full replays recorded before streams were tracked count against both streams. Only runs completed since a family's last change count, so each step is judged by the runs it produced and the budget moves one step at a time. It stays between `adaptive_floor_bytes` and `adaptive_cap_bytes`, by default half and four times `max_output_bytes`.

A family is the run's output family, such as `cargo test`, `pytest` or `cargo clippy`, or its command label when that is more specific, such as `git log` within `git` or `make test` among commands with no family. Commands labeled `other` keep the default, and so do runners, whose label names the runner rather than the work it runs: a shell or interpreter given a script, such as `sh -c` or `python3 tool.py`, a `run` or `exec` subcommand, such as `npm run` or `cargo run`, and a tool given options before its subcommand or target, such as `make -j8 lint`. A command with its own profile, such as the linter `golangci-lint run`, is not a runner. Hook-routed and typed runs share the budget. An explicit `--max-bytes` always wins and does not change the family's budget. Its runs are left out of the family's evidence when their previews show another budget: a larger one that showed more than the family's budget allows, or one under about half of it on a clipped stream. On a clipped stream, an explicit budget between about half and twice the family's can still be counted, because run records do not store the budget a run used. A rerun is compared with the last run of its command in the same directory and session at the same budget. When a family's budget changes, its rerun baselines, including those of explicit runs, are forgotten, so the next run of each of its commands shows a fresh preview instead of reporting output unchanged since a run made before the change. Each family's current step, and how often it was raised back after a lower, is kept in `.justless/budgets.json`. `.justless/budget-runs.json` lists the runs already read and keeps the sizes of the newest 320 runs of each family that a budget could clip, so a run lists the runs directory and reads only records it has not seen. `justless stats` lists the families whose budget moved, whose previews were clipped or whose summarized runs were reopened, with the reason, such as `cargo test  8.0 KB · raised from 4.0 KB: 4 of the last 9 clipped runs were reopened in full`, and `stats --json` reports every family under `preview_budgets`. Set `adaptive_previews = false` to give every run `max_output_bytes`.

Use `justless run --fresh -- COMMAND` for a fresh preview. Use `--session NAME` or `JUSTLESS_SESSION` to choose a comparison session from the terminal. Project hooks scope comparisons to the client's session identifier when supplied. Captured runs remain searchable across sessions. A marker such as `stdout:8-15` expands with `justless log RUN_ID --stream stdout --from 8 --to 15`. A range marked `prior stdout` belongs to the comparison run.

`run` returns the status a shell would: the command's own exit status, 127 when the command is not found and 126 when it cannot be executed. If Justless cannot create its state directory or the run archive, as in a read-only checkout, it prints one warning and runs the command without capture. Once the command has started, its output and status always come back. If the archive cannot be read or saved afterwards, for example because `make clean` deleted `.justless/`, Justless prints both complete streams instead of a preview. SIGTERM and SIGHUP sent to Justless are forwarded to the command. A terminal interrupt reaches the command directly. A SIGINT or SIGQUIT that Justless receives is forwarded if the command is still running 200 ms later, so one sent to Justless alone also stops the command. A command that takes longer than that to exit from a terminal or group interrupt receives it a second time. Justless waits for the command to exit, then records the run and shows its preview. If the command died from a signal that Justless also received, Justless then exits from that signal, so a calling shell stops its script as it would for the command. A run whose Justless process was killed outright, as by a client that gave up on a hook-routed command, stays in `history` as unfinished, and `log` replays the output it produced. It counts against `max_runs` and `max_archive_bytes` like a finished run. The command's output pipes close with Justless, so a command that keeps writing then receives SIGPIPE, as it would at the end of a shell pipeline. On Unix, Justless returns as soon as the command exits, even when a background process it started keeps its output open or keeps writing. A small hidden `justless drain` process, in its own process group, then stores that process's later output in the archive, within `max_run_bytes`, until the process closes it. `log` replays those bytes and notes how many arrived late. On Windows, `run` waits until every process holding the command's output has closed it, so a background process that keeps it open delays the preview, and its output counts as the command's own.

Container and cluster listings use the default table output. `docker ps` and `kubectl get pods`, `deployments`, `statefulsets` and `nodes` report how many rows were listed and name only the rows that need attention, such as unhealthy or exited containers, pods that are not ready or have restarted, and rollouts that are behind. A few healthy names are shown as examples. Each flagged row links to its original line. A failing command, an unrecognized header or a custom output format such as `-o wide` or `--format` keeps the ordinary preview.

JavaScript test runners (`npm`, `pnpm`, `yarn` and `bun test`, `jest`, `vitest run`, `node --test`) drop passing-file lines and keep failures with the lines that follow them. `docker logs` and `kubectl logs` collapse consecutive identical lines. Both report retained, filtered and repeated line counts. Watch, interactive, coverage and machine-readable reporter flags run the command with the ordinary preview.

Compilers and linters report findings grouped by severity and rule across stdout and stderr. Each group shows its severity, rule, count, first message and up to four locations sorted by file and position, with the original log lines of its first occurrence. Errors also show the detail lines of their first occurrence that has any, such as rustc's expected and found types or a TypeScript elaboration. File names may contain spaces, parentheses or a Windows drive letter. rustc lints keep their name when rustc states it only once or under `-D warnings`. `--message-format=short` names no lint, so common rustc lints such as `unused_variables`, `dead_code`, `non_snake_case` and `unreachable_code` are recognized by their wording there, and other findings, including Clippy's, are grouped by message. Short-format errors such as `error[E0308]` count under their error code. Recognized output includes rustc and Clippy (`cargo build`, `check` and `clippy`), Ruff, flake8, pylint, mypy, Pyright, ESLint, TypeScript `tsc`, ShellCheck, golangci-lint, `go vet`, staticcheck, RuboCop, SwiftLint and hadolint's default output. pylint's E and F messages count as errors. After the groups come general outcomes: errors without a source location, such as a failed build script or link step, with their last detail lines, the tool's own totals, one combined `could not compile` line that names each failed package and target, and a count of further outcome lines. The header counts errors without a location separately. When no error has a source location, the outcomes come right after the header instead. A stream that produced no finding, such as a crash traceback on stderr, is summarized by its line count, range, first line and last two lines. A rerun compares groups by severity and rule. It lists groups that appeared, changed count, reported different messages at the same count or are no longer reported, and notes when the same findings moved to other lines. A compile error can stop later lint passes, so a rule that is no longer reported is not necessarily fixed. A report that exceeds the preview budget keeps its header, comparison and outcomes, drops error detail lines first and then whole groups with a count. Output that fits the preview budget and is smaller than its report stays verbatim, also on a rerun. Output with no recognized finding, and machine-readable formats such as `--format json` or `--output-format`, keep the ordinary preview.

Searches and file listings are grouped by file and directory when their stdout exceeds the preview budget. Smaller output stays verbatim, and stderr keeps its own preview. `rg`, `git grep`, `ag`, `ack` and `grep` with `-r` or several files report the total matches and files, then one group per file in order of match count. Each group names its original stdout lines and shows its first matching lines exactly as printed, followed by the number of remaining matches. Files beyond the budget are summarized by directory. Matches inside dependency and build directories, such as `node_modules` or `target`, collapse into one line per directory unless the command names it, and a `Binary file … matches` line counts as a match in its file. Context lines from `-A`, `-B` and `-C` extend a file's line range and are counted separately from matches. `find`, `fd`, `git ls-files`, `tree`, `ls -l`, `ls -R`, `rg --files` and the `-l` file lists of the search tools report exact entry and directory counts, then list each directory with its entry count, its first few names and its original lines. Dependency, build and cache directories such as `node_modules`, `target`, `.git`, `dist`, `build`, `__pycache__` and `.venv` collapse into a count unless the command names them. When the groups do not fit, deeper directories fold into their parents with a count. A rerun lists files or directories whose counts changed. Counts such as `grep -c`, `--json`, NUL-separated output, `--heading`, `find -exec`, `-delete`, `-print0` and `-ls`, and options outside each tool's recognized display set keep the ordinary preview. A search that finds nothing keeps its exit status 1, and task packs do not list search or listing runs as failures. A recursive `grep` or `ack` run through Justless skips `.justless/`, so it never matches archived logs or its own output.

Package runners are recognized, so `npx eslint`, `pnpm exec tsc`, `uv run ruff check`, `poetry run mypy`, `python -m mypy` and `bundle exec rubocop` select the same profile as the tool itself. Hooks also route `make`, `ninja`, `cmake --build`, Gradle, Maven, `dotnet build`/`test` and `swift build`/`test` through the bounded preview.

`search` and `history` read every stored stream to the end, so they can count what they leave out. `search` lists a run's matches stream by stream in line order. `history` lists runs newest first, or oldest first with `--oldest`, and each run gets the places its predecessors left. When the limit or the budget cuts a run's matches, each of its streams with a match keeps a place, and a failed run's lines that name a failure, such as `FAILED`, `error`, `panicked` or `Traceback`, come first, then its stderr lines. A pipe or list that the Bash post-command hook recorded with exit 0 counts as possibly failed. Lines keep their order within a stream, and a stream with failure lines, or else stderr, is listed first. When every match of a stream lies in runs past the cut, as for an older run's stderr under a newer run's stdout, `history` shows that stream's first match in place of its last line. A last line then counts the matches shown and found per stream, as in `[20 of 26 matches shown (stdout 19 of 25, stderr 1 of 1); --limit 26 shows all]`, and for `history` how many of the matching runs it listed and whether a larger `--budget` shows the rest. When that line would leave out a stream's matches, or every match, under a small `--budget`, it drops the run count and the hint, and then the counts per stream. `history` skips a run whose streams cannot be read, with a warning. Reading every stream costs time in proportion to the archive, at most `max_archive_bytes`. `history`, `resume`, `stats` and `pack` order runs by the time each command completed. A run record that cannot be read is skipped with a warning that names the run, so the other runs and the compaction handoff stay available. `resume` covers Justless activity, including each recent run's outcome, such as its failing test count or most frequent lint rule, the most recent 32 source ranges and files used in the last pack. They are listed in that order. The runs come first within the budget, and the source ranges and last-pack files share what remains. Up to half of that space is held for the pack, and the ranges may use the rest, so neither list pushes the other out. Source ranges are dropped oldest first. `history` and `resume` name each run by its command label: the program and, for tools with subcommands, the subcommand, such as `git log`, without other arguments. When the output family adds detail, it follows in parentheses, as in `python (pytest)`. Runs recorded by older versions show their family. `stats` counts full-stream reads separately from range, search, and run-context requests. Hook pass-through counts cover shell calls observed by the installed hooks, including each command of a list that a hook leaves whole. They record command families without arguments. Preview-byte totals exclude footer text and subsequent lookups. Token figures are estimates of UTF-8 bytes divided by four, the same convention the benchmark scripts use, and are always shown next to the measured bytes. Provider token counts are measured separately by the benchmark. A high reopen rate for a command family suggests its preview drops evidence agents need, and adaptive budgets raise that family's budget in response.

`resume` lists a source range as `Read FILE:A-B` and an outline as `Outline FILE`. Both count toward the 32 entries, and `[source changed]` marks one whose file no longer has the digest it had when shown. Each command that prints a file's lines records the range it printed: `read` of a small file records `FILE:1-N`, the same entry `expand FILE L1-N` records, and `expand`, `code` and `context` record theirs. An outline from `read`, or from Claude Code's Read hook, records the outline: the agent holds its revision and line ranges, which go stale when the file changes, but not its lines. A file keeps one outline entry whatever the page, and outlines never count as repeated reads. An outline that stands in for a range of lines, such as a wide ranged Read in Claude Code, is listed as `Outline FILE:A-B` for that range. Once the project has a `.justless/` directory, the Read hook also records the lines of each text Read of a workspace file that it passes through, as Claude Code returned them. `pack` records its files as the last pack instead, and `lookup`, `find` and `query` are searches that record nothing. The same range of unchanged source shown again counts as a repeated read in `stats`, and the pre-compaction hook asks the summary to keep outlines with the ranges.

`stats` reports two savings figures. Initial savings are the raw output minus the preview and footer the agent received when the command ran. Net savings also subtract every byte that a later request printed from the same run: `log`, whole or ranged, `search`, `context --run`, the matching lines of `history TEXT`, the log windows of `lookup --logs` and the recorded failure in a `pack`, from a shell or through MCP. `context --run` and `pack` count their log lines and notes, not the source they show beside them, and an MCP `log` page counts the bytes that page returned. A run's net savings can be negative: output of 19,000 bytes shown in 364 and then replayed whole put 364 bytes more in context than running the command directly. Retrievals are not timed, so the daily timeline counts them on the day their run completed. Run lists from `history` without TEXT and `resume` show a run's ID, status, headline and source locations, not its log, and are not counted. In `--json`, `saved_bytes`, `saved_percent` and `estimated_tokens_saved` keep their meaning as initial savings, and `replayed_bytes`, `net_saved_bytes`, `net_saved_percent` and `estimated_tokens_net_saved` add the net view, also per day and per command. Retrievals recorded before Justless counted their bytes have no size. Each old whole replay is estimated at the size of the run's archived streams and reported apart from measured bytes, in `estimated_replay_bytes` and `estimated_replays`. Old range, search and context requests cannot be sized, so `unmeasured_retrievals` counts them and the text names them, but they are not subtracted.

`justless recall` records the size of each archived tool result it prints, whole or in part, and `stats` reports their number and bytes for the `--days` window, in `archive_recalls` with `--json`. For the proxy, `proxy.newly_cleared.bytes` is the initial size marked for clearing. `proxy.recalled_bytes` counts bytes printed from its tool-use archive during the same window, including repeat and partial reads. `proxy.net_cleared_bytes` is their signed difference. The API supplies its own token counts. Older recalls without an archive source remain in the combined recall count. `mask --stats` reports the byte change for that request.

`discover` reads Claude Code's session transcripts for this workspace from `~/.claude/projects/` or `$CLAUDE_CONFIG_DIR/projects/`, including the transcripts of subagents and workflow subagents, which count toward their parent session. A command counts as routed when a line of its output is a Justless run footer, either the full `[justless run-ID] exit N · …` line or a hook-routed run's bracketed marker such as `[justless run-ID · exit N]`, when it invokes a `justless` binary by any path, or when a bare `cat FILE` returned the outline that the hooks substitute. A command counts as supported only when the Claude Code hook would route it, so a plain `grep` or a list that starts with an absolute `cd` counts as not supported. Supported commands that reached the agent unfiltered usually mean the hooks are missing or the session started before they were installed. A hook-routed command that succeeded and reached the agent verbatim prints no footer, so it also counts as not routed. A supported tool in a form the hooks pass through whole, such as `cargo test 2>&1 | tail`, is counted as passed through (`passed_through` in `--json`). The report prints command labels and byte counts, never full command lines or output. A label is the program name, plus the subcommand for tools that have them, such as `cargo test` or `git log`. Other arguments, such as host or database names, are left out.

## Configuration

The optional `.justless.toml` controls project defaults. The values below are the built-in defaults.

```toml
context_budget = 2400
max_output_bytes = 4096
max_file_bytes = 2097152
max_runs = 200
max_archive_bytes = 268435456
max_run_bytes = 268435456
adaptive_previews = true
# Four times and half of max_output_bytes unless set. The floor is at least 512.
adaptive_cap_bytes = 16384
adaptive_floor_bytes = 2048
```

The cap must be at least `max_output_bytes`, and the floor between 512 and `max_output_bytes`.

Add `[[filters]]` entries to omit known noise from a command's preview. `command` is an argument prefix matched against the program name and its leading arguments. The longest matching prefix applies. `omit` is a regular expression tested against each trimmed line. Lines that look like failures, errors, warnings or assertions, and the four lines after them, are always kept. The full stream stays in the log.

```toml
[[filters]]
command = ["make"]
omit = '^make\[\d+\]: (Entering|Leaving) directory'
```

An empty prefix, an invalid expression or one that matches empty text stops the run with an error.

`[test]` tunes `justless test --affected`. Both lists take gitignore-style patterns and are empty by default. `full_suite_on` names files whose change runs every runner's full suite. `ignore` names changed files that have no test impact, which otherwise count as unlinked data and run the full suite.

```toml
[test]
full_suite_on = ["migrations/**"]
ignore = ["*.svg", "notebooks/**"]
```

The 60% share that counts as too broad, the 300 test names and 32 KiB of names before Cargo filters collapse to whole targets, and the 64-hop walk are fixed.

`pack`, `map`, `affected`, `history`, `resume`, `query`, `path`, `find`, `context`, and `code` accept `--budget`. The renderer uses one UTF-8 byte per budget unit as a conservative ceiling for byte-based tokenizers. Agent totals can also include the prompt, history, and tool metadata.

Full output is retained locally under `.justless/runs/`, including output short enough to display in full. The archive keeps the streams separately, with byte-for-byte replay for each. A run with `--merge` keeps both in the stdout archive, in the order the command wrote them, within the same `max_run_bytes` cap, and its stderr archive stays empty. `log` replaces secrets with markers unless given `--unredacted`, as [Secret redaction](#secret-redaction) describes. Justless creates `.justless/` with its own `.gitignore`, so `git status`, `git add -A` and `context --changes` never pick up archived output. When a shell hook rewrites a command that starts inside a nested Git repository or submodule, the rewrite adds `--root`, so the run lands in this workspace's archive instead of a new `.justless/` there. Keep the directory private if a command prints secrets. The command writes into pipes that Justless copies to the archive, and each stream of a run stores at most `max_run_bytes`, 256 MiB by default, so a command such as `sh -c 'grep -rn x .'` that reads its own growing archive stops at the cap instead of filling the disk. Justless keeps reading and counting a stream past the cap, so the command never blocks and runs to completion with its own exit status. Everything stored still replays byte for byte. The record keeps how many bytes the command wrote, and the footer, the fallback replay and `log` end with a marker such as `[stdout archive truncated at 256.0 MB of 1433.6 MB]`, also after a hook-routed run. A truncated run is never reported as unchanged. After each run, Justless keeps the newest `max_runs` runs within `max_archive_bytes` and deletes older ones. The run just recorded and runs that a live Justless process is still recording are always kept, and runs that a rerun compares against go last. An unchanged rerun links to the streams of the run it matched instead of storing a second copy, unless a background process may still write to the streams of either run. A linked stream counts once against `max_archive_bytes` and stays counted until every run that links it is deleted, so the limit holds for the bytes on disk. The CLI runs offline after installation, except for the opt-in `justless proxy` and `justless wrap claude`, which forward your agent's own API requests. `--raw` skips recording.

Reading the index takes no lock, so concurrent queries over unchanged sources run in parallel. Saving a refreshed index, recording a run baseline and applying edits take a project lock under `.justless/`. When another Justless process holds it, Justless says so on stderr after a second and gives up after 30 seconds, or after the number of seconds in `JUSTLESS_LOCK_TIMEOUT`. A query that gives up still answers from the files it read and leaves the saved index unchanged, and a run that gives up records no baseline.

## MCP server

`justless mcp` serves the read-only commands as MCP tools over stdio. Use it with clients that have no Justless hooks, or when an agent should call navigation tools instead of shell commands. Each tool runs the matching command against the project root and returns its output as text. A failed command or an invalid argument comes back as a tool error that the agent can correct. `run`, `test`, `edit`, `mask` and `init` are not exposed, so every change still goes through the client's own permission prompts.

The server uses `--root` when given. Otherwise it finds the project the way other commands do, searching upward for `.git`, `.justless` or a project marker. The search starts from `CLAUDE_PROJECT_DIR`, which Claude Code sets to its launch directory, or else from the directory the client starts the server in. A server started in a subdirectory therefore shares the project's state with hooks and commands. A command a tool result prints for the agent's shell, such as lookup's `Next:` line, starts with `justless --root ROOT`, so it reaches the same project from any directory. The tool list is kept under 8 KB because clients send it with every model request. Arguments are fully validated when a tool is called.

| Tool | Command | Arguments |
| --- | --- | --- |
| `query` | `query SYMBOL` | `symbol`, `view`, `budget` |
| `path` | `path FROM TO` | `from`, `to`, `budget` |
| `find` | `find TEXT` | `text`, `ignore_case`, `path` and `glob` (a string or a list), `limit`, `after`, `budget` |
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

File arguments must be workspace-relative, and unknown arguments are rejected. The server resolves the project root once at startup. Clients limit the size of a tool result, so the `log` tool returns at most 32 KiB of replayed output per call, with or without a line range. A longer replay ends on a line boundary with a note naming the stream and the `from` line of the next page. A single line longer than a page is cut, and the note gives the shell command that replays it whole. Queries refresh the index under `.justless/`, and each `log` page counts in `stats` as the bytes it returned. Source files are never modified. The `.justless/` directory holds its own `.gitignore` containing `*`, so Git never lists that local state, even in a checkout or nested repository that does not ignore it.

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

For a standalone `sed -n 'A,Bp' FILE` (or several ascending ranges, such as `sed -n '12,107p;209,260p' FILE`), `head -n N FILE`, `tail -n +N FILE` or, in Claude Code only, `tail -n N FILE` on a workspace text file, the command hooks can show a range outline. Claude Code runs the original command and checks the resulting bytes before substituting the outline. Other supported Unix shell clients use a workspace read for eligible files. `tail -n +N` starts at line N (counting from 1) and continues to the end of the file at execution. Small ranges return their original text. A range over 150 lines or 6 KiB can show the symbols it intersects if at least two fit and the result is substantially smaller. The `Exact:` command replays the selected bytes from the archive even after the file changes. `stats` groups these runs under `sed` or `tail` and subtracts replay bytes. `resume` records the line range of a read or outline. Pipelines, redirections, multiple input files and other sed programs use their usual handling.

Claude Code's post-command hook compacts Bash output that the command hook left alone, after the command ran. It handles one supported command, including Claude Code's own `grep` and `find`, written with an environment prefix, a stderr redirection such as `2>&1`, a leading `cd DIR`, a note such as `echo` before it, or a pipe into filters that keep whole lines: `head`, `tail`, `grep` without counts or line numbers, `sort`, `uniq` without `-c`, `cat` and `tee`. The pre-command hook leaves a wrapper as written, because a rewrite would hide the original command from clients that auto-approve. The post-command path treats a wrapped single command as a one-piece list, and the mixed read lists below look through wrappers per piece after the run. A list that runs several supported commands is handled when they all get the same compaction, such as two searches. When the output is larger than both the context budget and the preview budget `run` would give the command, adaptive budgets included, the hook archives it as a run, within `max_run_bytes` like any run, and shows the preview `justless run --hook` gives the same command, ending with the run marker, so `log`, `search`, `history` and `context --run` work as for any run. Secrets are redacted in the replacement, including the error lines it adds and Claude Code's `stderr` field, as in any preview, and `log --unredacted` returns the exact output. The run counts as hook-routed, so `doctor` and the status line count it with the session's other routed runs. Claude Code's shell interleaves stderr with stdout, so a report other than a lint report is followed by the error lines it did not show, such as `error[E0425]: …` or `find: …: Permission denied`, and a test report that names no failing test, such as one whose `FAILED` line a `| tail` cut off, by the panic, assertion and `left:`/`right:` lines, each with its line number in the archive. A result over Claude Code's inline limit is read whole from the copy Claude Code saved in the session's `tool-results` directory. Claude Code would show its first 2,000 characters, so the report gets three quarters of that space and replaces the result only when it fits in the same bytes, and `stats` counts those bytes as the raw size. Results over 20,000 lines or 2 MiB pass through, so the hook finishes well within its timeout. Claude Code runs this hook only for results it treats as successful, so a command that fails reaches the agent unchanged, and the run records the whole command line's exit status: 0, or 1 where Claude Code read exit 1 as a valid result, such as a `git diff` that found changes. A pipe can hide its first command's failure, so `history` and `resume` name such a run by its pipe, such as `cargo test | tail · exit 0 (pipe)`, show the failing summary line of a test run whose failing test names were cut off, and its record notes that the output was captured after the command ran. Comparisons with earlier runs are off for these runs, and they never supersede a run of the plain command. Commands already routed through Justless, file readers such as `cat` and `sed`, filters that reshape lines such as `cut`, `awk` or `wc`, a note such as `echo "exit=$?"` or `&& echo OK || echo FAIL` after the command, searches with context options such as `-A`, `-B` or `-C` or a pattern that matches every line such as `""`, listings filtered by `grep` such as `find . | grep debug`, lists of different tools, command substitutions, subshells, loops, here-documents, redirections of stdout, background and interrupted commands, and output the run would show verbatim all pass through. Claude Code runs every PostToolUse hook on the original Bash output in parallel and keeps the last rewrite, so this hook's rewrite can replace that of another hook on Bash results, such as one that redacts secrets, and `init` warns when the project settings have one. Run `justless init --hooks --agent claude` again to add this hook to an existing setup.

Claude Code also runs mixed read lists as written. A wrapped single command is classified as a one-piece list. For outputs over 4 KiB, Justless can show a separate `$ command` section for each executed piece. It recognizes literal `cat`, numeric `sed -n` selections, `head`, `tail`, and `nl -ba | sed -n`, including downstream `cut -c`, `head`, or `tail`, and looks through `env -u`, `env -i`, `stdbuf` and `bash -c` before classifying a piece. It can also recheck display forms of `git diff`, `git show`, and literal `rg` or `grep` searches. Git uses a disposable index and configuration. A file change, unsupported option, ambiguous pipeline status, or byte mismatch passes the original result through. Short pieces are shown whole. Longer pieces use symbol outlines, grouped search matches, diff summaries, or excerpts. Each header points into the same stdout archive. The single `Exact: justless log RUN_ID` command replays the captured bytes, including the original final newline when Claude kept it. For file-backed results, savings use Claude's inline preview as the baseline.

Claude Code's post-read hook can replace a successful whole text read with the smaller outline. Reads with `allow_large`, which asks for a file or range whole, PDF `pages`, truncated reads and non-text results pass through. A Read with `offset` or `limit` that returns at most 150 lines always passes through, so a Read of the lines an outline points to is never outlined. A wider window of a file the parsers outline, Markdown included, can become the rows that overlap it, under a header that names the revision and the window, such as `window L110-409`. The rows keep their usual IDs, and they drop their signatures when that is what fits the context budget. The hint says that the same Read with `allow_large: true`, which the hook passes through, returns the window's lines as they are, and names `justless expand FILE 'REV#ID'` for one row. `justless expand FILE LA-B` also prints the window exactly, but Claude Code shows a Bash result over about 30,000 characters as a saved file and a preview. The window passes through when its content differs from those lines of the file on disk or from the requested range, when the file has no parser rows, such as plain text or Rust that does not parse, when one row holds more than half of the window, such as a window inside one long function, when more than 20 lines of top-level code in it are in no row, when the rows do not fit the context budget, or when the outline and hint would be more than half the window's size. The threshold comes from 1,234 ranged Reads of workspace files in Claude Code transcripts: the 68% that returned at most 150 lines held 28% of the bytes, and 44% of them were followed by an edit of the file within eight tool calls, against 6% of the wider ones. Like the whole-file outline, the window outline shows signatures from the file Read returned and is not redacted, as `read`, `expand` and the Grep hook are not. The built-in Read executes before the hook, preserving the read-before-edit workflow, and an Edit after an outlined ranged Read works as it does after a native one. Every shell hook can rewrite a simple single-file `cat` for a large workspace text file. Piped commands and multi-file reads pass through. In shell, Python, JavaScript, TypeScript, Ruby, PHP, Lua and SQL files and in Kotlin or Scala scripts, top-level code runs outside any definition. When up to 20 lines of code are in no outline row, such as a Python `__main__` block, the Read hook lists them verbatim with their line numbers after the rows. A file with more, such as a deploy script made of commands, keeps its full read, and a `cat` is rewritten only when every line of code is in a row. Blank lines, comments, docstrings and statements that only bind modules or names do not count: JavaScript and TypeScript `require` bindings, export lists, re-exports and `module.exports` set to names, and Python `from __future__` imports and `if TYPE_CHECKING:`. Agents can invoke `read`, `expand` and `edit` directly in any client. [Claude Code hook output](https://code.claude.com/docs/en/hooks#posttooluse-decision-control)

Claude Code's post-edit hook checks Edit and Write, and MultiEdit, which Claude Code 2.1.327 no longer has, the same way. After a successful call on a workspace file in a checked language within `max_file_bytes`, it parses the file as the tool left it. When the edit left it unparseable, where it parsed before or with a new error that starts in the text the edit wrote, the hook adds a note for the model, such as `Justless parse check: after this Edit, src/lib.rs:1:19: unclosed `{` (Rust parser). The file parsed before it.`, and leaves the tool result as it is. When the error is one that would not block `edit`, such as one a lagging grammar reports for valid newer syntax, the note adds `The parser may not know newer syntax, so check the line before changing it.` A new file counts as one that parsed. Every other edit gets no note. The earlier text comes from the `originalFile` field that Claude Code passes, and it is parsed only when the new file fails. A call the hook cannot judge, such as one without that field, passes without a note, and a failure inside the hook answers `{}`. Run `justless init --hooks --agent claude` again to add this hook to an existing setup.

Compaction replaces earlier tool results with a summary. In Claude Code, the pre-compaction hook asks the summary to keep recent run IDs with their commands and exit statuses, inspected file ranges and recall IDs word for word, and to note what the runs the task depends on showed. After compaction, Claude Code and Codex start the next turn with the same bounded handoff as `justless resume`. Its introduction points the agent to `justless context --run`, `justless search` and ranged `justless log` replays instead of rerunning commands, since replaying a whole log can cost more than the rerun. The handoff runs synchronously, stays within 1,800 bytes including its introduction, and is skipped when the project has no recorded activity. Antigravity CLI and OpenCode do not provide a post-compaction hook. Cursor, Gemini CLI and Copilot only notify hooks before compaction and do not run one afterward. Run `justless init --hooks` again to add these hooks to an existing setup.

Install automatic history retirement with `justless init --hooks --agent opencode` in an OpenCode v1 project. Its plugin transforms the outgoing message view before each model request. The session keeps its original results. The same plugin also compacts a successful Bash result through the post-command hook, leaving a result whose `metadata.exit` is nonzero alone. Set `JUSTLESS_BIN` to point either path at a build that is not first on `PATH`. For the other clients, automatic history retirement requires a separate integration at the request boundary. [OpenCode v1 message transform](https://github.com/anomalyco/opencode/blob/v1.2.27/packages/opencode/src/session/prompt.ts#L621-L640)


Open a new session in the trusted workspace. In Codex, review and trust the project hooks with `/hooks`. Cursor runs project hooks only in trusted workspaces, and Gemini CLI warns before it runs a new or changed project hook. The session-start hook refreshes the repo index. Before supported shell commands run, the command hook routes them through Justless. Each client handles permissions for the rewritten command, and the Claude Code hook first checks the original against Claude Code's permission rules, as described below.

The command hook routes searches and file listings in their display formats: `rg`, `grep -r` or `grep` over several files, `git grep`, `ag`, `ack`, `find`, `fd`, `tree`, `git ls-files`, and `ls` with `-l` or `-R`. A plain `ls`, single-file `grep`, and machine-readable or side-effect options such as `--json`, `-print0`, `-0`, `--null`, `-z` and `find -delete` pass through. Claude Code's shell replaces `grep` with a bundled ugrep that honors `.gitignore` and skips binary files, so the Claude Code hook leaves `grep` to that shell and routes `rg`, `git grep`, `find` and the listing commands. The post-command hook groups a large `grep` result after it ran. Commands that change files, such as `find -delete` or `find -exec`, always pass through.

The command hook also rewrites command lists joined by `&&`, `||` and `;`. Each supported command is wrapped in place, and other commands, operators and spacing are kept, so `cargo fmt && cargo test` becomes `cargo fmt && justless run --hook -- cargo test`. A plain `cd DIR` is allowed when `DIR` has no `..`, `~` or leading `/` and, with symlinks resolved, names an existing directory inside the workspace. Any other change of directory, such as a bare `cd`, `cd -P DIR`, `cd -`, `builtin cd`, `pushd`, `popd`, `source` or `eval`, passes the whole list through, so a wrapped command never runs or records its output outside the workspace. Expansions, pipes, redirections other than a final `2>&1`, subshells, background jobs, comments and multi-line scripts pass through whole, and so do commands the client starts in the background: Claude Code's `run_in_background`, Gemini CLI's `is_background` and Antigravity's `RunPersistent`.

Leading `NAME=value` assignments and a final `2>&1` stay outside the wrapped words, alone or together, in a single command or in each command of a list, including after `cd DIR &&`. `RUST_BACKTRACE=1 cargo test 2>&1` becomes `RUST_BACKTRACE=1 justless run --hook --merge -- cargo test 2>&1`. The variables reach both Justless and the command, and the command words alone choose how the output is compacted. The `2>&1` joins Justless's own output and footer into one stream, and `--merge` joins the command's streams the same way, so the archive and a preview shown in full keep the order in which the command wrote its lines, with its exit status. A value may be quoted but not expanded, so `$`, backticks and a `~` the shell would expand pass the command through. So do assignments to `PATH`, zsh's `path`, `LD_*`, `DYLD_*` and `JUSTLESS_*`, which would change which Justless runs or how it runs, and `env NAME=value COMMAND`. A list passes through whole when one of its commands sets those variables for the commands after it: a command made only of assignments, as in `PATH=/x; cargo test` or `PATH=/x 2>&1 && cargo test`, an assignment before a special builtin such as `:`, or `export`, `declare`, `typeset`, `readonly`, `local` or `unset` naming one of them. Other variables set that way, as in `export FOO=1 && cargo test`, leave the rest of the list to the usual rules. Any other redirection passes the list through, as does a `2>&1` that is not the last word of its command, such as one before `cd`, a bare `cd 2>&1`, `>&2`, `&>`, `|&`, and `2>&1` followed by a pipe or by `&&` without a blank.

Claude Code decides whether exit status 1 is an error from the command text alone. Status 1 is a valid result when the last statement starts with `grep`, `rg`, `egrep`, `fgrep`, `find`, `diff`, `test` or `[`, exactly as written, or with `git` followed by `diff` or `grep` after any options. A routed command starts with `justless`, so the Claude Code hook ends a line whose routed last command is one of these with `&& test 1`: `rg foo` becomes `justless run --hook -- rg foo && test 1`, and `cd web && git diff --exit-code` becomes `cd web && justless run --hook -- git diff --exit-code && test 1`. Claude Code reads `test` the same way, so a search that finds nothing or a `git diff --exit-code` that finds a change reaches the agent as it would without Justless, while status 2 and above is still an error. `test 1` runs only after the command succeeded and then succeeds itself, so the line exits with the command's own status. Claude Code allows the builtin `test` without a permission rule, so the tail asks for no more approval than the rest of the line. A last command with an assignment, such as `LC_ALL=C rg foo`, or one the hook leaves alone keeps its rewrite, since Claude Code reads its status 1 as an error either way. Claude Code's own background tasks also count a status 1 after `&&` as failed, so this does not extend to a routed search that Claude Code moves to the background. Other clients show the exit status as it is and keep the plain rewrite.

Claude Code checks its permission rules against the rewritten command, so a `Bash(cargo test *)` allow rule does not match `justless run --hook -- cargo test`, and neither do ask or deny rules written for the original. When the hook input reports `permission_mode`, the Claude Code hook therefore judges the original command first, so that every mistake errs toward Claude Code's own judgment. It reads `permissions.allow`, `ask` and `deny` from the managed settings files, `managed-settings.json` and then `managed-settings.d/*.json` in `/Library/Application Support/ClaudeCode/` on macOS or `/etc/claude-code/` on Linux; from the user's `settings.json` in `$CLAUDE_CONFIG_DIR`, or else `~/.claude`; and from `.claude/settings.json` and `.claude/settings.local.json` in `$CLAUDE_PROJECT_DIR`, the directory the session started in, or else the workspace root. When the session started elsewhere, the workspace root's own `.claude/` files add their ask and deny rules. A missing file is skipped, and one that exists but cannot be read or parsed counts as a file that might deny. Ask and deny rules match loosely: on any command of a list, through leading assignments and wrappers such as `timeout`, `nice`, `nohup` and `env`, with `*` standing for any text and a `:*` prefix matched without a word boundary. Allow rules count only as `Bash`, `Bash(*)`, an exact command, or a command prefix followed by `:*` or ` *` and matched at a word boundary, and only for a command that has no expansion and starts with its program rather than an assignment, a wrapper, `find` or a change of directory. A managed `allowManagedPermissionRulesOnly` leaves only the managed allow rules. Claude Code also checks the paths that file commands such as `cat` name against `Read` and `Edit` rules and the working directories, which the hook does not model, so while any settings file has a `Read`, `Edit` or `Write` ask or deny rule, or sets `blockReadsOutsideWorkingDirectories` or `blockWritesOutsideWorkingDirectories`, allow rules do not count for a command that starts with a program that reads files, such as `cat`, `rg`, `ls`, `find` or `git`.

| Rules for the original command | `default`, `acceptEdits`, `dontAsk` | `bypassPermissions` | `auto` |
| --- | --- | --- | --- |
| A deny or ask rule might match, or a settings file is unreadable | left as written | left as written | left as written |
| Allow rules cover every command of the line | rewritten, with `permissionDecision: "allow"` | rewritten, with `permissionDecision: "allow"` | rewritten, without a decision |
| Neither | left as written | rewritten, without a decision | rewritten, without a decision |

In `auto` mode Claude Code drops broad allow rules and lets its classifier review the call, so the hook gives no decision there. Plan mode and any mode the hook does not know leave the command as written. Without `permission_mode` the hook rewrites as it always has. A command left as written runs without Justless, and the post-command hook still compacts its successful output. The hook cannot see rules from MDM profiles, server-managed settings, `--settings`, `--allowedTools`, `--disallowedTools` or the current session. Approving a routed command with "Yes, and don't ask again" saves a rule such as `Bash(justless run *)`, which lets the agent run any command as `justless run -- COMMAND` without asking, so `justless doctor` warns about allow rules that start with `Bash(justless` and match such a command. Remove them and allow the commands themselves instead. `JUSTLESS_TEST_CLAUDE_MANAGED_DIR` is internal: Justless's own tests use it in place of the managed settings directory.

Codex approves a rewritten command without asking, so its hook rewrites a list only when every command in it is supported and each one runs a local tool with a conventional target. Its assignments may set only display and diagnostic variables to plain values without a `/`: `NO_COLOR`, `FORCE_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`, `TERM`, `COLUMNS`, `LINES`, `LANG`, `LANGUAGE`, `LC_*`, `RUST_BACKTRACE`, `RUST_LIB_BACKTRACE`, `RUST_LOG`, `RUST_LOG_STYLE`, `RUST_TEST_THREADS`, `RUST_TEST_NOCAPTURE`, `CARGO_TERM_COLOR`, `CARGO_TERM_VERBOSE`, `CARGO_TERM_QUIET`, `CARGO_INCREMENTAL`, `CARGO_BUILD_JOBS`, `PYTHONUNBUFFERED`, `PYTHONDONTWRITEBYTECODE`, `PYTHONHASHSEED`, `PYTHONFAULTHANDLER`, `PYTHONUTF8`, `PYTHONIOENCODING`, `GOTRACEBACK` and `GOMAXPROCS`. Others, such as `RUSTFLAGS`, `RUSTC_WRAPPER`, `NODE_OPTIONS`, `PYTEST_ADDOPTS` or `GIT_EXTERNAL_DIFF`, can run other code or write elsewhere, and builds, test setups and Makefiles branch on `CI` and `NODE_ENV`, so the command passes through to Codex's own approval. `make` reads every environment variable as a make variable, so a `make` command with any assignment passes through, as one with a `NAME=value` argument does. The Cursor, Copilot and Antigravity hooks apply the same limit to assignments, while Claude Code and Gemini CLI check the rewritten command themselves and keep any assignment. Package runners that may download the tool or install packages first, `npx`, `bunx`, `uvx`, `pnpx`, `pnpm`, `yarn` and `bun dlx`, and `uv`, `hatch`, `pdm` and `rye run`, pass through to Codex's own approval, including behind another prefix such as `python -m`. So do `make` and `ninja` targets other than `all`, `build`, `test`, `check` and `clean`, `cmake --build` with another `--target`, Maven goals other than `clean` and the phases up to `verify`, Gradle tasks other than `build`, `test`, `check`, `assemble` and `clean`, build options outside a short display list, such as `make` variables like `VERBOSE=1`, `make -C DIR`, Maven profiles and Gradle properties given with `-P`, `dotnet` targets, properties and package sources, `cargo --config`, `go test -exec`, `mypy --install-types`, and any argument with a path outside the working tree, including one after an `=`, `:` or `@` inside the argument, such as `make -C /elsewhere`, `pytest --basetemp=/tmp/x` or `pytest --override-ini=log_file=/tmp/x`. Runners of tools the project already installed, `pnpm`, `yarn` and `bun exec`, `bundle exec`, `poetry run` and `pipenv run`, are judged by the tool they run. Cursor's hook also answers a rewrite with `permission: "allow"`, so it applies the same checks. Cursor and Copilot do not document whether they ask again for a rewritten command, so their hooks also rewrite a list only when every command in it is supported. Gemini CLI runs the hook before its policy check and confirmation, so like Claude Code it checks the rewritten list. [Codex hooks](https://learn.chatgpt.com/docs/hooks) · [Claude Code hooks](https://code.claude.com/docs/en/hooks) · [Antigravity CLI hooks](https://antigravity.google/docs/hooks?tab=cli) · [Cursor hooks](https://cursor.com/docs/hooks) · [Gemini CLI hooks](https://geminicli.com/docs/hooks/reference/) · [Copilot hooks](https://docs.github.com/en/copilot/reference/hooks-reference)

Cursor, Gemini CLI and Copilot can block a tool call when its hook fails or exits with status 2, so their handlers answer any error with an empty decision and exit successfully. The Copilot hook command ends in `|| true` because Copilot also loads `.github/hooks/` in cloud agent sessions, where Justless may not be installed. Cursor and Copilot CLI also run Claude Code hooks from `.claude/settings.json` by default. Copilot CLI passes a shell call to those hooks as `Bash`, so in a project configured for both Claude Code and Copilot, the Claude Code handler also runs. The rule that only lists of supported commands are rewritten applies to the Copilot handler alone. A command that already runs through `justless run` passes through unchanged, so it is not wrapped twice. [Cursor third-party hooks](https://cursor.com/docs/reference/third-party-hooks)

`justless init` defaults to Codex. Omit `--hooks` to install only the selected client's project instructions and add `.justless/` to `.gitignore`. The `init-codex` alias is also supported. When an instructions file is a symlink to a file inside the project, such as `CLAUDE.md -> AGENTS.md`, setup edits that file once and keeps the link. A link that leaves the project is skipped with a warning, and the hooks are still installed. Setup checks every file before it creates `.justless/`, so a failed setup leaves the project unchanged.

Run `justless run -- cargo test`, `justless run -- git diff`, or `justless run -- rg -n pattern src`. For interactive commands, live streaming, or pipelines that consume raw machine-readable output, use `justless run --raw -- …` or call the command directly. For shell expressions, pass the shell explicitly, as in `justless run -- sh -c 'cargo test && git diff'`.

### Claude Code status line

`justless init --statusline` adds `"statusLine": {"type": "command", "command": "justless statusline"}` to the project's `.claude/settings.local.json` and keeps its other settings. Claude Code runs the command after each assistant message and shows a line such as `justless · net ≈41.2k tokens saved this session · 23 runs · 4 reads`, or `justless · ready` before anything was saved. The line counts what the Claude Code hooks route and replace, so setup says so when they are not installed; `init --hooks --agent claude --statusline` sets up both. Without `--hooks` or `--agent`, the flag writes no agent instructions or hooks. It is not part of `--hooks`, because a status line takes a row of Claude Code's interface and hides some of its footer hints.

The status line goes in the local file because it is personal: Claude Code applies that file to you in this project only. A `statusLine` in the committed `.claude/settings.json` would replace every teammate's own status line and stay blank where `justless` is not on `PATH`. When setup creates the local file in a Git repository that does not ignore it yet, it adds `/.claude/settings.local.json` to `.gitignore`. To share the line with the team anyway, move the key to `.claude/settings.json`. Delete the key to turn it off. In a Git worktree, Claude Code also reads the local file of the main checkout, and a status line set there wins.

Setup never replaces a status line. When `.claude/settings.local.json`, `.claude/settings.json` or your user settings (`~/.claude/settings.json`, or the one in `CLAUDE_CONFIG_DIR`) already set one, it leaves every file unchanged and prints a command that shows both lines from the same session data, with your command on its own line between the braces:

```sh
input=$(cat); printf '%s' "$input" | {
YOUR_COMMAND
}; printf '%s' "$input" | justless statusline
```

For a status line in the local file it suggests that command for the same file. For one in the shared or user settings it suggests a whole `statusLine` for the local file, with your `padding`, `refreshInterval` and other fields kept, so teammates and other projects keep only their own line. Setup reads every settings file before it writes anything. It stops with an error when one of them is not valid JSON, since that file might set a status line, and when the project's settings file is your user settings file, as in the home directory.

The line counts runs that the hooks routed through `justless run` and that finished after the session began, and the Read, Grep and Glob results that the hooks replaced for this session ID, which they log in `.justless/sessions/`. It reads the projects around the session's current and launch directories, and nested repositories where the session recorded runs or replacements, which are listed for the session in the launch project, so the total stays when the session leaves one. After a replay or recall, a wide line shows initial and net savings together. Narrow terminals prioritize the net figure. Net savings are the raw output minus the preview, footer or outline the agent received, minus what later requests printed from those runs, counted as in `stats`, and minus the non-proxy archived tool results that `justless recall` printed during the session, such as the original of a grouped Grep result. When the replays cost more than the previews saved, it reads `net ≈91 extra tokens`. Tokens are estimated as bytes ÷ 4, as in `stats`. A Read that follows an outline is the agent's own read and is not subtracted. The session begins at the first timestamp in its transcript, else when the transcript was created, else `cost.total_duration_ms` before now. Run records do not name their session, so two sessions open in one project at the same time count each other's runs, and a resumed session also counts runs made in the project while it was closed.

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

Code is split along the symbols the index finds, so each function, method, class or struct is one result, and lines outside any symbol form file sections. A symbol's range includes the doc comments and decorators directly above it. Markdown is split at the headings `read` outlines, each section ending at the next heading. Configuration and other text is split at `[section]` headers and every 60 lines. Lockfiles, CSV, minified files and other data files are matched only by name, path and first line, and rank low. Use `find` for their contents.

Words match across naming styles and word forms. `parseHttpHeader`, `parse_http_header` and `parse-http-header` share the words parse, http and header, and the first two also match as one identifier. "retries", "retrying" and "retry" match each other. A match in a symbol's name counts most, then its signature, then its doc comments or the section's parent headings, then its body. Results that match more of the question's words rank higher. Under each result, the first line shown has the rarest question words the header does not already show, and the second adds words the first lacks. Lines longer than 160 bytes are clipped around the words they show. Tests and data files rank lower unless the question mentions tests. Common words such as "where", "how" and "the" are ignored. Text written without spaces, such as Chinese or Japanese, matches only whole runs, so use `find` for it.

At most two results come from one file. Files with identical contents give one result, and its header names the other files after `same in`, which JSON lists as `same`. `--limit` sets the number of results, 8 by default, and `--budget` caps the output. A result that does not fit is left out whole and counted, and `[context budget reached]` follows the counts. The top result always comes first: at a budget too small for it, the word lists shorten and then the closing command is dropped, and a top result too large on its own keeps just its header line. The last lines say how many sections matched, which of your words matched and which did not, and give a command for the top result. That command is `expand` when the whole result fits the budget, and otherwise a 40-line `code` excerpt around its best lines. `--json` returns the same results with scores and line excerpts, and `omitted` counts the results the budget left out. At very small budgets it also shortens the echoed question and word lists, and `words_omitted` counts the words left out.

`--logs` also ranks 40-line windows of output from the 200 most recent runs captured by `justless run`. Log windows are ranked apart from source, with their own statistics. Up to three of them come first and take no places from source results, and the closing command replays the top one with `justless log`, narrowed to the lines shown when the whole window would not fit the budget. A log result names its run, stream, lines and exit status. When both streams have matching windows but every place would go to one, the other stream's best window takes the last place and is listed second, so a budget that fits two windows shows both streams. When windows are left out, the counts line adds them per stream, as in `· log windows stdout 2 of 4, stderr 1 of 1`. `--limit` adds no log windows, so `history` is the way to list every matching line. With `--json`, `logs` gives the matching windows' `total`, the number `omitted` and each stream's `total` and `shown`. The MCP `lookup` tool does not rank logs. Use `history` there.

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

`--list` matches TEXT against the tool name, the call summary in each row and the call's input, ignoring case, so a Grep pattern, a glob or a search query finds its call. Summaries read `PATTERN in PATH` for Grep and Glob and `FILE:OFFSET+LIMIT` for a partial Read. It lists the newest first, 20 by default (`--limit N`). `justless recall ID` also accepts a tool_use ID, and so does the MCP `recall` tool. It prints text exactly as the tool returned it and notes other blocks, such as images. With `--json` it prints the original content value. `--from N` and `--to N` print the exact bytes of a line range, and `--grep TEXT` prints the numbered lines that contain TEXT, up to `--limit`, so a cleared log does not have to come back whole. When the limit leaves matches out, a last line counts them, as in `[20 of 31 matching lines shown; --limit 31 shows all]`. The first copy the proxy saw is kept, and only results from requests the API accepted are archived, so a request rejected for its credentials adds nothing. The index is in `.justless/tool-uses/` and the content in `.justless/observations/`. Archived results can contain secrets from tool output, and each index entry keeps up to 512 bytes of the call's input, such as a command or the text an edit wrote.

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

Archived tool results printed by `justless recall`, last 7 day(s): 9, 41.3 KB back in context. Request-boundary and mask savings do not subtract them.
```

The last line appears once `justless recall` has printed an archived result in the window. Those bytes returned to context after the boundary cleared them, so read the savings above beside them.

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

## Learned command rules

`justless learn` reads this workspace's archived runs and its Claude Code transcripts from the last 30 days (`--days`), including subagent and workflow transcripts. It proposes short rules for command mistakes that agents corrected or keep repeating. It runs locally, makes no model calls and runs none of the recorded commands; the only program it starts is `git check-ignore`, to learn which instruction files Git shares.

Without `--apply`, the report shows what `--apply` would write. An instructions file it cannot read, a learned block with bad markers or an unreadable `.justless/learned.json` becomes a note in the preview; `--apply` stops on it before writing anything.

Each rule comes with:
- failure and correction counts, sessions and agent threads
- the bytes of failed output the mistakes returned to the agent; for a mistake inside a list of commands or an exit-0 result, only its error line counts
- the last date it was seen
- its scope: `project` for facts about this repository, `machine` for facts about this computer's programs and shell
- a check against the current tree, `PATH` or `$SHELL`
- its evidence: the run behind it, so `justless log RUN_ID --stream stderr` replays the original failure, or else the transcript file and line

Add `--json` for structured fields, and `--limit N` (default 8, at most 32) to show fewer rules.

| Pattern | Evidence | Example rule |
| --- | --- | --- |
| Missing program | The shell reports `command not found` and the program is still not on `PATH`. A later command with the same arguments under a related name supplies the alternative. | Use `python3`, not `python`; `python` is not installed on this machine. |
| Wrong directory | Make, Cargo, npm or uv finds no `Makefile`, `Cargo.toml`, `package.json` or `pyproject.toml`, then the same command succeeds in another directory | Run `make` from `server/`; the repository root has no Makefile. |
| Unknown package, script or target | Cargo, npm, Make or Git rejects a name, then the same command succeeds with another | Run `npm run test:unit`; there is no `test` script. |
| Unknown or missing option | The tool rejects a flag or asks for one, then the command succeeds without it or with it | Pass `--config` to `./deploy.py`. |
| Wrong test path | pytest or `node --test` cannot find a path, then succeeds with a path in another directory | `pytest` tests are under `api/tests/`, not `tests/`. |
| Empty test run | `cargo test` runs no tests or pytest exits 5, then a variant runs some | Run tests with `cargo test -p core`; plain `cargo test` here runs no tests. |
| Wrong package manager | npm rejects a `workspace:` dependency or Corepack names the project's manager, then another manager succeeds | Use `pnpm`, not `npm`; this project has pnpm-lock.yaml. |
| Shell quirks | zsh stops a command at an unmatched glob or at a word that starts with `=` | Quote glob patterns in shell arguments, as in `--include='*.rs'`; zsh stops a command at an unmatched glob. |

How rules qualify:
- A rule needs evidence from two sessions (`--min-sessions`). Subagents count toward their parent's session, so one agent fanning out cannot create a rule alone.
- A missing program with an alternative, a wrong directory or a wrong package manager needs one correction when the current tree confirms it, such as a lockfile that names the manager.
- Rules are ranked by how many sessions showed the mistake, with recent sessions weighing more.
- Test failures, compiler errors, searches that find nothing, permission denials, and rejected, interrupted, timed-out or background tool calls are not mistakes. A flag named in a failed test run's output, such as Cargo's `to rerun pass --lib`, is not a missing option.
- A missing `./script` counts only when the command that ran it can be read, so its directory is known.
- A correction of one or two characters is a typo and is not learned.
- A rule is held back when the wrong form later succeeded, or when its check fails now, for example because `python` is now installed. The report counts held rules and how to list them.

What rules contain:
- Program names, flags, script, target and package names, and workspace-relative paths.
- A correction is dropped whole when either command carries a credential-like environment assignment or option, a known token format, a URL, an e-mail address, a host, or a path outside the workspace.
- Nothing is learned from network and credential tools such as `ssh`, `curl` and `aws`, or from a command with a credential, even when its error alone would show the mistake.
- Command lines and output are never printed.

`--apply` writes the proposed rules into a block between `<!-- justless:learned:begin -->` and `<!-- justless:learned:end -->`.
- Without `--target`, it updates each existing `AGENTS.md`, `CLAUDE.md` and `GEMINI.md` once. It skips symbolic links and any file that imports another, as `@AGENTS.md` does at the start of a line or after a space, outside code.
- When none exists, it creates `CLAUDE.md` if the rules came from Claude Code transcripts, and `AGENTS.md` otherwise.
- When it has rules for only `AGENTS.md` but a `CLAUDE.md` here or in a parent directory would stop Claude Code from reading it, the report says so.
- Rules with `machine` scope stay out of files Git shares, tracked or not ignored, because teammates and CI may have other programs and another shell. The report lists them. `--include-machine` writes them anyway, and an ignored file, such as a `CLAUDE.md` listed in `.gitignore`, takes them by default.
- Repeat `--target agents|claude|gemini` to choose the files.
- `--target claude` does not create `CLAUDE.md` next to an existing `AGENTS.md`, because Claude Code then stops reading `AGENTS.md`. Start `CLAUDE.md` with the line `@AGENTS.md` first. [Claude Code and AGENTS.md](https://code.claude.com/docs/en/memory#agents-md)
- The block goes after the Justless instructions block when the file has one, and at the end otherwise.
- Text outside the block keeps its exact bytes, and a second run with the same evidence changes nothing.
- A file with an incomplete or repeated learned block, or one inside the Justless instructions block, stops the run before any file is written.

How written rules are kept or removed:
- Written lines keep their place and wording. New rules are added at the end, and a full block holds new rules back instead of replacing old ones. With `--apply`, the report lists the rules in the block and, separately, those that did not fit.
- A written rule stays when its evidence ages out of the window, because a rule that works stops the failures that produced it.
- It is removed when the wrong form succeeds again, or when its check fails. A failing check against the tree removes it at once. A failing `PATH` or `$SHELL` check removes it only when it still fails on an `--apply` a day or more after the first failure, so one shell with an activated environment does not rewrite the file.
- A removed rule that qualifies again returns to the line it left.
- Only the clone that wrote a rule removes it, so a teammate with a different `PATH` does not change the committed file.
- Delete a rule's line to dismiss it, or to make room in a full block. Justless records the dismissal in `.justless/learned.json` and does not add the rule back.
- Lines you add inside the block are kept.
- The block holds at most 8 rules (`--limit`) and 1,200 bytes. Claude Code removes HTML comments such as the block markers before the file reaches the model.

The run archive keeps each command line in `.justless/runs/*/record.json` so `learn` can pair runs. A command line that looks like it carries a credential, an address or a URL, or runs a network tool, is not stored; `learn` counts such runs as dropped for a possible credential. Runs recorded before this version carry no command line and are skipped, and so are `justless exec` runs, which record a script rather than a command line, and runs whose Justless was stopped before the command finished.

```sh
justless learn
justless learn --days 60 --min-sessions 1 --json
justless learn --apply --target agents
justless learn --apply --include-machine
```

## Script runner

`justless exec` runs a short script that the agent writes to analyze data too large to read, such as a multi-megabyte JSON file, a long log, a CSV export or an API response. Only what the script prints reaches the agent. The script's stdout and stderr are archived like any captured run, so `log`, `search`, `history`, `resume`, `stats` and `context --run` work with its run ID.

```sh
justless exec --lang python3 --file data/issues.json - <<'PY'
import collections, json, sys
issues = json.load(open(sys.argv[1]))
print(len(issues), "issues", dict(collections.Counter(i["state"] for i in issues)))
PY
justless exec --lang sh --file logs/app.log 'grep -c " ERROR " "$1"'
justless exec --script tools/slow_requests.py --file logs/app.log --intent "payments timeouts"
gh api repos/OWNER/REPO/issues | justless exec --lang node 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).length))'
justless exec --lang python3 --timeout 100 --max-bytes 8192 --script tools/report.py -- --top 20
```

Quote the heredoc delimiter, as in `<<'PY'`, so the shell passes the script through unchanged.

### Giving it a script

Pass the script as one argument, as `-` to read it from stdin, or with `--script FILE` to run an existing file in place. A script read from stdin is limited to 1 MiB. Without a script, `exec` stops with an error and does not run what is on stdin. An argument that names an existing file is also rejected, with a pointer to `--script`. An argument that starts with `-` is read as an option, so start such code with a space or pass it on stdin.

`--lang` selects the interpreter: `sh`, `bash`, `python3` (or `python`), `node` (or `js`), `ruby`, `perl`, and `deno` or `bun` when installed. Without it, a `#!` first line that names one of these decides. After that comes the extension of a `--script` file (`.sh`, `.bash`, `.py`, `.js`, `.mjs`, `.cjs`, `.rb` or `.pl`). Otherwise `exec` asks for `--lang`; it never guesses from the code. Justless runs the interpreter it finds on `PATH`, and `python3` falls back to `python` when only that is on `PATH`.

Options on a `#!` line reach the interpreter as they would if the file ran directly, so `#!/bin/sh -e` and `#!/usr/bin/env -S bash -eu` keep their exit status. They are split at spaces, and variables that `env` sets on the line, as in `#!/usr/bin/env PYTHONUTF8=1 python3`, are set too. They apply only when the line names the language that runs. `deno` runs with `run --allow-all --quiet` unless its `#!` line gives options, which then replace those, so a narrower grant such as `#!/usr/bin/env -S deno run --allow-read=./data` holds. `bun` runs with `run` followed by any options from the line.

Inline code is saved as `script.EXT` in the run's directory under `.justless/runs/` and runs from there. The archive therefore keeps the code that produced the output, and a traceback names a file that `justless code` and `justless context --run` can show. The interpreter receives that path relative to the current directory, as in `.justless/runs/run-7Gq2/script.sh`, which shell, Perl and Ruby messages repeat. Python and Node print it as an absolute path. Python does not add the working directory to `sys.path`, so a project file such as `json.py` cannot shadow the standard library. Node resolves packages from the directories above `.justless/`, which include the workspace root. A `package.json` beside the script makes `require` work in projects that declare `"type": "module"`, and Node 22.7 and later also accept `import`.

### Handing it data

`--file PATH` hands a file to the script without the agent reading it. Repeat it for several files. The script receives the absolute paths first, in order, followed by any arguments after `--`. The first path is also in `JUSTLESS_FILE`.

| Language | First `--file` path |
| --- | --- |
| `sh`, `bash` | `"$1"` |
| `python3` | `sys.argv[1]` |
| `node`, `bun` | `process.argv[2]` |
| `deno` | `Deno.args[0]` |
| `ruby` | `ARGV[0]` |
| `perl` | `$ARGV[0]` |

Justless checks that each path exists and reads nothing from it, so paths outside the workspace work. A script given as an argument or with `--script` shares the stdin of `exec`, so piped output reaches it. A script read from stdin, or started from a terminal, gets an empty stdin. The script runs in the current directory with your environment, plus `NO_COLOR=1` when unset.

### Output

Each stream within `--max-bytes`, which defaults to `max_output_bytes` (4096) with a minimum of 512, is shown verbatim. Longer stdout shows whole lines from its start and its end, filling the budget, around one line that names the lines left out and the command that replays them:

```text
slow requests (>20s) by line
130: auth db_deadlock 25017ms request_id=671854443
[justless] stdout lines 38-2140 left out · justless log run-7Gq2 --stream stdout --from 38 --to 2140
total 2174 slow error requests
```

A line too long for its half of the budget is cut on a character boundary, marked with `…`, and counted in the range, which then reads `not shown in full`. Longer stderr gets the usual head, diagnostics and tail preview, so a traceback stays visible. Command profiles and rerun comparisons do not apply, so every run shows its own output.

Like a hook-routed run, `exec` adds a footer only when it adds information:

- nothing when the script succeeded and every byte was shown;
- `[justless run-7Gq2 · exit 1]` when it failed;
- `[justless run-7Gq2 · 4.0 KB of 116.9 KB shown]` when output was clipped or ranked.

Runs appear in `stats` and `history` as `exec python3`, `exec sh` and so on. Runs in the same directory with the same language, `--file` paths and arguments share a command, whatever the code, so in a task pack a later success supersedes the failed attempts before it. `stats` counts the bytes the script printed. The input files never entered context, so they are not part of its savings.

### Ranking by intent

With `--intent TEXT`, stdout that exceeds the preview is replaced by the lines most relevant to TEXT. Justless ranks lines by BM25 over their words. It splits `camelCase` words, folds plurals, `-ed`, `-ing` and a final `e`, ignores common words such as "the" and "what", and treats a double-quoted phrase as one case-insensitive term. Lines that contain every term rank first.

Ranking works line by line, so print one record per line, for example JSON Lines or one `json.dumps` per row, so that each line carries the fields the intent names. When no line holds every term but neighboring lines do, as in pretty-printed JSON, a second header line says so.

The chosen lines appear in log order with `stdout:N` markers, which `justless log RUN_ID --stream stdout --from N --to N` expands. The first and last lines of stdout are kept with them when they fit, because they often hold a title and a total. The last line counts the matching lines not shown, lists the first runs of them, and gives the `justless log` command for the first run. When no line matches, the preview shows the first and last lines with markers. The same output and intent always give the same preview. An intent made only of common words is rejected before the script runs.

```text
[focus] "payments timeouts": 61 of 2176 lines match every term, 589 match some
stdout:1: slow requests (>20s) by line
stdout:9: 685: payments timeout 28314ms request_id=666844578
…
stdout:2176: total 2174 slow error requests
[focus] 5 more match every term (1943, 2038, 2072, …), 589 more match some · justless log run-7Gq2 --stream stdout --from 1943 --to 1943
```

A line longer than 240 bytes shows the parts of it that hold the most terms, the closest together first, so one `print(json.dumps(rows))` still fills the preview with matching records, each part marked `stdout:1:`. The last line then counts the matches the parts leave out. Lines are ranked on their first 4 MiB. When a longer line is present, the preview never says that nothing matched or that every match is shown; it says the lines were not read whole, and `justless search RUN_ID TEXT` searches them in full.

### Time limit and exit status

`--timeout` defaults to 60 seconds, and `0` removes the limit. Keep it below the client's own shell timeout, such as Claude Code's 2 minutes, so Justless can stop the script and keep its output.

When the limit passes, Justless sends SIGINT to the script and every process it started, then SIGKILL 2 seconds later, which also goes to the interpreter itself in case it moved to another process group. It records what was printed, ends with `[justless] stopped at --timeout 60s` and exits with 124. Python prints where it stopped. An interpreter can lose output it had not yet written. The script runs in its own process group and Justless's watchdog in another, so signals the script sends its own group cannot disable the limit.

When the script exits, processes it left running get up to 2 seconds, within the time limit, to finish writing, so the reader of a process substitution such as `> >(sort)` completes its output. Anything still running then is stopped, and `[justless run-7Gq2] stopped processes the script left running; output they had not written is missing` names the run. A plain `cmd &` that the script never waits for therefore adds up to 2 seconds. If Justless itself is stopped, for example by the client's timeout or an interrupt, the script and everything it started are stopped too, and the run stays in `history` as unfinished, with the output so far in `justless log`. A process that starts its own session or process group escapes both. As after `run`, a hidden `justless drain` then stores what it writes later, within `max_run_bytes`, and `log` notes how many bytes arrived after the script exited.

Otherwise `exec` exits with the script's status, or with 128 plus the signal number when a signal ended it. An error before the script starts, such as a missing interpreter or input file, exits with 2 and records nothing. A script that fails always prints a `[justless run-…` line. On Windows there is no process group: the time limit stops only the interpreter, and processes it leaves running are not stopped.

### Not a sandbox

`exec` keeps output out of context. It does not isolate code. Scripts run as you, with your files, network and environment, like any shell command, and they cannot read the terminal.

Hooks pass `exec` commands through unchanged, so the client decides whether to ask, as for any other shell command. A rule that allows every `justless` command also allows every script, so limit allow rules to read-only subcommands such as `justless query`. The MCP server does not offer `exec`.

The archive keeps the script and whatever it printed, so keep `.justless/` private. As for `run`, each stream stores at most `max_run_bytes` for exact replay, and older runs are deleted under `max_runs` and `max_archive_bytes`. A recursive search over the whole workspace, such as `grep -r` from the root, also reads archived runs under `.justless/`, so exclude that directory.

## Secret redaction

`run` replaces API keys, tokens, passwords and private keys in captured output with markers before the agent sees them. The archive in `.justless/runs/` keeps the original bytes, so keep that directory private.

```text
$ justless run -- printenv
…
GITHUB_TOKEN=[redacted:github-token:4f1c]
DATABASE_URL=postgres://app:[redacted:url-password:91d0]@db:5432/app
…

[justless run-8Kq2xZ] exit 0 · 1204 of 1260 output bytes · 2 secrets redacted · justless log run-8Kq2xZ
```

A marker names the kind of secret and a four-character hash keyed by a random value in `.justless/redact.key`:
- The same value gets the same marker in every run, search and handoff in the project. Different values usually get different hashes.
- A marker cannot be used to confirm a guessed value.
- If the key cannot be read or created, markers omit the hash, as in `[redacted:github-token]`.

| Kind | What is replaced |
| --- | --- |
| `aws-access-key` | Access key IDs starting with `AKIA`, `ASIA`, `ABIA`, `ACCA` or `A3T` |
| `github-token` | Tokens starting with `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_` or `github_pat_` |
| `gitlab-token` | Tokens starting with `glpat-`, `gldt-`, `glrt-` or `glptt-` |
| `slack-token`, `slack-webhook` | `xox…-` and `xapp-` tokens, and the secret path of a `hooks.slack.com` webhook |
| `stripe-key` | Live secret and restricted keys, `sk_live_` and `rk_live_` |
| `openai-key`, `anthropic-key`, `google-api-key` | `sk-proj-`, `sk-svcacct-`, `sk-admin-` and legacy OpenAI keys, `sk-ant-` keys and `AIza` keys |
| `npm-token`, `pypi-token`, `huggingface-token` | `npm_`, `pypi-` and `hf_` tokens |
| `jwt` | Signed JSON Web Tokens |
| `private-key` | The body of a PEM or OpenSSH private key, whether wrapped over several lines or unwrapped, on one line with `\n` escapes, inside a string literal, or base64-encoded as in a Kubernetes secret or by `base64`. Body lines may carry a prefix that the `BEGIN` line also has, such as a diff marker, `path:line:` from a search, `cat -n` numbering, a timestamp or terminal colors. The prefixes and the `BEGIN` and `END` lines stay. |
| `url-password` | The password in `scheme://user:password@host` when it has at least four characters |
| `authorization` | The credential after `Authorization: Bearer`, `Basic`, `Token` or `Digest`, and a standalone `Bearer` token |
| `secret` | A value assigned with `=`, `:`, `:=` or `=>` to a name containing password, passwd, passphrase, secret, token, API key, access key, private key or credential, or passed to a flag such as `--password`. The value runs to the next space, quote or backslash, so symbols such as `$`, `@` and `(` are replaced with the rest. It needs at least 12 characters, high entropy, a letter, and a digit, two symbols such as `@` or `$`, or 20 mixed-case letters that are not CamelCase. |

Some values stay visible:
- values containing `EXAMPLE`, `example`, `placeholder`, `changeme`, `dummy` or `your_`;
- runs such as `xxxxxx` or `***`;
- variable references, templates and patterns such as `$TOKEN`, `${TOKEN}`, `{token}` or `[A-Za-z0-9]+`;
- identifiers such as `ML_KEM_512_SHARED_SECRET_LENGTH`, and type names after a bare `:`, such as the `Ed25519PrivateKey` in `private_key: Ed25519PrivateKey`;
- code after an unquoted assignment, such as `hashlib.sha256(x)`, `Option<SecretString>` or `&secp256k1::SecretKey`;
- paths such as `config/secrets/prod-2024` or `/Users/me/.secrets/db.json`, and checksums after a source file name, as in a checksum list;
- checksums labelled with their algorithm, such as `sha256=…`, and `0x` account addresses of 40 hex digits;
- the tag of an image reference such as `docker://ubuntu:22.04@sha256:…`.

Every view of a captured run is redacted:
- the `run` preview, with its grouped test, lint, search and listing reports and rerun comparisons;
- the `exec` preview, including its line outline and `--intent` ranking;
- the Bash result that Claude Code's post-command hook replaces, with the error lines it adds and the result's `stderr` field;
- the footer;
- both complete streams, printed in place of a preview when the run record cannot be saved;
- `log`, `search` and `history`;
- `context --run`, the recorded failures in `pack`, and the log results of `lookup --logs`;
- `resume` and the compaction handoffs.

MCP tools run the same commands, so their results are redacted too, and the server offers no way to turn this off. The run record stores how many secrets were found, and `stats` reports the total and how often originals were replayed.

A hook-routed command whose output was shown in full still ends with a footer when something was redacted, such as `[justless run-8Kq2xZ · 2 secrets redacted]`, so the agent knows Justless wrote the markers and which run holds the originals. A run reported as unchanged carries no count.

Source views are not redacted:
- `read`, `expand`, `code`, `find`, `query`, `context --at`, source lines in `pack`, and Claude Code's Read, Grep and Glob results show workspace files that the agent can also open with its own tools.
- `edit` relies on `expand` returning the exact bytes a replacement will overwrite.
- `recall` returns archived tool results exactly, since they came from the agent's client.

The source lines that an `edit` rejection quotes around a parse error are a diagnostic, not a source view, so they are redacted. The post-edit hook's note quotes no source text.

Redaction replaces only the secret and never adds or removes a line break, so line numbers, `--from` and `--to` ranges, and markers such as `stdout:8-15` match the archive:
- A private key keeps its line count: its first body line becomes the marker and the other body lines become empty.
- Output with nothing to redact replays byte for byte.
- A line that holds both a secret and terminal color codes, including a line of a key block, loses its color codes.
- A marker can make a preview up to 24 bytes longer than `--max-bytes` per secret shown. Output that fits the budget is still shown whole, including output that fits only once a private key collapses to its marker.

Add `--unredacted` to `log` or `search` for the original bytes. Without it, `search` and `history` match the redacted text: searching for a secret's value finds nothing, and searching for `redacted:github-token` lists where tokens appeared. `history` always matches redacted text; find the run there, then replay the range you need.

```sh
justless search run-8Kq2xZ redacted:url-password
justless log run-8Kq2xZ --stream stdout --from 2 --to 2 --unredacted
```

```toml
redact = true
redact_allow = ['^sk_live_0{24}$', 'EXAMPLE$']
```

`redact = true` is the default, and `redact = false` turns redaction off for the project. Each `redact_allow` entry is a regular expression tested against every detected value; a match leaves that value visible.

An invalid expression, or one that matches empty text, stops `run` with an error before the command starts, like `[[filters]]`. Replays of earlier runs with `log`, `search` and `history`, and the compaction handoff, keep working when the configuration cannot be read. They redact with the built-in rules and print a note on stderr. `--unredacted` works whatever the configuration. Commands that take their default `--budget` from `.justless.toml`, such as `history` and `resume`, still need the file to parse unless `--budget` is given.

Test fixtures that look like secrets are redacted too. Two markers with different hashes mean the values differ. To see the original, read the test source, replay the range with `--unredacted`, or add a `redact_allow` entry.

Redaction keeps secrets out of the agent's context by default. It is not access control: these all show the original bytes:
- `--unredacted`;
- `run --raw`, and a command that `run` starts without capture because it cannot create the archive;
- commands that hooks pass through unchanged, such as pipelines;
- reading `.justless/runs/` directly.

These are not detected:
- short or dictionary-word passwords in a plain assignment, unless they appear in a URL or an Authorization header;
- generated passwords with no digit and fewer than two of `! @ # $ % ^ * ? ~ + = |`, under the same condition;
- the last character of an unquoted value when it is `,`, `;`, `:`, `.` or a closing bracket the value did not open, which reads as punctuation;
- cookies, `.netrc` lines and Docker `auth` entries;
- Kubernetes secret values under names without a keyword;
- a keyword broken up by terminal color codes inside the word;
- formats not in the table.

Lines without a trigger word pass a single literal prefilter, and only the lines it hits are matched against the rules. Lines longer than 1 MiB are redacted in windows that overlap by 8 KiB, so memory stays bounded and any secret up to 8 KiB is found wherever the window falls. Replaying a run in which nothing was redacted copies the archive directly, unless a background process could still add to its streams after the scan. `scripts/redact_benchmark.py` measures the cost. [Benchmarks](benchmarks.md#secret-redaction)
