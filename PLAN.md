# Modron — a token-efficient Rust AI coding agent harness

*Founding document — 2026-10-30. Renamed from the working title "Kestrel" on
2026-10-31: the old name collides with Microsoft's ASP.NET Core web server and
23 crates on crates.io (including a `kestrel-cli` and a `kestrel-rs` in our own
space). "Modron" is free on crates.io; the only notable collision is Niantic's
Go-based GCP security scanner — different ecosystem, different domain. Fitting
name: modrons are D&D's clockwork constructs of Mechanus — hierarchical,
deterministic, rule-following.*

Modron is a terminal-first AI coding agent for working developers, written in Rust.
Its thesis: **the model decides how smart the agent is; the harness decides how fast,
safe, and expensive that intelligence is to use.** We compete on the harness.

---

## 1. The evidence for the thesis

A 14-day, 30-task benchmark (Tensorlake, Aug 2026) ran identical tasks through four
harnesses on the same model (DeepSeek V4 Flash):

| Harness | Passed | Avg tokens/task | Total cost | Cost per success |
|---|---|---|---|---|
| Pi (4 tools, <1k-token prompt) | **20/30** | **559k** | **$0.56** | **$0.028** |
| Claude Code (10+ tools, ~33k prompt) | 16/30 | 742k | $3.12 | $0.195 |
| Codex | 16/30 | 665k | $1.29 | $0.081 |
| OpenCode | 14/30 | 692k | $1.03 | $0.073 |

The minimalist harness beat Claude Code with **7x lower cost per solved task**. A
separate 300-run study ("The Scaffold Effect") found up to **40x variance in
tokens-per-solved-task across harnesses** while pass rates differed only 0–8 points.
Token efficiency is not a feature; it is the product.

Supporting data points:

- Pi deliberately omits MCP because MCP servers add **13–18k tokens of tool schemas**.
- Claude Code now cuts 80%+ of its system prompt for its newest model generation —
  the vendor with the most to lose is treating prompt weight as a first-class cost.
- Pi's 4-tool setup wins benchmarks but lacks safety (no permissions, no sandbox);
  Claude Code/Codex are heavy but safe. Modron's target: **Pi's economy with
  Codex-class safety**.

## 2. Research base (what we verified, Oct 2026)

