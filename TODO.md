# TODO

All work was stopped on 2026-11-19 to save usage; nothing is running. Each item says what is wrong or missing, the evidence, the plan, how far the work got and the next step. Byte figures come from the owner's real Claude Code and Codex transcripts, measured locally (labels and sizes only). Token figures elsewhere in the project are bytes ÷ 4 estimates.

**Progress scale:** *not started* · *designed* (approach decided, no code) · *partial* (code exists, unfinished or untested) · *built* (complete with tests, not independently reviewed) · *reviewed* (an independent reviewer tried to break it and its findings were fixed) · *on main*.

---

## 1. Stopped bug fixes (highest priority)

These three were the last work running. All are on the current history, so they can be rebased onto `main` directly. None has been through the independent reviewer.

### 1.1 Changes with no line diff are missed — high — done
- **Problem.** `context --changes` and `test --affected` build the change set from line hunks only. Removing a script's executable bit (Git shows `100755 → 100644`) or changing a fixture marked `-diff` in `.gitattributes` produced "No changed files"; `test --affected` ran nothing and exited 0 while full `cargo test` failed with Permission denied. A wrong "nothing to test" is worse than running everything.
- **Plan.** Build the change set from Git's file-level listing (`git diff --raw -z` with rename detection, plus untracked files): mode changes, binary and `-diff` files, symlink and submodule changes, renames, type changes and deletions all stay in. `context --changes` says what changed ("mode 100755 → 100644", "no line diff"); `test --affected` falls back to broader testing with a stated reason whenever a changed file has no line diff. Same for `--staged` and `--base`.
- **Where.** `src/context.rs` (change detection, around the `changes()` reader), `src/selection.rs`.
- **Progress.** *on main* (2026-11-19). The change set comes from `git diff --raw` plus untracked files; `context --changes` names each file-level change, and `test --affected` falls back to the full suite with reason `no-line-diff`. Checked end to end: a test script that lost its executable bit now gives the full suite and exit 101, matching `cargo test` (before: nothing run, exit 0). Reviewed by hand, not by an independent reviewer.
- **Note.** The branch's three new tests had failed only because a shared Cargo target directory reused a binary built from another worktree; after forcing a rebuild they pass. When building several worktrees with one `CARGO_TARGET_DIR`, touch `src/` and `tests/` first.

