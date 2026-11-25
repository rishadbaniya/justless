# How justless compares

Several strong projects attack the same context-window problem from different angles: command-output filtering, repository knowledge graphs, LSP symbol editing, API-level context compression, PR impact analysis, and repo packing.

**justless** brings those capabilities together inside one local, offline Rust binary with exact byte-for-byte recall (`log`, `expand`, `recall`), zero LLM calls, and zero paid tiers. Each specialized peer still has genuine strengths worth knowing before you choose.

## At a glance

| Tool | Primary focus | Command hooks | Code graph & NL | PR review & tests | Outlines & edits | Context hygiene / proxy | Local & zero-LLM |
| --- | --- | :---: | :---: | :---: | :---: | :---: | :---: |
| **justless** | Full local context layer | ✅ | ✅ (26 langs) | ✅ (`review`, `--affected`) | ✅ (43 formats + dirs, verified `edit`) | ✅ (`mask`, `proxy --compact-live`) | ✅ |
| **RTK** | Shell output compression | ✅ (100+ cmds, 18 agents) | — | — | File reads | Opt-in proxy | Partly |
| **graphify** | Multi-modal knowledge graph | — | ✅ (~40 langs + docs/media) | — | — | — | Partly |
| **Serena** | Live LSP code agent toolkit | — | Live LSP | — | ✅ (LSP-backed) | — | ✅ |
| **Headroom** | API-level context proxy | — | — | — | — | ✅ (Smart Crusher / proxy) | Partly |
| **code-review-graph** | Visual PR impact graph | — | ✅ | PR impact | — | — | ✅ |
| **Repomix** | Whole-repo bundle export | — | — | — | `--compress` skeleton | — | ✅ |
| **Aider Repo-Map** | PageRank map inside Aider | — | Repo map | — | Signatures in map | — | ✅ |

<sub>Checked against each project's public documentation on 2026-11-25; "—" means the project does not target that capability.</sub>

## RTK

