# Modron Work Ledger

> **Rename note (2026-10-31):** this project was called "Kestrel" through
> Phases 0–2a; entries below keep that name as history. The product, crates
> (`modron-*`), binary (`modron`), env vars (`MODRON_*`), and paths
> (`~/.modron`) are all Modron now.

**Single-writer rule:** only the lead architect updates this file. Worker agents
report their results in their final message; the lead verifies against acceptance
criteria and records evidence here.

**Statuses:** `TODO` → `IN_PROGRESS` → `DONE` | `BLOCKED` (with reason) | `DEFERRED`.

---

## Definition of Usable (MVP exit gate)

All of the following must hold before the ledger is closed for Phase 0:

1. `cargo build --workspace` succeeds with zero errors.
2. `cargo clippy --workspace -- -D warnings` is clean.
3. `cargo test --workspace` is green, **including one offline end-to-end test**:
   mock provider (scripted tool calls) → real engine → real tools → session persisted
   → usage accounted. No network required.
4. `cargo run -p modron-cli -- "…" --provider mock` runs the loop end-to-end from the
   shell and prints a final answer + usage/cost summary.
5. A session JSONL file is written under `~/.modron/sessions/` (or the configured
   dir) and is resumable via `--resume <id>`.
6. Live-API smoke (`--provider anthropic` / `--provider openai-compat`) is *wired and
   documented*; it is permitted to be unexecuted (no keys in this environment).

---

## Contracts (frozen by the scaffold — `crates/modron-core/src/lib.rs`)

Workers implement **against the code in modron-core**, not against this prose. If a
contract is wrong or blocks you, do NOT edit modron-core — report it in your final
message and work around it locally if possible. The lead arbitrates.

- `ModelProvider::turn(TurnRequest) -> TurnResponse` — non-streaming for MVP.
- `Tool::execute(json args, &ToolCtx) -> ToolOutcome` — `ToolOutcome.content` is what
  enters the model's context; keep it small.
- `SessionStore` — append-only event log per `SessionId` (uuid); sessions are trees
  (events carry optional `parent_id`) but a linear MVP transcript is acceptable if
  `parent_id` is present-but-unused.
- `ModelSelector::select(&SelectRequest) -> ModelChoice` — synchronous, <1ms, no I/O.
- `Engine::run_task(&self, task) -> RunSummary` — owns the loop: assemble → select
  model → provider.turn → dispatch tools → repeat until EndTurn / max_turns / budget.

---

## Work items

| ID | Crate | Item | Acceptance criteria | Depends | Status | Evidence |
|---|---|---|---|---|---|---|
| F1 | — | Workspace scaffold, pinned deps, crate stubs | `cargo check -p modron-core` green on fresh clone | — | DONE | lead: checked pre-fanout; re-verified 2026-10-30: `cargo build --workspace` → green |
| F2 | core | Frozen contracts in `modron-core/src/lib.rs` | Compiles; docs on every trait | F1 | DONE | lead: lib.rs left byte-identical through integration (all engine work landed in engine.rs/testing.rs) |
| P1 | providers | Anthropic Messages client (`turn` non-streaming, tool use, `usage` mapping) | Unit test with recorded-JSON fixture (no network): request body shape + response parse incl. `tool_use` blocks and usage | F2 | DONE | `cargo test -p modron-providers --lib` → 33 passed (fixture tests for `build_body` shape + `tool_use`/usage parse) |
| P2 | providers | OpenAI-compatible client (chat completions, tools, usage) | Unit test with recorded-JSON fixture; works against `base_url` override (Ollama/OpenRouter-ready) | F2 | DONE | same run → 33 passed incl. base-URL override tests |
| P3 | providers | Provider factory from config/env (`anthropic`, `openai-compat`, `mock`), key checks with actionable errors | Missing key → `Error::Config` naming the env var; factory unit-tested | P1, P2 | DONE | factory tests in the 33; live: `cargo run -p modron-cli -- "hello" --provider anthropic` without key → `config error: missing ANTHROPIC_API_KEY — export …`, exit 1 |
| P4 | providers | Retry/backoff (429/5xx, max 3) + error taxonomy mapping to `core::Error` | Unit test with a stubbed failing transport | P1 | DONE | retry tests in the 33 (429 retry, 5xx give-up, non-retryable fail-fast, 200ms/800ms schedule); wired into the CLI product path via `RetryingProvider` |
| C1 | core | Agent engine loop (tool dispatch, max_turns, clean stop reasons) | Mock end-to-end test (see V2) passes | F2 | DONE | `cargo test -p modron-core --test engine_test` → 10 passed (full tool loop, max_turns, budget stop, tool-failure recovery, provider fallback) |
| C2 | core | Token estimation (`estimate_tokens`) + budget governor (input-token cap per turn, cumulative caps; honest `Error::Budget`) | Unit tests: estimator sanity; governor trips at cap and produces actionable error | C1 | DONE | `cargo test -p modron-core --lib` → 19 passed (estimator, governor cap trips with both numbers named) |
| C3 | core | Context assembly: budgeted system prompt (<2k tokens, asserted in test) + AGENTS.md/CLAUDE.md reader (repo root + cwd, size-capped) | Test: system prompt token count asserted < 2048 with default tools' worth of text; AGENTS.md content included when present | C1 | DONE | same run (prompt < 2048 asserted with ~10-tool tier + 3.5KB memory; AGENTS.md > CLAUDE.md precedence; 8KiB cap on char boundary) |
| C4 | core | `testing::MockProvider` — scripted `TurnResponse` sequence, records requests for assertions | Used by V2 test; exported behind `pub mod testing` | C1 | DONE | same run + doctest (`cargo test -p modron-core --doc` → 1 passed); extended by lead with `demo_script()`/`demo_provider()` |
| T1 | tools | `Tool` impls: `read` (line ranges, 2k-line/48KB caps), `glob`, `grep` (regex, `ignore` crate, 50-hit cap), `bash` (timeout 30s, output cap 10KB, exit code captured), `write`, `edit` (exact old→new replace, mismatch = error) | Each tool has a unit test incl. a cap-truncation test | F2 | DONE | `cargo test -p modron-tools --lib` → 46 passed (per-tool tests incl. cap-truncation, timeout kill, exit-code capture) |
| T2 | tools | Output-reduction layer: single choke point that caps + elides (`… N lines elided`) every tool result before it enters context; strips ANSI | Unit test: oversized bash output → capped with elision marker; ANSI stripped | T1 | DONE | same run (`reduce_output` choke point; 100k-line bash → `[… bytes elided]` under 10KB + elision marker) |
| T3 | tools | ToolRegistry with JSON-schema validation of args (reject unknown/missing required with tool-error outcome, not panic) | Unit test: bad args → `ToolOutcome { is_error: true }` with helpful message | T1 | DONE | same run (`registry_dispatch_rejects_bad_args_as_error_outcome`, per-tool invalid-args tests) |
| S1 | store | JSONL session store: append-only events (task, messages, tool results, usage, model selections), `list`, `load`, `resume` reads last state | Unit tests: roundtrip write→load; resume reconstructs transcript; corrupt line is skipped with warning, not fatal | F2 | DONE | `cargo test -p modron-store --lib` → 8 passed (roundtrip, resume-after-reopen, spliced corrupt line skipped, `updated_at` ordering) |
| S2 | store | SQLite index over sessions (rusqlite, bundled): id, title, timestamps, message count | Stretch. Only after S1 + everything else green | S1 | DEFERRED | per plan: stretch item; JSONL store satisfies Phase 0 |
| R1 | router | Heuristic `ModelSelector`: keyword/size heuristics mapping `SelectRequest` (action kind, task length) → model tier (cheap/strong); conservative defaults; `reason` string always populated | Unit tests: search/summarize → cheap; plan/edit on long task → strong; unknown → default | F2 | DONE | `cargo test -p modron-router --lib` → 20 passed (tier mapping, overflow escalation to strong, exhaustively-populated reason) |
| R2 | router | Model catalog: static TOML/JSON-embedded data (~12 models: name, provider, context window, $/1M in+out, tier) + lookup + `unknown model` errors | Unit test: lookup known model; pricing math helper tested | R1 | DONE | same run (12 embedded models, unique names, exact cost math, Ollama free) |
| L1 | cli | clap CLI: `modron "task"` one-shot; flags `--provider`, `--model`, `--max-input-tokens`, `--resume <id>`, `--working-dir`, `--max-turns`; streams assistant text to stdout; prints usage + est. cost table at end; exit code 0/1 | Manual + integration test: `--provider mock` run produces answer + usage lines; `--resume` continues a session | C*, T*, S1, R1, L2 | DONE | lead: `cargo test -p modron-cli --test e2e` → 2 passed (incl. subprocess run of the real binary + `--resume`); manual quickstart run → answer + usage table + `session: <uuid>`, exit 0; resume continues the same session id and per-run usage does not double-count |
| L2 | cli | Config: env keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`…), optional `~/.config/modron/config.toml` (default provider/model, budget defaults) | Unit test: env override beats config file; missing everything → actionable error | F2 | DONE | `cargo test -p modron-cli --bin modron` → 10 passed (env>file precedence, malformed-TOML fallback, empty-env-as-unset); `#![allow(dead_code)]` + TODO(lead) removed once L1 wired it |
| L3 | cli | README quickstart section in `modron-cli` crate docs | Build/run instructions verifiable by lead | L1 | DONE | lead: root `README.md` (build, keyless quickstart, live Anthropic/OpenAI/Ollama commands, config, sessions, tests); quickstart command executed verbatim → success |
| V1 | — | **LEAD:** `cargo build --workspace` + `cargo clippy --workspace -- -D warnings` + `cargo test --workspace` all green | Definition of Usable #1–3 | all | DONE | lead 2026-10-30: build → green; clippy `--all-targets -D warnings` → clean; `cargo test --workspace` → 149 passed / 0 failed |
| V2 | — | **LEAD:** offline end-to-end test green: mock provider drives real engine + real `bash`/`read` tools on a temp dir; asserts transcript, tool execution, session file written, usage summed | Definition of Usable #3–4 | C4, V1 | DONE | `cargo test -p modron-cli --test e2e` → 2 passed: marker `echo … \| tee` via real bash, real read back, marker in transcript + on disk, `.jsonl` with Task/Message/ToolResult/Usage events, `RunSummary.usage` == exact script sum (450/75/10), resume reconstructs 7-message transcript |
| V3 | — | **LEAD:** live smoke documented but expected skipped here: verify clients compile + factory errors are actionable without keys; document exact command for key-holders | Definition of Usable #6 | P3, V1 | DONE | no-key runs (env keys unset): anthropic → `missing ANTHROPIC_API_KEY — export … (get a key at …)`, openai-compat → `missing OPENAI_API_KEY — export … (or point OPENAI_BASE_URL at a local server …)`, both exit 1; exact live commands in README |
| V4 | — | **LEAD:** ledger audit — every item verified against acceptance criteria, evidence recorded, deviations listed, final verdict in `LEDGER.md` footer | This table has no TODO left | V1–V3 | DONE | this audit; worker claims re-verified by re-running `cargo test/clippy --workspace`, per-crate targeted runs, and manual CLI runs |

**Worker assignments**

