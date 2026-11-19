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

### 2.2 Outline wide ranged `Read` calls — done
- **Progress.** *on main* (2026-11-19). A ranged Read returning more than 150 lines of a source or Markdown file becomes the outline rows of that window in Claude Code's Read shape; the way back (`allow_large`, any Read of at most 150 lines, or `expand`) is never outlined. Measured: a 300-line window of `src/agents.rs` 14,924 → 1,555 bytes; checked with the real Claude Code binary (outline, native escape, Edit inside the window).

### 2.3 Parse check after edits — done
- **Progress.** *on main* (2026-11-19). `justless edit` blocks (exit 2, file unchanged, unless `--allow-parse-errors`) a Rust syntax error, a missing token or brackets that no longer pair; other grammar errors only warn, because the bundled grammars reject some valid newer syntax. Already-broken files are never blocked. A Claude Code Edit/MultiEdit/Write hook adds a short note only when an edit introduced a parse error. Cost: 63 ms vs 26 ms on a 690 KB Rust file.

### 2.4 `find` scoping, paging and search guidance — done
- **Progress.** *on main* (2026-11-19, `src/find.rs`). `--path` and `--glob` (rg -g semantics) on the CLI and MCP; an empty scope says so; a cut page names the bound that cut it and prints a `--after CURSOR` next page, rejected with a clear message when the search or the stop file changed. The instruction block gained one line on which search to use (882 → 995 bytes). The MCP tool list is 7,971 of its 8,000-byte cap, so the next MCP parameter must make room.

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

### 3.2 `justless test --compare REF` — done
- **Progress.** *on main* (2026-11-19). Runs the tests in the working tree, reruns only the failing ones at the merge base in a temporary worktree (base builds in `.justless/compare/cargo-target`), and groups them as new failures, changed failures and already failing with the same normalized message. Cargo and pytest (pytest only tested against a stand-in; real pytest still to confirm); other runners say "not supported". Works with `--affected` and `--base`. Exit status is the working tree's.

### 3.3 Codex code mode
- **Problem.** Since late August, 68% of the owner's Codex output (503 MB of 734 MB overall) ran inside code-mode `exec`, and the Codex hook matches only `^Bash$`. If nested calls bypass the hook, Justless does nothing for current Codex sessions. Codex is `init`'s default client.
- **Evidence so far.** Codex's hooks docs say hook decisions apply to tools called from code mode, but that is untested here.
- **Plan.** Verify with one real Codex session; then route or post-compact nested commands; then a Codex PostToolUse compactor (`decision: "block"` with the compact text) for piped `rg` and failing output, which a replay put at 15.3 MB → 1.2 MB in older sessions.
- **Progress.** *not started* (research done).

### 3.4 Permission parity for rewritten commands — done
- **Progress.** *on main* (2026-11-19, `src/claude_permissions.rs`). The Claude Code hook reads allow/ask/deny rules from managed, user, project and local settings and judges the original command: deny or ask leaves it alone in every mode; allow rewrites with `permissionDecision: allow` (not in `auto`, where the classifier reviews instead); no matching rule rewrites only in `bypassPermissions` and `auto`. `doctor` warns about `Bash(justless…)` allow rules that would let `justless run -- <anything>` through. Checked in an isolated project for each case.

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

### 3.11 Better token figures without a bundled tokenizer
- **Why not a tokenizer.** Claude's tokenizer is not public; exact counts need Anthropic's token-counting API (network, against the zero-network promise), and a public tokenizer such as `o200k_base` is a different model's and adds megabytes. Every compaction decision already uses exact bytes; tokens appear only in reports.
- **Plan.** (1) Calibrate bytes-per-token per content type (code, JSON, prose, logs) once, offline, with the benchmark harness and the API, and ship the constants; code runs about 3-3.5 bytes per token, so today's flat bytes ÷ 4 undercounts savings on code. (2) Read the provider-reported usage Claude Code already stores in its local session files (input, cache reads, cache writes) to show real per-session totals in `stats`, offline. Keep the API benchmarks as the only source of measured before-and-after savings.
- **Progress.** *designed*.

