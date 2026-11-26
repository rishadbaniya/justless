# How justless compares

Several strong projects attack the same context-window problem from different angles: command-output filtering, repository knowledge graphs, LSP symbol editing, API-level context compression, PR impact analysis, and repo packing.

**justless** brings those capabilities together inside one local, offline Rust binary with exact byte-for-byte recall (`log`, `expand`, `recall`), zero LLM calls, and zero paid tiers. Each specialized peer still has genuine strengths worth knowing before you choose.

## At a glance

| Tool | Primary focus | Command hooks | Code graph & NL | PR review & tests | Outlines, rename & edits | Context hygiene / proxy | Local & zero-LLM |
| --- | --- | :---: | :---: | :---: | :---: | :---: | :---: |
| **justless** | Full local context layer | ✅ (12 agents) | ✅ (26 langs + `--html`) | ✅ (`review`, `--affected`) | ✅ (48+ formats, AST `rename`, `edit`) | ✅ (`mask`, multi-provider `--compact-live`) | ✅ |
| **RTK** | Shell output compression | ✅ (100+ cmds, 18 agents) | — | — | File reads | Opt-in proxy | Partly |
| **graphify** | Multi-modal knowledge graph | — | ✅ (~40 langs + docs/media) | — | — | — | Partly |
| **Serena** | Live LSP code agent toolkit | — | Live LSP | — | ✅ (LSP-backed) | — | ✅ |
| **Headroom** | API-level context proxy | — | — | — | — | ✅ (Smart Crusher / proxy) | Partly |
| **code-review-graph** | Visual PR impact graph | — | ✅ | PR impact | — | — | ✅ |
| **Repomix** | Whole-repo bundle export | — | — | — | `--compress` skeleton | — | ✅ |
| **Aider Repo-Map** | PageRank map inside Aider | — | Repo map | — | Signatures in map | — | ✅ |

<sub>Checked against each project's public documentation on 2026-11-26; "—" means the project does not target that capability.</sub>

## RTK