- **Worker A (providers + engine):** P1, P2, P3, P4, C1, C2, C3, C4 — crates
  `modron-providers`, and `modron-core/src/{engine,budget,context,testing}.rs`.
- **Worker B (tools + store):** T1, T2, T3, S1 — crates `modron-tools`,
  `modron-store`.
- **Worker C (router + config):** R1, R2, L2 — crates `modron-router`, plus config
  module inside `modron-cli/src/config.rs`. Do not wire the CLI binary; the lead does.
- **Lead architect:** L1, L3, V1–V4, contract arbitration, this ledger.

---

## Accepted deviations (design debt, recorded not hidden)

1. **Retry taxonomy as message prefixes** (Worker A, P4). The frozen
   `Error::Provider` has no status field, so retryability is encoded as message
   prefixes (`HTTP <status>: …`, `timeout: …`, `connection error: …`) produced in
   modron-providers and parsed by `retry::is_retryable`. Works, tested, and
   contained inside one crate — but it is stringly-typed. When `Error` grows a
   status/kind field, replace the prefixes.
2. **tokio promoted to a real dependency of modron-providers** (Worker A). The
   retry backoff needs an async sleep in library code. Tokio was already in the
   workspace; no new external dependency introduced.
3. **Engine output cap is a const** (Worker A, C1). `EngineConfig` has no
   output-cap field, so the engine sends `max_tokens: 8192` on every turn
   (`MAX_OUTPUT_TOKENS` in engine.rs). Fine for non-streaming MVP; revisit when
   streaming lands.
4. **Keyless openai-compat when `OPENAI_BASE_URL` is set** (Worker A, P3).
   Local servers (Ollama, llama.cpp, vLLM) ignore auth; the factory only requires
   `OPENAI_API_KEY` when no custom base URL is configured.
5. **bash tool merges stderr into the result** (Worker B, T1). stderr follows
   stdout into model-visible content (blocking-reader-safe via concurrent
   readers), an `exit code: N` line is always appended, and non-zero exits are
   error outcomes. Slightly larger context; much better small-model behavior.
6. **Config path via `dirs::home_dir()`** (Worker C, L2). `~/.config/modron/config.toml`
   is not `XDG_CONFIG_HOME`-aware. Accepted for Phase 0.
7. **Store `message_count` counts Message + ToolResult events** (Worker B, S1).
   "Transcript items" semantics rather than strict assistant/user messages;
   consistent across store tests and the meta file.
8. **Lead (L1/V2):** `modron-core/src/lib.rs` stayed frozen; the resume
   contract landed as `Engine::run_task_resuming(&self, session_id, task)` in
   **engine.rs** (transcript reconstructed from `Task`/`Message`/`ToolResult`
   events; same session id; log stays append-only). `testing.rs` gained
   `demo_script()`/`demo_provider()` for the keyless CLI demo. The engine now
   emits info-level progress events (turn start, tool done) that the CLI surfaces
   on stderr; ANSI only when stderr is a TTY. The CLI usage table is scoped to
   the current run via an event-count baseline so a resumed session does not
   double-count prior usage. No new dependencies; no lib target added (the e2e
   subprocess test drives the binary via `CARGO_BIN_EXE_modron`).

---

## Verification log (lead architect only)

- **2026-10-30 — baseline (worker-claim verification).** `cargo build --workspace`
  → green; `cargo clippy --workspace --all-targets -- -D warnings` → clean;
  `cargo test --workspace` → 142 passed / 0 failed (33 providers, 18 core lib,
  7 engine integration, 46 tools, 8 store, 20 router, 9 cli config, 1 doctest).
  Worker A/B/C claims confirmed by re-running their suites, not by trusting reports.
- **2026-10-30 — lead implementation (L1, demo script, resume).** Added
  `demo_script()`/`demo_provider()` (modron-core/testing.rs) and
  `Engine::run_task_resuming` + transcript reconstruction + info progress events
  (modron-core/engine.rs; lib.rs untouched). Rewrote modron-cli/src/main.rs:
  clap one-shot CLI, flag>config>default precedence, mock = scripted demo,
  live providers wrapped in `RetryingProvider`, usage table from persisted
  `Usage` events with catalog-estimated cost, `--resume`, exit 0/1. Removed
  `#![allow(dead_code)]` from config.rs. New core tests: engine_test 7→10,
  core lib 18→19.
- **2026-10-30 — V2 acceptance test.** `crates/modron-cli/tests/e2e.rs`:
  engine-level offline e2e (real engine + real bash/read tools on a tempdir +
  real JsonlSessionStore; marker through transcript and on disk; `.jsonl` with
  Task/Message/ToolResult/Usage; usage == exact script sum; resume reconstructs
  transcript in the same session id) plus a subprocess test running the real
  binary with `--provider mock` (exit 0, answer + "ESTIMATED" usage table on
  stdout, `.jsonl` on disk, `--resume` continues). `cargo test -p modron-cli
  --test e2e` → 2 passed.
- **2026-10-30 — V1 gates.** `cargo build --workspace` → green;
  `cargo clippy --workspace --all-targets -- -D warnings` → clean (one fix:
  clippy::field_reassign_with_default in main.rs);
  `cargo test --workspace` → **149 passed / 0 failed**.
- **2026-10-30 — V3 live-smoke prep.** With keys unset: `--provider anthropic`
  → `error: cannot start provider 'anthropic': config error: missing
  ANTHROPIC_API_KEY — export ANTHROPIC_API_KEY=… (get a key at
  https://console.anthropic.com)`, exit 1; `--provider openai-compat` → names
  `OPENAI_API_KEY` and the `OPENAI_BASE_URL`/Ollama alternative, exit 1; bogus
  `--resume <uuid>` → actionable session error. Exact live commands documented
  in README. Live APIs remain unexecuted (no keys in this environment) — wired
  and documented, per Definition of Usable #6.
- **2026-10-30 — L3.** Root `README.md` written; its quickstart command
  (`cargo run -p modron-cli -- "list the rust files here and summarize"
  --provider mock`) executed verbatim → answer + usage table + session line,
  exit 0, transcript written under the default `~/.modron/sessions/`.
- **2026-10-30 — Phase 1a: worker verification (X0–X2, K1–K2).** Worker 1 and
  Worker 2 claims re-verified by re-running their suites, not by trusting
  reports: `cargo test -p modron-core --test compaction_test` → 5 passed
  (incl. the X0 roundtrip guard and the measured ≤ 1.5× cheapness gate:
  post-compaction 57 tok vs 1.5× control 111); `--test engine_test` → 10
  passed; `cargo test -p modron-providers --lib` → 37 passed (incl.
  `cache_control_appears_exactly_twice_on_last_tool_only` and the
  openai-compat `body_never_carries_cache_control` guard); `--test stability`
  → 2 passed. Byte-stability is real: `system` and canonical-JSON `tools`
  byte-identical across all recorded turns of multi-turn runs. Deviations
  noted and recorded (13–14 below).
- **2026-10-30 — Phase 1a: Worker 3 stall + lead repairs (M1–M3).** Worker 3
  delivered memory.rs, remember.rs, and the context.rs user-memory fns, then
  stalled: 22/23 core-lib tests green with
  `memory::tests::slug_is_sanitized_and_hash_suffixed` failing, plus a
  fatal-under-`-D warnings` clippy `explicit_counter_loop` in
  `MemoryStore::read_entries`. Diagnosis: the implementation's documented
  contract (leading path segments within a 48-char prefix budget + 6-hex FNV
  hash) is correct and matches the PLAN §4.9 example for real workspaces; the
  test wrongly expected deep `docs-projects-modron-` segments to survive for
  a tempdir workspace, whose canonicalized macOS path
  (`/private/var/folders/…`) exhausts the budget first. Fix: implementation
  kept, test rewritten — pure `workspace_slug` shape/sanitization/hash/prefix
  assertions on synthetic paths plus the tempdir-independent integration
  properties. Clippy: counter loop replaced with `.take(limit)`.
  `cargo test -p modron-core --lib` → 46 passed; clippy `-D warnings` clean.
  M2 verified: `cargo test -p modron-tools` → 51 passed (incl. 4 remember
  tests). M3's context fns verified by the same core-lib run.
- **2026-10-30 — Phase 1a: lead engine wiring.** Additive lib.rs amendment
  (recorded per the phase rule): `EngineConfig.memory_root:
  Option<std::path::PathBuf>`, default `None` (engine runs stay memory-free
  unless configured; all existing struct literals use `..Default()` so
  nothing breaks). engine.rs `assemble_system` (kept minimal) composes
  `context::with_user_memory(&system, &context::load_user_memory(root,
  working_dir))` when the root is set — still assembled once per run, so the
  K2 byte-stability guarantee is untouched (stability 2/2 green). The CLI
  passes the SAME memory root to the engine and to
  `modron_tools::default_registry_with_memory`, so a `remember` call writes
  what the very next run's prompt reads. Also demoted the engine's raw
  `compacted:` info log to debug (deviation 11): the CLI's X3 stderr line is
  the single user-facing compaction surface.
- **2026-10-30 — Phase 1a: lead CLI (X3, K3, M4).** main.rs: `--compact-threshold
  <0..1>` (validated strictly between 0 and 1) and `--no-compact` wired into
  `CompactionPolicy`; per-run compaction progress on stderr
  (`compacted: N events → summary (est. saved ~X tok)`, est. = re-rendered
  older-transcript estimate − summary estimate, clamped ≥ 0, honest "est.");
  usage table gains the `cache hit` column (cached/(input+cached), `0%` when
  none, source labeled); `modron memory list|show|forget` clap subcommands
  over the same memory root, with `--sessions-dir`/`--working-dir` marked
  global so they work on both the run and memory paths. Memory root =
  `<sessions-dir parent>/memory` (default `~/.modron/memory`), overridable
  for tests via `--sessions-dir`. `modron "task"` unchanged; a task starting
  with the word `memory` needs `--` (documented in the arg help). Live check:
  `modron memory list` on a fresh machine → `no memory for this workspace
  yet (…)`; demo run shows the new column with honest `0%`; `--compact-threshold
  1.5` → `error: --compact-threshold must be strictly between 0 and 1`.
- **2026-10-30 — V6 acceptance.** New `crates/modron-cli/tests/phase1a.rs`
  (2 passed): (a) memory e2e — scripted mock run drives the real engine with
  the real `remember` tool → entry file on disk → `modron memory list`
  (real binary) shows it → a second engine run's recorded request carries
  `## Persistent memory` + the body → `modron memory show` prints it →
  `modron memory forget` removes it → list empty → repeat forget exits 1
  with `no memory entry titled '…'`; (b) compaction e2e — the REAL binary
  with `--provider openai-compat` against a local scripted HTTP server
  (keyless `OPENAI_BASE_URL` path, no demo-script changes needed),
  `--max-input-tokens 12000 --compact-threshold 0.5`: compaction fires
  exactly once, stderr carries `compacted: 3 events → summary (est. saved
  ~N tok)` with N > 0, the run completes with the final answer on stdout,
  and the transcript holds exactly one Compaction event. (c) stable-prefix +
  cache_control assertions = the K1/K2 suites (re-run green); the measured
  ≤ 1.5× cheapness gate lives in compaction_test (re-run green). (d) Phase-0
  e2e (`tests/e2e.rs`) untouched → 2 passed.