## 4. Remaining build batches (from the 2026-11-19 triage)

Closed ideas and their reasons are in [docs/decisions.md](docs/decisions.md). These batches touch mostly disjoint files; each starts after the in-progress branch named in it merges.

### 4.A Reads outside the workspace, and the duplicate instruction block
- **Outside-read outlines.** A successful whole text Read (no offset, limit, pages or `allow_large`) of a parser-supported file outside the workspace is outlined from the Read result in memory, archived with `observations::archive` so the first line names `justless recall ID`; dotfiles, `$HOME/.*` and Claude Code's tool-results files are skipped; `expand`/`edit` stay workspace-only. Measured: 351 such reads over 4 KB, 6.81 MB, of which parsable 4.16 MB; outline ratios 0.04–0.14.
- **Duplicate instruction block.** When `CLAUDE.md` imports `@AGENTS.md` and `AGENTS.md` holds the block, `init` writes no second copy into `CLAUDE.md` (and removes one on re-run); `doctor` and `init --remove` agree. Saves about 905 B on every request.
- **Starts after:** Read/Edit hooks (§2.2/§2.3), permission parity (§3.4) and `find` (§2.4) merge.
- **Progress.** *designed*.

### 4.B Commit-aware diff summary for `git show` patches
- Map the patch forms of `git show` (and `git log -p`) to the diff summary, reading each commit header as an item before the per-file and per-hunk items; anything unrecognized still falls back to the preview. Measured: 138 results over 4 KB, 1.53 MB, clipped today because the diff parser requires `diff --git` first.
- **Starts after:** the compaction-formats branch (§3.5–§3.7) merges.
- **Progress.** *designed*.

### 4.C Grouped context searches
- Parse rg/grep/git grep output with `-A/-B/-C` into blocks (the `--` separators and `:` vs `-`), group by file with block ranges and `[stdout:A-B]` tags, fall back if any line is unaccounted for. Measured: 227 results over 4 KB, 1.42 MB, passed through today on purpose.
- **Progress.** *in progress*.

### 4.D MCP and history fixes
- MCP `context` and `affected` take a `base` ref with `changes: true`; `history` keeps one place for the newest match from a failed run when the limit cuts; no cut notice suggests a `--limit` above MCP's 1000.
- **Starts after:** `find` (§2.4) and changed-since-last-read (§3.1) merge.
- **Progress.** *designed*.

### 4.F Follow-ups from permission parity
- `would_route`, `discover` and `doctor`'s "has not routed a command yet" note still assume the Claude Code hook always rewrites; make them use the same permission verdict, and have `doctor` say when a settings file is unreadable (which silently stops pre-routing in every mode).
- "Don't ask again" rules saved in `settings.local.json` at the Git root count only when that directory is `$CLAUDE_PROJECT_DIR`; consider reading the Git root's (and a worktree's main checkout's) local file for allow rules too, if Claude Code does.
- `scripts/agent_bench/cc_hooks.py` drives the hook with `bypassPermissions`, so it now reads the machine's real settings; pin `CLAUDE_CONFIG_DIR` and the managed override in the harness.
- **Progress.** *designed*.

### 4.E `git show REV:path` reads
- The largest unhandled read family: 803 results, 3.41 MB. Treat `git show REV:path` like the other shell reads (§2.1): exact for small output, outline for wide, verified bytes.
- **Starts after:** the review of the batch code merges.
- **Progress.** *designed*.

## 5. Benchmark and release (last, in this order: 5.3 then 5.1)

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

## 7. Do not build

LLM summarization or retrieval, telemetry, client-side rewriting of earlier requests (it invalidates thinking on current Claude models; the opt-in proxy uses server-side context editing instead), networked graph databases, bundled embedding models, and fetching filters from the network. Each breaks the local, deterministic, zero-network promise in the README.

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