### 1.2 Editing one Markdown section can delete the next — high — done
- **Problem.** A level-1 heading written as a setext heading (`Usage` underlined with `=====`) is not recognized. With `# Install` before it, the outline listed only Install, `expand` of Install included all of Usage, and replacing Install deleted both (the dry run showed it). This destroys user text.
- **Plan.** One shared CommonMark heading scanner used everywhere Markdown is split (outline/expand/edit in `src/source.rs`, the index's Markdown sections, `lookup` chunks, `pack`): `=`/`-` underlines under a paragraph line, `---` after a blank line as a thematic break, front matter skipped, ATX headings with 1–6 `#`, optional closing `#`s, up to 3 spaces of indent and a required space, nothing inside fenced, indented or HTML blocks. A test edits each section of a mixed document and checks every other byte is unchanged.
- **Where.** `src/source.rs` (around line 324), `src/lookup/chunks.rs`, the index.
- **Progress.** *on main* (2026-11-19). One shared CommonMark heading scanner (`src/markdown.rs`) now splits Markdown for outlines, `expand`, `edit` and `lookup`: setext `=`/`-` underlines, ATX headings, nothing inside code fences, front matter or HTML blocks, and a top `---` followed by a blank line read as a rule. Checked end to end: with `# Install` followed by an underlined `Usage`, the outline lists both, `expand` of Install stops before Usage, and replacing Install leaves Usage intact. Reviewed by hand, not by an independent reviewer.

### 1.3 Savings ignore bytes replayed later — medium
- **Problem.** A command printed 19,000 bytes; Justless showed 364 and reported 18,636 saved. After `justless log` returned all 19,000 bytes, the full-read counter went from 0 to 1 but savings stayed at 18,636, though across both responses Justless sent 364 bytes more than the plain command.
- **Plan.** Record the bytes each later retrieval returns (`log`, `search`, `context --run`, MCP log tools, `lookup --logs`) and report initial savings and net savings after replays side by side in `stats`, `stats --json` and the status line; apply the same to the proxy's retire-and-recall numbers.
- **Where.** `src/history.rs` (around line 390), `recalls.json` per run, `src/statusline.rs`.
- **Branch.** `worktree-wf_25a301f5-fae-3` (1 commit `dfd096e fix(stats): report savings net of later replays and recalls`, 14 files, +1,103 −160).
- **Progress.** *built*: complete commit with tests; not reviewed.
- **Next.** Review (old `recalls.json` records, MCP and `lookup --logs` replays, status line wording), then integrate.

---

## 2. Parked work on local branches

Stopped mid-build on 2026-11-19. The last commit on each is a `wip:` snapshot that may not compile. None has been reviewed. The first three branches sit on the pre-redate history (old `53a53ff`, identical in content to current `main` before `0d190cf`), so cherry-pick their commits onto `main` rather than merging. Their measured targets and acceptance rules are in the workflow brief `justless-reads-and-edit-check` (session scripts).

### 2.1 Shell range reads — biggest measured token win
- **Branch.** `worktree-wf_7175cb10-fff-1` (1 wip commit, 11 files, +1,534 −15).
- **Problem.** Agents read files through the shell, and hooks pass these through by design. Untouched in Claude Code (Nov 7–18): 4,079 reads, **20.8 MB of 93.6 MB** tool output. `sed -n` ranges alone were 1,754 results / 11.6 MB (ranges over 150 lines: 384 results / 3.9 MB); piped `cat FILE | …` 241 / 2.2 MB; `git show REV:path` 341 / 2.6 MB. In older Codex sessions: `sed -n` 48 MB, `nl -ba | sed -n` 25 MB, `tail FILE` 7 MB, `awk` 3 MB.
- **Plan.** Recognize single-file read shapes inside the workspace (`sed -n 'A,Bp'`, `nl -ba | sed -n`, `head`/`tail -n`, `awk 'NR>=A && NR<=B'`, `cat FILE [| head|tail|sed -n]`, `git show REV:path`, with `cd DIR &&`). Small reads return exactly what the original prints and are recorded for `resume`; wide reads in a supported language return an outline of that range plus a one-command exact escape (`expand`/`code`) that is never compacted again. Errors such as a missing file surface exactly as before. Rewrite in PreToolUse for all clients, so Codex benefits.
- **Progress.** *partial, about half*: a new `src/shellread.rs` (1,149 lines) with the shape parser and output, wiring in `agents.rs`, `postroute.rs`, `history.rs`, `source.rs`, `statusline.rs`, `discover.rs`, and 237 lines of tests. Not yet compiled clean, measured against the transcripts, checked with the real Claude Code binary, or reviewed.
- **Next.** Finish and compile; byte-exactness tests (trailing newline, CRLF, ranges past EOF, `tail -n +N`, nl formatting); transcript replay for before/after bytes; adversarial review.

### 2.2 Outline wide ranged `Read` calls
- **Branch.** `worktree-wf_7175cb10-fff-2` (1 real commit + 1 wip, 6 files, +656 −9).
- **Problem.** The Claude Code post-read hook returns early whenever `offset`, `limit` or `pages` is set, so 1,234 ranged workspace Reads (8.5 MB) were never outlined; windows of 151–300 lines were 211 reads / 2.1 MB and windows over 300 lines 186 reads / 4.0 MB. The current outline hint tells agents to use Read with offset/limit, so a naive change would loop.
- **Plan.** Above a line/byte threshold and in a supported language, replace a ranged Read with an outline of that window plus an exact path that cannot be outlined again (a smaller window, or `expand`/`code`). Keep `allow_large`, small windows, PDFs, images and files outside the workspace untouched. Validate the replacement and read-before-edit with the real Claude Code binary.
- **Progress.** *partial, most of the way*: commit `a9ae072 feat(hooks): outline wide ranged Claude Code Reads` (agents.rs +132, source.rs +86, 428 lines of tests, a benchmarks note), plus uncommitted follow-ups in the wip. Not reviewed or probed against the real binary.
- **Next.** Finish the wip, run the real-binary probe (`scratchpad/probe/cc_probe.py` pattern), review.

### 2.3 Parse check after edits
- **Branch.** `worktree-wf_7175cb10-fff-3` (1 wip commit, 10 files, +1,211 −22).
- **Problem.** `justless edit` writes a replacement that leaves the file unparseable (reproduced with Rust `pub fn a() -> u32 {\n 1 +\n`), prints success and exits 0; the next outline silently loses functions. Python edits that leave a bracket open do the same. Claude Code's own Edit/Write get no syntax feedback without an LSP plugin.
- **Plan.** Parse before and after each `justless edit` batch (syn for Rust, Tree-sitter ERROR/MISSING nodes elsewhere); reject a batch that introduces a parse error, leaving the file unchanged, with line/column and context, unless `--allow-parse-errors`. Files already broken are not blocked. A PostToolUse hook for Claude Code Edit/MultiEdit/Write adds a one-line note only when the edit introduced a parse error.
- **Progress.** *partial, past half*: new `src/syntax_check.rs` (594 lines), `source.rs` +156, `rust_syntax.rs` +33, the Claude Code hook in `agents.rs` +66, 337 lines of CLI tests and progressive-test updates. Not compiled clean, probed or reviewed.
- **Next.** Finish; test for false rejections across all 21 languages (macros, decorators, templates); measure parse cost on large files; review.

### 2.4 `find` scoping, paging and search guidance
- **Branch.** `worktree-wf_c5c37048-0d9-1` (1 wip commit, tests only: `tests/cli.rs` +487, `tests/mcp.rs` +91).
- **Problem.** Reviewers asked for `--path`/`--glob` on `find`; with 60 matching files and `--budget 512` only five matches appeared, and raising `--limit` changed nothing, with no way to get the next page. The installed instructions do not say which search to use.
- **Plan.** `--path` and `--glob` (rg -g semantics, `!` negation) on CLI and MCP; an empty scope says so; a cut notice naming which limit bound ("budget reached after 5 of 60 files") and a ready `--after CURSOR` next-page command, also in JSON and MCP; one compact instruction line: `find` for literals, `query` for symbols and callers, `lookup` for questions, `rg` for regular expressions.
- **Progress.** *partial, tests first*: the failing tests are written; no implementation yet.
- **Next.** Implement against those tests; measure the instruction block (now about 882 bytes, budget +120).

### 2.5 `resume` misses whole-file reads — done
- **Branch.** `fix/resume-whole-file-reads` (1 commit `5faf9dd`, on the current history, 6 files, +298 −21).
- **Problem.** `justless read worker.py` printed the whole small file, but `resume` reported no activity, even after the file changed; `expand` on the same file was recorded.
- **Plan.** Every path that shows source records the inspected range the way `expand` does: `read` verbatim files, outlines, `code`, `context --at`, `pack` excerpts and the Read hook's pass-through.
- **Progress.** *on main* (2026-11-19): `read`, outlines and Claude Code Read pass-throughs are recorded; `resume` lists `Read FILE:A-B` or `Outline FILE` with `[source changed]` after an edit. Reviewed by hand, not by an independent reviewer.
- **Next.** When 2.1 lands, record shell reads through the same `history::saw_source` / `saw_outline` functions.

---

## 3. Next features (not started unless noted)

### 3.1 Show what changed since the agent last read a symbol — top token idea from a reviewer
- **Problem.** The read → edit → test → inspect loop resends whole functions. A reviewer inspected a real test with `context --symbol`, changed one line, inspected again, and got 2,342 bytes for 46 lines.
- **Plan.** `context` and `pack` remember the revision of each symbol actually shown in the current session; the next read of a changed symbol shows the changed lines with surrounding context and a command to expand the whole function; after compaction or in a new session, full context again.
- **Progress.** *not started*. The pieces exist: revision-bound outlines, the session ID on hook runs, and the activity file that 2.5 extends.
- **Measure.** Total input tokens and task success over complete tasks, not bytes per call (use the benchmark in 5.1).

### 3.2 `justless test --compare REF` — top debugging idea from a reviewer
- **Problem.** An agent cannot tell which failures its change introduced and which already failed on the base branch.
- **Plan.** Run the tests at the merge base and in the working tree; group failures as newly failing, already failing (same test and same assertion) and fixed. Start with Cargo; rerun only the failing test IDs against the base, in a temporary worktree.
- **Progress.** *designed*. Builds on `test --affected --base` (on main) and the grouped test summaries.

### 3.3 Codex code mode
- **Problem.** Since late August, 68% of the owner's Codex output (503 MB of 734 MB overall) ran inside code-mode `exec`, and the Codex hook matches only `^Bash$`. If nested calls bypass the hook, Justless does nothing for current Codex sessions. Codex is `init`'s default client.
- **Evidence so far.** Codex's hooks docs say hook decisions apply to tools called from code mode, but that is untested here.
- **Plan.** Verify with one real Codex session; then route or post-compact nested commands; then a Codex PostToolUse compactor (`decision: "block"` with the compact text) for piped `rg` and failing output, which a replay put at 15.3 MB → 1.2 MB in older sessions.
- **Progress.** *not started* (research done).

### 3.4 Permission parity for rewritten commands
- **Problem.** Claude Code checks permission rules against the rewritten `justless run --hook -- …`, so allow rules such as `Bash(cargo test:*)` may stop matching and users get extra prompts; deny rules may also stop matching. Likely adoption friction.
- **Plan.** Test once in a live session; then either evaluate the user's allow/ask/deny rules against the original command in the hook (as RTK does) or document the `Bash(justless:*)` rule clearly.
- **Progress.** *not started*.

### 3.5 Generic compaction floor
- **Problem.** Ad-hoc scripts are the largest untouched family by count; unknown commands get only a head/tail preview.
- **Plan.** In every preview, collapse `\r` progress frames, elide long base64 runs and fold repeated lines; let the post-run hook apply this floor to unknown commands above a high threshold.
- **Progress.** *not started*.

### 3.6 Lossless compaction for JSON and JSONL output — next new token-saving feature after 2.1
- **Problem.** JSON arrays and JSONL records pay tokens for the same keys on every row, even when every value differs. §3.5 does not cover this.
- **Evidence (external reviewer).** 38 real Cargo targets from `utils` (fields package, kind, name), o200k_base tokenizer, replay pointer included in each: compact JSON 627 tokens, column names printed once 360 (−43%), grouped by package and kind 245 (−61%). All 38 records were rebuilt in order from the grouped output.
- **Plan.** Detect JSON arrays of objects and JSONL streams with shared fields; build a table view (column names once, constant columns lifted to one line) and optionally a grouped view, before clipping; emit it only when it is smaller than the original, otherwise keep the original. Apply only when the output reaches the agent directly: pipes and redirects stay untouched (a documented exception to "machine formats pass through"). Footer with the run ID; `justless log` returns the original bytes.
- **Lossless rules.** Before emitting, decode the view and check it equals the parsed original; on any mismatch show the original. Keep `null` and a missing field distinct, `"1"` and `1` distinct, key order, nested values (as inline compact JSON) and escapes. Irregular or deeply nested data stays as is.
- **Open questions.** Justless has no tokenizer (zero dependencies), so "fewer tokens" means fewer bytes unless one is bundled. Grouping saves more but is harder for a model to read when it must count, filter or quote one record: make the table form the default and enable grouping only if the benchmark (5.1) shows answers stay correct.
- **Measure.** Bytes on real sessions' JSON outputs (`gh api`, `kubectl -o json`, `cargo metadata`, `npm ls --json`, cloud CLIs), then task success, before building more command-specific formatters.
- **Progress.** *designed*.

### 3.7 Dependency-aware diffs for lockfiles, starting with Cargo.lock
- **Problem.** A `git diff` that touches a lockfile is summarized by hunk location, which can cost more than saying which dependencies changed, and still does not answer the review question.
- **Evidence (external reviewer).** Nine real commits from `utils` and `justless`, o200k_base tokenizer, same replay pointer in both formats: the dependency summary was smaller on 7 of 9; choosing the smaller preview per commit cut the total from 2,937 to 2,219 tokens (−24%). On one `utils` commit the current preview spent 282 tokens listing nine hunk locations, while a 147-token summary named all nine added packages and showed that `tokio` gained `signal-hook-registry`.
- **Plan.** For a diff that touches `Cargo.lock`, parse both revisions (`git show REV:Cargo.lock` and the working or target version) and show package additions, removals, version changes and changed dependency links. Always flag a `source` or `checksum` change on an existing version (a supply-chain signal), even when the rest is clipped. Use the summary when it is smaller than the hunk preview; keep the exact patch one command away through the run's `justless log`.
- **Measure.** How often agents reopen the full patch after a summary (`stats` already counts full reopenings per family) before adding `package-lock.json`, `pnpm-lock.yaml`, `uv.lock`, `poetry.lock`, `go.sum` and others.
- **Progress.** *designed*.

### 3.8 `init --global`
- **Problem.** Hooks are per project; 12.5 MB was lost in sessions whose project had no hooks. RTK offers `init -g` for 13 agents.
- **Plan.** Opt-in user-level hooks that resolve the project root per call, with a per-project opt-out, and `doctor`/`init --remove` support.
- **Progress.** *not started*.

### 3.9 Global project registry — machine-wide stats and run lookup
- **Problem.** All state lives in each project's `.justless/`, so nothing sees across projects: `stats` covers one checkout, runs made in a git worktree or nested repository count only there, and `justless log RUN` from another directory answers "unknown run ID". On 2026-11-19 this repo's `stats` showed 5 runs because a session's other work ran in worktrees and scratch folders.
- **Plan.** Keep every project's data where it is (archives can hold secrets; deleting `.justless/` must still remove them; index, `resume` and `[source changed]` are per checkout). Add one small local registry, for example `~/.local/state/justless/projects.json` (honouring `XDG_STATE_HOME`), listing each project root, its repository identity (`git rev-parse --git-common-dir`) and when it was last used. Use it for: `justless stats --all` (one total, worktrees of one repository grouped together); `justless log`/`search`/`context --run RUN` falling back to the registry when the run is not in the current project (run IDs are already unique); `doctor` listing projects with hooks. Exclude temporary folders (`$TMPDIR`, test and benchmark roots) from totals by default, with a flag to include them. `init --remove --purge` drops the project's entry.
- **Constraints.** Local only (no network), private file permissions, never stores run output or command lines (paths and counts only), and safe to delete (it is rebuilt as projects are used).
- **Relation.** Shares the user-level home that `init --global` (3.8) needs; build them together if both are taken up.
- **Progress.** *designed*.

### 3.10 Hook kill switches and diagnostics
- **Plan.** `exclude_commands` (prefix or regex) in `.justless.toml`, an environment variable that disables routing, and `justless rewrite "<cmd>"` that prints what a hook would do.
- **Progress.** *not started*.

---

## 4. Later

| Item | Why | Progress |
| --- | --- | --- |
| OpenCode routing via `tool.execute.before/after` | OpenCode is advertised, but on current Claude models its integration does almost nothing | not started |
| Command parity: `terraform`/`tofu plan`, `gh pr/run view`, `docker compose logs`, Maven/Gradle, `dotnet test`, `swift test`, `rspec`, `phpunit` | users compare on coverage (RTK lists 100+ commands); few bytes locally | not started |
| Post-run compaction for Copilot CLI and Gemini CLI | their hooks rewrite only lists of supported commands, so pipes arrive raw | not started |
| Codex rollouts in `discover`, `learn`, `stats` | cannot see what Justless misses in Codex | not started |
| Vue, Svelte, Astro components; shebang detection | most of that code is invisible to the index | not started |
| Outlines for large reads outside the workspace; compaction for large MCP and WebFetch results | bypass Justless today; MCP bytes unmeasured locally | not started |
| Richer `[[filters]]` and user-global filters | in-house tools need compaction without code changes | not started |
| Structural search, graph analytics (dead code, cycles), more languages (Elixir, Objective-C, PowerShell, Julia, Groovy) | parity with graphify and code-graph MCPs | not started |
| Read deduplication | demoted: redundant re-reads were only 1.2% of read bytes here | not started |

---

## 5. Benchmark, docs and release

### 5.1 Paid benchmark pilot
- **Goal.** A defensible end-to-end number: cost per successful task, justless vs plain Claude Code vs Anthropic's server-side context editing, at equal or better success.
- **Progress.** The four-arm harness is *on main* (`scripts/agent_bench`, 118 tests): Claude Code 2.1.327 parity (65 of 68 tool results byte-identical), 32 frozen tasks from the owner's repos, sandboxed runs, a $90 hard cap enforced per request, pre-registered analysis. Offline validation passed (dry runs with 0 mismatches). The task store (about 60 GB) is in the session scratchpad. Spent so far on paid runs: about $1.10 (the earlier repair benchmark).
- **Next.** Move the store to a durable private directory; redo `run.py cc-capture` and `parity-diff` with the current binary (hooks changed since the capture); re-run `prepare --generate` for the one lint task that exposes a source file; `validate-tasks`; `review-tasks --approve`; then the pilot (estimate $2.53, cap $3). Scale up only if the pilot is sane.

### 5.2 README — done
- **Progress.** *on main* (2026-11-19): the README now names `lookup`, secret redaction, `exec` and `learn`, next to the existing `test --affected`, `doctor`, `init --remove` and status-line mentions. Its measured table still holds: a re-run of the repair benchmark on 2026-11-19 gave 87.2–87.5% fewer input tokens across the four models, within run-to-run noise of the published 87.3–87.5%.

### 5.3 First release
- **Problem.** No tagged release, so every install compiles from source and needs Rust.
- **Progress.** `release.yml` and the checksum-verified installer are *on main*; no tag yet. Tag `v0.1.0` after the in-progress fixes land.

---

## 6. Known limits found in reviews (on main, documented, low severity)

- `test --affected`: a quiet (`-q`) doctest step is trusted when any doctest ran; MCP tools cannot pass `--base`; generic or trait-impl methods, inline-module docs and some other shapes fall back to the full suite (safe, slower).
- `affected --changes` after a deletion: unlinked paths into external crates other than std/core/alloc are reported as possible; callers of a `macro_rules!` that names the deleted symbol are not followed.
- `history TEXT` ranks newest runs first, so an older failed run's line can sit behind newer matches (it is counted in the cut notice). MCP search tools cap `limit` at 1000 while the notice can suggest more.
- `--root` given both before and after the subcommand silently uses the second.
- Routed searches with ` && test 1`: Claude Code's UI label reads "Condition is false" instead of "No matches found" (the model never sees it); a routed search moved to the background still reads exit 1 as failed.

---

## 7. Do not build

LLM summarization or retrieval, telemetry, client-side rewriting of earlier requests (it invalidates thinking on current Claude models; the opt-in proxy uses server-side context editing instead), networked graph databases, bundled embedding models, and fetching filters from the network. Each breaks the local, deterministic, zero-network promise in the README.