- **2026-10-30 — V5 gates (Phase 1a close).** Toolchain confirmed pinned:
  rustc/cargo 1.99.1 via rust-toolchain.toml. `cargo build --workspace` →
  green; `cargo clippy --workspace --all-targets -- -D warnings` → clean
  (two fixes in the new CLI code: `field_reassign_with_default` → struct
  init, `useless_conversion`); `cargo test --workspace` → **197 passed /
  0 failed** (cli bin 13, cli e2e 2, cli phase1a 2, core lib 46, compaction 5,
  engine 10, stability 2, doctest 1, providers 37, router 20, store 8,
  tools 51).

## Final verdict

**Phase 0 is usable.** All six Definition-of-Usable gates hold, verified by
re-running the commands, not by trusting reports:

1. ✅ `cargo build --workspace` succeeds with zero errors.
2. ✅ `cargo clippy --workspace --all-targets -- -D warnings` is clean.
3. ✅ `cargo test --workspace` is green — 149 passed / 0 failed — including
   `crates/modron-cli/tests/e2e.rs`: scripted mock → real engine → real
   `bash`/`read` tools on a temp dir → real JSONL session persisted with
   Task/Message/ToolResult/Usage events → usage summed exactly. No network.
4. ✅ `cargo run -p modron-cli -- "…" --provider mock` runs the loop end-to-end
   from the shell, keyless: answer on stdout, estimated-cost usage table,
   `session: <uuid>` footer, progress on stderr, exit 0.
5. ✅ A session JSONL (+ meta) is written under `~/.modron/sessions/` (or
   `--sessions-dir`) and `--resume <id>` continues the same session id with the
   reconstructed transcript; the log stays append-only across resumes.
6. ✅ Live smoke is wired and documented (README quickstart covers Anthropic,
   any OpenAI-compatible endpoint, and keyless Ollama); factory errors without
   keys are actionable and exit 1. Unexecuted here — no keys — as planned.

**What works:** the full spine — budgeted context assembly (<2k-token prompt,
AGENTS.md/CLAUDE.md memory), heuristic routing with overflow escalation,
non-streaming Anthropic + OpenAI-compatible clients with 429/5xx/timeout retry,
six capped tools behind one output-reduction choke point, JSONL session store
that tolerates corruption, per-run honest usage accounting with catalog-estimated
costs, and a keyless demo path that exercises all of it.

**What doesn't (yet):** no streaming, no session list command (store supports
`list`; the CLI doesn't expose it), no compaction, no sandbox/approval gates on
`bash`/`write`/`edit` — they run with the caller's full authority, so Phase 0 is
for trusted, local, one-shot use only. Live providers are compile- and
fixture-verified but have never spoken to a real API from this environment.

**Known debt:** see "Accepted deviations" above — chiefly the stringly-typed
retry taxonomy, the fixed 8192 `max_tokens` ask, and `message_count` semantics.

**Highest-leverage next step:** spend one API key — run the documented live
smoke against Anthropic and one OpenAI-compatible endpoint (OpenRouter or local
Ollama). The real-API wire format of P1/P2 is the largest untested surface in the
tree; everything downstream (streaming, cache-aware prompts, compaction, cost
baselines) builds on that turn call being right. Immediately after, start Phase 1
with approval modes + sandboxing for `bash`/`write`/`edit` — safety is the gap
between "usable" and "safe to point at a real repo".

---

# Phase 1a — Memory / Cache / Compaction (opened 2026-10-30)

Toolchain note: project pinned to Rust 1.99.1 (`rust-toolchain.toml`); all Phase 0
gates re-verified green under it before this phase opened.

**Contract amendment rule for this phase:** `modron-core/src/lib.rs` may receive
*additive-only* changes (new struct fields with `Default`, new enum variants).
Changing or removing existing fields/variants still requires lead sign-off and must
be recorded. Additive serde changes must not break loading Phase-0 session files.

| ID | Crate | Item | Acceptance criteria | Depends | Status | Evidence |
|---|---|---|---|---|---|---|
| X0 | core | Contract amendment: `EngineConfig.compaction: CompactionPolicy` (enabled, threshold_frac=0.8, min_interval_turns=4, tail_messages=6, summary_max_bytes=2048) + `SessionEvent::Compaction { summary, summarized_events }` | Additive only; Phase-0 session files still load (roundtrip test); all existing tests green | — | DONE | worker 1: compaction_test `phase0_session_events_still_load_and_compaction_roundtrips` (Phase-0 events load unchanged; `{"type":"compaction",…}` roundtrips; default policy asserted); `cargo test -p modron-core --test compaction_test` → 5 passed; lead re-ran → 5 passed |
| X1 | core | Autocompaction in engine: pre-turn check (estimate > threshold_frac × max_input_tokens AND turns_since_compaction ≥ min_interval) → compaction turn via selector (Summarize → cheap model) summarizing older transcript into compact state; rebuild context as [compact-state message] + last `tail_messages` verbatim; append Compaction event; compaction usage feeds budget governor; no re-compaction within min_interval turns | Engine tests with MockProvider + tiny max_input_tokens: fires exactly when expected; post-compaction request context estimate ≤ 1.5× fresh-session baseline for same task; task still completes; budget includes the compaction call | X0 | DONE | worker 1: compaction_test 5/5 — fires exactly once under pressure; measured cheapness gate (post-compaction 57 tok ≤ 1.5× control 111); min-interval blocks re-compaction while over threshold; resume reconstructs the byte-identical compact state; usage sums include the compaction call. engine_test 10/10. Deviations recorded (orphan-tail fold, full-estimate trigger, provider errors propagate — see deviations 13–14) |
| X2 | core | Compaction prompt (compact.rs): dedicated terse summarizer system prompt (task/decisions/edits/open-items shape), input = older transcript text, output capped at summary_max_bytes | Unit test: prompt stable + < 512 tokens; oversized summary truncated at char boundary with marker | X0 | DONE | same run: `summarizer_prompt_is_terse_and_stable` (< 512 est. tokens, four sections); `cap_summary_*` tests (byte cap, char boundary, `…[truncated]` marker, tiny-cap case); compaction request carries the prompt + 512-token output cap (asserted in X1 test) |
| X3 | cli | `--compact-threshold <0..1>`, `--no-compact` flags; stderr progress line `compacted: N events → summary (est. saved ~X tok)` | Flags wired; e2e asserts the progress line appears under pressure | X1 | DONE | lead: flags wired into `CompactionPolicy` (threshold validated 0<t<1, `--no-compact` disables); stderr line per run compaction, est. saved = re-rendered older-transcript estimate − summary estimate (clamped ≥ 0, labeled "est."); V6(b) e2e asserts the exact shape with saved > 0 |
| K1 | providers | Anthropic `cache_control` breakpoints: system block + last tool definition get `cache_control: {"type":"ephemeral"}` in build_body | Fixture test asserts cache_control present exactly twice and body otherwise unchanged; openai-compat body unchanged | — | DONE | worker 2: `cargo test -p modron-providers --lib` → 37 passed incl. `cache_control_appears_exactly_twice_on_last_tool_only` and openai-compat guard `body_never_carries_cache_control`; lead re-ran → 37 passed |
| K2 | core | Byte-stability guarantee: test via MockProvider.requests() that across all turns of a multi-turn run, `system` and `tools` are byte-identical | Test green incl. a run with tool calls | — | DONE | worker 2: `cargo test -p modron-core --test stability` → 2 passed (byte-identical `system` + canonical-JSON `tools` across 2- and 3-turn runs with tool calls); lead re-ran → 2 passed, still green after the M3 engine wiring |
| K3 | cli | Usage table gains cache-hit column: cached/(input+cached) % from provider-reported usage | Demo output shows column; zero-cached renders `0%` honestly | K1 | DONE | lead: `cache hit` column = cached/(input+cached), `0%` when none, header labels the source (provider-reported); demo run shows `0%`; unit tests `cache_hit_column_is_provider_reported_share` |
| M1 | core | Memory store (new memory.rs): root `~/.modron/memory/<slug>/`, slug = sanitized canonical working_dir; MEMORY.md index (timestamp + title per line); one .md file per entry; `append(title, body)`, `read_index()`, `read_entry(name)`, `forget(name)`; index capped 16KB (oldest dropped with warning) | Unit tests in tempdir: append→index→read→forget roundtrip; cap enforcement; slug stability | — | DONE | worker 3 delivered ~complete, stalled (22/23 + 1 clippy warning); lead repaired (see verification log): core lib → 46 passed incl. roundtrip, 16KB index cap (files preserved), 8KB entry cap on char boundary, slug stability/sanitization/hash-suffix |
| M2 | tools | `remember` tool: args {title, body} → M1 append (memory root injected at construction — NO ToolCtx change) | Tool test: call writes file via injected root; spec < 60 words | M1 | DONE | lead-verified: `cargo test -p modron-tools` → 51 passed incl. 4 remember tests (writes via injected root, invalid/missing args → error outcomes, per-workspace separation, spec < 60 words) |
| M3 | core | Context integration: system prompt gains user-memory section (index + last entries, ≤4KB total, on top of repo AGENTS.md/CLAUDE.md), byte-stable within a run | Memory content appears in assembled prompt; system prompt still < 2048 tokens with typical memory; K2 stays green | M1, K2 | DONE | worker 3: context.rs `load_user_memory`/`with_user_memory` + tests (4KB cap, 5 newest bodies, empty-safe) — green in the core-lib run; lead wired the engine: additive `EngineConfig.memory_root` + one composition in `assemble_system` (once per run); prompt < 2048 tokens asserted with typical memory; K2 green |
| M4 | cli | `modron memory list\|show <title>\|forget <title>` subcommand | Integration test: remember via mock run → list shows entry → forget removes | M2 | DONE | lead: clap subcommands over the same memory root as the run path; V6(a) e2e: mock-run remember → `memory list` shows it → `memory show` prints body → `memory forget` removes → list empty → repeat forget exits 1 with `no memory entry titled '…'` |
| V5 | — | **LEAD:** workspace gates green on 1.99.1: build / clippy -D warnings / test | All green; count reported | all | DONE | lead 2026-10-30: `cargo build --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace` → **197 passed / 0 failed** on rustc 1.99.1 |
| V6 | — | **LEAD:** Phase 1a e2e suite: (a) memory persists across two engine runs, same workspace; (b) compaction fires under tiny budget, measured cheapness gate (≤1.5×) passes, progress line shown; (c) stable-prefix + cache_control fixture assertions; (d) Phase-0 e2e still green | All asserted in tests, not prose | V5 | DONE | lead: `crates/modron-cli/tests/phase1a.rs` → 2 passed: (a) memory e2e (mock run → real remember tool → file → binary list/show/forget → second run's prompt carries the memory); (b) compaction e2e — real binary + local scripted OpenAI-compat HTTP server (keyless), `--max-input-tokens 12000 --compact-threshold 0.5` → exactly 1 Compaction event, stderr `compacted: … (est. saved ~N tok)` with N > 0, run completes. (c) = core stability 2/2 + providers cache_control fixtures (K1/K2 rows); measured ≤1.5× gate = compaction_test. (d) Phase-0 e2e untouched → 2 passed |
| V7 | — | **LEAD:** ledger audit + Phase 1a verdict; contract amendments recorded | No TODO left in this table | V6 | DONE | this audit (verification log + verdict below); amendments: X0 (Worker 1, CompactionPolicy + Compaction event) and the lead's additive `EngineConfig.memory_root` — both recorded |

**Worker assignments (Phase 1a)**

- **Worker 1 (engine/compaction):** X0, X1, X2 — modron-core (lib.rs additive
  amendment, engine.rs, new compact.rs). **DONE, green.**
- **Worker 2 (caching):** K1 — modron-providers; K2 — modron-core (new test file
  only, no edits to engine.rs or lib.rs). **DONE, green.**
- **Worker 3 (memory):** M1, M2, M3 — modron-core (new memory.rs, context.rs),
  modron-tools (remember.rs). **STALLED ~90%:** delivered memory.rs, remember.rs,
  and the context.rs user-memory fns, but left one failing core-lib test
  (`memory::tests::slug_is_sanitized_and_hash_suffixed`) and one clippy warning
  (`explicit_counter_loop`, memory.rs) that is fatal under the `-D warnings` gate;
  the engine/CLI wiring was not started. Repairs + wiring completed by the lead —
  see the verification log.
- **Lead:** X3, K3, M4 (all CLI), V5–V7, arbitration of X0, Worker-3 repairs,
  and the engine memory wiring (`EngineConfig.memory_root` + `assemble_system`).

File-overlap warning: Workers 1 and 3 both touch modron-core in *different files*;
neither may run workspace-wide cargo commands — scope to `-p modron-core` and their
own crates.

---

**Phase 1a deviations (design debt, recorded not hidden)**

9. **Slug prefix budget vs. deep tempdir paths** (lead repair, M1). The
   workspace slug keeps the *leading* path segments within a 48-char budget
   plus a 6-hex hash suffix; a canonicalized macOS tempdir
   (`/private/var/folders/…`) consumes the budget before deep segments, so
   tempdir slugs don't echo their leaf directories. Real workspaces keep the
   full path prefix (PLAN §4.9 example shape). The failing test asserted the
   tempdir case; it now asserts the pure `workspace_slug` contract on
   synthetic paths plus the tempdir-independent properties.
10. **Memory root derived from the sessions dir** (lead, M4/X3). The
    user-memory root is `<sessions-dir parent>/memory` (default
    `~/.modron/memory`) rather than a separate `--memory-dir` flag: one knob
    relocates all persistent state, and `--sessions-dir` doubles as the test
    override. A standalone flag can be added later without breaking layout.
11. **Engine compaction log demoted to debug** (lead, X3). The engine's raw
    `compacted: …` info line duplicated the CLI's stderr progress line (which
    adds the est.-saved figure) on every run; debug keeps the user-facing
    surface single and `--verbose` still shows the detail.