**[RTK](https://github.com/rtk-ai/rtk)** rewrites an agent's shell commands through hooks and compresses their output.

- **What RTK does well:** Broadest shell command coverage (100+ commands) and the widest agent catalog (18 coding tools), plus smart file reads, a savings dashboard, `rtk recall` for trimmed or failed commands, and an optional API proxy that compresses large tool results already in the conversation.
- **Where justless differs:** justless supports **12 coding agents** (Claude Code, Codex, Antigravity CLI & IDE, Cursor, Gemini CLI, GitHub Copilot, OpenCode, Windsurf, Cline, Roo Code, Kiro CLI, and Kilo Code), archives *every* captured run by default (not only trimmed or failed ones) with per-group `[stdout:A-B]` / `[stderr:A-B]` line pointers, offers opt-in `lossless_only = true` bijective-only mode, folds 18 polyglot lockfiles, RFC 4180 CSV/TSV, Cloud/K8s/Helm/Terraform/Terragrunt, and Flutter/Dart output, discovers unrouted commands across 8 agents, compares reruns within the same command/directory/session, calibrates token estimates against Claude's tokenizer by content type (rather than `bytes ÷ 4`), ships built-in secret redaction in the free binary, and pairs command compaction with a local code graph, interactive HTML map, PR blast-radius review, affected-test selection, workspace-wide AST `rename`, revision-verified `edit`, and cross-compaction handoffs.
- **Honest trade-off:** RTK supports more shell commands and more coding agents out of the box (18 vs. 12). Because both tools hook and rewrite the same shell commands, pick one of them for command interception in a given project.

## graphify

**[graphify](https://github.com/Graphify-Labs/graphify)** turns a folder into a queryable knowledge graph so agents can answer architectural questions without broad grepping.

- **What graphify does well:** Parses code locally with Tree-sitter in ~40 languages, extends the graph beyond code to docs, PDFs, and images via a model pass (and video via local transcription), writes an interactive HTML graph visualization and markdown report, and offers `query`, `path`, and `explain`.
- **Where justless differs:** justless indexes 26 code-graph languages (plus Vue/Svelte/Astro and shebang scripts) into a compact string-interned binary `.justless/index.bin` (`1.96 MiB` vs. `20.6 MB` JSON, **10.5× smaller**) with dense CSR graph algorithms (`query` **14.2× faster** at `17.7 ms`, `map` **12.4× faster** at `34.3 ms`), shares warm read-only index caches across linked Git worktrees with zero subprocesses, and refreshes incrementally on every query without `--update`, a watcher daemon, or a Git hook. It also ships **`justless map --html [--out PATH] [--explain SYMBOL]`** — a self-contained, zero-CDN, XSS-safe offline interactive HTML5 Canvas graph explorer (`31.7 KB` on `fixtures/mixed`, `138.5 KB` on Justless's 250 top hubs / 98 communities / 40 bridges) — alongside `map --explain SYMBOL`, natural-language `lookup --graph`, blast-radius `review`, `test --affected`, command compactors, AST `rename`, and verified edits, all locally with zero LLM calls.
- **Honest trade-off:** graphify supports more Tree-sitter languages (~40) and ingests non-text media (PDFs, images, and video) via an LLM/transcription pass. justless stays strictly deterministic, offline, and code/text/notebook/config-focused.

## Serena

**[Serena](https://github.com/oraios/serena)** gives coding agents semantic symbol navigation and editing backed by real Language Server Protocol (LSP) servers.

- **What Serena does well:** Because it speaks directly to each language's LSP server, Serena resolves full compiler types, generic instantiations, trait/interface dispatch, and compiler-backed cross-file refactoring with IDE-level type accuracy.
- **Where justless differs:** justless requires zero language-server installation, warmup time, or background RAM: `syn` (Rust) and Tree-sitter (25+ other languages) index and outline repositories in milliseconds (with `LazyLock` query caching and binary `.justless/index.bin`), cover **48+ structured outline formats** (including Markdown, Jupyter `.ipynb`, Nix, GitHub Actions, OpenAPI/Swagger, Nginx/Caddy/HAProxy, Terraform/HCL/Terragrunt, TOML, YAML, JSON, and IDL files that lack LSPs), and provide both revision-verified parse-checked `edit` and deterministic workspace-wide AST-safe **`justless rename <SYMBOL> <NEW_NAME> [--dry-run] [--file ...] [--json]`** (zero LSP daemon, zero closed-file blind spots, atomic revision + `syntax_check` + rollback), alongside command compaction, PR review packs, affected tests, and tool-history retirement in one binary.
- **Honest trade-off:** Static syntax and call graphs cannot replace a full compiler type-checker. When a call target is ambiguous under dynamic dispatch or method overloading, justless explicitly marks the link as `possible` and fails closed on ambiguous renames rather than guessing; if you need full compiler-inferred type-checking across overloaded methods or traits, Serena's live LSP integration is stronger.

## Headroom

**[Headroom](https://github.com/chopratejas/headroom)** sits at the LLM request/API proxy layer to compress conversation history, tool outputs, and context payloads before they hit the model provider.

- **What Headroom does well:** Operates transparently across frameworks and model providers at the HTTP boundary, combining structural compression, semantic summarization/routing ("Smart Crusher"), and cache-aware prompt optimization without modifying how individual CLI tools run.
- **Where justless differs:** justless attacks context bloat at *both* layers. First, shell hooks, native Read/Grep/Glob hooks, and code-graph commands prevent multi-megabyte outputs from entering the conversation in the first place. Second, opt-in **`justless proxy --compact-live`** (and `justless wrap claude`) compacts oversized newest tool results in the uncached live zone across **Anthropic `/v1/messages`, OpenAI `/v1/chat/completions`, OpenAI `/v1/responses`, and Google Gemini `:generateContent` / `:streamGenerateContent`** (**82.7%–96.6%** live-zone byte cut, **0%** prompt-cache invalidation, anti-recursion recall guard), adds read-only SHA-256 prompt-cache **prefix-drift detection** (`system`, `tools`, `message[i]`) surfaced in `stats`, pairs with `mask` for duplicate and superseded-read retirement, and guarantees exact byte-for-byte restoration via `justless recall` — with zero external LLM calls.
- **Honest trade-off:** Headroom offers broader multi-provider proxy routing and learned/semantic compression strategies; justless keeps every transformation strictly local, deterministic, and lossless-on-recall.

## code-review-graph

**[code-review-graph](https://github.com/tirth8205/code-review-graph)** builds a local Tree-sitter knowledge graph focused specifically on change impact and PR review workflows.

- **What code-review-graph does well:** Clean, dedicated PR blast-radius analysis and visual graph exploration to help humans and agents review diffs with structural context rather than flat patches.
- **Where justless differs:** `justless review [--base REF]` (available as both a CLI command and an MCP tool) combines semantic diff folding (**18 polyglot lockfiles**, generated/snapshot files, large SVGs, pure renames, and whitespace-only hunks), reverse blast-radius walking, PageRank hub and cross-community bridge detection, and per-symbol test coverage-gap detection with ready-to-run `justless test --affected` commands (including Cargo, nextest, pytest, Jest, Vitest, Bun, Node, Playwright, RSpec, Go, Flutter/Dart, and Deno) — all inside one strict byte budget, sharing the same incremental `.justless/index.bin` used by `query`, `map`, `map --html`, `pack`, and `lookup`.
- **Honest trade-off:** code-review-graph focuses on dedicated PR-centric visual diff graphs for human reviewers; justless pairs compact budget-bounded `review` text dossiers for agents with its general offline `map --html` graph explorer.

## Repomix

**[Repomix](https://github.com/yamadashy/repomix)** packs an entire repository (local or remote) into a single AI-friendly XML, Markdown, or plain-text file, with an optional `--compress` Tree-sitter skeleton mode.

- **What Repomix does well:** Best-in-class single-file repository exports for pasting a codebase into web chat UIs, auditing token budgets across a whole repo, fetching remote GitHub repositories directly, and stripping comments/bodies via `--compress`.
- **Where justless differs:** Inside an active coding agent session, dumping an entire repo upfront is rarely needed. `justless read DIR --outline` delivers budgeted directory Tree-sitter code skeletons on demand — for example, `justless read src/proxy --outline` compacts 7 files (`112.9 KB`, 3,114 lines) down to `4.1 KB` (96.4% cut; full unclipped skeleton `13.1 KB`, 88.4% cut) — and `justless read FILE --outline` spans **48+ formats** including Jupyter `.ipynb` notebooks (`52.3 KB → 745 B`, 98.6% cut) while representing every file fairly and attaching `REV#N` anchors so the agent can `expand` any single symbol or cell verbatim. Pair that with `map`, `lookup --graph`, `query`, and `pack` for incremental pull-based navigation.
- **Honest trade-off:** Repomix is the right tool when you want a portable whole-repo XML/Markdown artifact or need to fetch a remote repository without cloning it.

## Aider Repo-Map

**[Aider](https://aider.chat/docs/repomap.html)** pioneered the Tree-sitter + PageRank repository map, ranking key files and signatures dynamically based on the files currently in chat.

- **What Aider Repo-Map does well:** Tightly woven into Aider's own editing loop, automatically adjusting the map token budget and personalizing PageRank weights to the exact files being edited in that session.
- **Where justless differs:** justless makes a dense CSR weighted PageRank repository map (`justless map`, `34.3 ms` warm) — enriched with consolidated communities, cross-community bridges, `map --explain SYMBOL`, offline interactive `map --html`, and code-graph savings telemetry in `justless stats` — available to *any* coding agent via CLI, hooks, or its slim 18-tool MCP server (Claude Code, Codex, Antigravity CLI & IDE, Cursor, Gemini CLI, GitHub Copilot, OpenCode, Windsurf, Cline, Roo Code, Kiro CLI, Kilo Code, and VS Code), alongside full command compaction, PR review, affected tests, AST rename, and context retirement.
- **Honest trade-off:** Inside Aider itself, Aider's native repo-map needs zero extra setup because it is built directly into the agent's prompt loop.

## Using tools together

- **Don't double-hook shell commands:** Avoid running both RTK and justless shell hooks in the same project, since both rewrite the same commands.
- **Safe complements:** You can use **Serena** alongside justless if you want live LSP type-checked refactoring on top of justless's command hooks and compaction, **Repomix** when exporting a whole repo for a web chat, or **graphify** when you need PDF, image, or video knowledge graphs.
