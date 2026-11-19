# Decisions

Ideas and limits that were considered and closed, with the reason. Each reason comes from a measurement of the owner's local Claude Code transcripts on 2026-11-19 (462 transcripts, 35,333 tool results, 103.4 MB: Bash 72.0 MB, Read 27.9 MB; MB = 10⁶ bytes; labels and byte counts only) or from the code. Reopen an item when new data contradicts its reason.

## Do not build

LLM summarization or retrieval, telemetry, client-side rewriting of earlier requests (it invalidates thinking on current Claude models; the opt-in proxy uses server-side context editing instead), networked graph databases, bundled embedding models, and fetching filters from the network. Each breaks the local, deterministic, zero-network promise in the README.

## Already covered by shipped features

| Idea | Covered by |
| --- | --- |
| pytest, go test, jest, vitest and nextest summaries | `summary.rs` test parser and the JavaScript test profile in `filters.rs` |
| Lint and typecheck formatters | `lint.rs` groups by rule for rustc, Clippy, ruff, mypy, pyright, eslint, tsc, golangci-lint, go vet, staticcheck, flake8, pylint, shellcheck, rubocop, swiftlint, hadolint |
| Wrapper coverage | `src/wrappers.rs`; the pre-command hook never unwraps a wrapper on purpose |
| Tree and listing compaction | the listing profile groups find, `ls -R`, tree, fd, `rg --files` and `git ls-files` by directory |
| `discover --plan` | `discover --plan` |
| Search cap per file | the search report's per-file counts, first matches, `+N more in file` and `[stdout:A-B]` tags |
| Build progress and timing lines | the lint report summarizes non-finding output by line range |
| Compiler warning dedupe | `lint.rs` groups rustc and Clippy findings by rule |
| File-level-only diffs | `context --changes` names mode, binary, symlink and submodule changes |
| OpenCode native read-only tool | the OpenCode `justless` tool |
| Paged recall | `recall --from/--to/--grep` |
| Per-family cut thresholds | `adaptive.rs` per-family budgets (2–16 KB), shown in `stats` |
| Transcript replay harness | `scripts/replay.py` |
| OpenCode after-tool compaction | OpenCode `tool.execute.after` for successful results |

## Closed

