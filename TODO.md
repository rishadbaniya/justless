# TODO

All work was stopped on 2026-11-19; it resumed the same day for the wrapper, OpenCode and coverage work recorded below. Each item says what is wrong or missing, the evidence, the plan, how far the work got and the next step. Byte figures come from the owner's real Claude Code and Codex transcripts, measured locally (labels and sizes only). Token figures elsewhere in the project are bytes ÷ 4 estimates.

**Progress scale:** *not started* · *designed* (approach decided, no code) · *partial* (code exists, unfinished or untested) · *built* (complete with tests, not independently reviewed) · *reviewed* (an independent reviewer tried to break it and its findings were fixed) · *on main*.

## Today — 2026-11-19

`main` is at `d01440e` plus this pass's setup commit, fourteen commits ahead of `origin/main`. Three stacked branches are merged into it: `feat/wrapper-coverage`, `feat/top5` and `feat/coverage`.

**What this pass was trying to do.** Reduce what coding agents spend on shell output by fixing reach and measurement rather than adding formatters: let the parsers see `cd DIR && …` lists, rank the remaining backlog by measured bytes (`discover --plan`), and stop paying fixed per-request overhead. The first two shipped (`7a96af7`, `d01440e`); the third measured under 1% of routed output, so it was closed without a change. Replay saves 287,973 of 329,112 eligible bytes (87.5%), which is 7.1% of all Bash output in this project's corpus and 29 of 1,867 results. Honest overall rating: 7/10. The engineering and safety model are strong; end-to-end agent savings are not yet proven, because the provider benchmarks are n=1 on a synthetic task and the installed binary can lag the source.

**Shipped.** Wrapper-aware post-command compaction for lists and single commands; `src/wrappers.rs`; the metacharacter and 16 KiB guard; single-piece pipelines and `cat` singles; OpenCode `tool.execute.after` and the read-only `justless` tool; `scripts/replay.py` (1,867 results; after the `cd` fix, 29 compacted results save 287,973 of 329,112 eligible bytes, 87.5%); `cd DIR && …` list prefixes as a piece base directory (`7a96af7`); `discover --plan` to rank unrouted families by output and mark what compacts now.