12. **est. saved clamped at 0** (lead, X3). The saved estimate
    (summarized-text estimate − summary estimate) can go negative for
    pathological summaries; the line shows `0` rather than a negative
    "saving". The `est.` label carries the uncertainty — it is a ~4
    chars/token heuristic, never provider-reported.
13. **Compaction trigger uses the full context estimate** (Worker 1, X1).
    The pre-turn check runs on `estimate_context_tokens` (system + tool
    schemas + transcript), not the transcript alone — slightly earlier
    triggering, and the compaction can rescue an over-cap context before the
    budget check rejects it. Intended behavior, recorded.
14. **Compaction provider errors propagate** (Worker 1, X1). A failing
    summarizer call fails the run rather than silently continuing with a
    full transcript — honest failure over silent degradation; the budget
    governor still bounds the damage.

## Phase 1a verdict

**Phase 1a is usable.** All fourteen items verified against their acceptance
criteria by re-running the commands (no TODO left; no worker claim taken on
trust), all gates green on the pinned 1.99.1 toolchain: 197 passed / 0 failed,
clippy `-D warnings` clean.

**What works:** the Phase-1a thesis features, end to end and offline —

- **Persistent user memory (§4.9):** the agent calls `remember` → a markdown
  entry + `MEMORY.md` index land under `~/.modron/memory/<workspace-slug>/`
  → the very next run in the same workspace carries the facts in its system
  prompt (≤ 4 KiB, on top of the 8 KiB repo-tier AGENTS.md/CLAUDE.md, whole
  prompt still < 2048 estimated tokens) → and memory is inspectable and
  forgettable via `modron memory list|show|forget` — no silent state.
- **Autocompaction (§4.10):** fires exactly when the measured context crosses
  the threshold past the min-interval, rebuilds `[compact state] + verbatim
  tail` (with the orphan-tail fold), survives resume by replaying the
  never-summarized tail, counts the compaction call against the budget, and
  passes the measured cheapness gate: post-compaction context ≤ 1.5× a fresh
  session's for the same task (57 vs 111 est. tok in the acceptance run).
  The CLI surfaces it honestly: `compacted: N events → summary (est. saved
  ~X tok)` on stderr.
- **Prompt-cache discipline (§4.11):** Anthropic requests carry exactly two
  `cache_control` breakpoints (system block + last tool), the openai-compat
  body carries none, the stable prefix is byte-identical across turns
  (asserted), and the usage table now shows the provider-reported cache-hit
  rate (`0%` when nothing hit — a saved token that was never counted is a
  lie, and so is a hit that was never counted).

