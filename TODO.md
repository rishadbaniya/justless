# TODO

Work parked on 2026-11-19 to save usage. Items are ranked within each section. Byte figures come from the owner's real Claude Code and Codex transcripts (measured locally, labels and sizes only).

## In progress

- Fix three reported problems (one workflow, still running): `context --changes` and `test --affected` miss changes that have no line diff (executable bit, `-diff` files), Markdown sections with `=====`/`-----` underlined headings (an edit of one section deleted the next), and savings that ignore bytes later replayed with `log` (report initial and net savings).

## Parked work on local branches

Unreviewed. Each was stopped mid-build; the last commit on each is a `wip:` snapshot. The first three sit on the pre-redate history (old `53a53ff`), so cherry-pick their own commits onto `main` rather than merging.

| Branch | What it was building |
| --- | --- |
| `worktree-wf_7175cb10-fff-1` | Route and outline shell range reads: `sed -n 'A,Bp' f`, `nl -ba f \| sed -n`, `head`/`tail`/`awk` ranges, `cat f [\| head]`, `git show REV:path`. Small reads byte-exact, wide reads outlined with a one-command exact escape. Measured untouched: 20.8 MB of 93.6 MB Claude Code tool output (`sed -n` alone 11.6 MB), plus ~80 MB in older Codex sessions. |
| `worktree-wf_7175cb10-fff-2` | Outline wide ranged `Read` calls (offset/limit) in the Claude Code post-read hook, with an exact path that cannot loop. Measured untouched: 1,234 reads, 8.5 MB. Has one real commit before the wip. |
| `worktree-wf_7175cb10-fff-3` | Parse check after edits: reject a `justless edit` that leaves the file unparseable (`--allow-parse-errors` to force), and a PostToolUse note when Claude Code's own Edit/Write introduces a parse error. |
| `worktree-wf_c5c37048-0d9-1` | `justless find --path/--glob`, a `--after CURSOR` next page when `--limit`/`--budget` cuts results, and one instruction line saying which search to use (`find` literals, `query` symbols and callers, `lookup` questions, `rg` regexes). |
| `fix/resume-whole-file-reads` | `resume` and the compaction handoff record whole-file `read`s and outlines, not only `expand`. One commit on the current history. |

## Next features

1. **Changed since the last read.** `context` and `pack` remember the revision of each symbol shown in this session; the next read of a changed symbol shows the changed lines with context and an expand command; after compaction or a new session, full context again. A reviewer re-inspecting a test after a one-line change got 2,342 bytes for 46 lines. Judge it by total input tokens and task success over whole tasks.
2. **`justless test --compare REF`.** Run tests at the merge base and in the working tree; group failures as newly failing, already failing (same test and assertion) and fixed. Start with Cargo; rerun only the failing test IDs against the base.
3. **Codex code mode.** Check whether the Codex hook fires for commands inside code-mode `exec` (our matcher is `^Bash$`). 68% of the owner's Codex output since late August ran in code mode, and Codex is `init`'s default client. Then a Codex PostToolUse compactor for piped `rg` and failing output.
4. **Permission parity.** Claude Code checks allow rules against the rewritten `justless run …`, so rules such as `Bash(cargo test:*)` may stop matching and prompt more. Test once by hand, then honor or document.
5. **Generic compaction floor** for unknown commands: collapse `\r` progress frames, elide long base64, fold repeated lines.
6. **`init --global`**: user-level hooks that find the project root per call, with a per-project opt-out. 12.5 MB was lost in sessions whose project had no hooks.
7. **Hook kill switches**: `exclude_commands` in `.justless.toml`, an environment variable to disable routing, and `justless rewrite "<cmd>"` to show what a hook would do.

## Later

- OpenCode: route commands with `tool.execute.before`, compact with `tool.execute.after`.
- Command parity with RTK: `terraform`/`tofu plan`, `gh pr/run view`, `docker compose logs`, Maven/Gradle, `dotnet test`, `swift test`, `rspec`, `phpunit` grouped failures.
- Post-run compaction for Copilot CLI and Gemini CLI; read Codex rollouts in `discover`, `learn` and `stats`.
- Vue, Svelte and Astro components in the index; shebang detection for extensionless scripts.
- Outlines for large reads outside the workspace; compaction and recall IDs for large MCP and WebFetch results.
- Richer `[[filters]]` (keep, max_lines, replace) and user-global filters.
- Structural search, graph analytics (dead code, cycles), more languages (Elixir, Objective-C, PowerShell, Julia, Groovy).

## Known limits from reviews

- `test --affected`: a quiet doctest step is trusted when any doctest ran; MCP tools cannot pass `--base`; several shapes (generic or trait-impl methods, inline-module docs) fall back to the full suite.
- `affected --changes` after a deletion: unlinked paths into external crates other than std/core/alloc show as possible; callers of a `macro_rules!` that names the deleted symbol are not seeded.
- `history TEXT` ranks newest runs first, so an older run's failure line can sit behind newer matches (it is counted in the notice). The MCP search tools cap `limit` at 1000 while the notice can suggest more.
- `--root` given both before and after the subcommand silently uses the second.

## Benchmark and release

- Run the paid pilot (estimate $2.53, hard cap $3): redo `run.py cc-capture` and `parity-diff` with the current binary, `prepare --generate`, `validate-tasks`, `review-tasks --approve`, then `pilot`. The task store lives in the session scratchpad (about 60 GB) and should move to a durable private directory first. See docs/benchmarks.md.
- README does not yet mention `lookup`, `exec`, `learn`, redaction, `test --affected`, `doctor` or the status line.
- Tag `v0.1.0` so installs use prebuilt binaries.

## Do not build

LLM summarization or retrieval, telemetry, client-side rewriting of earlier requests (it invalidates thinking on current Claude models), networked graph databases, bundled embedding models, and fetching filters from the network. Each breaks the local, deterministic, zero-network promise.