**Decisions.**
- Pre-command rewriting never unwraps a wrapper. Doing so would hide the original command from clients that auto-approve, so `env RUST_BACKTRACE=1 cargo test` and `git -C . diff` stay as written.
- Failure-aware Claude is blocked: PostToolUse carries no exit status and failed results are not rewritten. OpenCode compacts only results whose `metadata.exit` is 0.
- Batch rerun comparisons stay off: a truncated archive cannot prove full equality. Routed runs still print `Output unchanged since`.
- Polyglot affected tests and the HTTP/SQL cross-language links already existed; `fixtures/mixed` proves both, so no code was added for them.
- Of the ten-item plan, #1, #2, #4 (partial) and #10 shipped, and #10 now includes `cd DIR && …` prefixes. #3, #5, #6, #7 and #8 remain open.
- The `cd` fix was worth more than a new formatter: the 4.28 MB `cd` pool was unreachable because `list` rejected the `cd` piece. `discover --plan` on this checkout now marks 3.8 MB of sed-family output and 1.5 MB of grep-family output as compactable with a hook or `run --`.
- Rerun delta stays unbuilt: measured 60 repeated calls / 0.11 MB out of 14.6 MB, so #3 is no longer a top-five item.
- Fixed per-request overhead (#5) is closed without a change: a navigation footer is about 78 B and a recall placeholder 82 B, which is under 1% of routed output, and the footer carries the only retrieval hint. Trimming it would trade navigation for tens of kilobytes.
- The before-run hook now leaves a `cd`-prefixed list alone instead of routing it, because the after-run compactor accepts it. `tests/cli.rs` records that for `cd src && rg foo`; unsupported forms such as `git diff --exit-code` still route.

**Hook deployment audit (2026-11-19, after the commits).** The hook definitions are correct; the deployment is not, and it explains most of the small measured win.
- Codex has no Justless hooks: `~/.codex/hooks.json` holds only graphify's `PreToolUse`, and `doctor` reports "Codex is on PATH but not set up here".
- Claude hooks exist only in this repository's `.claude/settings.json`; user-level `~/.claude/settings.json` has none, so other repositories get nothing.
- Clients read hooks at session start. The 7-day transcript holds 13,900 Claude Code commands but only 620 routed (4.5%) and 63 recorded runs; that session began before the hooks were installed.
- The hooks invoke `~/.cargo/bin/justless`, built at 02:32 today rather than from `d01440e`, so the `cd` fix, `discover --plan`, read tool and retirement were not running. Nothing checks the binary's revision.
- Even where hooks were installed, `discover` reports 2,295 supported commands (3.6 MB) passed through (`git diff` 1.3 MB, `git show` 863 KB, `git log` 492 KB, `cargo test` 234 KB) plus 451 supported commands (1.6 MB) not routed.
- **Next:** install at user level and for Codex (`init --hooks` merges), reinstall the binary from `main`, add a stale-binary check to `doctor`, restart the client, then inspect the passed-through `git`/`cargo` forms. Do not quote the 87.5% replay figure as a session-wide win until a hooked session is measured.

**Why this file is long.** §8 is the 50-idea token backlog; §9 and §10 are branch logs kept for provenance.

## Review queue for Claude Code — 2026-11-19

Everything below is committed on `main`; the merge is `24a8984`, the follow-ups are `7a96af7`, `d01440e` and `616f0c7`, and the hook audit above is documentation only. Review the code, not just this summary.

- `2afa5ea` wrapper lookup for read lists — `src/wrappers.rs`, `src/batch/parse.rs::piece`, `src/filters.rs`; check the pre-command permission contract (no wrapper rewriting, no PATH/LD stripping for auto-approve clients).
- `d3727fe` docs correction for wrapper coverage.
- `5c267f1` wrapped single commands as one-piece lists — `src/batch.rs::parse`, `tests/read_batch.rs`.
- `8e07e1b` `src/wrappers.rs` extraction and the single-command metacharacter/16 KiB guard — `src/batch/parse.rs::literal`, `single`.
- `5477565` OpenCode after-tool compaction — `integrations/opencode.js` `tool.execute.after`, `metadata.exit` guard; verify `JUSTLESS_BIN` and a failed result staying raw.
- `cbb0f10` and `3d68e4b` TODO logs; no code.
- `20dbad2` one-piece pipelines and `cat` singles — `list` now accepts one piece, `single` allows `cat`; confirm `sed`/`head`/`tail` singles still use the plan path.
- `74929a3` OpenCode read-only tool — review the `readonly` list and the in-`execute` command guard.
- `24a8984` merge — the only conflict was `TODO.md`; the main branch log was kept and §10 added.
- `7a96af7` `cd DIR && …` list prefixes — `src/batch/parse.rs::change`/`enter`, `src/batch.rs` resolves each piece's directory inside the workspace and reopens Git per directory; `tests/read_batch.rs` and `tests/cli.rs`. Check that backtracking, `.git`, outside-root and `||` lists stay raw, and that the canonical root check does not read through a symlinked temp path.
- `d01440e` `discover --plan` — `src/discover.rs::PlanEntry`, `src/main.rs`; verify the output has labels and byte counts only, never command text.
- `616f0c7` setup — `AGENTS.md`/`CLAUDE.md` instruction block, `.claude/settings.json`, `.gitignore`; confirm the hooks call a binary that exists and that `settings.local.json` stays untracked.

Verification already run on the merged tree: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, 248 CLI tests, 10 batch tests, 212 binary tests, and `scripts/context_benchmark.py`. Replay: `python3 scripts/replay.py --binary ./target/debug/justless`.

Known open after this queue: #3 batch rerun delta (measured small), #6 watch/incremental index/daemon, #7 `explain` and graph exports, #8 session ledger. #5 fixed per-request overhead is measured and closed without a change. An uncommitted `src/setup.rs` draft was removed; no `setup` command shipped.

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

### 1.3 Savings ignore bytes replayed later — medium — done
- **Problem.** A command printed 19,000 bytes; Justless showed 364 and reported 18,636 saved. After `justless log` returned all 19,000 bytes, the full-read counter went from 0 to 1 but savings stayed at 18,636, though across both responses Justless sent 364 bytes more than the plain command.
- **Plan.** Record the bytes each later retrieval returns (`log`, `search`, `context --run`, MCP log tools, `lookup --logs`) and report initial savings and net savings after replays side by side in `stats`, `stats --json` and the status line; apply the same to the proxy's retire-and-recall numbers.
- **Where.** `src/history.rs` (around line 390), `recalls.json` per run, `src/statusline.rs`.
- **Progress.** Implemented and verified on 2026-11-19. Run stats report initial savings, bytes returned, and signed net savings. MCP charges the delivered log page. Proxy stats subtract recalls from the bytes marked for clearing and keep API-reported token counts separate. Regression tests cover full and partial reads, mixed source and log output, and older records.

---

## 2. Parked work on local branches

Stopped mid-build on 2026-11-19. The last commit on each is a `wip:` snapshot that may not compile. None has been reviewed. The first three branches sit on the pre-redate history (old `53a53ff`, identical in content to current `main` before `0d190cf`), so cherry-pick their commits onto `main` rather than merging. Their measured targets and acceptance rules are in the workflow brief `justless-reads-and-edit-check` (session scripts).

### 2.0 Batched reads in command lists — top token-saving item
- **Problem.** Agents batch several reads into one Bash call to save turns, e.g. `git diff --stat; sed -n '12,107p;209,260p' src/shellread.rs; sed -n '38,44p' TODO.md | cut -c 1-210; rg -n 'X' docs/reference.md | cut -c 1-300`. The pre-hook skips lists with pipes, the post-run hook handles a list only when every command gets the same compaction, and the `sed` outline (4c690c2) covers only a standalone `sed -n 'A,Bp' FILE`.
- **Evidence (measured 2026-11-19, local Claude Code transcripts, 15.6 MB of Bash output).** Lists of commands containing a read: 5,312 calls, 11.3 MB (72%), of which 6.0 MB in outputs over 4 KB. Single read commands: 209 calls, 1.1 MB (7%). One read piped into `head`/`cut`: 466 calls, 1.0 MB (7%). No file or search read: 1,035 calls, 2.2 MB (14%).
- **Plan (Claude Code post-run hook, command left exactly as written, so permissions are unaffected).** Split the list into its top-level commands. Reproduce each read's output: pure file reads (`sed -n` with one or several ranges, `head`, `tail`, `cat`, `nl -ba | sed -n`, plus trailing `| cut -c`, `| head -N`, `| tail -N`) from the file itself; read-only commands (`git diff`, `git show`, `rg`, `grep`) only if they can be re-checked without side effects. Accept the split only if the combined output equals the reproduced pieces exactly, byte for byte; otherwise leave the whole result untouched. Then compact each piece with its own engine (range outline, search grouping, diff summary), show each under a `$ command` header, archive the original as one run with per-piece line ranges, and print one exact-replay command.
- **Estimate (not measured).** If the 6 MB in large outputs compacts like other reads (80-90%), about 4-5 MB, roughly a third of all Bash output. Small, already-trimmed lists pass through unchanged.
- **Limits.** Claude Code only (Codex has no post-run hook). Medium effort: the outline, post-run hook and compaction engines exist; the reproduce-and-verify step is new.
- **Progress.** Implemented for Claude Code. The example on the clean `e2c34e6` checkout took 6,505 bytes inline and 1,788 bytes with the hook (72.5% less), including the replay command. The archive replayed the original inline result byte for byte. Workload-wide savings still need transcript replay.

### 2.1 Shell range reads — biggest measured token win
- **Update 2026-11-19.** On main now: standalone `sed -n` with one or several ascending ranges, `head -n N`, `tail -n +N`, and in Claude Code `tail -n N` (the pre-hook skips it because a growing file could change which lines it prints). Wide reads become a range outline with an exact replay; small ones keep their native bytes. Still open: reads inside command lists (2.0), `cat`, `nl -ba | sed -n`, `awk` ranges and `git show REV:path`.
- **Branch.** `worktree-wf_7175cb10-fff-1` (1 wip commit, 11 files, +1,534 −15).
- **Problem.** Agents read files through the shell, and hooks pass these through by design. Untouched in Claude Code (Nov 7–18): 4,079 reads, **20.8 MB of 93.6 MB** tool output. `sed -n` ranges alone were 1,754 results / 11.6 MB (ranges over 150 lines: 384 results / 3.9 MB); piped `cat FILE | …` 241 / 2.2 MB; `git show REV:path` 341 / 2.6 MB. In older Codex sessions: `sed -n` 48 MB, `nl -ba | sed -n` 25 MB, `tail FILE` 7 MB, `awk` 3 MB.
- **Plan.** Recognize single-file read shapes inside the workspace (`sed -n 'A,Bp'`, `nl -ba | sed -n`, `head`/`tail -n`, `awk 'NR>=A && NR<=B'`, `cat FILE [| head|tail|sed -n]`, `git show REV:path`, with `cd DIR &&`). Small reads return exactly what the original prints and are recorded for `resume`; wide reads in a supported language return an outline of that range plus a one-command exact escape (`expand`/`code`) that is never compacted again. Errors such as a missing file surface exactly as before. Rewrite in PreToolUse for all clients, so Codex benefits.
- **Progress.** Single-file literal `sed -n 'A,Bp' FILE` and `tail -n +N FILE` are implemented. Open-ended tail reads preserve an unterminated final line and use the end of the file at execution. Verified with the Codex hook and real Claude Code, including persisted output, exact historical replay, and net savings after replay. On the generated Rust fixture Claude received 1,197 bytes for a 4,498-byte range. Reopening all 4,499 native bytes made the net cost 1,198 extra bytes.
- **Next.** Use transcript replay to measure workload-wide savings. The parked branch also covers `nl`, `head`, last-N-line `tail`, `awk`, `cat`, `git show` and command lists. Bring those forms forward separately and check their native byte behavior.
- **Command-list follow-up.** Test inspection batches that combine `git diff --stat`, a multi-range sed expression, and `rg` piped to `cut`. Both hooks currently pass the tested batch through. Compare automatic handling against `justless exec --lang sh -` on the same script. Preserve shell quoting, the status of the final pipeline, and measure bytes after the pipeline's own filters.

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

### 3.4 Permission parity for rewritten commands — confirmed, design ready, halted
- **Problem (confirmed in code on 2026-11-19).** The Claude Code PreToolUse handler (`client_command` in `src/agents.rs`) rewrites supported commands to `justless run --hook … -- CMD` but sets no `permissionDecision` (only the Codex branch does), and `init` adds no allow rule. Claude Code checks permission rules against the rewritten command, so in default mode a `Bash(cargo test:*)` allow rule stops matching and every routed command prompts as `justless run …`. Approving with "don't ask again" stores a rule like `Bash(justless run:*)`, after which the agent can run any command as `justless run -- <cmd>` without asking. Ask and deny rules on the original command stop matching too. The docs do not mention any of this.
- **Design.** Read `permissions.allow/ask/deny` from managed, user (`$CLAUDE_CONFIG_DIR` or `~/.claude`), project and local settings; an unreadable file counts as "might deny". Evaluate the original command's top-level commands: allow strictly (`Bash`, exact, `prefix:*` at a word boundary only), deny/ask liberally (through `NAME=value`, `timeout`, `nice`, `env`, globs). Deny or ask: no rewrite, in every mode. Allow: rewrite with `permissionDecision: allow`. No match: rewrite as today in `bypassPermissions` and `auto`, no rewrite in other modes (the post-run hook still compacts successful output; failed output stays full because hooks cannot rewrite a failed result). `permission_mode` absent: today's behaviour. `doctor` warns about allow rules that start with `Bash(justless`. Isolate tests from real settings with `CLAUDE_CONFIG_DIR` and a test-only managed-path override.
- **Progress.** *designed*. Two attempts were stopped to save usage before any code was written. Use the official docs (https://code.claude.com/docs/en/settings and the permissions page) for rule syntax; the fail-safe design does not need the binary reverse-engineered.
- **Interim note for users.** Do not allow `Bash(justless:*)` or `Bash(justless run:*)`: it lets the agent run anything through `justless run`.

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
- Routed searches with ` && test 1`: Claude Code's UI label reads "Condition is false" instead of "No matches found" (the model never sees it); a routed search moved to the background still reads exit 1 as failed.

---

## 7. Do not build

LLM summarization or retrieval, telemetry, client-side rewriting of earlier requests (it invalidates thinking on current Claude models; the opt-in proxy uses server-side context editing instead), networked graph databases, bundled embedding models, and fetching filters from the network. Each breaks the local, deterministic, zero-network promise in the README.

---

## 8. Token-usage ideas only (unplanned, ranked)

None of these is planned in the sections above. Order is expected bytes kept out of context, not measured savings; size each with transcript replay before building. Effort: S (small), M (medium), L (large).

1. **Failed and stderr-bearing command compaction.** A non-empty stderr skips compaction today, yet failing builds and tests are usually the largest results. Compact stdout and stderr separately after verifying each, keep the exit status, and archive one exact replay. (M)
    - **Status.** OpenCode's after-tool hook now compacts a successful Bash result and skips one whose `metadata.exit` is nonzero. Claude Code's PostToolUse carries no exit status and does not rewrite a failed result, so the Claude half stays open.
2. **Rerun delta.** When a command reruns, compare against its previous run in the same directory and session: show only what changed, or a pointer when the output is identical. Test and build loops rerun dozens of times. Routed runs already print `Output unchanged since`; after-bash batch archives skip rerun comparisons because a truncated record cannot prove equality. (M)
3. **Session excerpt dedupe in `pack`, `lookup` and `context`.** Do not resend a file chunk or section already shown this session; point to the earlier run or range. This is excerpt-level, not the demoted Read dedupe. (M)
4. **pytest, `go test`, jest/vitest and `cargo nextest` summaries.** The §4 parity list names rspec, phpunit, Maven, Gradle, dotnet and swift but not these, which dominate polyglot repositories. Group by test file, keep failures and counts. (L)
5. **Lint and typecheck formatters.** ruff, mypy/pyright, eslint/tsc/biome and golangci-lint: deduplicate by rule and location, show counts per rule, keep the first occurrence with code. (L)
6. **Parameterized failure dedupe.** Two hundred cases failing the same assertion become one representative failure, a count and the case IDs. (M)
7. **Language-aware exception folding.** Collapse Python chained exceptions, Rust backtraces and JS stacks to the inj! point plus frames grouped by file/crate. §3.5 only folds identical lines. (M)
8. **Severity sampling for CI logs.** Keep errors and warnings with N context lines, elide info noise with a count. Samples by severity rather than folding repeated bytes. (M)
9. **`git blame` compaction.** Group consecutive lines by commit, one header per commit, total counts. Currently unsupported and often the largest git output. (S)
10. **Wrapper coverage.** `git -C DIR`, `bash -c '…'`, `env VAR=1 …`, `timeout 60 …`, `nice`, `command` and `time` prefixes currently stop routing, so large git and test outputs arrive raw. Unwrap them for the same filters. (S)
    - **Branch `feat/wrapper-coverage`.** Post-command compaction now looks through `env` (including `-u` and `-i`), `time`, `nohup`, `command`, `nice`, `timeout`, `stdbuf`, `bash -c` and git's global options, for a single command as a one-piece list and for each piece of a mixed read list. The recorded command keeps the original words for replay. Wrapper parsing lives in `src/wrappers.rs`; the single-piece path now applies the same metacharacter and 16 KiB guard as multi-piece lists, so substitutions, globs and redirections stay unparsed. Pre-command rewriting stays strict: a rewrite would hide the original command from clients that auto-approve, so `env RUST_BACKTRACE=1 cargo test` and `git -C . diff` remain unwrapped. Tests cover `timeout`, `env -u`, `env FOO=1`, `nice`, `bash -c`, wrapper options, unsafe wrappers and both list and single forms; unit tests cover the unwrap itself. (M)
11. **Tabular text compaction.** psql, sqlite3, csv/tsv, aws and gcloud tables: column names once, constant columns lifted, aligned columns dropped. Lossless sibling of §3.6. (M)
12. **Multi-commit history views.** `git log -p`, `git show --stat`, `git range-diff`: summarize per commit, keep patches one command away. (M)
13. **Tree and listing compaction.** `find`, `ls -R`, `tree` and `rg --files`: group by directory with counts, keep only interesting entries. (M)
14. **Log prefix and timestamp normalization.** kubectl, journalctl and systemd logs repeat timestamp, pod and level on every line; strip to a header and deduplicate. (S)
15. **Non-CR progress folding.** pip/npm/docker dots, percentages and spinner lines that never use `\r`. §3.5 covers only carriage-return frames. (S)
16. **HTML to text for `curl`, `wget` and `gh`.** TODO covers WebFetch, not shell HTTP fetches, which often dump whole pages. (M)
17. **Non-git diff compaction.** `diff -u`, `patch` output and `gh pr diff`: file-level summary plus hunks. (M)
18. **Dependency-tree folding.** `cargo tree`, `npm ls` and `pnpm list` in text mode: one line per repeated subtree, counts. JSON is §3.6. (S)
19. **Duplicate content inside one result.** The same file printed twice in a batched command becomes a pointer on the second copy. (S)
20. **Per-result overhead.** Shorter replay footers, run IDs and cut notices; at thousands of results the fixed text is a measurable share. (S)
21. **Per-request overhead.** Deduplicate the instruction block across AGENTS.md and CLAUDE.md, and shorten MCP and plugin tool descriptions. Paid on every request, not per result. (S)
22. **Scoped maps.** `map --changed`, `--since REV` and `--path DIR`, with top hubs as the default; a monorepo map is otherwise 10–20 KB per call. (S)
23. **Adaptive budgets for `pack`, `context` and `lookup`.** Reuse the run-budget learning: same reopen signal, one step at a time. (M)
24. **`rg`/`grep` context grouping.** With `-A`, `-B` or `-C`, show a range outline per file instead of every line the pattern touched. (S)
25. **`discover --plan`.** Rank unsupported command families by measured bytes and print the build order, turning telemetry into a deterministic roadmap. (S)
26. **Post-run JSON compaction for piped JSON.** §3.6 intentionally excludes pipes; the post-run hook can still verify and compact the captured bytes losslessly. (M)
27. **YAML and TOML structural compaction.** Helm, kubectl and compose config: lift keys, fold lists, omit defaults, verify by re-parse. (M)
28. **XML, SOAP and HAR summaries.** Structure, counts and repeated nodes; rarely worth full bytes. (M)
29. **Generated, minified and vendored files.** Detect `*.pb.go`, `*.min.js`, source maps, `dist/`, `vendor/` and minified single-line content in previews and the index; show shape and count. §3.5 only elides base64 runs. (M)
30. **Moved-block detection in diffs.** Git renders a move as delete plus add; detect identical blocks and emit "moved from A to B". (M)
31. **Task-aware diff clipping.** Keep hunks that touch task terms and paths, summarize the rest by file. (M)
32. **Search cap per file.** For a match-heavy `rg`, show first and last N per file plus a count and a ready range command instead of every match. (S)
33. **Test collection output.** `cargo test -- --list` and `pytest --collect-only`: counts by module, not one line per test. (S)
34. **Build progress and timing lines.** Compiling/Fetching/Downloading/Finished lines and per-step timings are unique, so folding identical lines does not help; summarize by crate and keep only failures. (S)
35. **Compiler warning dedupe.** Group warnings by lint code and target, keep one full example and a count. (M)
36. **Package-manager resolution logs.** npm/pnpm/yarn/uv/pip resolution trees and "already satisfied" lines become counts and only conflicts. (M)
37. **Docker build step folding.** Keep the failing step and warnings, fold successful layer logs by step. (M)
38. **`kubectl describe` field elision.** Keep status, conditions, containers and events; drop managed fields, annotations and default env. (S)
39. **File-level-only diffs.** Binary, mode, symlink and submodule changes: one line each with hashes instead of a patch attempt. Complements 1.1, which is change detection. (S)
40. **OpenCode native read-only tool.** Register `pack`, `map`, `lookup`, `query` and `path` as a plugin tool so the agent gets compact answers without shelling out for whole files. Distinct from the §4 routing item. (M)
41. **OpenCode retirement tuning.** The plugin keeps the last three results and ignores under 256 bytes; size the window and threshold from real sessions. (S)
42. **Proxy clear-threshold tuning.** Set `clear_at_least` and the minimum token count from measured sessions so clearing maximises removed bytes without dropping cache hits. (S)
43. **Antigravity read-outline parity.** §4 covers Copilot and Gemini post-run compaction but not Antigravity's separate hook contract. (M)
44. **Binary-output detection.** NUL or high-byte-ratio output shows type, size and first/last bytes instead of dumping binary into context. (S)
45. **Pointer summaries.** A retired result could carry a one-line summary in its placeholder; it costs bytes per request but may avoid a recall. Measure before adopting. (S)
46. **Range navigation in footers.** Offer `--from/--to` in the footer so a model asking for more does not replay the whole log. (S)
47. **Paged recall.** `recall --outline` or a range so only the needed part of an archived result returns. (S)
48. **Formatter-level adaptive widening.** If a family's output is reopened, switch that formatter to a fuller preview automatically; today only the byte budget adapts. (M)
49. **Empirical cut thresholds per family.** Choose preview sizes from transcript replay instead of one global default. (M)
50. **Transcript replay harness for sizing.** A repeatable script that puts every new formatter against the stored transcripts before it is built, so this list can be ranked by bytes rather than intuition. (M)

---

## 9. Top-five work (merged into main)

`feat/wrapper-coverage` and `feat/top5` are merged into `main` at `24a8984`.

### `feat/wrapper-coverage` (4 commits ahead)

- `2afa5ea` **Post-command wrapper coverage.** A read list looks through `env` (including `-u`/`-i`/`VAR=1`), `time`, `nohup`, `command`, `nice`, `timeout`, `stdbuf`, `bash -c` and git's global options per piece. The recorded command keeps the original words for replay. Pre-command rewriting stays strict, so `env RUST_BACKTRACE=1 cargo test` and `git -C . diff` are not rewritten for clients that auto-approve.
- `d3727fe` Correct the coverage claim in the docs.
- `5c267f1` A wrapped single command is now classified as a one-piece list and compacted after verification.
- `8e07e1b` Wrapper parsing moved to `src/wrappers.rs`; the single-piece path now applies the same metacharacter and 16 KiB guard as multi-piece lists, so substitutions, globs and redirections stay unparsed.
- Tests: wrapper options, unsafe wrappers, list and single forms. Verified: `cargo fmt`, `clippy -D warnings`, 248 CLI tests, 8 batch tests, plus 211 binary tests; `scripts/context_benchmark.py` passes.

### `feat/top5` (stacked on `feat/wrapper-coverage`, one commit on top)

- `5477565` connects the OpenCode plugin to the post-command hook: `tool.execute.after` compacts a successful Bash result and leaves a result whose `metadata.exit` is nonzero alone. It also adds `scripts/replay.py`, the measured transcript replay harness, and updates the reference.
- **Top five status**
  - Failure-aware compaction: OpenCode successful results covered; nonzero exits skipped. Claude Code's PostToolUse has no exit status and failed results are not rewritten, so the Claude side remains open.
  - Rerun delta: routed runs already print `Output unchanged since`; after-bash batch archives deliberately skip rerun comparisons, because a truncated archive cannot prove equality of full output.
  - Polyglot affected tests: already built; pytest, jest/vitest and `go test` selection are covered by existing tests.
  - OpenCode post-command compaction: works end to end; a wrapped `sed` result measured 10,470 bytes before and 1,327 after, and an exit-1 result stayed untouched.
  - Transcript replay: `scripts/replay.py` replayed 1,867 stored results, 164 eligible, 26 compacted, and saved 278,084 of 314,460 bytes (88.4%). The rest were under 4 KiB, pass-throughs, already compacted, or a directory that no longer existed.
- Not merged. To use the plugin: `cargo install --path . --locked --force`, refresh `init --hooks --agent opencode`, restart OpenCode.

---

## 10. Coverage branch (`feat/coverage`, merged)

- `20dbad2` **Single-piece pipelines and `cat` singles.** The batch parser now accepts a single-piece command list, so `sed … | head` and similar pipelines compact after verification. A plain `cat FILE` single is handled by the same path; plain `sed`/`head`/`tail` singles stay on the existing plan path so their preview and replay tests keep their exact behavior.
- `74929a3` **OpenCode read-only tool.** The plugin registers `justless` again for `pack`, `map`, `lookup`, `query`, `path`, `read`/`expand`/`code`, archived runs, `recall`, `affected`, `stats` and `discover`, and refuses every other command. The after-tool compaction from `feat/top5` stays.
- Tests: 248 CLI, 8 batch and 211 binary tests; `cargo fmt` and `clippy -D warnings` clean.
- Still open from the ten-item plan: rerun delta for batch archives, fixed per-request overhead, watch/incremental index/daemon, `explain` and graph exports, and the session ledger.