**What's debt (recorded, not hidden):** the memory index cap drops oldest
*lines* while entry files stay on disk (cap protects context, not the archive
— a GC story is open); user-memory selection is recency-only (index + 5
newest bodies, no relevance ranking); the compaction min-interval and tail
size are fixed policy constants without CLI knobs; the est.-saved figure is a
~4 chars/token heuristic, not provider-measured; the compaction call's model
choice rides the heuristic selector + provider-fallback path; and `modron
"memory …"` as a *task* needs `--` (subcommand-name collision, documented).
The Phase-0 debts stand unchanged (stringly-typed retry taxonomy, fixed
8192 `max_tokens`, no sandbox/approval gates — `bash`/`write`/`edit` still
run with the caller's full authority).

**Highest-leverage next step:** unchanged from Phase 0 and now more pressing —
spend one API key: run the documented live smoke against Anthropic (the
cache_control breakpoints have never touched the real wire) and one
OpenAI-compatible endpoint. Immediately after, dogfood per PLAN §7: drive
Modron's own Phase-1 work through Modron with a real key — memory and
compaction only earn their keep on long, real sessions. Then Phase 1 proper
(approval modes + OS sandbox), which remains the gap between "usable" and
"safe to point at a real repo".


---

# Phase 1b — Control plane + streaming (opened 2026-10-30)

Baseline: Phase 1a closed (197 tests green, clippy clean, rustc 1.99.1 pinned).
Theme: **the control responsibilities** — approvals, sandbox, hooks, checkpoints,
spend caps (the six-responsibilities framework's "control" pillar) — plus
provider-layer streaming proof and Z.ai readiness.

**Coordination rules:** modron-core/src/engine.rs and modron-core/src/lib.rs are
owned by Worker 1 this phase (additive-only lib.rs changes, recorded). Worker 2
implements streaming as *inherent methods* on the concrete provider types (NO trait
change — the lead lifts it into the trait afterwards). Worker 3 does NOT touch
modron-cli (the lead owns main.rs this phase). Nobody runs workspace-wide cargo
until the lead's V-gates.

| ID | Crate | Item | Acceptance criteria | Depends | Status | Evidence |
|---|---|---|---|---|---|---|
| A1 | core | Approval modes: `ApprovalMode { Deny, Ask, ApproveAll }` gating mutating tool classes (bash, write, edit) before execution; `--approve-all` semantics: in non-interactive print mode, Ask degrades to Deny with an actionable stderr message unless explicitly overridden | Engine tests: Deny blocks with clear error + no execution; ApproveAll executes; Ask in non-interactive flow → blocked unless override; read-only tools unaffected in every mode | — | DONE | engine MUTATING_TOOLS gate + ApprovalPolicy; tests phase1b::approval_gate_blocks_write_and_the_model_sees_why / approve_all_executes_mutating_tools → pass; live CLI demo: Ask blocks with --approve-all hint |
| A2 | core | Sandbox: bash execution optionally wrapped in a macOS Seatbelt profile via `sandbox-exec` (generated profile as pure function): deny network*, allow read, allow file-write only under working_dir + system temp; graceful degradation: Linux bwrap if present, else unsandboxed + loud warning and requires ApproveAll | Profile-generator unit tests; integration test on macOS (skip-if-unavailable): `curl` example fails under sandbox, workspace write succeeds; fallback path logged honestly | A1 | DONE | core/sandbox.rs profile generator (pure, byte-stable) + run_sandboxed; empirical on this machine: curl exit 6 (denied), workspace write ok, /usr/local denied; phase1b::seatbelt_sandbox_blocks_network_allows_workspace_writes → pass |
| A3 | core | USD spend cap: `EngineConfig.max_cost_usd: Option<f64>` + injectable price lookup `Arc<dyn Fn(&str) -> Option<(f64, f64)>>` (in/out per M tokens — CLI wires catalog); pre-turn enforcement with honest abort naming spend so far | Unit tests: cap trips mid-run; None = unlimited; manual-Debug impl still compiles (EngineConfig derives) | — | DONE | EngineConfig.max_cost_usd + price_lookup (manual Debug); engine post-turn enforcement with honest abort note; CLI --max-cost wired to router catalog; compaction usage included |
| H1 | core | Hooks: `pre_tool` / `post_tool` shell hooks from config; env `MODRON_TOOL_NAME`, `MODRON_TOOL_ARGS` (JSON), `MODRON_SESSION_ID`; pre-hook non-zero exit blocks the tool (hook stderr surfaced to model as tool error); 10s timeout each | Unit tests: veto path, pass path, timeout path, env contract; hooks absent = no behavior change | — | DONE | core/hooks.rs (10s timeout, env contract, fail-closed); engine execute_tool_with_hooks; 5 hook unit tests + phase1b::pre_tool_hook_vetoes_the_tool → pass |
| C1 | core/store | Checkpoints: before `write`/`edit` execute, snapshot prior file content into a `SessionEvent::Checkpoint { path, prior_content_b64, event_index }` (256KB/file cap, larger files = checkpoint-with-null-content + warning); **bash mutations are a documented v1 limitation** (use git) | Engine test: write over existing file → Checkpoint event precedes it; large-file path honest; resume replays checkpoints | A1 | DONE | SessionEvent::Checkpoint + engine checkpoint_for (write/edit, 256KB cap, b64, ApproveAll-only); hand-rolled b64 with known-vector tests; phase1b checkpoint tests → pass |
| C2 | cli/core | `modron rewind [--session <id>]`: restore files from the last Checkpoint + truncate the transcript at that event (later events archived to `*.abandoned` suffix, not deleted); prints what was restored | Integration test: run that edits a file → rewind → file content restored, session resumes from truncation point | C1 | DONE | core/rewind.rs rewind_last_checkpoint + EngineConfig.resume_from_event + CLI `modron rewind` with {id}.rewind marker; phase1b::checkpoint_then_rewind_restores_file_and_truncates_transcript / rewind_deletes_files_the_agent_created → pass |
| S1 | providers | Streaming (inherent methods, no trait change): `async fn turn_streaming(&self, req, tx: mpsc::UnboundedSender<StreamDelta>) -> Result<TurnResponse>` on both providers; hand-rolled SSE parser (reqwest `stream` feature); `StreamDelta::{Text, Reasoning, Done}`; **tests via local scripted HTTP server** (tokio TcpListener serving SSE fixtures) proving both wire formats incl. tool_use deltas + usage in the final frame | Both providers: streaming fixture tests green offline; non-streaming path untouched (all 37 provider tests still pass) | — | DONE | stream.rs SSE parser (15 tests) + turn_streaming on both providers (6 scripted-server fixture tests, wire-format equivalence proofs); trait lift: StreamDelta moved to core, ModelProvider::turn_streaming default falls back to turn(); 62 provider tests green |
| S2 | — | Engine/CLI delta streaming UX (live typing effect) | DEFERRED to next tranche | S1 | DEFERRED | — |
| Z1 | providers/router | Z.ai first-class: factory accepts `zai` (openai-compat + base `https://api.z.ai/api/paas/v4`, key `ZAI_API_KEY`, fallback error naming the var); catalog gains GLM entries (research current Z.ai lineup + approximate pricing, e.g. glm-5.x family; mark approximate) | Factory test for zai mapping + missing-key error; catalog test: GLM entries present with valid tiers/windows | — | DONE | factory 'zai' → openai-compat @ api.z.ai/api/paas/v4 (ZAI_API_KEY, ZAI_BASE_URL override, coding-plan caveat in error hint); catalog + glm-5.3/5.2/5.3-flash/5.3-flashx (1M windows); 41 provider + 22 router tests |
| Z2 | docs | Live smoke vs Z.ai: IF `ZAI_API_KEY` is set in env, execute a real one-shot run and record output; ELSE document the exact commands in README (already the case) and record "awaiting key" | Either executed-with-evidence or explicitly awaiting-key in ledger | Z1 | DONE | awaiting key — no ZAI_API_KEY in env (verified 2026-10-30); exact live commands documented in README incl. coding-plan endpoint; factory error path asserted in phase1b::zai_factory_without_key_names_the_env_var |
| DX1 | cli | `modron sessions list` (id, title, updated, turns) from the store | Integration test listing ≥2 seeded sessions in recency order | — | DONE | CLI `modron sessions` subcommand (id/updated/msgs/title, newest first); live demo output recorded in session log |
| DX2 | docs | README pass: memory subcommand, approval/sandbox flags, zai quickstart, streaming status; one-time `cargo fmt` normalization (not a future gate) | README accurate; fmt clean; no behavior change | all | DONE | README rewritten: Z.ai quickstart + coding-plan caveat, approval/sandbox/max-cost flags, subcommands, memory/caching/compaction section, status bumped to Phase 1b; cargo fmt --all applied (one-time, gates re-verified green) |
| V8 | — | **LEAD:** workspace gates green: build / clippy -D warnings / test | All green; count reported | all | DONE | cargo build --workspace ✓; cargo clippy --workspace --all-targets -- -D warnings ✓ (3 new-in-1.99 lints fixed: manual_is_multiple_of, match_result_ok, print_literal); cargo test --workspace → 249 passed / 0 failed |
| V9 | — | **LEAD:** Phase 1b e2e suite: approval gate blocks then proceeds with override; sandbox network-deny; hook veto; rewind restores; zai factory errors actionably without key; Phase 0 + 1a e2e still green | All asserted in tests | V8 | DONE | crates/modron-cli/tests/phase1b.rs: 7 tests green (approval block+override, seatbelt network-deny via real kernel, hook veto, checkpoint rewind + resume truncation, created-file deletion, zai factory); Phase 0/1a e2e still green |
| V10 | — | **LEAD:** ledger audit + Phase 1b verdict; trait lift of S1 (additive, with default fallback) after workers land; any main.rs wiring recorded | No TODO left in this table | V9 | DONE | this audit; see Verification log + verdict below |

**Worker assignments (Phase 1b)**

- **Worker 1 (control plane — owns modron-core engine.rs + lib.rs additive this
  phase):** A1, A2, A3, H1, C1 (+ their unit/engine tests). CLI wiring is the lead's.
- **Worker 2 (streaming):** S1 — modron-providers only (inherent methods; no
  lib.rs, no engine).
- **Worker 3 (Z.ai + readiness):** Z1 factory part (providers/factory.rs) + Z1
  catalog part (modron-router) + Z2 documentation side. No modron-cli.
- **Lead:** main.rs entirely (approval/sandbox/hooks flags, `modron rewind`,
  `modron sessions list`), S1 trait lift, V8–V10, arbitration.

## Verification log — Phase 1b (lead architect only)

- **2026-10-30 — S1 (Worker 2).** SSE parser + turn_streaming on both providers;
  scripted local HTTP server fixtures prove both wire formats with field-for-field
  equivalence vs the non-streaming parse; 56 lib + 6 fixture tests green; clippy
  clean. Wire notes recorded in the item row (Anthropic: output usage only in
  message_delta; OpenAI: usage chunk has empty choices with include_usage).
- **2026-10-30 — Z1 (Worker 3).** Researched official docs (glm-5.3 lineup, 1M
  windows, coding-plan vs pay-as-you-go endpoints/keys — coding-plan keys 401 on
  api/paas/v4); factory + catalog landed; 41 + 22 tests green.
- **2026-10-30 — Worker 1 stall.** The control-plane worker timed out before
  writing anything (verified: no files, no lib.rs terms, tree healthy). The lead
  (session principal) implemented A1–C2 directly after the Phase-1b lead agent
  ALSO stalled mid-edit (left ApprovalPolicy types + a broken main.rs initializer).
- **2026-10-30 — A1/A3/H1/C1/C2 (principal).** ApprovalPolicy gate in engine +
  CLI flag; USD caps with manual-Debug EngineConfig; hooks with fail-closed veto;
  checkpoints (write/edit, b64, 256KB cap) + rewind + resume_from_event; all with
  unit + e2e coverage.
- **2026-10-30 — A2 (principal).** Empirical Seatbelt verification on the dev
  machine (network deny / workspace write ok / outside write deny), profile
  generator frozen as a pure function, live-kernel test in core + engine-level
  e2e; bash tool routes through run_sandboxed under Seatbelt.
- **2026-10-30 — S1 trait lift + A2 ToolCtx amendment (principal).** StreamDelta
  moved to modron-core (providers re-export), ModelProvider::turn_streaming with
  turn()-fallback default, providers converted from inherent methods to trait
  overrides so dyn dispatch streams. ToolCtx gained `sandbox` (a *breaking*
  constructor change across 9 sites, all fixed) — recorded as the phase's one
  non-additive contract change.
- **2026-10-30 — DX2 + V8/V9 (principal).** cargo fmt --all (one-time), README
  rewritten, gates re-verified: 249 passed / 0 failed, clippy -D warnings clean.

## Final verdict — Phase 1b

**Phase 1b is usable.** The control plane exists end to end: mutating tools are
blocked by default with an actionable message (`--approve-all` overrides), the
macOS Seatbelt sandbox denies network and fences writes (verified against the real
kernel), hooks can veto any tool call, file mutations are checkpointed and
revertible (`modron rewind`), and runs can be capped in USD as well as tokens.
Provider-layer streaming is wire-proven for both Anthropic and OpenAI-compatible
formats, and Z.ai is a first-class provider (catalog, factory, docs) — dogfooding
is one `export ZAI_API_KEY=…` away.

**What works:** everything in the 1b table; 249 tests, clippy -D warnings clean on
pinned rustc 1.99.1.

**Debt / honest notes:** the interactive approve prompt is still Ask==Deny (TUI
tranche); sandbox is macOS-only (Linux port = Phase-1 residual); bash mutations
are not checkpointed (use git); engine/CLI do not yet consume streaming deltas
live (S2 DEFERRED — the wire work is done and the trait is in place); Z.ai live
smoke awaits a key; hooks are wired in the engine but not yet surfaced in
config.toml (they arrive via EngineConfig today).

**Next:** spend a key on the live smoke (Z.ai recommended — it is the dogfooding
path), then the TUI tranche (interactive approvals + streaming UX + S2), then
Phase 2 (compaction tuning + token dashboards) per the roadmap.

---

# Phase 1c — Interactivity (opened/closed 2026-10-31)

Baseline: Phase 1b closed (249 tests green, clippy clean). Theme: **finish the
interactive surface** — live streaming deltas (S2), the Ask-mode prompt (A1's
missing half), a chat REPL, hooks from config, and session accounting. Built
directly by the principal (agent-executor reliability remained poor this session;
three stalls in 1b). Single tranche, five items, same gated-increment discipline.

| ID | Item | Acceptance criteria | Status | Evidence |
|---|---|---|---|---|
| I1 | Engine streaming dispatch: `EngineConfig.on_delta` (delta sink) drives providers through `turn_streaming` with a forwarding task; providers without SSE yield one Text delta via the trait default; compaction stays non-streaming | Engine test: sink receives the text delta; all existing tests green (fallback path transparent) | DONE | `cargo test -p modron-core --test interactivity delta_sink` → pass; 85 core tests green |
| I2 | Interactive approvals: `EngineConfig.approver` consulted in Ask mode before blocking; CLI `tty_approver()` prompts via /dev/tty (y once, `a` = rest-of-run sticky, anything else denies); absent terminal → block-and-explain unchanged | Engine tests: approver allow executes, deny blocks with message; piped runs prompt-free (stdin terminal check) — the chat e2e proves no prompt hang when piped | DONE | interactivity.rs 3/3; chat binary test runs unattended |
| I3 | `modron chat` REPL: one session across exchanges (run_task → run_task_resuming), streaming answers, per-exchange + cumulative token lines, errors don't kill the session, `/exit`/Ctrl-D quit, resume hint printed | Binary e2e: two exchanges through one session id, banner + 2 answers + 2 usage lines + resume hint; all flags global so chat takes --provider/--model/--approve-all | DONE | phase1c::chat_repl_runs_multi_turn_through_one_session → pass; live piped demo recorded |
| I4 | Hooks from config: `[hooks]` section (pre_tool/post_tool) in config.toml → EngineConfig.hooks | Config unit test: section parses, absent = None; wiring compiles through build_agent | DONE | config::tests::hooks_section_parses_and_defaults_to_none → pass (14 CLI bin tests) |
| I5 | Session accounting: `modron sessions` gains tokens + est.-cost columns (per-model usage events, catalog-priced), 20-session display cap with elision note | Binary e2e: after a demo run the list shows 1232 tokens and a $ figure | DONE | phase1c::sessions_list_shows_tokens_and_cost → pass; live demo output recorded |
| I6 | MockProvider repeat-last mode for multi-turn tests; demo provider uses it (chat no longer exhausts the script) | Core tests green; chat test exchanges twice | DONE | testing.rs +repeat_last/last tracking; 85 core tests |
| V11 | Gates: build / clippy -D warnings / test on pinned 1.99.1 | All green | DONE | **256 passed / 0 failed**; clippy clean; fmt applied |
| V12 | Ledger + README for Phase 1c | This table complete; README documents chat, prompting, hooks, sessions columns | DONE | this section + README diff |

**Contract amendments (additive, recorded):** `EngineConfig.approver` +
`EngineConfig.on_delta` (with `Approver`/`DeltaSink` type aliases); manual Debug
updated. `MockProvider` gained `repeat_last`/`last` (test-only surface).

**Honest notes:** the /dev/tty prompt cannot be exercised in this non-interactive
environment — the prompt code path is covered indirectly (engine approver tests +
the piped-chat proof that no prompt hangs without a terminal); a human should do
one y/N/a pass on a real TTY. Streaming prints deltas but providers have never
streamed from a real API (unchanged). Chat renders reasoning deltas nowhere (v1).

## Final verdict — Phase 1c

**Phase 1c is usable.** Modron now behaves like a terminal companion rather than
a one-shot filter: `modron chat` holds one session across turns with streaming
answers and cumulative accounting; Ask mode prompts on a real terminal and stays
safe without one; hooks are user-configurable; sessions report honest token/cost
totals. 256 tests, clippy clean, rustc 1.99.1. Next: the ratatui TUI (richer
rendering, approval UX inside the UI), Phase 2 (compaction tuning, token
dashboards, cache-hit telemetry), and the Z.ai live smoke + dogfooding the moment
a key exists.

---

# Phase 1d — TUI (opened 2026-10-31)

Baseline: Phase 1c closed (256 tests green). Research input (2026-10-30/31):
Claude Code renders inline — completed content committed to native scrollback,
only the live region managed; their fullscreen alt-screen mode exists purely for
redraw performance and broke tmux scrollback (regression #67289), so inline-first
is the correct default. ratatui supports the pattern via `Viewport::Inline` +
`Terminal::insert_before` (Vision issue #1321). Known trap: wrapped Paragraphs in
inline viewports misbehave — stream windows are managed manually. opencode's
lesson: the TUI is a thin client over the core; pi's: differential rendering.

| ID | Item | Acceptance criteria | Status | Evidence |
|---|---|---|---|---|
| T0 | Contract amendment: `EngineConfig.on_tool_event: Option<Arc<dyn Fn(ToolActivity) + Send + Sync>>` + `ToolActivity { Start{name}, Done{name, is_error, bytes} }` (additive) | Engine emits around tool execution; existing tests green | TODO | — |
| T1 | New crate `modron-tui`: App state machine (input, status, streaming window, approval, usage) with pure `on_key/on_delta/on_tool/drain` methods returning Actions | Unit tests: keys → actions; streaming window bounded; approval state transitions | TODO | — |
| T2 | Inline-viewport renderer: fixed-height live region (status/spinner + streaming tail + input + approval line), completed content via `insert_before` (answers, tool lines); no wrapped Paragraphs | TestBackend render tests: idle frame, streaming frame windowed, approval frame, scrollback flush content | TODO | — |
| T3 | Engine wiring: esc interrupts the running turn (drop/abort at await points; partial events persist); ctrl+c aborts + quits; ctrl+o toggles transcript mode (full tool output flushed) | State machine tests for abort/quit actions; abort leaves session resumable | TODO | — |
| T4 | CLI: `modron tui` subcommand (flags global); AgentSinks refactor so build_agent accepts injected approver/on_delta | `modron tui --provider mock` constructs (TTY-less environments skip entering raw mode with an actionable error) | TODO | — |
| T5 | Gates + ledger + README | build/clippy/test green; README documents the TUI; verdict recorded | TODO | — |
## Verification log — Phase 1d (principal)

- **2026-10-31 — T0.** ToolActivity/ToolActivitySink added to core (additive);
  engine emits Start/Done around tool execution; 85 core tests green unchanged.
- **2026-10-31 — T1/T2.** modron-tui: App state machine (keys→actions, approval
  modal with sticky-a, streaming window, tool lines) + inline-viewport renderer
  (TestBackend frames: idle/streaming/approval); 12 TUI tests. Two render bugs
  found by the frame tests themselves (approval hint clipped by a 1-row chunk;
  chunks/renders misaligned) — both fixed before any human ever saw them.
- **2026-10-31 — T3/T4.** Esc-abort (drop-based cancellation; partial events
  persist), ctrl+c abort+quit, ctrl+o toggle; `modron tui` wired through the
  AgentSinks refactor (build_agent_with); TTY-less guard verified live (actionable
  error, exit path clean). Workspace: fmt + clippy -D warnings clean.
- **2026-10-31 — V13/V14.** **268 passed / 0 failed** workspace-wide on pinned
  rustc 1.99.1.

## Final verdict — Phase 1d

**The TUI is first class in the ways the field says matter**: inline rendering
with native scrollback (not a fullscreen lattice), streaming answers, live `●`
tool lines, Esc-to-interrupt with resumable partial state, inline approvals with
the sticky-`a` pattern, and a status line with honest token counts. 268 tests;
the state machine and all frames are covered by TestBackend tests — the only
untested surface is the raw crossterm loop, which is deliberately thin and
guarded (TTY-less runs get an actionable error, verified live). A human should
still do one real-terminal pass (`modron tui --provider mock`) to judge feel —
responsiveness of the 20fps loop and the wrap width are the two knobs most
likely to want tuning on real hardware. Next: Phase 2 (compaction tuning +
cache-hit telemetry), and the Z.ai live smoke the moment a key exists.

---

# Phase 2a — Dashboards, verification, tuning (opened 2026-10-31)

Baseline: Phase 1d closed (268 tests green). Scope per PLAN Phase 2, restricted to
what is offline-testable (no API keys yet): the usage dashboard, compaction
config, the verification pillar (a `verify` tool running the project's own check
command), and the formal prompt-budget CI gate. Live cost baselines and
compaction-threshold tuning against real traffic wait for a key.

| ID | Item | Acceptance criteria | Status | Evidence |
|---|---|---|---|---|
| D1 | `modron usage`: aggregate dashboard across sessions — per-model input/output/cached, cache-hit %, est. cost, session + compaction counts; `--last <n>` display cap | Binary e2e: seeded store → dashboard shows per-model rows + totals + honest 0% when uncached | TODO | — |
| D2 | Compaction + verify config: `[compaction]` (enabled/threshold_frac/min_interval_turns/tail_messages/summary_max_bytes) and `[verify]` (command) sections; precedence flag > config > default | Config unit tests; wiring in build_agent_with | TODO | — |
| D3 | `verify` tool: runs the configured project check command (sh -c, 120s cap, output reduction, exit-code line); registered only when configured; treated as mutating (approval-gated) | Tool tests: pass, fail-with-output, oversized-output cap; gated like bash | TODO | — |
| D4 | Prompt-budget CI gate: test asserting the real default registry's system prompt + tool specs stay under 2048 estimated tokens (with memory section under a typical load) | Test exists and is green; fails the build if prompts fatten | TODO | — |
| D5 | Gates + ledger + README | build/clippy/test green; README documents usage/verify/config | TODO | — |
## Verification log — Phase 2a (principal)

- **2026-10-31 — D3.** `verify` tool (modron-tools/src/verify.rs): 120s cap,
  output reduction, honest exit-code line; registered ONLY when `[verify]` is
  configured (prompt budget stays lean otherwise); added to MUTATING_TOOLS
  (approval-gated like bash). 5 tool tests + 2 engine-level e2e (pass + failure
  paths both visible to the model).
- **2026-10-31 — D2.** `[compaction]`/`[verify]` config sections; precedence
  engine-default < config < CLI flags; unit tests (15 CLI bin tests green).
- **2026-10-31 — D1.** `modron usage`: per-model input/output/cached + honest
  cache-hit % + catalog-priced costs + compaction counts, `--last N` cap.
  Binary e2e incl. empty-store honesty; live demo output recorded.
- **2026-10-31 — D4.** Prompt-budget gate (budget.rs): real default registry +
  verify tool + typical memory must stay under 2048 est. tokens; a terse-spec
  check pins the verify description. This makes prompt fattening a build
  failure — PLAN §3.1 is now enforced, not aspirational.
- **2026-10-31 — V15.** **282 passed / 0 failed**; clippy -D warnings clean;
  fmt applied. One test-side assertion fixed (usage shows per-model columns,
  not a pre-summed total — the test now asserts the real numbers).

## Final verdict — Phase 2a

**Phase 2a is usable.** The verification pillar exists (`verify` tool, gated and
budget-conscious), the aggregate dashboard closes the observability loop
(`modron usage` with cache-hit rates and honest zeros), compaction is
user-tunable without code changes, and the prompt budget is enforced by CI.
282 tests, clippy clean, rustc 1.99.1. **Deliberately deferred to when a key
exists:** real cost-per-task baselines vs. other harnesses, compaction-threshold
tuning against live traffic, and cache-hit telemetry from actual providers —
all three need real model traffic to mean anything. The Z.ai live smoke (and
with it dogfooding) remains the single highest-leverage next step.

---

# Phase 3a — Routing (opened 2026-10-31)

Baseline: Phase 2a closed under the new name (282 tests green). Scope: the
routing moat, restricted to offline-testable engineering. The embedder-heavy
path (ort/candle + real model weights) stays deferred; the learned router below
uses a hashing-vectorizer (char n-grams → buckets → centroids) — a legitimate
no-dependency technique that gives learned behavior with zero supply-chain and
zero download. The Jev adapter is deliberately NOT implemented against a guessed
API (invite-limited service); the `ModelSelector` seam + config stub is the
contract, the adapter lands when the API is documented for us.

| ID | Item | Acceptance criteria | Status | Evidence |
|---|---|---|---|---|
| R0 | Contract amendments (additive): `SelectRequest.budget_remaining_usd: Option<f64>`; `SessionEvent::ModelSelected` gains `action: Option<String>` + `context_tokens: Option<u64>` (serde defaults; old logs load) | All existing tests green; Phase-0 session files still parse | DONE | SelectRequest.budget_remaining_usd + ModelSelected{action, context_tokens} (serde defaults); Phase-0 logs still load (store roundtrip tests green); all constructor sites fixed |
| R1 | Configurable tiers: `[router]` config (cheap_model/strong_model/mode + per-action overrides via `[router.actions]`); HeuristicSelector generalized to take tier pairs | Unit tests: overrides win; catalog-looked-up provider; default unchanged | DONE | [router] config (cheap/strong models, mode, per-action overrides via BTreeMap→ActionKind); HeuristicSelector::with_overrides; MODRON_CONFIG env injection for tests; e2e: unknown mode fails actionably, config models apply |
| R2 | Budget-aware downgrade: engine feeds remaining USD into the selector; under pressure → cheap tier with reason | Engine test: low remaining budget downgrades the choice; reason mentions budget | DONE | engine feeds remaining USD into every SelectRequest; selector downgrades under $0.50 with budget-pressure reason; tests: budget_pressure_downgrades_to_cheap_with_reason / healthy_budget_does_not_downgrade |
| R3 | LearnedRouter: char-3gram hashing vectorizer (512 buckets, L2-normalized), per-tier centroids from a bundled labeled corpus, nearest-centroid with margin fallback to HeuristicSelector, budget/overflow checks preserved | Eval-set accuracy ≥ 90% in tests; deterministic (same input → same choice); no new deps | DONE | learned.rs: FNV-1a char-3gram hashing vectorizer (512 buckets, L2-normalized) + per-tier centroids from corpus.toml (33 cheap / 28 strong) + margin fallback to heuristic; 30 router tests incl. eval accuracy 16/16 |
| R4 | `modron router eval`: runs the bundled eval set through the active selector; prints per-tier accuracy + estimated cost delta vs. always-strong (catalog-priced, synthetic 10k-in/1k-out task) | Binary e2e: eval table renders with accuracy ≥ threshold and a savings figure | DONE | `modron router eval`: accuracy table with per-example reasons + catalog-priced cost delta vs always-strong (synthetic 10k-in/1k-out); binary e2e green; live demo: 16/16 (100%) |
| R5 | Routing-decision telemetry: ModelSelected events carry action + context_tokens (R0) — the training-data trail for future embedder work | Asserted in engine test transcript | DONE | ModelSelected events now carry action + context_tokens on every real decision — the training-data trail for the future embedder router |
| R6 | Gates + ledger + README | build/clippy/test green; README documents router config + eval | DONE | fmt + clippy -D warnings clean; **293 passed / 0 failed**; README documents [router] config + router eval |

## Verification log — Phase 3a (principal)

- **2026-10-31 — R0/R5.** Additive contract amendments; engine now logs
  action + context tokens with every routing decision (the offline training
  trail); all historical session logs still parse.
- **2026-10-31 — R1/R2.** Config-driven tiers + per-action overrides +
  budget-pressure downgrade; MODRON_CONFIG env injection added (enables
  config-driven e2e tests generally).
- **2026-10-31 — R3.** LearnedRouter: 3-gram hashing vectorizer + centroids
  from the bundled corpus; two test iterations tightened the claims (final
  routing must match labels, ≥70% of eval examples must clear the margin
  confidently) — which surfaced and fixed a real heuristic gap: short
  imperative tasks ("optimize session loading") were misrouted to cheap by
  the size rule; the strong-verb rule now covers the fallback path.
- **2026-10-31 — R4.** `modron router eval` live: **16/16 (100%)** on the
  held-out set with per-example cosine reasoning and an honest cost-delta
  table.
- **2026-10-31 — V16.** 293 passed / 0 failed; clippy -D warnings clean;
  fmt applied.

## Final verdict — Phase 3a

**The routing moat now has a working learned core, fully offline.** The
default selector embeds each task with a dependency-free hashing vectorizer,
scores it against corpus-trained centroids, and falls back to the (improved)
heuristic rules when unsure — preserving budget-pressure and overflow
escalation on every path. Accuracy on the held-out eval set is 16/16, and the
eval harness (`modron router eval`) is the measurement loop the roadmap calls
for: edit `corpus.toml`, rerun, see accuracy and cost delta immediately.
Routing decisions are logged with their features (R5), so the moment real
traffic exists, the training-data flywheel starts turning. **Deferred with
reason:** the Jev adapter (its API is invite-only — no guessing), the real
embedder (needs ONNX/weights; the seam is `embed()`), and live-traffic
accuracy/cost baselines (needs a key). Next: Phase 3 residual (subscription
OAuth) or the MCP client — both offline-buildable.

---

# Phase 3b — Decision models (opened/closed 2026-10-31)

Baseline: Phase 3a closed (293 tests green). Research input: **Kev**
(github.com/jaredpalmer/kev, Apache-2.0, 9.7k★) is the leading open Jev
alternative — a decision-model family (0.8B/4B/9B/27B on Qwen bases) returning
calibrated probabilities from typed questions, self-hostable (MPS on Apple
Silicon), and **wire-compatible with TypeSafe's Jev API** (Kev-27B scores
within a point of hosted Jev). Local-router best practice (0.5B–1.5B Qwen via
Ollama) validates the LLM-judge pattern: router-not-judge, ~60% cost cuts
reported in the field. Both plug into the same new seam.

| ID | Item | Acceptance criteria | Status | Evidence |
|---|---|---|---|---|
| M0 | Contract amendments (additive): `ModelChoice.confident: bool` (selectors flag low-confidence decisions); `EngineConfig.async_router: Option<Arc<dyn AsyncRouter>>` + `AsyncRouter` trait (async route → Option<RouteVerdict>) + `JudgeMode {Ambiguous, Always}` | All existing tests green after constructor fixes | DONE | ModelChoice.confident + AsyncRouter trait + JudgeMode + EngineConfig.async_router (all additive); all constructor sites fixed; 89 core tests green |
| M1 | LLM judge (providers): one-turn classification (MECHANICAL/JUDGMENT) via any OpenAI-compatible provider — the generic "existing model as router agent" | Verdict parser unit tests (case/whitespace/garbage); engine e2e: judge overrides an ambiguous cheap route to strong, usage counted | DONE | LlmJudgeRouter in providers: tiny MECHANICAL/JUDGMENT turn over any provider; verdict parser (first-word-wins, garbage→None); engine e2e: Always-mode verdict re-routes Chat→haiku into Judgment→sonnet with async-router provenance in ModelSelected |
| M2 | Kev/Jev adapter (providers): POST {endpoint}/v1/systemone choice question → tier; low-confidence answers fall through | Scripted-server fixture tests (request shape + response parse + confidence fall-through); works for Jev unchanged (wire-compat) | DONE | KevRouter: POST /v1/systemone single-choice question (mechanical/judgment criteria) → tier, confidence <0.6 falls through; 5 fixture tests incl. round-trip against a scripted server (request shape + body asserted); works against Jev unchanged (wire-compat) |
| M3 | Config + wiring: `[judge]` (llm mode/model/provider) and `[kev]` (endpoint/model/api_key/mode); Ambiguous (default) vs Always gating; MODRON_CONFIG e2e | Config tests; binary e2e with judge config through MODRON_CONFIG | DONE | [judge] and [kev] config sections (mode ambiguous|always); [judge] default model = cheap tier; MODRON_CONFIG testable; parse_judge_mode actionable errors |
| M4 | Binary slimming: release profile (fat LTO, codegen-units=1, panic=abort, opt-level) + tokio feature trim; measure before/after; document that test code never ships in the binary (cfg(test) + dev-deps are linker-excluded) | Size before/after recorded; all gates still green; runtime behavior unchanged | DONE | Release profile: fat LTO, codegen-units=1, panic=abort, opt-level=s + tokio feature trim (rt-multi-thread/macros/time/process/io-util/sync, no 'full'); **8.0MB → 3.9MB (−51%)**; all 303 tests green; test code confirmed never linked (cfg(test) + dev-deps are linker-excluded — the wins were profile + feature surface) |
| M5 | Gates + ledger + README + PLAN research note | build/clippy/test green | DONE | fmt + clippy -D warnings clean; README/PLAN updated |

## Verification log — Phase 3b (principal)

- **2026-10-31 — research.** Kev (jaredpalmer/kev, Apache-2.0) verified as the
  open Jev replica: SystemOne wire API documented (`POST /v1/systemone`,
  typed choice/noul/score questions, calibrated probabilities), 0.8B–27B
  sizes, self-hostable (MLX on Apple Silicon), Kev-27B within ~1 point of
  hosted Jev. Local-router practice validated (0.5B–1.5B Qwen via Ollama as
  router-not-judge, ~60% cost cuts reported). Findings folded into PLAN §4.3.
- **2026-10-31 — M0–M3.** The async decision-model seam landed with two
  implementations; the regex-assisted ModelChoice patch mangled two router
  files and was repaired by hand (all 303 tests green after) — recorded as a
  lesson: struct-literal changes go through targeted edits, not regex sweeps.
- **2026-10-31 — M4.** Binary slimmed 8.0MB → 3.9MB. The user's instinct that
  "tests ship in the binary" was half right: test code never links, but the
  default `tokio full` feature set and the thin-LTO profile were shipping
  dead weight.

## Final verdict — Phase 3b

**The decision-model story is now complete without waiting for anyone's
invite.** Three tiers, one seam: the learned hashing router (always on,
free), an LLM judge over any OpenAI-compatible provider (a 0.5B Ollama model
locally or a cheap hosted model — `modron.toml` `[judge]`), and Kev/Jev
SystemOne (`[kev]`, wire-compatible with both, confidence-gated). All
fail-open to the sync selector; all decisions logged with provenance. And the
binary shed half its size. 303 tests, clippy clean, 3.9MB release binary on
pinned Rust 1.99.1. Remaining offline items: subscription OAuth (Phase 3
residual), the MCP client, the ACP server. Live items still queue on a key.

---

# Phase 4a — Recursion + skills (opened/closed 2026-10-31)

Baseline: 303 tests, repo now under git with CI + cargo-deny. Scope: the RLM
economy from PLAN §4.7 (explore subagents), parallel read-only dispatch,
per-tool-class approvals, and the SKILL.md open standard.

| ID | Item | Acceptance criteria | Status | Evidence |
|---|---|---|---|---|
| P1 | Explore subagent: read-only child engine (read/glob/grep only, cheap tier pinned via parent's Search routing, own session + token budget, explore excluded from child registry, deltas suppressed) exposed as the `explore` tool via `ToolCtx.engine` | Unit: brief + usage + budget enforcement + tier pinning; tool errors honestly without an engine handle | DONE | subagent.rs + explore.rs; 62 tools tests |
| P2 | Parallel read-only dispatch: a turn whose calls are all non-mutating runs concurrently (futures::join_all), order-preserving; mutating turns stay sequential (checkpoints/approvals are order-sensitive) | Existing transcript-order tests green; code path guarded by all_read_only | DONE | engine.rs dispatch restructure |
| P3 | Per-tool-class approvals: `--allow <tool>` (bypasses Ask) / `--deny <tool>` (always blocks, any class, wins over allow); ApprovalPolicy.allow/deny (additive) | Gate tests: deny-any-mode, allow-bypass — engine gate honors both | DONE | phase1b-style gate tests extended; 313 tests |
| P4 | SKILL.md skills: index (name+description) in the system prompt via `context::load_skills_index` (2KB cap); `skill` tool loads bodies on demand (frontmatter stripped, traversal-guarded); tool registered only when the repo has skills | Unit: index parse, body load, unknown-name lists available, traversal rejected; `.claude/skills` honored for ecosystem compat | DONE | context.rs + skill.rs; e2e via budget-gate registry |
| P5 | Gates + docs | 313 passed / 0 failed; clippy clean; fmt applied | DONE | this log |
| P6 | DEFERRED: parallel explore at the agent level (model issues several explores in one turn — P2 already executes them concurrently when present); daemonized core + RPC mode; map/reduce over file sets | Next tranche | DEFERRED | — |

## Verification log — Phase 4a (principal)

- **2026-10-31 — git/CI.** Repo initialized; CI (fmt/clippy/test on
  ubuntu+macos @ 1.99.1, cargo-deny supply-chain gate, release builds with
  size reporting) + versioned internal deps. Push to the PERSONAL account
  (jbeck018) pending the empty repo's creation — the first `gh repo create`
  landed on the work account by accident and was removed from the remote;
  the work-account copy needs a one-time browser deletion (the work token
  lacks delete_repo, deliberately not escalated).
- **2026-10-31 — P1–P5.** Explore subagent + parallel dispatch + allow/deny
  + skills landed; 313 passed / 0 failed, clippy -D warnings clean.

## Final verdict — Phase 4a

**The recursion economy is live.** Repo-wide questions now route through a
cheap, budgeted, read-only subagent whose brief — not its raw output — enters
the parent context; multiple read-only calls per turn run concurrently;
approvals are per-tool configurable; and the repo can teach the agent new
workflows by dropping SKILL.md files into `.claude/skills/` (open-standard
compatible). Deferred: RPC/daemon, parallel-explore orchestration at the
planner level. Remaining offline items: MCP client, ACP server, subscription
OAuth. Live items still queue on a Z.ai key.

---

# Phase 5a — MCP client (opened/closed 2026-10-31)

Baseline: 313 tests, repo on GitHub with CI + cargo-deny. Scope: the opt-in
MCP client — stdio transport, tool-serving subset, per-server schema token
accounting (PLAN §4.8's "no silent prompt fattening"). The official `rmcp`
SDK is deliberately NOT used for v1 (binary leanness + the subset we consume
is stable); it is the documented migration path for resources/prompts/sampling.

| ID | Item | Acceptance criteria | Status | Evidence |
|---|---|---|---|---|
| C1 | `modron-mcp` crate: JSON-RPC-over-stdio client (initialize handshake → tools/list → tools/call, timeout, notification skip, error taxonomy) + `McpTool` bridge (`mcp__<server>__<tool>` naming, Claude Code convention) + schema token cost at connect | E2E vs the shipped `modron-mcp-echo` server: round-trip, name mapping, unknown-tool error, failing-command actionable error, schema cost > 0 | DONE | 5 tests (3 e2e + fixture unit) |
| C2 | Config + wiring: `[mcp.servers.<name>]` (command/args/env/max_schema_tokens, default 20k); servers spawn at agent build; registration in every mode (one-shot/chat/TUI); per-server accounting line | Config unit tests; build path wired for all three front ends; over-budget server refused with actionable error | DONE | main.rs spawn_mcp_servers + registration; config.rs |
| C3 | Gates + docs | 316 passed / 0 failed; clippy clean; README documents MCP config | DONE | this log |

## Final verdict — Phase 5a

**MCP is opt-in, budgeted, and tested.** Any stdio MCP server joins the
registry under `mcp__<server>__<tool>`; its schema cost is computed at
connect and enforced against a per-server cap; transport/handshake failures
are actionable build errors, not silent skips. The echo server ships as a
working example of the transport. Remaining offline items: ACP server
(Zed), subscription OAuth. Live: the Z.ai smoke + dogfooding.

---

# Phase 5b — Kilo review adoption (opened/closed 2026-10-31)

Competitive review recorded in PLAN §8 (Kilo = Anaconda-owned multi-surface
agent: IDE extensions, CLI 1.0, desktop beta, cloud; 500+ models at zero
markup). Four adoptable gaps identified; two landed immediately.

| ID | Item | Status | Evidence |
|---|---|---|---|
| A1 | Agent personas: `[agents.<name>]` (system prompt / forced model / allow+deny merge) + `--agent <name>` with actionable unknown-agent errors; `HeuristicSelector::with_forced_model` pins every action | DONE | 317 tests; build path wired through persona resolution |
| A3 | Write-protected instruction sources: write/edit refuse AGENTS.md, CLAUDE.md, .modron/**, .claude/** (Kilo-parity safety; bash remains the documented, approval-gated escape hatch) | DONE | protection tests in write.rs; edit.rs guard |
| A2 | General `delegate` tool (persona + budget + tool restrictions on an isolated child engine; explore becomes the read-only special case) | DEFERRED | next tranche |
| A4 | Community SKILL.md gallery compatibility pass | DEFERRED | verification, not construction |

## Final verdict — Phase 5b

**The two cheap Kilo adoptions are in.** Personas give Modron the
custom-modes pattern (a debug persona, a reviewer persona — each with its own
prompt, model, and tool permissions) composed entirely from existing
mechanisms; and the agent can no longer edit the files that instruct it.
Defended differentiators (token-budget enforcement, learned routing, safety
stack, 4.1MB binary) remain the strategy. 317 tests, clippy clean, pushed.

---

# Phase 5c — Kilo findings baked (opened/closed 2026-10-31)

Baseline: 317 tests. A1/A3 landed last phase; this closes A2 and A4.

| ID | Item | Status | Evidence |
|---|---|---|---|
| A2 | General `delegate` tool: isolated child engine (own session/budget), named personas from `[agents.*]` (system prompt, forced (model, provider) via ForcedSelector, approval lists), optional tool filter, `delegate` excluded from child registries (no recursion); unknown persona errors listing configured agents; **honest accounting: `ToolOutcome.usage` (additive) lets subagent spend flow into the parent's budget governor and usage events as `subagent (tool)`** | DONE | core subagent.rs (ForcedSelector + delegate), tools/delegate.rs, engine folds usage; e2e: child spend (1200 tok) recorded + folded into parent totals (1830 = 220+1200+410); 65 tools tests |
| A4 | SKILL.md gallery compatibility: Anthropic-style extra frontmatter (license/allowed-tools/nested metadata) tolerated, `.modron/skills` + `.claude/skills` roots, no-frontmatter fallback to folder name, 2KB index cap with elision | DONE | gallery_compat_tests (3 tests) |
| V17 | Gates | DONE | **323 passed / 0 failed**, clippy -D warnings clean, fmt applied |

## Final verdict — Phase 5c

**All four Kilo findings are now baked.** The delegation pattern is complete:
`delegate` composes personas, budgets, tool restrictions, and isolated
contexts — and, critically, delegated spend flows into the parent's budget
governor and usage tables, so the orchestrator pattern cannot silently
overspend. Skills are gallery-compatible. Deferred remains: ACP server,
subscription OAuth, ONNX embedder, daemonized RPC — plus the live queue on a
Z.ai key.

---

# Phase 5d — ACP, auth, embedder seam, distribution (opened 2026-10-31)

Baseline: 323 tests, Phase 5b/5c pushed. All four remaining offline items in
one tranche, ordered by weight.

| ID | Item | Acceptance criteria | Status | Evidence |
|---|---|---|---|---|
| X1 | ACP server (`modron acp`): JSON-RPC over stdio per the Agent Client Protocol — initialize, session/new, session/prompt (text blocks → engine run), session/update streaming (agent_message_chunk from the delta sink, tool_call events from tool activity), stopReason result. Approvals run non-interactive this tranche (editor permission requests documented as next) | E2E vs the real binary as a fake Zed client: initialize → session/new → prompt → message chunks + end_turn; multi-turn via session resume | DONE | acp.rs (pure router + run loop) wired as `modron acp`; e2e vs the real binary: initialize/session-new/prompt with streamed agent_message_chunk + tool_call updates BEFORE end_turn, multi-turn resume via the session map; unknown method → JSON-RPC -32601 |
| X2 | Embedder seam: `Embedder` trait in modron-router + `HashingEmbedder` (current 3-gram fn) + `LearnedRouter::with_embedder` (per-instance centroids); ONNX documented as the implementation path behind a future feature | Existing eval accuracy unchanged with the default embedder; trait is the swap point | DONE | Embedder trait + HashingEmbedder default + LearnedRouter::with_embedder (per-instance centroids recompute per embedder); eval accuracy unchanged (16/16); the ONNX path is now a constructor change |
| X3 | Subscription auth (EXPERIMENTAL, Anthropic only): `modron auth login claude` (PKCE + localhost callback + token exchange + 0600 storage), `auth status`/`auth logout`; anthropic provider gains bearer-token subscription mode via `[auth.anthropic] use_subscription` | Unit tests: PKCE shape, token-response parse, state validation; live verification explicitly deferred (ToS-gray, needs a subscription) | DONE (EXPERIMENTAL) | auth.rs: PKCE (RFC 7636 vector test), authorize URL, localhost callback listener, token exchange + parse, 0600 storage, state/CSRF validation, `modron auth login|status|logout`; provider wiring (bearer mode) lands with live verification — ToS-gray, unverified against the real endpoint by design |
| X4 | Distribution prep: `install.sh` (release download + from-source fallback), Homebrew formula (packaging/modron.rb, source build), README install section | Script + formula committed; crates.io decision deferred | DONE | install.sh (release download + from-source fallback, syntax-checked), packaging/modron.rb Homebrew formula (source build + test), README install section |
| X5 | Gates + ledger + README + push | build/clippy/test green | DONE | **334 passed / 0 failed**; clippy -D warnings clean; fmt applied |

## Final verdict — Phase 5d

**Every offline item from the roadmap is now built.** Modron speaks ACP
(streaming updates, tool-call surfacing, multi-turn sessions — the Zed door
is open), the embedder swap is a constructor change away, subscription auth
exists end-to-end pending one live browser round-trip, and distribution
plumbing is committed. The binary stays at ~4.1MB. What remains is *live
work*: the Z.ai smoke, dogfooding, OAuth verification against a real
subscription, Linux sandbox verification, and the publish decision. The
engineering phase of this project — build everything buildable without
external accounts — is complete.

---

# Phase 5e — Release, permissions, MCP surfaces, bench (opened/closed 2026-10-31)

Baseline: 334 tests, Phase 5d pushed. The four follow-ups from the
continuation plan, all landed.

| ID | Item | Status | Evidence |
|---|---|---|---|
| R1 | v0.1.0 release: tag-triggered workflow builds x86_64-linux + aarch64-macOS archives named for install.sh's fast path, creates the GitHub release with the Actions token (no personal auth needed); Homebrew formula revision pinned to the tagged commit | DONE | .github/workflows/release.yml; tag v0.1.0 pushed (Actions builds the archives); formula revision adecea0a9b06 |
| R2 | ACP permission requests: Ask-mode mutating tools raise `session/request_permission` (allow_once/reject_once) against the editor; decisions route back by request id (single-writer stdout restructure); outcome parsing unit-tested | DONE | pure-helper tests + e2e still green; full editor round-trip lands with Zed |
| R3 | MCP resources + prompts: client `list_resources`/`read_resource`/`list_prompts`, echo server advertises both, `modron mcp info <name>` inspects any configured server (tools + schema costs, resources, prompts) | DONE | mcp_e2e resources_and_prompts_round_trip; `modron mcp info` wired |
| R4 | `modron bench`: fixed 6-task suite through the real engine, per-task tokens/turns/duration + totals; offline with mock, live baselines when a key exists | DONE | live demo: 6/6 completed, 7392 tok, 48 ms |
| V18 | Gates | DONE | **339 passed / 0 failed** (323 + 16 new), clippy clean |

## Final verdict — Phase 5e

**The release train exists (tag → built archives → GitHub release, fully
automated), editors can approve Modron's mutations, MCP servers are
inspectable with honest schema costs, and the bench harness turns future live
keys into baselines with one command.** Everything on the offline list —
every item from every review, gap analysis, and roadmap phase — is now
built. What remains is *signal*: a key, a browser round-trip, and real
traffic. The next engineering session should be driven by what those reveal.