**[RTK](https://github.com/rtk-ai/rtk)** rewrites an agent's shell commands through hooks and compresses their output.

- **What RTK does well:** Broadest shell command coverage (100+ commands) and the widest agent catalog (18 coding tools), plus smart file reads, a savings dashboard, `rtk recall` for trimmed or failed commands, and an optional API proxy that compresses large tool results already in the conversation.
- **Where justless differs:** justless archives *every* captured run by default (not only trimmed or failed ones) with per-group `[stdout:A-B]` / `[stderr:A-B]` line pointers, compares reruns within the same command/directory/session, calibrates token estimates against Claude's tokenizer by content type (rather than `bytes ÷ 4`), ships built-in secret redaction in the free binary, and pairs command compaction with a local code graph, PR blast-radius review, affected-test selection, revision-verified edits, and cross-compaction handoffs.
- **Honest trade-off:** RTK supports more shell commands and more coding agents out of the box. Because both tools hook and rewrite the same shell commands, pick one of them for command interception in a given project.

## graphify

**[graphify](https://github.com/Graphify-Labs/graphify)** turns a folder into a queryable knowledge graph so agents can answer architectural questions without broad grepping.

- **What graphify does well:** Parses code locally with Tree-sitter in ~40 languages, extends the graph beyond code to docs, PDFs, and images via a model pass (and video via local transcription), writes an interactive HTML graph visualization and markdown report, and offers `query`, `path`, and `explain`.
- **Where justless differs:** justless indexes 26 code-graph languages (plus Vue/Svelte/Astro and shebang scripts) and searches other text files literally with `find`, refreshing its incremental index automatically on every query without `--update`, a watcher daemon, or a Git hook. It adds weighted PageRank `map` with consolidated communities and bridges, `map --explain SYMBOL`, natural-language `lookup --graph`, blast-radius `review`, `test --affected`, command compactors, and verified edits — all locally with zero LLM calls.
- **Honest trade-off:** graphify supports more Tree-sitter languages, ingests PDFs, images, and video, and produces an interactive HTML graph viewer. justless does not process non-text media or generate an HTML graph UI.

## Serena

**[Serena](https://github.com/oraios/serena)** gives coding agents semantic symbol navigation and editing backed by real Language Server Protocol (LSP) servers.

- **What Serena does well:** Because it speaks directly to each language's LSP server, Serena resolves full compiler types, generic instantiations, trait/interface dispatch, and cross-file symbol renames with IDE-level accuracy.
- **Where justless differs:** justless requires zero language-server installation, warmup time, or background RAM: `syn` (Rust) and Tree-sitter (25+ other languages) index and outline repositories in milliseconds (with `LazyLock` query caching), cover 43 structured outline formats (including Markdown, TOML, YAML, JSON, DevOps, and IDL files that lack LSPs), verify edits against path-bound content revisions and parse checks, and also handle command compaction, PR review packs, affected tests, and tool-history retirement in one binary.
- **Honest trade-off:** Static syntax and call graphs cannot replace a full compiler type-checker. When a call target is ambiguous under dynamic dispatch or method overloading, justless explicitly marks the link as `possible` rather than guessing; if you need type-checked cross-workspace refactoring, Serena's live LSP integration is stronger.

## Headroom

**[Headroom](https://github.com/chopratejas/headroom)** sits at the LLM request/API proxy layer to compress conversation history, tool outputs, and context payloads before they hit the model provider.

- **What Headroom does well:** Operates transparently across frameworks and model providers at the HTTP boundary, combining structural compression, semantic summarization/routing ("Smart Crusher"), and cache-aware prompt optimization without modifying how individual CLI tools run.
- **Where justless differs:** justless attacks context bloat at *both* layers. First, shell hooks, native Read/Grep/Glob hooks, and code-graph commands prevent multi-megabyte outputs from entering the conversation in the first place. Second, opt-in `justless proxy --compact-live` (and `justless wrap claude`) compacts oversized newest tool results in the live zone with **0% prompt-cache invalidation** (cutting oversized JSON, logs, tables, and tracebacks by 79.8%–96.6%), pairs with `mask` for duplicate and superseded-read retirement, and guarantees exact byte-for-byte restoration via `justless recall` — with zero external LLM calls.
- **Honest trade-off:** Headroom offers broader multi-provider proxy routing and learned/semantic compression strategies; justless keeps every transformation strictly local, deterministic, and lossless-on-recall.

## code-review-graph

**[code-review-graph](https://github.com/tirth8205/code-review-graph)** builds a local Tree-sitter knowledge graph focused specifically on change impact and PR review workflows.

- **What code-review-graph does well:** Clean, dedicated PR blast-radius analysis and visual graph exploration to help humans and agents review diffs with structural context rather than flat patches.
- **Where justless differs:** `justless review [--base REF]` (available as both a CLI command and an MCP tool) combines semantic diff folding (13 lockfiles, generated/snapshot files, large SVGs, pure renames, and whitespace-only hunks), reverse blast-radius walking, PageRank hub and cross-community bridge detection, and per-symbol test coverage-gap detection with ready-to-run `justless test --affected` commands — all inside one strict byte budget, sharing the same incremental index used by `query`, `map`, `pack`, and `lookup`.
- **Honest trade-off:** code-review-graph offers dedicated visual graph exploration for human reviewers; justless optimizes for compact, budget-bounded text dossiers that fit directly into an agent's context window.

## Repomix

**[Repomix](https://github.com/yamadashy/repomix)** packs an entire repository (local or remote) into a single AI-friendly XML, Markdown, or plain-text file, with an optional `--compress` Tree-sitter skeleton mode.

- **What Repomix does well:** Best-in-class single-file repository exports for pasting a codebase into web chat UIs, auditing token budgets across a whole repo, fetching remote GitHub repositories directly, and stripping comments/bodies via `--compress`.
- **Where justless differs:** Inside an active coding agent session, dumping an entire repo upfront is rarely needed. `justless read DIR --outline` delivers budgeted directory Tree-sitter code skeletons on demand — for example, `justless read src/proxy --outline` compacts 7 files (`112.9 KB`, 3,114 lines) down to `4.1 KB` (96.4% cut; full unclipped skeleton `13.1 KB`, 88.4% cut) while representing every file fairly and attaching `REV#N` anchors so the agent can `expand` any single symbol verbatim. Pair that with `map`, `lookup --graph`, `query`, and `pack` for incremental pull-based navigation.
- **Honest trade-off:** Repomix is the right tool when you want a portable whole-repo XML/Markdown artifact or need to fetch a remote repository without cloning it.

## Aider Repo-Map

**[Aider](https://aider.chat/docs/repomap.html)** pioneered the Tree-sitter + PageRank repository map, ranking key files and signatures dynamically based on the files currently in chat.

- **What Aider Repo-Map does well:** Tightly woven into Aider's own editing loop, automatically adjusting the map token budget and personalizing PageRank weights to the exact files being edited in that session.
- **Where justless differs:** justless makes a weighted PageRank repository map (`justless map`) — enriched with consolidated communities, cross-community bridges, and `map --explain SYMBOL` — available to *any* coding agent via CLI, hooks, or its slim 18-tool MCP server (Claude Code, Codex, Cursor, Gemini CLI, Copilot CLI, OpenCode, Antigravity CLI, VS Code), alongside full command compaction, PR review, affected tests, and context retirement.
- **Honest trade-off:** Inside Aider itself, Aider's native repo-map needs zero extra setup because it is built directly into the agent's prompt loop.

## Using tools together

- **Don't double-hook shell commands:** Avoid running both RTK and justless shell hooks in the same project, since both rewrite the same commands.
- **Safe complements:** You can use **Serena** alongside justless if you want live LSP type-checked refactoring on top of justless's command hooks and compaction, **Repomix** when exporting a whole repo for a web chat, or **graphify** when you need PDF/image/video knowledge graphs or an interactive HTML visualization.