| Reference | What it is | What we take from it |
|---|---|---|
| [RTK (Rust Token Killer)](https://github.com/rtk-ai/rtk) | External proxy filtering tool output before the model sees it. Claims 60–90% savings; independent benchmarks found neutral-to-negative real savings. | The *technique* (filter/compress tool results at the source) is right; the numbers are inflated. We build filtering natively and **measure honestly** — usage is API-reported, never estimated and claimed. |
| [Jev / TypeSafe "System One Models"](https://typesafe.ai/blog/introducing-system-one-models-and-jev) | Decision-only model: typed question in, probabilistic typed choice out (~200x faster/400x cheaper claims; 145–271ms router latency in the wild). Access is invite-limited. | Pluggable `DecisionModel` backend for routing and permission triage. Never a hard dependency — the open default is a local embedder/heuristics. ([OpenRouter guide](https://openrouter.ai/docs/guides/community/jev)) |
| [weave-os/router](https://github.com/weave-os/router) | Apache-2.0 Go proxy routing each *action* to the best model in <50ms using a local ONNX embedder + cluster scoring from [Avengers-Pro (arXiv:2508.12631)](https://arxiv.org/abs/2508.12631). GPT-5-level accuracy at 27–63% lower cost. | Validates per-action routing. We route natively (we own the loop, no proxy needed). Use it alongside during development to collect labeled routing data, then internalize. |
| [prime-agent](https://github.com/PrimeIntellect-ai/prime-agent) | PrimeIntellect's Rust harness (MIT, ~22.5k★) built on Recursive Language Models: context-as-variables, `rlm.spawn()` subagents, daemon sessions, auto-compaction at 95% context. | Adopt RLM *context management* (subagents, compaction). Skip Python-REPL-as-single-tool for MVP — discrete tools are more predictable; revisit later. |
| [pi](https://github.com/earendil-works/pi) (Zechner) | Minimal harness: 4 tools, <1k-token prompt. Won the benchmark above. No permissions/sandbox. | Prompt-budget discipline; extensibility via packages; the economy target. |
| [pi_agent_rust](https://github.com/Dicklesworthstone/pi_agent_rust) | From-scratch Rust rewrite of Pi: 36 tiered tools, session trees, `--max-cost` spend governors with crash-safe journals, capability-gated QuickJS extensions, worktree-isolated parallel subagents, TUI/print/RPC/ACP modes, LSP/DAP tools. | The most complete Rust harness; primary codebase to study. Several features (cost governor, session trees, print/RPC/ACP modes, LSP tool) are lifted directly into our roadmap. |
| Claude Code / Codex / OpenCode | Feature ceiling / sandbox ceiling / architecture reference. Codex: Seatbelt + bubblewrap/seccomp sandbox, network off by default, more multi-provider than assumed. OpenCode: client/server split (sessions survive SSH), soft-delete compaction; **lost Claude subscription access Jan 2026 (ToS enforcement)**. | Sandbox design (port codex-rs, Apache-2.0); core-as-server architecture; subscription paths must never be load-bearing (OpenRouter/API/local are first-class). |
| [Harness landscape survey](https://github.com/FlorianBruniaux/claude-code-ultimate-guide/blob/main/guide/ecosystem/agent-harness-landscape.md) | Six responsibilities: observation, context, control, action, state, verification. Verification is the most neglected. | Organizing checklist; verification loop is a roadmap item, not an afterthought. |

**cc-code** (the rumored "Claude Code in Rust") could not be verified — appears only
in an awesome-list PR with no canonical repo. Treat as vaporware; nothing lost.

## 3. Product principles

1. **Token budget is a CI metric.** System prompt + tool schemas are budgeted
   (target < 2k tokens combined) and enforced like a test. Every MCP server's schema
   cost is accounted before it is enabled.
2. **Measured, never claimed.** Token accounting uses provider-reported usage as
   ground truth. Savings claims come from dashboards, not marketing.
3. **Small essential tool tier.** ~10 tools built-in, each with a tight description;
   everything else is discoverable or opt-in. Prompt weight is the tax on every turn.
4. **Deterministic first.** Ripgrep/glob/LSP before any model call. Models are for
   judgment, not for listing files.
5. **Safety is not optional.** OS-level sandbox (network off by default), approval
   modes, spend governors. Pi's lack of guardrails is the cautionary tale.
6. **Subscription paths are opportunistic, never load-bearing.** API keys, OpenRouter,
   and local models are first-class citizens. (Anthropic cut off OpenCode's
   subscription access in Jan 2026.)

## 4. Architecture

### 4.1 Crates

```
modron-core        types, traits (ModelProvider, Tool, SessionStore, ModelSelector),
                    agent engine, budget, token estimation, mock provider for tests
modron-providers   Anthropic Messages + OpenAI-compatible clients (SSE streaming later)
modron-tools       read/glob/grep/bash/edit/write + output-reduction layer + registry
modron-store       session persistence (JSONL trees now, libsql later; Postgres eventually)
modron-router      heuristic ModelSelector + model catalog (pricing, context windows)
modron-cli         clap CLI: one-shot print mode, session resume, config
```

Later: `modron-tui` (ratatui), `modron-mcp`, `modron-acp` (Zed), `modron-sandbox`.

### 4.2 Core-as-server

The engine is a long-running process; TUI/print/RPC are thin clients (OpenCode's
client/server split, prime-agent's daemon). Sessions survive terminal close and SSH
drops; background subagents become natural. MVP is print-mode-only, but the core/UI
boundary is day-one architecture.

### 4.3 Model routing

Per-action, not per-turn (weave-router's key insight): search/summarize → cheap model,
edits/planning → strong model. `ModelSelector` tiers, all live as of Phase 3b:

1. Heuristic rules (task type, size, diff scope) — <1ms, the fallback floor.
2. **Learned hashing router** (Phase 3a): char-3-gram vectorizer + corpus
   centroids — dependency-free learned behavior; `corpus.toml` is the tuning
   surface, `modron router eval` the measurement loop.
3. **Async decision models** (Phase 3b): an LLM judge over any
   OpenAI-compatible provider (locally: Ollama + a 0.5B–1.5B model — the
   field-validated "router, not judge" pattern), and the **Kev/Jev SystemOne
   client** (`POST /v1/systemone`; Kev is the Apache-2.0 open Jev replica —
   0.8B/4B/9B/27B, self-hostable, wire-compatible, Kev-27B within ~1 point of
   hosted Jev). Confidence-gated; both fail open to the sync choice.
4. Real ONNX embedder (Avengers-Pro style) — deferred; `embed()` is the seam.

Routing decisions are logged as structured events → training data for tier 2.

### 4.4 Subscriptions & providers

`ModelProvider` trait. MVP: Anthropic Messages + any OpenAI-compatible endpoint
(covers OpenAI, OpenRouter, DeepSeek, Ollama, llama.cpp). **Z.ai is a first-class
target** (2026-10-30): OpenAI-compatible at `https://api.z.ai/api/paas/v4/`
(`--provider zai`, key via `ZAI_API_KEY`), Anthropic-compatible endpoint also
available on GLM 4.7+; current lineup GLM-5.x — this is the intended dogfooding
provider once a key is present. Later: ChatGPT/Claude OAuth subscription
flows (ToS-gray — see risk register), Copilot, Bedrock/Vertex.

### 4.5 Storage

Session **trees** (branch-and-retry via parent pointers) persisted as JSONL with a
SQLite index (pi_agent_rust's proven pattern). Migration to libsql (embedded, vector
search for router embeddings, optional Turso sync) behind the same `SessionStore`
trait; Postgres later for team/server mode. Transcripts zstd-compressed at the app
layer.

### 4.6 Token-efficiency machinery (the moat)

- **Output reduction at the source** (the native RTK layer): structured shell output
  (exit code, errors, tail, diff-only), hard caps on grep/read results, truncation
  markers (`… N lines elided`), dedup of repeated tool results.
- **Cache-aware prompt structure**: stable prefix (system → tools → context →
  history), no timestamps in the prefix; provider cache hits are tracked in usage.
- **Compaction** at ~90% context (prime-agent style): summarize older turns, keep
  recent verbatim. MVP: budget errors surface honestly; compaction lands Phase 2.
- **Budget governor**: `--max-cost`/`--max-input-tokens` with a crash-resilient
  journal (pi_agent_rust design).

### 4.7 Recursive search

Escalation ladder: deterministic tools (glob/grep, ~free) → `explore` subagent (cheap
model, read-only tools, hard token budget, returns a brief with `file:line` citations,
not raw dumps) → map/reduce across files for repo-wide questions. Subagent results
enter parent context as compact summaries (RLM insight).

### 4.8 Extensibility

MVP: hooks (lifecycle events: pre/post tool, on-turn-end) + SKILL.md reader (open
standard, free ecosystem) + AGENTS.md/CLAUDE.md memory files. MCP is opt-in with
per-server token accounting. Dynamic extension runtimes (QuickJS/WASM) are deferred —
hooks + MCP + skills cover 90% at far less complexity.

### 4.9 Persistent memory (baked in — Phase 1a)

Two tiers, mirroring Claude Code:
- **Repo tier (shared):** `AGENTS.md`/`CLAUDE.md` in the repo — already loaded by context assembly. Checkable into git, visible to other harnesses.
- **User tier (private, auto-memory):** `~/.modron/memory/<workspace-slug>/MEMORY.md` plus topic entries. The agent persists durable facts via a built-in `remember` tool; every later session in the same workspace gets a size-capped memory section in its system prompt. Memory must be inspectable and forgettable (`modron memory` subcommand) — no silent state.

### 4.10 Autocompaction (baked in — Phase 1a)

Goal: **long conversations are almost as cheap as new ones.** Before each turn, if the
estimated context exceeds a threshold (default 80% of the per-turn cap) and enough
turns have passed since the last compaction, the engine runs one compaction turn:
a cheap-model call (router-selected, `ActionKind::Summarize`) that summarizes the
older transcript into a compact state (task, decisions, edits, open items), stored as
a `Compaction` session event. The next turn's context is `[compact state] + recent
tail verbatim`. Design constraints: compact once, not continuously (min-interval);
the post-compaction prefix is byte-stable so prompt caches re-warm instead of
thrashing; compaction cost counts against the budget governor. Acceptance is a
measured one: post-compaction input tokens ≤ 1.5× a fresh session's for the same
task, asserted in the e2e suite. (Prior art: prime-agent's 95% trigger, OpenCode's
soft-delete, Claude Code auto-compact.)

### 4.11 Prompt-cache discipline (baked in — Phase 1a)

Usage already distinguishes `cached_input_tokens`. Now we *engineer* for it:
- Anthropic: explicit `cache_control` breakpoints on the stable prefix (system block + last tool definition). OpenAI-compatible: automatic caching — ordering discipline only.
- Byte-stability guarantee: system prompt + tool specs are built once per run and never mutate across turns (asserted by a test comparing recorded `TurnRequest`s).
- Cache-hit rate is surfaced in the CLI usage table — a saved token that was never counted is a lie.

## 5. Roadmap

| Phase | Scope | Exit criteria |
|---|---|---|
| **0 — Spine** (now) | Workspace, core traits, agent engine, Anthropic + OpenAI-compat providers, 6 essential tools, output reduction, JSONL session trees, budget governor, heuristic router, model catalog, print-mode CLI, mock-provider end-to-end test | `cargo test --workspace` green offline; one-shot CLI answers a question about a repo through mock end-to-end; live smoke when a key is present |
| **1a — Memory/cache/compaction** (now) | §4.9 persistent memory (`remember` tool + workspace memory), §4.10 autocompaction with measured cheapness gate, §4.11 Anthropic cache_control + byte-stability + cache-hit column, `modron memory` subcommand | e2e: memory persists across sessions; compaction triggers under pressure and post-compaction input ≤ 1.5× fresh-session baseline; stable-prefix test green; cache-hit % shown |
| **1b — Control plane + streaming** (now) | §Phase 1 safety: approval modes, macOS Seatbelt sandbox (network off), hooks, checkpoints + `modron rewind`, USD spend cap; provider-layer SSE streaming (wire-format proof); Z.ai first-class provider + `modron sessions list`; docs pass | e2e: mutating tool blocked without approval; sandbox blocks network but allows workspace writes; hook can veto a tool; rewind restores file + transcript; streaming fixtures prove both wire formats; gates green |
| 1 — Agent (residual) | Linux sandbox port (Landlock/bubblewrap), per-tool-class approval overrides, hardening from real usage | Agent can complete a multi-file edit task with approval gates and rollback on macOS + Linux |
| 2 — Efficiency | Compaction, cache-hit tracking, LSP diagnostics tool, token dashboard, prompt-budget CI gate | Measured cost-per-task baseline vs. Claude Code on a fixed task suite |
| 3 — Routing | Embedder router (Avengers-Pro style), DecisionModel trait + Jev adapter, subscription OAuth, routing-decision logging | Router beats static model choice on cost at equal quality on the task suite |
| 4 — Recursion | explore subagents, parallel spawn, daemonized core, RPC mode | Repo-wide questions answered under a fixed token budget vs. flat context |
| 5 — Ecosystem | MCP client (opt-in, token-accounted), SKILL.md + AGENTS.md polish, ACP server, packaging (single static binary) | Installs via one curl; works in Zed via ACP |

## 6. Risk register

| Risk | Severity | Mitigation |
|---|---|---|
| Subscription OAuth is ToS-gray (Anthropic cut off OpenCode Jan 2026) | High | API/OpenRouter/local are first-class; subscriptions are opportunistic add-ons |
| Aggressive output filtering hurts small-model success (RTK's measured failure) | High | Reduction layer ships with per-tool accounting; task-suite regression gate in Phase 2 |
| Jev access-limited | Medium | Open embedder default; Jev is a plugin |
| Router misroutes → quality drop | Medium | Per-action routing table starts conservative; log everything; cheap-model-first with escalation |
| Prompt-injection via tool output | Medium | Network-off sandbox, untrusted-content marking, exec mediation patterns |
| Scope creep vs. minimalism | Medium | Prompt budget CI metric is the forcing function |

## 7. Decisions made (do not re-litigate without new evidence)

- Build from scratch in Rust; study pi_agent_rust + codex-rs; no fork.
- libsql over Postgres for local-first MVP (store trait keeps Postgres open).
- JSONL+SQLite sessions now; libsql migration later behind `SessionStore`.
- Discrete tools, not Python-REPL-as-single-tool, for MVP.
- Discrete providers, no external routing proxy in the product path.
- MVP runs offline-gated: mock provider is the acceptance harness until keys exist.
- Toolchain pinned to Rust **1.99.1** (`rust-toolchain.toml`) — 2026-10-30; the 1.99.1
  macOS `rust-lld`/libLLVM fix is directly relevant on our dev platform.
- **Dogfooding aspiration:** once Phase 1a lands, subsequent Modron development
  should be driven *through Modron itself* (the harness building the harness) — an
  explicit goal, not a hard requirement. Keep the harness able to run on its own repo.

## 8. Companion document

`LEDGER.md` — the live work ledger. Single writer: the lead architect.
Workers report; the lead verifies against acceptance criteria and records evidence.
