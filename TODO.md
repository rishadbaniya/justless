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

## Review of the 2026-11-19 DeepSeek commits — done

Reviewed against the code on 2026-11-19; 7 real bugs fixed, each with a failing test first: a batch whose recheck failed skipped the ordinary report; `would_route` and `discover --plan` miscounted read lists and supported commands; OpenCode compacted killed commands (`exit: null`); the OpenCode tool passed `log --unredacted` through without a prompt; a skipped `cd` (after `||` or a failed search) was applied to later reads; a relative `cd` was applied twice in Claude Code's main session. `doctor` now warns when the `justless` on PATH is older than the checkout (`--version` prints the build revision). Open: OpenCode's after-tool call omits `--root`, so its runs can land elsewhere when OpenCode starts in a subdirectory.

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

### 2.1 Shell range reads — biggest measured token win — done
- **Progress.** *on main* (2026-11-19). Single-file `sed -n` (one or several ranges), `head -n`, `tail -n ±N`, `cat FILE`, `cat FILE | head/tail/sed -n`, `git show REV:path` and any of them after one `cd DIR &&` are verified and outlined when wide, exact when small, in Claude Code's post-run path, read lists and the pre-command rewrite for other clients. On 18 real reads of this repo: Codex 142,885 → 35,660 bytes, Claude Code 35,028 → 15,999 (native 561,367). `nl -ba | sed -n` and `awk` ranges are closed in docs/decisions.md. Follow-up in §4.K: reads inside Claude Code read lists are not yet recorded for `resume`.

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

### 3.1 Show what changed since the agent last read a symbol — done, default activation pending
- **Progress.** *on main* (2026-11-19, `src/seen.rs`). In one session, `context --symbol`, `context --at` and `pack` print only the changed lines of a symbol read before (a real 46-line test with one line edited: 2,039 → 512 bytes; unchanged re-read: 150 bytes), with an exact `expand` command for the whole symbol; compaction hooks forget the records. Sessions come from `--session` or `CLAUDE_CODE_SESSION_ID`, which is ignored when `CLAUDE_CODE_CHILD_SESSION` is set. That flag turned out to be set in top-level Claude Code sessions too, so today deltas need `--session`; see 4.H.

### 4.H Turn changed-since-read on by default — done
- **Progress.** *on main* (2026-11-19). The Claude Code hook appends a `--session` digest (session ID plus subagent `agent_id`) to `justless context`/`pack`, judged by the same permission rules as other rewrites, so in `default`/`acceptEdits` mode it applies when an allow rule covers those commands (e.g. `Bash(justless context:*)`), and always in `auto` and `bypassPermissions`. The instruction line and `stats` counting are in. Unverified: a real subagent hook input carrying `agent_id`.

### 3.2 `justless test --compare REF` — done
- **Progress.** *on main* (2026-11-19). Runs the tests in the working tree, reruns only the failing ones at the merge base in a temporary worktree (base builds in `.justless/compare/cargo-target`), and groups them as new failures, changed failures and already failing with the same normalized message. Cargo and pytest (pytest only tested against a stand-in; real pytest still to confirm); other runners say "not supported". Works with `--affected` and `--base`. Exit status is the working tree's.

### 3.3 Codex code mode — done
- **Progress.** *on main* (2026-11-19). Codex's hooks reference and source show hook decisions apply to shell calls made from code-mode `exec` scripts and that `exec_command` matches as `Bash`, so the existing `^Bash$` hook already covers them (tested with Codex's exact nested-call input). A Codex post-run compactor was not built: in a script `block` rejects the call and `continue: false` changes nothing the script receives, and on a direct call a replacement would drop the exit status; direct `exec_command` calls have been zero since September anyway. `discover`, `learn` and `stats` now read Codex rollouts (`src/rollouts.rs`). Measured over 200 days: 23,327 supported commands (149.7 MB) never went through Justless because Codex had no Justless hooks installed.

