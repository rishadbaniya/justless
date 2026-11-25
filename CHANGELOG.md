# Changelog

Notable changes to Justless, newest release first, in the style of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## 0.1.0 (unreleased)

### Command output

- `justless run -- COMMAND` keeps the exit status, shows a bounded preview and archives both streams; `log` replays any line range byte for byte.
- `search RUN TEXT` finds text in one run and `history TEXT` in every run, failures first, and both count the matches they leave out; `search` also hoists shared path prefixes, centers long-line windows, and collapses repeated intra-file matches.
- Structured polyglot test compactors (`cargo_tests`, `js_tests`, `polyglot_tests`, `rspec`, `phpunit`) group failures by normalized root cause (masking ephemeral tempdirs, hex addresses, UUIDs, timestamps, and durations), fold parametrized test families and empty Cargo workspace targets, group pre-test compiler warnings, report rerun diffs, and link every failure to its exact `[stdout:A-B]` / `[stderr:A-B]` lines across `cargo test`, `cargo nextest`, Jest, Vitest, Bun, Playwright, `node --test`, `pytest`, Python `unittest`, `go test` (including races and panics), RSpec, PHPUnit/ParaTest/Pest, `dotnet test`, Maven Surefire/Failsafe, Gradle, Swift (`XCTest` and Swift Testing), CTest, Elixir `mix test`, and `deno test`.
- `cargo test` reports its totals with one line per test binary, also after `2>&1`, and a clean `cargo build` or `clippy` is one line: this repository's 58.7 KB test run shows as 0.7 KB, and `fixtures/failing` compacts from 18,038 B (380 lines) to 517 B (9 lines).
- Compiler and linter findings are grouped by rule with counts and up to four locations across 27+ tools: rustc, Clippy, `cargo doc`, Ruff, flake8, pylint, mypy, Pyright, Ty, ESLint, Biome, Oxlint, `tsc`, ShellCheck, golangci-lint, `go vet`, `staticcheck`, RuboCop, SwiftLint, Hadolint, gcc, clang, swiftc, MSBuild, Maven, Gradle, plus 12 ecosystem linters (`phpstan`, `clang-tidy`, `cppcheck`, `yamllint`, `markdownlint`/`markdownlint-cli2`, `sqlfluff`, `ktlint`, `stylelint`, `deno lint`/`check`, `ansible-lint`, `actionlint`, and `credo`), with clean-build summaries.
- BuildKit and container build step compactor (`buildkit`) collapses Docker, Docker Compose, and Podman BuildKit progress and cached layers while preserving fatal error details (`18.4 KB → 498 B`).
- Universal lossless CLI table compactor (`tableview`) handles ASCII grids, `psql` aligned tables, Unicode box/aligned tables, DuckDB typed tables, and standalone Markdown pipe tables with bijective Tier 1 encoding (hoisting constant and ≥70% majority-default columns) and bounded Tier 2 previews carrying `[stdout:A-B]` elision ranges (`27.5 KB → 4.0 KB`).
- Dependency tree and graph deduplicator (`deptree`) compacts `cargo tree` (forward, `-i`, `-d`), `npm ls`, `pnpm list`/`ls`, `yarn list`, `pipdeptree`, `go mod graph`, `mvn dependency:tree`, and `gradle dependencies`, highlighting errors and multi-version duplicate packages with exact archive pointers (`15.7 KB → 1.5 KB`).
- Searches are grouped by file and listings by directory, with uniform `ls -l` metadata hoisting, ISO timestamp handling, single-child path collapsing, extension breakdowns for dense directories, and context-block search grouping (`rg -n "fn " src`: `357,380 B → 4,058 B`, `88×`).
- Tiered semantic diffs (`summary`, `lockdiff`) summarize `git diff`, `git show`, `git log -p`, `gh pr diff`, `glab mr diff`, and merge commits by commit and hunk, folding whitespace-only hunks, pure renames, generated/snapshot files, and large SVGs, and summarizing 13 lockfiles (`Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`, `uv.lock`, `go.sum`, `poetry.lock`, `Gemfile.lock`, `composer.lock`, `flake.lock`, `Package.resolved`, and `mix.lock`) with checksum/source-substitution flags.
- Enriched Terraform, OpenTofu, and Pulumi plan summaries fold refreshes while surfacing `# forces replacement`, destroys, updates, and outputs (`14.6 KB → 1.3 KB`).
- Extended CLI, DevOps, and Git views (`cli_views`, `inventory`, `gitview`): `kubectl get` (with replica-prefix pod grouping, deployments, daemonsets, replicasets, jobs, and events), `kubectl describe`, Helm releases, Docker/Compose/images inventories, `gh pr checks`, `gh run view`, `gh pr view`/`issue view`/`pr list`, `glab mr view`/`ci`, security audits (`npm`/`pnpm`/`yarn`/`cargo`/`pip` audit), package lists, lossless `git blame`, and compact `git status` (stripping hints and grouping dense dirs), `git log`, `git branch` (prioritizing active/gone branches), `git tag`, `git stash list`, and `git reflog`.
- Bijective deep-schema JSON compaction (`jsonview`) unpacks nested structures, hoists majority defaults, extracts sub-tables with outlier escapes, and verifies round-trip decoding (`gh api` 80 nested objects: `30.2 KB → 3.6 KB`).
- Prefix-aware, JSON-aware, non-consecutive log template folding (`logview`, including on merged streams) and workspace-aware external stack-frame collapsing (`stacktrace`), wired into `run`, post-run compaction, and `exec` (`kubectl logs`: `45.9 KB → 1.1 KB`).
- Previews fold progress frames, timestamped repeats, long base64 and hex runs, and list failures first while keeping progress filters on overflow; reruns show what appeared, changed, or disappeared.
- Hooks route 172+ build, test, check, install, and display command forms (plus monorepo runners, `cargo +toolchain` families, Docker Compose, `git -C`/`--no-pager`/read-only global flags, and safe command wrappers like `env`/`stdbuf`), with build/install progress filters. Commands meant to change remote state or read secrets stay as written, and clients that auto-approve rewrites take only safe local commands.
- Hook-routed runs add a footer only when output was cut or the command failed: on 13 routine commands it adds 3.7% to the output instead of 29.1%.
- Preview budgets adapt to how often each command family's clipped output is replayed, persisting replaced streams and budget metadata while keeping the archive within `max_run_bytes`, `max_runs` and `max_archive_bytes`.
- Secrets such as cloud keys, tokens and private keys are masked in previews, logs, searches and MCP results; `log --unredacted` returns the original.
- `justless exec` runs a script over large data with `run`'s archive, exit status, lossless `jsonview`, `logview`, and `stacktrace` folding; `--intent TEXT` ranks the output lines that match.
- `justless test --affected` runs only the tests the current changes reach across Cargo, pytest, Jest, Vitest, Go, and Ruby (leveraging indexed JS/TS/Ruby test blocks), labeled as a subset, or the full suite with a reason; `test --compare REF` separates new failures from baseline ones, and `--base REF` reviews a branch in `test --affected`, `affected`, and `context`.