| Idea | Reason |
| --- | --- |
| Compacting failed or stderr-bearing results | Claude Code's `PostToolUseFailure` accepts only `additionalContext`, so a failed result cannot be replaced; failed Bash results are about 1% of Bash bytes. |
| Rerun delta | 60 repeats, 0.11 MB of 14.6 MB; routed runs already print `Output unchanged since`. |
| Session excerpt dedupe in pack, lookup and context | Superseded by changed-since-last-read; all their output is 0.56 MB. |
| Parameterized failure dedupe | 5 results with 10 or more failing tests, 0.01 MB. |
| Exception folding | Python tracebacks 0.33 MB, other stacks a handful; the generic floor covers the rest. |
| Severity sampling for CI logs | 34 results, 0.08 MB; the logs profile keeps errors with context and folds repeats. |
| `git blame` | 4 results, 0.01 MB. |
| Tabular text from database and cloud CLIs | 8 results, under 0.01 MB. |
| `git log --stat` and `range-diff` views | 35 results with `git log -p`, 0.05 MB; `--stat` is already a summary. `git show` and `git log -p` patches now get the commit summary. |
| Log prefix and timestamp normalization | 34 results, 0.08 MB. |
| Non-CR progress folding | 14 results, 0.03 MB. |
| HTML to text for curl, wget and gh | None of 471 results contained an HTML page. |
| Non-git diffs | 192 results, 0.39 MB, mostly small. |
| Dependency-tree folding | 184 results, 0.24 MB; 0.06 MB over 4 KB. |
| Duplicate blocks inside one result | 0.03 MB. |
| Per-result footer overhead | About 78 B, under 1% of routed output, and the footer carries the only retrieval hint. |
| MCP tool-description overhead | `tools/list` is 7,979 B, but MCP is opt-in. |
| Scoped maps | 73 calls, 0.05 MB, already budgeted. |
| Adaptive budgets for pack, context and lookup | 432 calls, 0.56 MB. |
| Post-run compaction for piped JSON | 0.04 MB; unpiped JSON arrays are handled by the JSON table view. |
| YAML and TOML views | 8 results, 0.01 MB. |
| XML, SOAP and HAR views | 14 results, 0.01 MB. |
| Generated, minified and vendored files | 0.08 MB minified; dependency directories are already out of the index. |
| Moved-block detection in diffs | 0.5% of large-diff bytes. |
| Task-aware diff clipping | A hook sees the command, not the task, so it would guess and could hide the hunk the agent needs; `pack` already selects by task. |
| Test collection output | 5 results. |
| Package-manager resolution logs | 14 results, 0.03 MB. |
| Docker build step folding | 0 results. |
| `kubectl describe` | 4 results. |
| OpenCode retirement tuning | 7 recalls of 345 archived results; nothing to tune from. |
| Proxy clear-threshold tuning | No proxy ledgers locally; measure in the benchmark's context-editing arm instead. |
| Antigravity read-outline parity | No local sessions; the shell `cat` rewrite already outlines large workspace files. |
| Binary-output detection | 8 results; previews already reject NUL and replacement characters. |
| Pointer summaries | Cost bytes on every later request; recalls are 2% of archived results. |
| Range navigation in footers | Replays were 35.7 KB of 847 KB routed; grouped previews already carry `[S:A-B]` tags. |
| Formatter-level adaptive widening | `adaptive.rs` already widens on reopenings, and there were 3 reopenings in 73 runs. |
| OpenCode pre-command routing | Rewriting would make OpenCode's permission patterns see `justless run …`, the problem fixed for Claude Code; the after-tool path already compacts successful results. |
| Command parity (terraform, gh views, compose logs, Maven/Gradle, dotnet, swift, rspec, phpunit) | 102 results, 0.17 MB of 72 MB (0.24%); the generic floor covers the rest. |
| Post-run compaction for Copilot CLI and Gemini CLI | No local sessions, and whether their after-tool hooks can replace output is unverified. |
| Compaction for MCP and WebFetch results | 0 MCP results; WebFetch results are already model-written answers, so only lossy trimming would remain. |
| Richer and user-global filters | `[[filters]] omit` exists and no in-house tool appears in the measured families. |
| Structural search and graph analytics | Neither saves tokens nor fixes correctness, and "possible" links make dead-code reports unreliable. |
| Read deduplication | Re-reads are 1.2% of read bytes; changed-since-last-read covers rereads of changed symbols. |
| Quiet doctest steps and generic or trait-impl fallbacks in `test --affected` | Both fall back safely to broader testing and are documented. |
| Unlinked external paths and macro callers after a deletion | Labelled "possible", and the report already names mentions outside indexed definitions. |
| Claude Code's UI label for routed searches | Display only; the model sees the correct result and no hook output can change the label. |
| Perl grammar | 93 Perl-related Bash results in 472 transcripts, 0.11 MB, and no Perl source reads among the large results. |
| `.mm` files as Objective-C | 209 of 357 real Objective-C++ files (59%) parse with errors in the Objective-C grammar; on average 6.4% of a file's bytes sit in error nodes. |
| `nl -ba FILE \| sed -n` reads | 0 results in 472 transcripts. |
| `awk 'NR>=A && NR<=B'` range reads | 42 results, 0.10 MB. |
| Cargo.lock descriptions inside `git log -p` | Two git lookups per lockfile section per commit; `git log -p` is 0.05 MB in total. |
| Compacting lists that mix `git show` with other git commands after they run | 29 results, 0.11 MB; `git diff` lists already pass through the same way. |
| Prebuilt release binaries | The owner decided on 2026-11-19 that every install builds from source with Cargo; the installer, release workflow and Homebrew formula were simplified to match. |
| Halving the test code | Cut on 2026-11-19 without deleting any protected check: integration tests 34671 → 29470 lines, unit tests 8,368 → 8,063 (before the day's bug fixes added their own regression tests), the ten small suites built as one binary. The rest is unique regression, safety and fixture data, plus rustfmt's 60-column wrapping; `use_small_heuristics = "Max"` would remove several thousand more lines without dropping a check, a formatting choice left to the owner. |
| The floor for heredoc scripts, `ssh` output, loops and lists after running | Replayed on 2026-11-19 over the owner's Claude Code results above 4 KB that the post-run hook leaves alone: the floor saved 28 KB of 10.7 MB (justless, 7 days) and 12 KB of 17.7 MB (prediction_market_workspace, 30 days), and no such result over the 9.6 KB threshold saved 25%. Folding lines identical apart from their digits (timestamped logs) would save about 0.5 MB of those 30 days and is the next candidate if logs grow. |
