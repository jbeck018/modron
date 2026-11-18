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
| **4a — Recursion** (done 2026-10-31) | explore subagents (read-only, budgeted, briefs-only into parent context), parallel read-only dispatch, per-tool allow/deny, SKILL.md skills | Repo-wide questions answered under a fixed token budget vs. flat context |
| 4 — Recursion (residual) | daemonized core + RPC mode; planner-level parallel explore | Sessions survive terminal close; multi-explore orchestration |
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

## 8. Competitive review — Kilo (2026-10-31)

[Kilo](https://kilo.ai) (Anaconda-owned; ex Kilo Code/Cline lineage) is the
closest high-momentum comparable: IDE extensions (VS Code/JetBrains/Cursor/
Windsurf), CLI 1.0 (Feb 2026), desktop app beta, Cloud, Slack, mobile; 500+
models at zero markup, BYOK + local models. Feature snapshot vs Modron:

**Modron ahead (defend):**
- Token efficiency as *enforced engineering*: prompt-budget CI gate, MCP
  schema caps (Kilo's MCP has no budget discipline), measured usage/cache-hit
  dashboards, autocompaction with a measured ≤1.5× cheapness gate.
- Learned routing (corpus centroids + LLM judge + Kev/Jev SystemOne, with an
  offline eval harness) — Kilo routes by user-picked model per mode.
- Safety: OS sandbox (Seatbelt), USD spend caps, approval allow/deny, hooks,
  checkpoints + rewind; single 4.1MB Rust binary, ms startup.

**Kilo ahead (adopt):**
1. **Custom agent personas** ("custom modes"): user-defined agents with
   tailored prompts + tool restrictions. → Adopt as `[agents.<name>]` config
   (system prompt + allow/deny + router override), `--agent <name>`. Cheap:
   composes existing EngineConfig pieces. **Backlog A1.**
2. **General delegation** (post-orchestrator pattern): large tasks split into
   subtasks run in isolated contexts by named agents. → Extend the explore
   subagent into a `delegate` tool (persona + budget + tool restrictions).
   **Backlog A2.**
3. **Write-protected memory**: Kilo protects AGENTS.md from its own agent.
   → Make write/edit refuse AGENTS.md/CLAUDE.md/.modron/* by default.
   **Backlog A3.**
4. **Surface breadth** (IDE/desktop/cloud/mobile): we will not chase this.
   Our answer is terminal-first + the planned ACP server (Zed) — one protocol
   instead of six GUI codebases. (Superseded 2026-11-15 by section 10: one
   NSH client codebase for every surface, still one protocol.)

**Marketplace posture:** Kilo ships a curated Skills/Agents/Plugins gallery.
Modron consumes the SKILL.md open standard already — run a compatibility pass
against popular community skills instead of building a marketplace.
**Backlog A4** (verification, not construction).

## 9. Companion document

`LEDGER.md` — the live work ledger. Single writer: the lead architect.
Workers report; the lead verifies against acceptance criteria and records evidence.

## 10. Program U - unified client, Cloudflare portal, harness gaps (2026-11-15)

Goal: a harness as easy to use as Claude Code or pi, reachable from any
device, running locally, in the cloud, or both in tandem. Three things
change: (a) every UI surface is rebuilt as ONE NSH app (https://nsh.dev,
Rust UI framework with web, iOS, Android, desktop and terminal backends);
(b) Cloudflare becomes the hub (portal, relay, cloud runs); (c) the harness
closes the gaps found against pi, oh-my-pi, jcode and dirge. Those four
projects are references for gaps only: nothing is adopted as a dependency
and no code is copied (dirge is GPL-3.0).

### 10.1 Target shape

- One client app (NSH, screens written in Rust). It only speaks
  modron-protocol and never embeds the agent. Targets: web (the portal)
  first, then phone, terminal, desktop.
- A session host that runs anywhere: `modron serve` on a local machine
  tunnelling out through the bridge, or in a Cloudflare container against
  a cloned repo.
- Cloudflare as the hub: one Durable Object per session relays frames and
  stores the event log so any number of devices attach, drop and resume.
- The existing ratatui TUI, gpui-kit desktop and TSX mobile clients are
  frozen (no new features) and deleted once the NSH app covers them. The
  Oculus theme and the main functionality carry over. Status 2026-11-16: the ratatui
  TUI crate is deleted (packet C9-TUI); plain `modron` and `modron tui` start the app.

### 10.2 Decisions (owner interview, 2026-11-15)

| Area | Decision |
|---|---|
| Hosting | Self-deploy into each user's own Cloudflare account; single owner first |
| Identity | One owner sign-in; any number of linked GitHub accounts and orgs; each repo or session picks the identity it clones and pushes as |
| Secrets | Worker secrets, injected into the container at start |
| Guests | Per-session invite links (viewer or approver), scoped and revocable, in v1; no guest accounts |
| UI language | Rust on NSH for every surface, including the terminal |
| Surface order | Web portal, then phone; desktop later |
| Tandem | Many devices drive one session; approvals answerable from any device with push; multi-person via invite links |
| Handoff | Any direction (local to cloud, cloud to local). furrow (github.com/Agent-Field/furrow) over R2 after a spike; a pushed session branch is the fallback |
| Writer model | One host runs the agent and owns the files; a handoff moves the host and the previous host stays attached as a full driver. Two hosts editing separate copies at once is out of scope |
| Idle cloud | Snapshot, sleep, restore on the next message or wakeup |
| Old clients | Delete and rebuild; keep the theme and main functionality |

### 10.2a Standing rule for product decisions (owner, 2026-11-15)

When a behaviour question comes up that the plan does not settle, the
lead compares how Claude Code and oh-my-pi handle it, picks the better
option for Modron, records the decision and the reason in LEDGER.md, and
proceeds. The owner is told, not asked.

Decisions made under this rule:

| Question | Claude Code | oh-my-pi / pi | Decision |
|---|---|---|---|
| Does the model see `!command` output? | Yes: bash-mode output joins the conversation | `!cmd` yes, `!!cmd` no | `!cmd` is added to the conversation; `!!cmd` runs and shows its output but never reaches the model |
| Does a user-typed command need approval? | No: the user typed it | No | No prompt for the owner's own command. It still runs in the sandbox and still honours the deny list, deny rules and pre_tool hooks (the owner's own hard limits). Approval mode, ask rules and the plan phase restrict the model, not the user. Guests cannot send it |
| What does saving a model from the picker do? | The /model choice becomes the default for later sessions | The choice is kept as the default; roles are separate config | Selecting a model changes the current session only; an explicit save (Shift+S, or a control) writes default model + provider to the server config, as the old terminal UI did. Explicit, because one server feeds several devices and a silent default change from a phone would surprise the laptop |
| Does the terminal app capture the mouse (wheel, click, drag-copy)? | No: it leaves selection and copy to the terminal | Mixed; fullscreen with its own handling | No capture. The terminal's own selection and copy keep working, which is what users expect from Claude Code; scrolling is PgUp/PgDn/Home/End. Revisit only if users ask |
| Keep `/issues`? | No equivalent | No equivalent | Not carried over. It reads a server-side file through the old in-process UI; no client can serve it without a new frame, and neither reference has it |
| Esc on an idle composer | Clears the draft (double Esc) | Clears the draft | Esc clears the draft when nothing else claims the key (approval, plan, suggestions, running) |
| Theme switching | /theme offers several themes (dark, light, colour-blind variants, ANSI) | Themes are selectable | Offer a /theme picker with what exists: Oculus dark, Oculus light and "system" (follow the terminal or browser appearance). No custom theme files for now; NSH can swap tokens at runtime, so a further palette is a data change |
| Removing a saved credential | /logout | /logout | Add it: a client can remove a provider credential it could have added. Local connections only, like adding one |
| Enter while a run is active | The message is taken as guidance for the running turn | Enter steers; a second key queues | Enter steers the running turn; Alt+Enter queues a follow-up (docs/PROTOCOL_V2.md section 2) |
| What `/settings` opens | A settings view | A settings view | The Settings screen (the old terminal UI opened its credentials wizard, which is now `/auth`) |
| Approval keys typed just before the prompt appears | Not accepted as an answer | Not accepted as an answer | Keep the old terminal UI's guard: a decision key pressed within a short window of the prompt appearing is ignored, so type-ahead cannot approve a command by accident |

### 10.2b Owner decisions, 2026-11-16 (answered directly)

- Plain `modron` launches the NSH app; crates/modron-tui is deleted, no
  fallback flag.
- The live bridge runs current code with the portal on (owner and host
  tokens set); bare share links need a sign-in or an invite.
- A pull request is opened for this branch and taken through merge.
- The cloud-run image is built and deployed from GitHub Actions, manually
  triggered (.github/workflows/cloud-deploy.yml).
- Handoff ships both ways: git session branch by default, furrow over R2
  as an opt-in setting (spikes/furrow-r2/RESULTS.md).
- Thinking-block replay: design the append-only transcript here first,
  then build; old sessions stay readable.
- Web push for approvals is built now.
- edit_lines stays opt-in until benchmarked; the owner will add a
  provider key for `modron bench`.

### 10.3 What survives, what goes

Keep: modron-protocol, modron-view, modron-server, mobile/rust-core (becomes
the single client core; already builds to wasm), the bridge Worker and its
per-session Durable Object, the Oculus palette as the one theme.

Discard at parity: crates/modron-tui view code, crates/modron-desktop,
mobile/app/App.tsx, the bridge's inline HTML viewer. Status 2026-11-16: crates/modron-tui and
crates/modron-desktop are deleted.

### 10.4 Harness gaps to close (in priority order)

| # | Gap today | Build |
|---|---|---|
| H1 | No steering: a message sent mid-run is refused | Steer (inject at next safe boundary) and queue (after the run) as protocol frames |
| H2 | No real plan mode; plan re-shown per message, not per turn | Enforced read-only plan phase, approve step, per-turn plan re-injection |
| H3 | Sub-agents throttled: approval-gated so sequential in Ask mode, children cannot ask, no live counters, no completion push, cap 4 | Approvals routed to the parent session, live turn/token counters, completion events, steerable per-agent view |
| H4 | No independent reviewer; finish gate only re-runs verify | Reviewer role reading the diff, verdict event before the turn reports done, cheap non-LLM claim checks |
| H5 | Background work thin: interval automations only, no notifications | Scheduled wakeups, pause reasons, push on completion or block |
| H6 | Edit is exact string replace only | Hash-anchored line edits; default only if `modron bench` shows a gain |
| H7 | No reasoning-effort control; only cheap/strong tiers | Effort parameter and named roles (plan, sub-agent, critic, summary) |
| H8 | Permissions by tool name only | Command and path rules, plus an explain-this-decision command |
| H9 | Small defects: /compact is a no-op, /context reports cumulative tokens, hooks only pre/post tool, MCP stdio only, no web fetch | Fix each |

### 10.5 Build order

1. Spikes (S1-S3): NSH web target in Rust (bundle size, load time, mobile
   Safari input); NSH terminal target (startup, streaming, scrollback);
   furrow over R2 from a container.
2. Protocol v2 (P1): events for steer/queue, plan approval, sub-agent
   approvals and counters, reviewer verdicts, pause/wake, actor and role on
   every event. Both tracks build against it. See docs/PROTOCOL_V2.md.
3. Harness track (H1-H9), each measured with `modron bench` / `modron eval`.
4. Client track (C1-C10): client core and theme, NSH app, portal (owner
   sign-in, linked GitHub, session list, invite links), tandem, cloud runs
   with sleep/restore and handoff, terminal, phone, CI.
5. Delete the old clients.

### 10.6 Risks

- NSH is pre-1.0 (0.5.x is "not yet a recorded compatibility baseline")
  and its web and terminal targets are on the critical path. Pin exact
  versions. Fallback for web: a plain web front end over the same wasm
  client core. The terminal is committed to NSH by owner decision; the
  spike decides how much must be worked around or upstreamed.
- furrow is very early (v0.3.x), has no Cloudflare support, and does not
  merge concurrent edits; R2 via its S3-compatible mode is unverified.
- AGENTS.md's parity contract describes three clients; it is rewritten
  when the NSH app lands (one codebase, divergence by target only).

Packet-level status and evidence: LEDGER.md, "Program U".

### 10.7 Design: thinking-block replay (H7b-2)

Finding first: the session log is already append-only (JSONL, one
SessionEvent per line; compaction, steering, tasks and private shell all
append; nothing rewrites a session file). So no storage rewrite is
needed; the work is carrying provider blocks through the message model.

Rules:

1. Shape. `Message` gains one additive field, `provider_blocks`
   (serde default, skipped when empty): an ordered list of
   `{ provider, api, kind, index, payload }`. `payload` is the provider's
   JSON, stored and replayed byte-for-byte, never interpreted. `index`
   places the block among the turn's text and tool calls. A field, not a
   new SessionEvent variant: older binaries drop whole lines of an
   unknown type (and fork re-serializes through the typed enum), but
   they ignore unknown fields, so old builds keep reading new sessions
   and only lose the blocks.
2. Capture. Anthropic: keep `thinking` blocks with their `signature`
   and `redacted_thinking` blocks from the stream (the signature delta
   is dropped today). OpenAI Responses: request
   `reasoning.encrypted_content` and keep reasoning items. OpenAI-
   compatible chat: nothing to keep.
3. Replay. When building a request, a block is sent only if its
   `provider` and `api` match the request's target; otherwise it is
   left out and the turn degrades to today's behaviour. This covers
   model switches, escalation and reviewer requests on another provider.
   Blocks go back in their original order inside the same assistant
   turn, before the tool calls they preceded.
4. Compaction. Blocks live and die with their message: a compacted
   message takes its blocks with it. The compaction boundary and the
   rewind indices do not change, because no new event kind is counted.
5. Atomic turns. The assistant message (with blocks) is appended
   before any early exit; the spend-cap stop currently breaks before
   the append and loses a billed turn. Fix that first.
6. Clients. Snapshots and live events strip `provider_blocks` at the
   server: clients never need them, payloads can be large, and
   redacted blocks are opaque by definition. The reasoning display
   keeps using summaries.
7. Tests. Stream fixtures with signatures round-trip to the next
   request unchanged; a provider switch drops them; compaction drops
   them with their message; an old-format session loads; a session
   written with blocks loads in a build without the field (fixture);
   snapshot frames carry no blocks; one live check against a real
   provider when a key is available (thinking plus tool use over two
   turns).

Unverified until the live check: the effect of replayed blocks on
prompt-cache prefixes.

### 10.8 Design: handoff between hosts (C6c)

One host owns the files at a time (10.2). A handoff moves three things
from the current host to the next: the working tree, the session log,
and ownership. Two carriers, chosen by `[handoff] carrier = "git" |
"furrow"` (default git; owner decision 10.2b).

1. `modron handoff export [--session ID]` on the current host, run
   only when the session is idle (refused while a run is active):
   - git carrier: commit everything `git status` shows (tracked changes
     and untracked files that are not ignored) to branch
     `modron/handoff/<session id>` with a fixed author and a message
     naming the session, WITHOUT moving the user's branch or index
     (build the commit from a temporary index, `git commit-tree`, update
     only that ref), then push that ref. Ignored files do not travel;
     the command lists how many were left behind.
   - furrow carrier: `furrow` CLI snapshot of the working tree to the
     configured store (S3 settings from the environment, never from
     the config file); everything travels, including ignored files.
   - Both: the session's JSONL log and its sidecars are written to a
     bundle (`<id>.handoff.tar`, provider blocks included, no
     credentials, no env files) stored with the carrier: on the git
     carrier as a blob on a second ref `modron/handoff/<id>-session`
     (never in the working tree), on furrow inside the snapshot's
     metadata directory.
   - It prints one line to give the next host: `modron handoff import
     <carrier-ref>`.
2. `modron handoff import <ref>` on the next host: fetch the ref,
   check out the tree into the working directory (refused when the
   directory has uncommitted changes of its own, unless `--force`),
   restore the session log into the local store under the same session
   id (refused when that id already exists with a different history,
   unless it is a prefix of the incoming log), and print the resume
   command. With `modron serve --remote-control` the session is then
   reachable under a NEW share key; the portal lists it under the same
   title.
3. Ownership: export writes a `HandedOff { to, at, carrier_ref }` event
   as the last line of the old host's log and the old host refuses new
   runs for that session until an import brings it back (a newer log
   with the old one as prefix). No distributed lock: the log's prefix
   rule is the arbiter, and a diverged log is refused, not merged.
4. Cloud: the container entrypoint accepts `HANDOFF_REF` and runs the
   import before `modron serve --remote-control`; on idle sleep the
   container runs the export so the next wake (or a laptop) can import.
5. Out of scope for v1: live handoff during a run, merging diverged
   trees, moving provider credentials.

### 10.9 Decision and design: provider credentials belong to the owner (C6d)

Owner, 2026-11-16: model auth must come from the harness sign-in, not
from how a container was deployed. Compared with Claude Code (model
access follows the signed-in user; environment secrets are for the
user's code): same rule here. This replaces the "Secrets" row of 10.2
for provider credentials; infrastructure tokens stay Worker secrets.

1. Vault in the bridge. Owner-only routes (owner cookie; invites and
   viewers refused): `GET /api/credentials` lists provider ids and
   whether each is set (never a value), `PUT /api/credentials/<id>`
   stores an API key, `DELETE` removes it. Values are stored in the
   Directory DO encrypted with AES-GCM under a key derived (HKDF) from
   a dedicated Worker secret `VAULT_KEY`; unset = 503 and nothing else
   changes. Values are never logged and never returned to a browser.
2. Host fetch. `GET /api/host/credentials` with the HOST_TOKEN bearer
   returns the decrypted set to a host. `modron serve
   --portal-credentials` (env MODRON_PORTAL_CREDENTIALS=1; the cloud
   container sets it) fetches them at start and after each tunnel
   reconnect, keeps them in process memory only (never written to the
   env file or the session log), and applies them to future turns. A
   locally configured key wins over a vault key.
3. App. On the web target, when the app is served by a bridge and the
   viewer is the owner, the credentials screen writes API keys to the
   vault (HTTPS to the bridge, not through the session socket). The
   existing rule stays: key entry and removal over a relayed session
   socket are refused.
4. The cloud deploy workflow and the container worker drop
   ANTHROPIC_API_KEY and OPENAI_API_KEY.
5. Not in this pass: subscription sign-ins (browser OAuth and token
   files), per-invite credentials, key rotation of VAULT_KEY.

### 10.10 Design: linked GitHub accounts and cloud runs from the portal (C6e, C6f)

Owner, 2026-11-16: no per-deploy repo or tokens; build the gaps. The
container worker is deployed once with no runtime secrets of its own.
Everything a run needs comes from the bridge at wake time.

Bridge API (owner cookie unless noted; JSON; `cache-control: no-store`;
same-origin `Origin` required on writes; 503 when `VAULT_KEY` is unset):

- `GET /api/github/accounts` -> `{ accounts: [{ id, login, kind }] }`
  (`kind` is "token" for a pasted access token; "oauth" later).
- `POST /api/github/accounts` body `{ token }`: the bridge calls
  GitHub `GET /user` with it, stores the token in the vault under a
  new account id with the returned login, and answers the account row.
  A token GitHub refuses is a 400 with GitHub's status, never the token.
- `DELETE /api/github/accounts/<id>`.
- `GET /api/github/repos?account=<id>&q=<text>&page=<n>` ->
  `{ repos: [{ full_name, private, default_branch, pushed_at }],
  next_page }` (GitHub `GET /user/repos`, sorted by pushed, filtered
  by `q` on the bridge side; 50 per page).
- `GET /api/runs` -> `{ runs: [{ id, repo, account, branch, state,
  created_at, session_url }] }`; states: starting, running, sleeping,
  stopped, failed (with a short `reason`).
- `POST /api/runs` body `{ account, repo, branch?, handoff_ref? }` ->
  the run row. The bridge creates a run id, and calls the container
  worker (`CONTAINER_URL` + `WAKE_TOKEN`, both bridge secrets; unset =
  503 "cloud runs are not configured") with the per-run environment:
  bridge URL (its own origin), host token, repo, branch, the account's
  git token, handoff ref, and the run id. The container starts
  `modron serve --remote-control --portal-credentials
  --handoff-on-exit` for that repo; the session it registers carries
  the run id so the portal can link run and session.
- `POST /api/runs/<id>/stop`, `POST /api/runs/<id>/wake`.
- Viewers and invited approvers get 403 on all of these.

Container worker: one route `POST /wake` (bearer WAKE_TOKEN) taking the
per-run environment in the body, one container instance per run id,
sleep after idle; `POST /stop`. It holds no provider keys, no repo and
no git token of its own. The git token is passed to git through
environment-based config and kept for the push that handoff-on-exit
needs (it is not exported to the agent's tools: `modron serve` scrubs
it from child process environments).

App (web target, owner only, served by a bridge): a "Cloud" screen
reachable from the portal's session list: GitHub accounts (add by
token, remove), a repo picker (type to filter), "start run", and the
runs list with state, open, stop, wake. Provider credentials on the
credentials screen write to the vault (10.9 item 3). The terminal
target shows none of this (divergence row).

Later: GitHub OAuth (needs an OAuth app's client id), per-run cost
limits, more than one container size.


### 10.11 Program V - parity gaps against oh-my-pi and oh-my-claudecode (2026-11-17)

Owner request 2026-11-17: build all five gaps from the comparison. Decisions
made under 10.2a (compare with Claude Code and oh-my-pi, decide, record).

Already present (found while planning, so out of scope): AGENTS.md / CLAUDE.md
memory, `.claude/skills`, session fork (`/fork`, protocol and store), rewind.

| Id | Gap | Decision |
|---|---|---|
| V1 | Read other agents' config | Rules: also load `.cursor/rules/*.md*`, `.github/copilot-instructions.md`, `GEMINI.md`, `.windsurfrules` as project memory (text only, size capped, source named in the prompt). Commands: `.claude/commands/*.md` become slash commands. MCP: discover `.mcp.json` and `.cursor/mcp.json`, but a discovered server runs only after the user trusts it for that workspace (Claude Code asks the same; oh-my-pi does not). `modron doctor` lists what was found. |
| V2 | Web search and browser | `web_search` is an on-demand tool with backends Brave, Tavily and Exa, chosen by which key is configured; no keyless scraping. Browser: no native CDP driver; `modron mcp add playwright` installs a preset MCP server entry (Claude Code's route; oh-my-pi ships its own Puppeteer). |
| V3 | Team mode | `/team [N] <task>` and `modron team`: plan, N workers as `delegate` children in worktrees, merge, verify with the `[verify]` command and the reviewer, fix rounds up to a cap. `--until-verified` keeps going until verify passes or the cap or budget is hit. Reuses delegate, merge_worktree, the finish gate and the swarm rows; lands in both app targets. |
| V4 | Session tree | Fork exists. Add: fork from an earlier turn (not only the tip), lineage stored on the session, and the sessions screen shows the parent. |
| V5 | Subscription logins | Add a native ChatGPT (Codex) login so reuse of `~/.codex/auth.json` is not the only route. GitHub Copilot chat login is NOT built: it needs another product's client id; GitHub Models tokens stay the supported route. Owner can overrule. |

Order: V1 and V2 in parallel (different crates), then V3, then V4 and V5.