Details: [Run output](docs/reference.md#task-packs-and-run-history) · [Affected tests](docs/reference.md#affected-tests) · [Script runner](docs/reference.md#script-runner) · [Secret redaction](docs/reference.md#secret-redaction)

### Code navigation

- A local incremental index of symbols, calls and imports covers 26 code-graph languages (Rust, JavaScript, TypeScript and TSX, Python, Go, Java, Kotlin, Scala, Groovy, C, C++, Objective-C, C#, Swift, Dart, Ruby, PHP, Elixir, Julia, Lua, Zig, Bash, PowerShell, SQL, and HCL/Terraform — plus Vue/Svelte/Astro components and shebang scripts), refreshed on every query.
- `justless review [--base REF]` builds a strictly budgeted blast-radius PR/branch review pack combining semantic diff folding, reverse blast-radius walking, hub and cross-community bridge detection, and per-symbol test coverage-gap detection with suggested `test --affected` commands (also exposed via MCP `review`).
- `map` ranks hub symbols by weighted PageRank centrality, consolidates communities and cross-community bridges, tightens budget density, fixes test-path classification, and adds architectural `map --explain SYMBOL` (CLI and MCP).
- `lookup QUESTION` ranks symbols, Markdown sections, and config blocks, adds natural-language cross-language graph traversal via `lookup --graph` (CLI and MCP), deduplicates overlapping chunks, trims shared indentation, stems `-ion`/`-or`/`-ery` suffixes, and supports `--logs` for recent run output.
- `query`, `path`, `find`, `context` and `code` answer definition, caller, test and dependency questions with file and line coordinates, grouping duplicate callers and deduplicating outgoing `(from :LINE)` edges, skipping stdlib inferred noise, and balancing `query` sections (`query` median `71.3×`, up to `153.9×` vs. Claude Code's 2,000-line Read).
- `find` groups same-file hits, hoists shared directory prefixes, collapses repeats, centers long-line windows, supports `--path` and `--glob`, and prints the command for the next page when clipped.
- `pack "TASK"` gathers definitions, links, source lines, changes and failures within a byte budget and records emitted excerpts in session `seen` state.
- `affected` lists what depends on a symbol, a file or the current changes hop by hop with reaching tests; `context --run RUN_ID` ties a failure to its code.
- In a session, `context` and `pack` re-read a symbol as only its changed lines (512 bytes instead of 2,039 for one changed line); `--fresh` shows it whole, and so does compaction, in every project the session read.

Details: [Indexing and languages](docs/reference.md#indexing-and-languages) · [Map and change impact](docs/reference.md#repository-map-and-change-impact) · [Ranked lookup](docs/reference.md#ranked-lookup)

### Reads and edits

- `read FILE --outline` covers 43 structured outline formats — code languages (including JS/TS/Ruby test blocks, SQL/Rust items, Vue/Svelte/Astro components, and `#!` scripts), Markdown (`h1`–`h6` without signature duplication plus frontmatter, code-fence, and table anchors: `docs/reference.md` `273,594 B → 1,598 B`, `171×`), and 13 config/DevOps/IDL/markup formats (TOML, YAML, JSON/JSONC including root arrays and minified JSON, Makefile/Justfile, Dockerfile/Containerfile, INI/Conf/EditorConfig/Properties, CSS/SCSS/Less, Proto/GraphQL/Prisma IDL schemas, and XML/HTML/MSBuild) — with window import folding, slim polyglot signatures, long Rust `const`/`static` initializer folding, external `mod` run folding, and `read --match` alternation.
- Budgeted directory Tree-sitter code skeletons via `justless read DIR --outline` fairly represent every file inside a byte budget with `REV#N` expand anchors (for example, `justless read src/proxy --outline` compacts 7 files / `112.9 KB` / 3,114 lines down to `4.1 KB`, a `96.4%` cut; full unclipped skeleton `13.1 KB`, `88.4%` cut).
- Cached compiled Tree-sitter queries (`LazyLock`) speed up 40-file outline batches by `>100×` (`159 ms` vs `>15 s`) and 10-file TypeScript outlines by `32×`, while eliminating redundant archive reads in `history`.
- `expand` supports `#A..#B` range expansion, multi-selector and cross-file batch expansion, overlap coalescing, hoisted multi-file prefixes, and session repeat-read delta dedup across CLI and MCP, returning exact bytes.
- `edit` writes a symbol or line range only if the file still matches the revision the agent read; a stale or overlapping edit rejects the whole batch.
- `edit` rejects a batch that would leave a parsing file unparseable, with the parser's line and message; `--allow-parse-errors` writes it anyway. In a crate with `#![feature(…)]`, Rust errors other than unpaired delimiters are reported, not rejected.
- Claude Code's Read hook outlines whole-file and ranged Reads of more than 600 lines (`read_outline_lines`), and shell reads keep their lines up to `read_outline_lines` as well.
- A Read of a file in a nested checkout (such as a Git worktree under `.claude/worktrees/`, a submodule, or a nested repository) is outlined for that checkout so its rows expand there and `resume` lists it there.
- Shell reads of a wide file with `cat`, `sed -n`, `head`, `tail`, `awk` line ranges, or `git show REV:path` show an outline and a replay command; Claude Code read lists also support `cat -n`, literal `echo`/`printf`, `pwd`, `true`, `ls`, `wc`, `git status --short`, `git log`, `grep` (with context, recursion, multiple patterns, counts, file lists, and final `head`), leading plain-word variables, globs, and safe out-of-workspace files.
- In Claude Code, unrecognized commands past the preview budget get a clipped preview rather than waiting for the 9.6 KB compaction floor.
- Outlines list up to 20 lines of uncovered top-level code (keeping the full read if more), and Claude Code and Antigravity CLI post-edit hooks report when a native Edit or Write leaves a previously parsing file unparseable.

Details: [Progressive reads and verified edits](docs/reference.md#progressive-reads-and-verified-edits) · [Shell and Read hooks](docs/reference.md#install-and-configure-agents)

### Tool history and sessions

- `mask` retires older tool results in an outbound request to `[justless recall ID]` references and `recall` restores them verbatim; duplicate tool outputs and superseded file reads or command runs are retired early even inside the recent keep-window, with deterministic secret-safe semantic cues in retired stubs.
- Opt-in live-zone prefix-cache-safe compaction via `justless proxy --compact-live` (or `JUSTLESS_PROXY_COMPACT_LIVE=1`) compacts oversized newest tool results (JSON, logs, tables, tracebacks) by 79.8%–96.6% with 0% prompt-cache invalidation and exact `justless recall` restoration.
- Opt-in `justless wrap claude` runs Claude Code through the local proxy that asks the API to clear old tool results; `justless proxy` serves any Messages API client.
- Claude Code and Codex hooks carry recent run IDs, outcomes and inspected file ranges across compaction within 1,800 bytes, and `resume` prints the same handoff with coalesced same-file ranges and `[source changed]` markers after edits.

Details: [Recoverable tool history](docs/reference.md#recoverable-tool-history) · [Request-boundary context management](docs/reference.md#request-boundary-context-management)

### Agent setup

- `install.sh` builds Justless from source with Cargo (using `git` so SSH `url.insteadOf` rules work) and sets up every supported agent found on `PATH`.
- `justless init --hooks` sets up 7 agents — Claude Code, Codex, Cursor, Gemini CLI, Copilot CLI, Antigravity CLI (`agy`), and OpenCode — with a compact 434-byte instruction block and safe idempotent reruns.
- Antigravity CLI (`agy`) gains two-tier mixed-chain routing, native before/after-read outlining with ranged and repeat-read guards, post-invocation edit parse checks, zero-token savings banners, global hooks, transcript discovery, and MCP install.
- OpenCode v1 plugin compacts native `Read`, `Grep`, and `Glob` tool outputs alongside `Bash`, and automatically retires older, duplicate, and superseded tool results on outbound requests.
- Hooks route safe segments in mixed `&&`, `||`, and `;` chains (including variable expansions, non-steering `NAME=value` assignments, single-quoted commands, and final `2>&1`); clients that auto-approve rewrites (Codex, Cursor, Antigravity) restrict auto-approval to safe local tools and skip exec-hook flags such as `rg --pre` or `go vet -vettool`.
- Claude Code's hook keeps permission rules in force by judging the original command, leaves file-reading commands untouched when `Read`/`Edit`/`Write` deny or ask rules exist, and compacts large unrouted Bash, Grep, and Glob outputs post-run.
- Codex code-mode scripts split lines as lists, route file reads through `justless read`, and preserve `| head -n N` / `sed -n` / `cut` windows with truncation notices; clients without post-run hooks route unpiped JSON reads (`gh … --json`, `kubectl get -o json`).
- `justless init --global` adds opt-in user-level hooks for Claude Code, Codex, and Antigravity CLI that serve every project unless `global_hooks = false` is set.
- `exclude_commands` (matched by shell words), `JUSTLESS_DISABLE=1`, invalid-config fail-open handling, and `justless rewrite "COMMAND"` control and inspect hook behavior.
- `justless mcp` serves 18 read-only commands (`query`, `path`, `find`, `lookup`, `context`, `code`, `read`, `expand`, `pack`, `map`, `affected`, `review`, `log`, `search`, `history`, `recall`, `resume`, `stats`) over stdio in a slim 6.1 KB schema with paged `recall`, path normalization, parity flags, and session read/expand dedup; `mcp --install` configures Claude Code, Cursor, Gemini CLI, VS Code, Codex, and Antigravity CLI.
- `justless doctor` checks the binary, agent CLIs, hooks, instructions, MCP entries, status line, and `.justless/` state, printing an exact fix for every problem.
- `justless init --remove` undoes setup and restores untouched files byte for byte (`--dry-run`, `--purge`, `--global`).
- `justless learn` scans Claude Code, Codex, and Antigravity CLI transcripts (including background task results) to turn repeated command mistakes into project rules via `--apply`.

Details: [Install and configure agents](docs/reference.md#install-and-configure-agents) · [MCP server](docs/reference.md#mcp-server) · [Check the setup](docs/reference.md#check-the-setup) · [Learned command rules](docs/reference.md#learned-command-rules)

### Reporting

- `stats` reports exact bytes kept out of context by day and command, net of later replays and outline follow-up reads, using per-content-type ratios calibrated on Claude's tokenizer (code, JSON, prose, logs); `--sessions` adds Claude Code session token usage.
- `stats --all` aggregates every project on the machine from the local registry across Claude Code, Codex, Antigravity CLI, and proxy clearing, and `log`, `search`, `context --run`, and MCP `log` locate runs across projects.
- `justless statusline` displays session savings with net savings percentage `(XX%)` in Claude Code's status line; `init --statusline` configures it without replacing an existing status line.
- `discover` summarizes unrouted commands across Claude Code, Codex, and Antigravity CLI transcripts as labels and byte counts only (never command arguments or output), with `discover --plan` producing an actionable coverage breakdown.
- Reproducible offline 18-workload compactor benchmark suite (`scripts/suite_benchmark.py`) and dark/light SVG + animated GIF asset generator.

Details: [Run history and stats](docs/reference.md#task-packs-and-run-history) · [Status line](docs/reference.md#claude-code-status-line) · [Projects on this machine](docs/reference.md#projects-on-this-machine)