### 3.4 Permission parity for rewritten commands — done
- **Progress.** *on main* (2026-11-19, `src/claude_permissions.rs`). The Claude Code hook reads allow/ask/deny rules from managed, user, project and local settings and judges the original command: deny or ask leaves it alone in every mode; allow rewrites with `permissionDecision: allow` (not in `auto`, where the classifier reviews instead); no matching rule rewrites only in `bypassPermissions` and `auto`. `doctor` warns about `Bash(justless…)` allow rules that would let `justless run -- <anything>` through. Checked in an isolated project for each case.

### 3.5 Generic compaction floor — done
- **Progress.** *on main* (2026-11-19, `src/floor.rs`). Progress frames collapse to the last one, long base64/hex runs keep 12 characters at each end with a marker, repeated lines fold; failure and patch lines are never touched. The Claude Code post-run hook applies the floor (and JSON tables) to a single unsupported command over 4× `context_budget` when that saves 25%. Synthetic progress log 20,446 → 465 bytes.

### 3.6 Lossless compaction for JSON and JSONL output — next new token-saving feature after 2.1 — done
- **Progress.** *on main* (2026-11-19, `src/jsonview.rs`). Arrays of objects and JSON lines become a verified table (fields named once, shared fields lifted); the view is decoded and compared with the original before it is shown. Grouping is behind `json_grouping`, off. Synthetic `gh api` issues 29,823 → 14,514, `kubectl` pods 25,493 → 9,307, `docker ps` JSONL 7,315 → 1,421 bytes; real `cargo metadata` −8% (nested arrays stay inline).

### 3.7 Dependency-aware diffs for lockfiles, starting with Cargo.lock — done
- **Progress.** *on main* (2026-11-19, `src/lockdiff.rs`). Cargo.lock sections of `git diff`/`git show` are described by added, removed and updated packages and changed links when smaller; `source`/`checksum` changes on an existing version are always flagged. Real commit: 16.5 KB → 1.2 KB.

### 3.8 `init --global` — done
- **Progress.** *on main* (2026-11-19, `src/global.rs`). `init --global` merges hooks into Claude Code's and Codex's user settings, keeping every other key; each call resolves the project from the hook input's cwd and stays out of the home folder, projects with `global_hooks = false` and projects whose own settings already run the hook. `init --remove --global` restores untouched files byte for byte. Follow-ups in §4.J.

### 3.9 Global project registry — machine-wide stats and run lookup — done
- **Progress.** *on main* (2026-11-19, `src/registry.rs`). A private `$XDG_STATE_HOME/justless/projects.json` (paths, repository, last use) feeds `stats --all` (worktrees grouped, temp folders excluded), run lookup across projects for `log`/`search`/`context --run`, and doctor's "This machine" section; `init --remove --purge` drops the entry.

### 3.10 Hook kill switches and diagnostics — done
- **Progress.** *on main* (2026-11-19, `src/rewrite.rs`). `exclude_commands` (prefixes or `^` regexes) in `.justless.toml`, `JUSTLESS_DISABLE=1` for every hook and the OpenCode plugin, and `justless rewrite "CMD" [--agent] [--mode]` to show what a hook would do.

### 3.11 Better token figures without a bundled tokenizer — done
- **Progress.** *on main* (2026-11-19, `src/tokens.rs`). Bytes per token measured once with Anthropic's token-counting endpoint on this repository's own files and output (221 samples; Opus counted the same): code 2.57, JSON 2.22, prose 2.78, logs 2.21; bytes ÷ 4 had counted only 56–70% of the tokens. `stats` classifies each run; the status line and `discover` use the command-output (logs) ratio; the opt-in proxy keeps ÷ 4, the unit of its clear floor, beside the API's own usage. `stats --sessions` adds the provider-reported usage from this workspace's Claude Code transcripts, apart from savings. Recalibrate with `scripts/token_calibration.py`.

### 4.A Reads outside the workspace, and the duplicate instruction block — done
- **Progress.** *on main* (2026-11-19). A whole Claude Code Read of a parser-supported file outside the workspace is outlined from the returned content and the original archived with `justless recall ID` (e.g. 35,392 → 2,764 bytes); dotfiles, `$HOME/.*` and tool-results files are skipped. When `CLAUDE.md` imports `@AGENTS.md`, `init` keeps one block in AGENTS.md (this repo's CLAUDE.md went from 941 to 11 bytes), and `doctor` flags a duplicate copy.

### 4.B Commit-aware diff summary for `git show` patches — done
- **Progress.** *on main* (2026-11-19). `git show` and `git log -p` get the diff summary: one item per commit header, then file and hunk items with exact `[stdout:A-B]` ranges; the Cargo.lock description and its flags work inside it; reruns compare only when they share a commit. `git show c5f088d` 11,462 → 1,008 bytes (2,046 clipped before). On commits over about 25 KB the summary is larger than the old head/tail clip (e.g. 34,734 → 2,802 vs 2,043) but names every hunk, as `git diff` already does. Remaining edges are closed in docs/decisions.md.

### 4.C Grouped context searches — done
- **Progress.** *on main* (2026-11-19). rg/grep/git grep output with `-A/-B/-C` is parsed strictly into blocks and grouped by file with exact `[stdout:A-B]` ranges, in `run` previews and the Claude Code post-run hook; anything the parse cannot account for falls back. Seven real 5–12 KB searches: 54,940 → 16,045 bytes. Open: ag/ack layouts unverified (not installed); Claude Code's own Grep tool with context options keeps the matches-only grouping.

### 4.D MCP and history fixes — done
- **Progress.** *on main* (2026-11-19). MCP `context` and `affected` take `base` (with `changes: true`); `history` keeps one place for the first failed run when the limit cuts (needs a limit of at least 2); no notice suggests a `--limit` above MCP's 1000.

### 4.F Follow-ups from permission parity — done
- **Progress.** *on main* (2026-11-19). `discover` judges each supported command with the mode its transcript recorded and today's rules, and reports commands the hook would leave in their own group with the right hint; `doctor` uses the sessions' default mode in its "has not routed" note and names settings files it cannot use; the repository-root `.claude/settings.local.json` is read for subdirectory and worktree sessions (allow rules only where Claude Code trusts it); the bench harness pins its config dirs. Follow-up in §4.I: record commands the hook left so `doctor` sees mode switches.

### 4.G Follow-ups from the new languages — done
- **Progress.** *on main* (2026-11-19). Components reach the JS tests that import them; languages without a runner fall back (`data-unlinked`) instead of planning nothing; a MATLAB `.m` is data. Elixir edits now block (0 of 1,376 real files had parse errors); Objective-C, PowerShell, Julia and Groovy stay report-only (8–30% of real files had errors). `.mm` and Perl are closed in docs/decisions.md.

### 4.I Route JSON-producing commands to the table view
- The PreToolUse hook never routes JSON-producing commands (`supported_command` rejects `--json`, `--output`, `--message-format`; `kubectl -o json` fails the display check; `gh`, `cargo metadata` and `npm ls` are not supported commands), so JSON tables reach agents only through `justless run`, `exec` and the Claude Code post-run hook. Route unpiped JSON-producing commands for clients without a post-run hook, keeping pipes and redirections untouched, and consider whitespace-only compaction of pretty-printed non-tabular JSON (would cut `npm ls` by 62%).
- **Progress.** *designed*.

### 4.J Follow-ups from global setup
- In a project served only by global hooks, `doctor` still fails with "not set up in this project" and "Hooks: none"; `discover` does not apply `exclude_commands`; `stats --all` ignores `--days`.
- **Progress.** *designed*.

### 4.K Record read-list reads for `resume`
- Reads served inside Claude Code read lists (`src/batch*`) never call `history::saw_source`/`saw_outline`, so `resume` and `[source changed]` miss them; single-file reads are recorded.
- **Progress.** *designed*.

### 4.E `git show REV:path` reads — done
- **Progress.** *on main* (2026-11-19). `git show REV:path`, `REV:./path` and `:path` are verified against the blob (`git cat-file`, no index lock, no lazy fetch) and outlined in Claude Code's post-run hook and in read lists; other Unix clients get a read pinned to the blob id. `HEAD:CHANGELOG.md` 29,191 → 345 bytes (1,930 when clipped before). Lists with `HEAD~1` now parse.

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

