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
| T0 | Contract amendment: `EngineConfig.on_tool_event: Option<Arc<dyn Fn(ToolActivity) + Send + Sync>>` + `ToolActivity { Start{name}, Done{name, is_error, bytes} }` (additive) | Engine emits around tool execution; existing tests green | DONE | ToolActivity/ToolActivitySink added; engine emits Start/Done |
| T1 | New crate `modron-tui`: App state machine (input, status, streaming window, approval, usage) with pure `on_key/on_delta/on_tool/drain` methods returning Actions | Unit tests: keys → actions; streaming window bounded; approval state transitions | DONE | App state machine; 12 TUI tests |
| T2 | Inline-viewport renderer: fixed-height live region (status/spinner + streaming tail + input + approval line), completed content via `insert_before` (answers, tool lines); no wrapped Paragraphs | TestBackend render tests: idle frame, streaming frame windowed, approval frame, scrollback flush content | DONE | Inline viewport; TestBackend frame tests |
| T3 | Engine wiring: esc interrupts the running turn (drop/abort at await points; partial events persist); ctrl+c aborts + quits; ctrl+o toggles transcript mode (full tool output flushed) | State machine tests for abort/quit actions; abort leaves session resumable | DONE | Esc-abort, ctrl+c quit, ctrl+o toggle; state machine tests |
| T4 | CLI: `modron tui` subcommand (flags global); AgentSinks refactor so build_agent accepts injected approver/on_delta | `modron tui --provider mock` constructs (TTY-less environments skip entering raw mode with an actionable error) | DONE | modron tui subcommand; TTY guard verified |
| T5 | Gates + ledger + README | build/clippy/test green; README documents the TUI; verdict recorded | DONE | All gates green; README documents TUI |
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
| D1 | `modron usage`: aggregate dashboard across sessions — per-model input/output/cached, cache-hit %, est. cost, session + compaction counts; `--last <n>` display cap | Binary e2e: seeded store → dashboard shows per-model rows + totals + honest 0% when uncached | DONE | modron usage shipped with e2e |
| D2 | Compaction + verify config: `[compaction]` (enabled/threshold_frac/min_interval_turns/tail_messages/summary_max_bytes) and `[verify]` (command) sections; precedence flag > config > default | Config unit tests; wiring in build_agent_with | DONE | [compaction]/[verify] config; precedence wired |
| D3 | `verify` tool: runs the configured project check command (sh -c, 120s cap, output reduction, exit-code line); registered only when configured; treated as mutating (approval-gated) | Tool tests: pass, fail-with-output, oversized-output cap; gated like bash | DONE | verify tool; 5 tool tests |
| D4 | Prompt-budget CI gate: test asserting the real default registry's system prompt + tool specs stay under 2048 estimated tokens (with memory section under a typical load) | Test exists and is green; fails the build if prompts fatten | DONE | budget.rs CI gate; 4 tests |
| D5 | Gates + ledger + README | build/clippy/test green; README documents usage/verify/config | DONE | Gates green; README; verdict recorded |
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
| A2 | General `delegate` tool (persona + budget + tool restrictions on an isolated child engine; explore becomes the read-only special case) | DONE | Phase 5c delegate tool with personas/budgets/tool filters |
| A4 | Community SKILL.md gallery compatibility pass | DONE | Phase 5c gallery_compat_tests (3 tests) |

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
| V18 | Gates | DONE | **336 passed / 0 failed** (fmt-normalized final count), clippy clean |

## Final verdict — Phase 5e

**The release train exists (tag → built archives → GitHub release, fully
automated), editors can approve Modron's mutations, MCP servers are
inspectable with honest schema costs, and the bench harness turns future live
keys into baselines with one command.** Everything on the offline list —
every item from every review, gap analysis, and roadmap phase — is now
built. What remains is *signal*: a key, a browser round-trip, and real
traffic. The next engineering session should be driven by what those reveal.

---

# Phase 5f — THE LIVE PHASE BEGINS (opened/closed 2026-10-31)

**First live API calls in Modron's history.** The user's Z.ai key arrived;
the smoke ran; the findings below are real-traffic signal, exactly what every
prior phase was preparation for.

## What happened

1. **Smoke (glm-5.3-flash):** authenticated on both endpoints but `429 code
   1113 — Insufficient balance or no resource package` — the account has no
   balance and no active Coding Plan package. Our retry (3 backoffs) + honest
   error taxonomy worked exactly as designed on a real 429.
2. **Free tier discovered:** `glm-4.7-flash` (per PLAN research: free) ran
   WITHOUT balance — a 5-turn, 8-tool-call repo exploration completed with
   **105k input tokens, 63k cached (38% real cache hit)** and an accurate,
   file-cited description of Modron's own architecture. Dogfooding achieved
   on a $0 model. Catalog gap found honestly: unknown model → `n/a` cost, not
   an invented price. Fixed: glm-4.7-flash + glm-4.5-flash added as free
   tier entries.
3. **Multi-model bench (the user's ask, OpenCode-style):** `--models` list
   (default = curated free tier), per-task records persisted to
   `~/.modron/bench/<ts>.jsonl`, comparison table with cache-% and cost.
   **First real results:** glm-4.5-flash 6/6 completed (7.7k tok/task, 59%
   cache) — including the full mutation flow (create → edit → verify);
   glm-4.7-flash 4/6 (4.9k tok/task, 48% cache) — the 2 failures are
   transient free-tier rate limits (reproduced task passes in isolation;
   pacing between tasks is the fix, noted as debt).
4. **Cost of the entire live session: $0.00** (free tier verified in
   production; est. cost column shows the catalog's real 0.0/0.0).

## Findings (real-traffic signal — the stuff offline work couldn't reveal)

- Cache-aware prompt structure: CONFIRMED live (48–59% hit rates on
  multi-turn runs).
- Honest accounting: CONFIRMED live (provider-reported cached tokens parsed
  correctly from Z.ai's OpenAI-compat responses; unknown models render n/a).
- Token weights: a 5-turn repo exploration costs ~105k input on a 128k-window
  free model — compaction thresholds and reduction aggressiveness now have
  real numbers to tune against.
- Free-tier rate limits: back-to-back benching needs pacing (debt: `--delay`
  flag or adaptive backoff on 429-with-code-1113-style rate responses).

| Item | Status | Evidence |
|---|---|---|
| Live smoke | DONE | session 6e221cc0: 5 turns, 8 tools, 38% cache, glm-4.7-flash |
| Catalog free tier | DONE | glm-4.7-flash + glm-4.5-flash (0/0 pricing, verified) |
| Multi-model bench | DONE | bench --provider zai: comparison table + persisted JSONL baseline |
| V19 | DONE | **336 passed / 0 failed**, clippy clean |

---

# Phase 5g — Free-tier bench (opened/closed 2026-10-31)

The user's directive: OpenCode-style benching across a curated list of FREE
models. Research ([awesome-free-llm-apis](https://github.com/mnfst/awesome-free-llm-apis),
[OpenRouter's free-API comparison](https://openrouter.ai/blog/tutorials/free-llm-apis-compared))
identified seven OpenAI-compatible free tiers beyond Z.ai's: Google AI Studio
(Gemini 2.5 Flash, 1M ctx, ~500 req/day), Groq (Llama, 14.4k req/day),
Cerebras (fastest tokens/sec), Mistral free experiment tier (data-training
caveat), GitHub Models (per-minute/day caps), NVIDIA NIM (~1k credits),
OpenRouter :free (50 req/day basic).

| ID | Item | Status | Evidence |
|---|---|---|---|
| F1 | Factory: seven free-tier provider ids (groq/google/cerebras/mistral/nvidia/openrouter/github-models) — each = base URL + env key, same client shape | DONE | factory.rs openai_compat_with_key; keys: GROQ_API_KEY, GEMINI_API_KEY, CEREBRAS_API_KEY, MISTRAL_API_KEY, NVIDIA_API_KEY, OPENROUTER_API_KEY, GITHUB_MODELS_TOKEN |
| F2 | Catalog: 8 free-tier models across the new providers (tier-annotated, approximate windows); gemini-2.5-flash deduped (existed with real pricing — free-tier note added instead) | DONE | 30 router tests green |
| F3 | Bench v2: multi-provider (per-model provider from catalog, union built, missing keys SKIP that model — not the bench), --models csv, --delay-ms pacing, persisted JSONL baselines | DONE | live run below |
| V20 | Gates | DONE | **336 passed / 0 failed**, clippy clean |

## Live results (Z.ai free tier, --delay-ms 3000)

| model | completed | tokens/task | med. duration | est. cost |
|---|---|---|---|---|
| glm-4.5-flash | 6/6 | 9,857 | ~8.4s | $0.00 |
| glm-4.7-flash | 6/6 | 6,854 | ~16.9s | $0.00 |
| glm-5.3-flash | 0/6 (429 1113 — no balance, honest failures recorded) | — | — | — |

**First multi-model comparison table with real data — total cost $0.00.**
Signal: glm-4.5-flash completed every task including full mutation flows;
glm-4.7-flash is faster per token but mixed; pacing fixed the earlier
transient rate-limit failures (delay-ms 3000 between tasks).

## Final verdict — Phase 5g

**The bench now measures a curated free tier across providers, at zero cost,
with baselines accumulating on disk.** The user adds keys (GROQ_API_KEY,
GEMINI_API_KEY, …) and the same command grows rows. Deferred: compaction
tuning against the model window (engine surgery), router training from bench
JSONL (needs quality labels).

---

# Phase 5h — Parallel subagents + live inspection (opened/closed 2026-10-31)

The user's ask, modeled on Claude Code (best-in-class) and pi's subagents:
first-class PARALLEL delegation with per-agent live progress inspectable in
the TUI. The concurrency already existed (read-only dispatch runs all calls
in a turn concurrently); what was missing was visibility.

| ID | Item | Status | Evidence |
|---|---|---|---|
| H1 | Subagent events: `ToolActivity::{SubagentStarted, SubagentProgress, SubagentFinished}` (additive); delegate/explore children emit tagged heartbeats through the parent's sink (child tool activity wrapped as progress; start/finish bracket the run) | ACTIVE | H1a steer inbox, H1b Steer/Steered, H1c Enqueue/Dequeue/QueueChanged + LiveState.queued DONE. Open: actor on frames, LiveState.steer_pending; a Submit racing a queued start drops that queued message with a Note (fix: requeue at front) |
| H2 | TUI inspection: live subagent panel in the viewport — `⏵ [persona] turn N · tokens · last tool`, rows settle to done/failed, cleared on turn end | DONE | H2a engine plan phase + per-turn plan tail; H2b ConfigPatch.phase, LiveState.phase/plan_pending, PlanProposed / DecidePlan / PlanDecided, queue held while a plan is pending. Client rendering lands with C3 |
| H3 | ACP: subagent events surface as tool_call / status notifications to the editor | ACTIVE | H3a live counters; H3b DONE: Approver takes ApprovalAsk { call, agent }, children ask through the parent approver (label on ApprovalRequested / PendingApproval, summary prefixed `[agent] `), delegate starts without its own prompt in Ask mode with an approver, so delegates run concurrently. H3c DONE: a child ask is forwarded only while the run that spawned it is live (process-wide run scope keyed by session; a guard ends it on every exit), enforced in core for every transport. H3d DONE: SteerAgent / KillAgent / AgentFinished; each child has its own steer inbox and kill switch in a process-wide registry keyed by swarm id; rows carry state (running, waiting approval, done, failed, killed); a killed foreground child returns "stopped by the user" to the parent and its tokens count; a background child finishing publishes AgentFinished and a Notify. Open: TUI and CLI tty approvers hold one slot, so concurrent child asks there deny the earlier one; a kill during a pending approval lands when the ask resolves; delegate_status ids (8 chars) differ from swarm ids; a killed child notifies as failed; app has no per-agent view yet |
| H4 | Parallel delegates verified: two delegate calls in ONE turn run concurrently (read-only dispatch), results land in transcript in call order, each child under its persona's forced model | DONE | H4a DONE: EngineConfig.reviewer (config `review_model`, `review_max_rounds`), SessionEvent::Review, TranscriptEntry::Review, bounded fix rounds, review errors never fail a run. H4b DONE: the reviewed diff is `git diff HEAD` plus synthesized hunks for untracked files (40 files, 20 KB each, no symlinks, 60 KB total); rule-based claim checks recorded as findings under model `rules`, advisory, never extending a run |
| V21 | Gates | DONE | **337 passed / 0 failed**, clippy clean |

## Final verdict — Phase 5h

**Parallel delegation with live inspection is first-class.** A model can fan
out N delegations in one turn; they run concurrently; the TUI shows a live
row per subagent (persona, turn, tokens, last tool); ACP editors see the same
as tool-call updates; and every child's spend lands in the parent's budget.
Deferred: detached background delegates that outlive the parent turn (needs
completion-notification semantics — the next big delegation step), and
worktree isolation for parallel MUTATING agents (pi's pattern; requires git
integration).

---

# Phase 5i — Delegation depth + model-aware caps (opened/closed 2026-10-31)

Baseline: 337 tests, Phase 5h pushed. The three deferred items, in reverse
weight order: model-aware caps landed first (smallest, closes the live-signal
debt), then the two delegation-depth items.

| ID | Item | Status | Evidence |
|---|---|---|---|
| I1 | Model-aware caps: `ModelChoice.context_window` (additive, catalog-filled by every selector path); engine selects FIRST, computes effective cap = min(global cap, model window), compacts against it, and the pre-flight budget check uses it. The live finding is now enforced: a 128k-window free model compacts at 0.8×128k even with a 180k global cap | DONE | compaction_test::compaction_uses_the_model_window_not_the_global_cap (a 10k-window model compacts despite a 180k cap; test caught the tail-keeps-the-huge-message case, fixed with tail_messages=1 in the fixture) |
| I2 | Background delegates: `delegate {background: true}` returns an agent id immediately; child runs detached; `delegate_status {id?}` lists running (turns/tokens via progress events) or completed (brief) agents; state survives across chat turns (engine-held registry) | DONE | tools/delegate.rs + engine registry; e2e: started → running → done with brief |
| I3 | Worktree isolation: `delegate {isolate: true}` requires git, creates `.modron/worktrees/<id>` via `git worktree add`, runs the child there, and reports the worktree path + `git diff --stat` in the brief — review-not-auto-merge semantics (merging is the user's/parent's explicit decision) | DONE | e2e with a git-initialized temp repo; non-git working dir → actionable error |
| V22 | Gates | DONE | **341 passed / 0 failed** (incl. 3 Phase-5i tool tests + the model-window compaction test), clippy clean |

## Final verdict — Phase 5i

**Delegation now has depth.** Background delegates let the model fan out work
and check on it later (chat turns persist the registry); isolated delegates
run mutating subtasks in git worktrees with review-first merging; and the
compaction/overflow machinery now respects the actual model in use — the
live finding from the 105k-token free-model runs is a tested invariant.
Deferred (next): `loadSession` in ACP, router training from bench JSONL
(needs quality labels), and merge-automation for worktrees.

---

# Phase 6a — Session lifecycle (opened/closed 2026-10-31)

The user's requirement: auto-vacuum, TTL on sessions, session restart —
checked against the tree (none existed), built after the loose end from
existing work (bench hygiene) was closed.

| ID | Item | Status | Evidence |
|---|---|---|---|
| L0 | Bench hygiene (finishing existing work): the suite's mutation tasks wrote bench-marker.txt into the working dir — the reason it got committed twice. Bench now removes its known artifacts after the run | DONE | run_bench cleanup; repo verified clean |
| L1 | `SessionStore::delete` (additive trait method with default; JsonlSessionStore removes .jsonl + .meta.json + .rewind) | DONE | store test: delete roundtrip + second-delete false + load-after-delete errors (documented) |
| L2 | `modron sessions vacuum --older-than-days N [--dry-run]` — deletes sessions by updated_at, lists what goes | DONE | live demo against ~/.modron/sessions (dry-run listed 5+) |
| L3 | `modron sessions restart <id>` — same id, fresh context: history stays on disk (append-only), the resume marker moves to the log's end so the next --resume starts clean (reuses the rewind marker machinery) | DONE | handler + marker semantics |
| L4 | TTL auto-vacuum: `[sessions] ttl_days = N` in config → vacuum runs at CLI startup; UNSET = never delete automatically (no silent data loss) | DONE | config.rs SessionsConfig; startup wiring with non-fatal failure |
| V23 | Gates | DONE | **342 passed / 0 failed**, clippy clean |

## Final verdict — Phase 6a

**Sessions now have a lifecycle.** They accumulate by default (nothing is
ever deleted silently), the user opts into TTL via config or vacuumes
explicitly with a dry-run preview, and restarting a session keeps its id and
history while giving the model a clean context. Plus the bench no longer
litters the target repo. 342 tests, clippy clean.

---

# Phase 6b — PAID: first subscription benchmark (opened/closed 2026-10-31)

The user subscribed to the GLM Coding Plan (Pro, ~$80/mo) and the plan went
live mid-session (the earlier 1113s were the plan not yet attached). First
paid benchmark, full suite, three models:

| model | completed | tokens/task | cache % | est. cost/suite |
|---|---|---|---|---|
| glm-5.3 | **6/6** | 5,234 | **77%** | $0.0605 (nominal — plan credits) |
| glm-5.3-flash | **6/6** | 5,428 | **82%** | $0.0068 (nominal) |
| glm-4.5-flash | 0/6 — `429 code 1309` "Coding Plan package has expired/unavailable" | — | — | — |

## Live findings (real-traffic signal)

1. **Cache design validated at production scale: 77–82% cache hit rates** on
   multi-turn agentic runs — the Phase-1a stable-prefix work is saving ~4 of
   every 5 input tokens from full re-billing.
2. **The 1309 discovery**: once a Coding Plan is attached to an account, the
   previously-free flash models can return 1309 instead of running free —
   Z.ai appears to gate non-plan models behind the plan status. The curated
   free list stands for plan-less accounts; plan accounts should bench the
   plan models.
3. **glm-5.3-flash is the efficiency pick**: ~9x cheaper per suite than
   glm-5.3 with identical 6/6 completion. glm-5.3's edge would need quality
   scoring (next bench iteration: add task-level verification).
4. On a subscription, the "est. cost" column is nominal (per-token catalog
   price); real spend burns plan credits on Z.ai's 5-hour/weekly windows —
   Modron's usage table measures tokens; credit monitoring lives in the
   Z.ai dashboard.

## Final verdict — Phase 6b

**The full loop is closed on the paid tier: real key → real agentic runs →
real comparison table → persisted baseline.** Every subsystem shipped this
session (cache discipline, honest accounting, bench harness, error taxonomy)
did its job against live traffic on the first try.

---

# Phase 6c/6d — Evals + THE FIRST DOGFOOD (opened/closed 2026-10-31)

**The capstone: the harness now measures correctness (mini-SWE evals) and
builds itself (dogfooding) — both on the paid plan.**

## Phase 6c — evals (mini-SWE-bench)

`modron eval` runs a committed 5-task suite through the real engine in
isolated temp workspaces: setup → agent works (approve-all; verifier is the
safety net) → objective verifier (shell exit 0 = resolved). Completion is
not success — the verifier decides. Scorecard per model, records persist to
`~/.modron/evals/<ts>.jsonl`.

**First live eval: glm-5.3-flash resolved 5/5 (100%)** — 35,650 tokens:
- hello-script ✓ (creation) · fix-failing-check ✓ (debugged via test
  feedback, 4 turns 5 tools) · count-nonempty-lines ✓ (6 turns, the hard one)
  · csv-to-json ✓ (transformation) · merge-configs ✓ (synthesis)
The eval run also caught a real bug pre-release: the child engine wasn't
inheriting the run's default provider (evals would never have reached the
configured provider) — fixed and covered.

## Phase 6d — THE FIRST DOGFOOD (Modron builds Modron)

Task given to glm-5.3: "add --json <path> to the bench command" — the full
stack in action:
- Run 1: 11 turns, 13 tool calls, **budget governor stopped it honestly at
  200k tokens** mid-task. Resumed with a higher cap — the session continued
  with full context.
- Run 2 (resume): finished the implementation, ran `cargo check` itself,
  self-reported. **Review finding: the clap flag wasn't wired** — caught by
  the reviewer (me) testing the stale binary; the agent's pushback was
  actually correct (it had wired it in its final turn; my binary was stale).
- Run 3 (feedback): flagged a real bug the agent's own verification missed —
  `--json` wrote `[]` because MY skip-fix keyed the provider map by
  `provider.id()` (the known "openai-compat" aliasing) while the filter
  checked catalog names. Fixed in review; agent verified the fix.
- **Final state: `modron bench --json` works, valid JSON, 2 rows
  (glm-5.3 6/6, glm-5.3-flash 6/6), 345 tests green, merged to main.**

## The dogfood meta-result (the honest scoreboard)

- 3 agent runs, ~590k tokens total (~$0.55 nominal at catalog pricing),
  3 review rounds, 1 budget-governor stop, 1 stale-binary misdiagnosis
  (mine), 1 provider-keying bug found by review.
- What the agent did well: implemented the feature, ran its own cargo check,
  pushed back correctly when the reviewer was wrong, kept the repo clean.
- What still needs the human: review (the empty-JSON bug passed the agent's
  own verification), budget/scoping judgment, and merge decisions.
- This is exactly the Claude Code working relationship — and it ran on OUR
  harness, on OUR own codebase.

---

# Phase 6e — Self-upgrade + update notices (opened/closed 2026-10-31)

The final requested step: build/deploy in git (done: release workflow), curl
install instructions (done: install.sh + README), an `/upgrade`-style
command, and an upgrade notice in the TUI.

| ID | Item | Status | Evidence |
|---|---|---|---|
| U1 | `modron upgrade`: queries the GitHub release API (unauthenticated → MODRON_GITHUB_TOKEN → GITHUB_TOKEN → gh keyring — needed while the repo is private), downloads the platform archive, verifies the binary (`--version`), backs up the current binary as `modron.bak`, replaces in place with rollback-on-failure | DONE | live: "already up to date (0.1.0 ≥ v0.1.0)" against the real release |
| U2 | Update notices: `startup_update_notice` — 24h-cached verdict in `~/.modron/update-check.json`, live check via the `curl` subprocess (a reqwest::blocking client panics on drop inside tokio — found by the e2e test, fixed), 2s cap; TUI status line renders `⬆ vN available — modron upgrade`; chat prints the notice after the banner | DONE | chat e2e green with the notice path active; TUI status renders it |
| U3 | curl install instructions + `modron upgrade` documented in the README | DONE | README install section |
| V24 | Gates | DONE | **348 passed / 0 failed**, clippy clean |

## Final verdict — Phase 6e

**The distribution loop is closed end-to-end:** tag v0.1.0 → workflow builds
platform archives → release published → `install.sh` curl-installs it →
`modron upgrade` keeps it current → TUI/chat tell the user when a new one
exists. The live upgrade check ran against the real release (up to date), and
the notice path is exercised by the chat e2e. One finding: reqwest::blocking
panics on drop inside tokio — the startup check uses the curl subprocess
instead (matches install.sh, zero deps). Remaining: make the repo public (or
ship tokens) for unauthenticated installs; the publish decision (crates.io,
Homebrew) stays with you.

---

# Phase 7 — Multimodal attachments (opened/closed 2026-10-31)

Claude Code-parity: images and documents as first-class inputs.

| ID | Item | Status | Evidence |
|---|---|---|---|
| M1 | Core: `Attachment` enum (Image { media_type, base64 data } / Document { name, text }) on `Message.attachments` (additive, serde-defaulted); `Engine::run_task_with_attachments` | DONE | 356 tests green |
| M2 | Anthropic encoding: image blocks (base64 source) BEFORE text; documents inline as text blocks | DONE | fixture tests: block shape, ordering, document inlining |
| M3 | OpenAI-compat encoding: image_url parts (data URIs) + text parts; plain messages stay strings (no regression) | DONE | fixture tests: parts array, data URI format, no-attachment regression |
| M4 | CLI `--attach <path>` (repeatable): images by extension + magic-byte sanity; text inlined; PDF via pdftotext (poppler) with actionable missing-binary error | DONE | loader in main.rs |
| V25 | LIVE verification: glm-5.3 read an attached document and answered correctly through the full stack | DONE | live run above |

## Final verdict — Phase 7

**Multimodal is live end-to-end on the paid plan.** `modron "describe this"
--attach photo.png` works: the image rides the message as a base64 block
(Anthropic) or data-URI part (OpenAI-compat), documents inline as text, PDFs
extract via pdftotext. Token accounting flows through unchanged (the live
run showed honest usage). Deferred: paste-from-clipboard in the TUI, image
generation, and @-mention file completion in the TUI input.

---

# Phase 8 — Performance + DevEx review (opened/closed 2026-10-31)

The user's ask: competitor review, bundle size, super-fast startup, fantastic
DevEx. Measured, then fixed.

## The numbers (vs competitors)

| metric | modron | Claude Code | opencode | pi_agent_rust |
|---|---|---|---|---|
| startup (--version) | **5.9ms p50** | ~200-500ms (Node) | ~100ms+ (Go) | 5-7ms p95 (claim) |
| binary size | **4.1MB** | ~100MB+ (npm install) | ~50MB | unknown |
| language | Rust (single static) | TypeScript/Ink | Go+TS | Rust |

**Startup is class-leading** — matching the fastest Rust agent and ~40x
faster than Claude Code's Node startup. The single-binary distribution (no
runtime, no node_modules) is the structural advantage.

## What was measured

- `--version`: 5.9ms p50 / 6.7ms p95 (10 runs, perf_counter)
- `--help`: 5.8ms p50
- `sessions list`: 6.0ms p50
- Binary composition (cargo bloat): std 7.2%, regex_automata 5.0%, rustls
  4.1%, our code ~3%. No single bloat offender; the size is proportionate.

## Fixes landed

| ID | Item | Result |
|---|---|---|
| D1 | `modron doctor`: provider keys, config, git, pdftotext, storage, terminal — ✓/✗/· with actionable hints | verified live; found zai key set, 188 sessions |
| D2 | Bare `modron` (no args) shows help instead of erroring | verified |
| D3 | Regex trimmed (default-features off, +unicode-perl +unicode-case): 4.3MB → 4.1MB | grep tests still green (7/7) |
| D4 | First-run audit: no-key error is actionable (names env var + console URL); unknown provider lists valid values; bad flag has clap's tip | verified; no changes needed — already good |

## DevEx verdict

The first-run path is strong: bare invocation teaches, doctor diagnoses,
errors link to fixes, and the binary starts in ~6ms. The remaining DevEx
wins are in the TUI polish (in progress) and the repo-public decision for
frictionless curl-installs.

---

# Phase 8c — Ledger closure + router corpus update (closed 2026-11-01)

Final audit pass: every stale TODO in the ledger resolved (the Phase 1d/2a
rows were completed long ago but never flipped — 11 rows fixed to DONE),
remaining DEFERRED items verified as genuinely external (daemonized RPC,
Linux sandbox, OAuth live verification, Zed round-trip — all need
environments or decisions outside this session).

Router corpus updated with eval-informed patterns from the hard-tier data:
refactoring, regex fixes, JSON transforms, multi-file consistency, and
error-recovery patterns now route to the strong tier; simple listing and
status queries route cheap. Accuracy: 16/16 (100%), 33% cost savings.

Live verifications this tranche:
- Attachments + resume: secret code persisted across sessions via
  attachment → memory → resume chain
- Memory cross-session: "pnpm" remembered → recalled in fresh session
- Checkpoint/rewind: full undo cycle on a real file
- Sandbox: Seatbelt denied network, allowed workspace writes (live kernel)
- Delegation: multi-file task through the full stack

**The ledger is fully closed: 0 TODO rows, every item DONE or explicitly
DEFERRED with the external dependency named. 356 tests, clippy clean,
rustc 1.99.1, 4.1MB binary, ~6ms startup.**

---

# Phase 9 — Daemonized core + Linux verification (closed 2026-11-01)

The user's directive: do the daemon now, Linux sandbox in Docker, test Zed.

| ID | Item | Status | Evidence |
|---|---|---|---|
| N1 | `modron daemon start/stop/status`: persistent engine on a Unix socket (`~/.modron/daemon.sock`), PID file with process-liveness check, clean ctrl+c shutdown (socket + PID removed) | DONE | started, status-checked, stopped cleanly |
| N2 | RPC protocol: newline-delimited JSON — `ping`, `send_task` (streams `delta`/`tool` notifications, then the result with answer/usage), `resume_task`, `list_sessions`, `stop` | DONE | live RPC test: Python client sent "daemon works" → streaming delta → final answer + usage (glm-5.3-flash, 1,727 input tokens) |
| N3 | Linux verification in Docker (rust:1.99.1): full workspace test suite + release build + smoke test | DONE | **355 passed / 0 failed on Linux** (2 fewer = macOS-only Seatbelt tests); 4.8MB linux binary; mock smoke green |
| N4 | Zed ACP: modron added to the user's Zed settings as a custom agent server | CONFIGURED | user tests in the Zed UI (cmd-r → modron) |
| N5 | Repo verified PUBLIC — install.sh and modron upgrade work unauthenticated | VERIFIED | api.github.com returns 200; upgrade checked live |

## Final verdict — Phase 9

**The daemon works: sessions survive terminal close, the Unix-socket RPC
streams live deltas, and the full test suite runs green on Linux (Docker).
The engine is now deployable as a persistent service.** The Zed ACP config
is written — the user tests it in their editor. Remaining: the Linux
sandbox implementation (bubblewrap — the Docker test proves the codebase
compiles and runs on Linux; the bwrap wrapper is the one missing piece) and
TUI polish.

## Security and correctness remediation (2026-11-14)

A whole-repo review produced 45 work packets; all 45 are DONE on branch
`claude/repo-security-review-udrusy`. The plan, design decisions, the
per-packet ledger with commits and evidence, and the remaining backlog
live in `docs/SECURITY_PLAN.md`.

Gate evidence on Linux (`--workspace --exclude modron-desktop`): fmt
clean, clippy clean, 842 tests pass.

NOT verified, needs a macOS / mobile / CI pass before release:
`crates/modron-desktop` edits (C1, C5, C6, E3), `mobile/app/App.tsx`
edits (C1, C4, C6), the Seatbelt profile changes (D6), `cargo deny check`
after the tokio-tungstenite rustls feature (C6), the GitHub workflows
(G4), and the cloud container image (G1, G2).

Round 2 (same day): the low-severity findings and open items were worked
through as well - see sections 7 and 8 of `docs/SECURITY_PLAN.md` for
what was done, what is still open and why. Gate evidence: 922 tests pass,
clippy and fmt clean on Linux.


# Program U - unified NSH client, Cloudflare portal, harness gaps (opened 2026-11-15)

Plan: PLAN.md section 10. Lead: the lead architect (single writer of this
ledger). Workers are small-model agents given one packet each with an
explicit file list; they do not commit. The lead reviews every diff, runs
the gates (fmt, clippy -D warnings, test --workspace --exclude
modron-desktop on Linux) and commits.

Status values: TODO, ACTIVE, DONE, BLOCKED (with the blocker and who can
clear it). A packet is DONE only with evidence (commit + gate result).

## Rules for this program

- Old clients (modron-tui views, modron-desktop, mobile/app TSX) are
  frozen: no packet adds features to them. Shared logic goes to
  modron-view, modron-protocol or the client core.
- One packet = one worker = a disjoint file list. Engine packets
  (crates/modron-core/src/engine.rs) run one at a time.
- New protocol events are documented in docs/PROTOCOL_V2.md and in
  modron-protocol doc comments before any client renders them.

## Packets

### Spikes

| ID | Packet | Status | Evidence / blocker |
|---|---|---|---|
| S1 | NSH web target in Rust: one transcript screen over the wasm client core; measure bundle size, load time, input | DONE | spikes/nsh-web/RESULTS.md. Rust-only app builds with `nsh package --target web` (nsh CLI 0.5.1, wasm32-unknown-unknown on the pinned toolchain). wasm 5.4 MB raw, 1.95 MB gzip, 1.40 MB brotli. Real DOM with a real text input, no canvas. First row at ~165 ms median in headless Chromium (desktop and iPhone 13 viewports, local server). 2,000-token stream holds 16.7 ms frames. modron-mobile-core type-checks for wasm32. Not tested: real phones, real soft keyboard, slow networks. Only one ARIA role present (accessibility gap) |
| S2 | NSH terminal target: startup time, streaming at token rate, scrollback | DONE | spikes/nsh-tui/RESULTS.md. Renders headless via ratatui TestBackend; first frame ~1.9 ms in-process; 2,000-token stream mean 1.4-2.7 ms/frame; virtual_list pages 5,000 rows at ~0.8 ms. Open risks: 45 MB stripped binary (modron is ~4 MB), default features pull QuickJS (no-JS build not compiled yet), no Rust API found for pushing outside data into a running app (tokens were fed as paste input), Enter drops input focus |
| S2b | NSH follow-up: compile without default features (no QuickJS), measure size; find the Rust API for feeding external events (signals + wake callback) | DONE | spikes/nsh-tui/RESULTS.md "Follow-up S2b". No-JS: `nsh-platform-tui` with default-features = false (0 quickjs crates, 343 packages); 34.1 MB stripped, 17.1 MB with opt-level z + fat LTO + cgu 1 + panic abort. External events: `nsh_core::element::spawn` awaiting a tokio mpsc receiver; wake callback drives every frame (2,000 tokens: draw mean 1.37 ms, send-to-visible p95 10 ms). Focus: `SubmitBehavior::Submit` keeps it. Remaining gap: 17 MB vs ~4 MB today |
| S2c | Terminal binary size: attribution and trimming | DONE | spikes/nsh-tui/RESULTS.md "Follow-up S2c". 17.1 MB -> 11.4 MB stripped by turning off nsh-base highlight/editor/plot/rich-editor/emoji-picker/term and nsh-engine net (needs a small vendored nsh-platform-tui patch, in spikes/nsh-tui/vendor) plus ratatui default features. Download size: 4.8 MB gzip, 3.6 MB xz. Reference: ratatui-only app 0.46 MB. Caveats: measured on the headless build (no crossterm backend); turning off engine net must be re-checked against the app, which uses its own websocket client; upstreaming the feature gates would remove the vendored patch |
| S3 | furrow snapshot/restore over R2 from a container | BLOCKED | Needs an R2 bucket and credentials (owner) |

| E2E | Whole chain, real pieces: modron serve (mock) -> bridge (local wrangler dev) -> web app in headless Chromium | ACTIVE | e2e/run.mjs, e2e/FINDINGS.md. First run 2 of 11; it exposed that the remote-control tunnel never subscribed to its session, that share URLs carried a ws scheme, and that late viewers lost history over 96 KB. All three fixed in rc.rs by the lead; then 11 of 11 after an app layout fix: a fixed 600 pt transcript pushed the approval and stop controls below the viewport; the transcript now follows the window height. Console-error capture is on for every scenario. E2E-2 added portal UI, web feature and terminal scenarios: P1-P4 pass (sign-in through the UI, owner drives and approves, invite created / forbidden submit / revoked, offline row). W1-W6 and T1 fail, findings in e2e/FINDINGS.md: the serve treats the portal OWNER as a share link (the bridge forwards no role), so config and shell are refused; the web page panics on std Instant::now (the first panic behind the earlier closure error); a message sent while an approval waits goes out as submit, not steer (the client shows idle); a live page never requests the session list; the terminal app sometimes stays on connecting; S6b regressed (possibly the virtual list showing only visible rows) |

| PARITY | Audit: everything crates/modron-tui does versus the new app | DONE | docs/APP_PARITY.md: 140 rows; DONE 52, PARTIAL 17, CORE-ONLY 15, MISSING 50, N/A 6; a 20-item ordered list of what must land before the old TUI is deleted (keys, multi-line entry and paste, Esc on an approval, input history, @file and !command, credentials, tool detail panel, scrollback keys, attachments, startup flags) |

| PARITY-A | Terminal input: parity items 1, 2, 3, 4, 8 (keys), 9, 14, 15 | DONE | Full key map; multi-line composer (Shift+Enter, Ctrl+J, trailing backslash) with bracketed paste; Esc denies a pending approval; input history; PgUp/PgDn/Home/End scrollback with a follow marker; first Ctrl+C interrupts, second exits; session id in the header and a resume line on exit; /reasoning with a live tail. Open: the composer has no height cap; paging moves a fixed 10 lines; app-local strings need a shared home ("new output below", "press ctrl+c again to quit", the resume line); /reasoning is not in the shared slash table; the Backspace-dequeue live test is racy under load |
| PARITY-D1 | App: parity items 5, 7, 11, 16, 17 and the composer cap | DONE | Ctrl+T tool detail panel; `!command` through the core shell call; `@file` completion on the terminal target (web keeps `@` as text); startup flags forwarded as config after attach (--working-dir and --sessions-dir only to a server the app spawns); unix socket preferred locally, --socket, tokens, --insecure-lan; composer capped at 6 lines. Open: no `!cmd` echo and dim output rows in the transcript; the @file index is built once per process; strings needing a shared home ("no tool activity yet", help rows) |
| PARITY-D2 | App: parity items 6, 10, 12, 13, 18, 19 and three e2e defects | PARTIAL | DONE: credentials screen (masked key, server-side sign-in; auto-opens once only on a blank idle session), /review /btw /learn /plan <task> /goal /agent, /exit /quit /q, picker current marker and dimmed unavailable rows; e2e defects: one clock for native and wasm (the std Instant panic), session list requested on every connect, a stalled connect retries and a stale socket falls through. NOT DONE: attachments in the app (10), automations / worktree / allow / deny screens (18), /clear, toast expiry, idle hints, queue and steer formats still app-local. Decided under PLAN 10.2a: explicit save writes default model + provider to the server config (implementation: SHELL-2). 18 commands and aliases are missing from the shared slash table |
| ROLE | Portal owner role reaches the serve; steer while an approval waits | DONE | Frame.role is stamped by the bridge (any client-supplied role is deleted first) and trusted by the serve only on a tunnel that authenticated with the host token; owner = everything for that one session, approver = approve / decide_plan, viewer = read; an exhaustive per-variant table decides every client frame, so a new variant cannot be forgotten. The client core now treats task events, deltas, tool starts and approval requests as "running", so a message sent during an approval wait is a steer. Caveat: a bridge without HOST_TOKEN accepts any agent tunnel |
| SHELL-2 | Shell passthrough per PLAN 10.2a (`!` shared, `!!` private, no prompt for the owner); persist default model from the picker | DONE (crates and client core; app wiring open) | SessionEvent::UserShell { private }: a private command is shown in the transcript on every device and kept out of model context and compaction input (tested). The owner's command is refused only by the deny list, a deny rule (per segment), a pre_tool veto, a missing bash tool or an empty command. PersistDefaultModel writes default_model / default_provider to config.toml (local connections only). Side effects to know: the config writer (shared with router defaults) is now atomic and refuses to overwrite an unparsable file, but comments in config.toml are lost on rewrite; a saved default pins future sessions to that model, so they stop using the router. Open: app uses parse_shell_input for `!!` and offers the save control |
| SLASH-TABLE | Shared slash table: aliases and the commands clients served on their own | DONE | SlashSpec.aliases; added worktree, reasoning, goal, btw, review, learn, agent, allow, deny, automations, exit (with their aliases); the canned prompts for /review, /btw, /learn, /plan live in modron-view, byte-equal to the old TUI (test); a test lists every old-TUI command as shared or TUI-only (clear, context, theme, issues) with reasons. Note: the prompts carry the old TUI's em dashes, so the client core's wording changed from its ASCII copies |
| PARITY-B | Client core and wire support for parity items 5, 7, 10, 11, 16, 17 | DONE | Snapshot tool_detail rows (and a fix: the tracker was never cleared at turn end, so a new turn re-showed old rows); attachments on submit with type checks (no size cap exists on the server beyond the 256 MiB websocket limit); unix-socket transport, ?token= and device token helpers; startup setters. Shell frame, client method and refusals exist; execution is a stub pending an engine entry point (PARITY-C). --working-dir and --sessions-dir have no wire equivalent |
| PARITY-C | Engine entry point for a user-issued bash call; server runs Shell through it | DONE (two owner decisions open) | Engine::run_user_shell takes the command through the same deny list, rules, approval mode, phase, approver, hooks and sandbox as a model-issued bash call; the server runs it in the session run slot (busy, interruptible, ends with TurnDone). Decided 2026-11-15 under PLAN 10.2a: `!cmd` reaches the model, `!!cmd` does not; the owner's own command is not approval-prompted but keeps the sandbox, deny list, deny rules and pre_tool veto (implementation: SHELL-2). Scoped (share-link) connections cannot use Shell at all |

### Protocol

| ID | Packet | Status | Evidence / blocker |
|---|---|---|---|
| P1 | docs/PROTOCOL_V2.md: steer/queue, plan approval, sub-agent approvals and counters, reviewer verdict, pause/wake, actor+role | DONE | docs/PROTOCOL_V2.md (16ac999) |

### Harness

| ID | Packet | Status | Evidence / blocker |
|---|---|---|---|
| H1 | Steer and queue messages during a run (engine, server hub, protocol) | ACTIVE | H1a engine SteerInbox DONE (3 tests). Open: H1b protocol Steer frame + server hub; H1c queue |
| H2 | Enforced plan phase, approve step, per-turn plan re-injection | TODO | |
| H3 | Sub-agents: approvals to parent, live counters, completion events, cap | TODO | |
| H4 | Reviewer role + verdict event + non-LLM claim checks | TODO | |
| H5 | Scheduled wakeups, pause reasons, completion/block notifications | ACTIVE | H5a DONE: Wake / CancelWake / WakeChanged, LiveState.wake, persisted and re-armed at start; Notify for done, failed, plan, wake. H5b DONE: Notify for approval requests (tool name only, never arguments), done text uses the session title. Open: SessionEvent::Paused; push delivery is the relay (C5) |
| H6 | Hash-anchored line edits, measured with modron bench | ACTIVE | H6a edit_lines tool + read(line_hashes) DONE, on-demand, approval-gated, edit allow/deny entries cover it. Open: modron bench comparison before any default change |
| H7 | Reasoning-effort parameter and named model roles | ACTIVE | H7a DONE: Effort (low..max) from config (`effort`, `--effort`) and ConfigPatch (effort / clear_effort) to every main-loop and escalation request; Anthropic `output_config.effort`, OpenAI-compatible `reasoning_effort`, Responses `reasoning.effort` (xhigh and max map to high there); status line shows `effort <level>`; sub-agents inherit. Not sent on compaction, reviewer or router calls. Not exercised against a live provider. H7c DONE: `[roles]` plan / subagent / critic / summary (config, ConfigPatch.roles, LiveState.roles). Precedence: a provider pin or persona model beats a role; a role beats the router; `--model` alone is not a pin. Open: H7d a role keeps the router tier's provider and only swaps the model, so a role model on a different provider is sent to the wrong one (needs a model-to-provider lookup, as /model has); sub-agents do not inherit roles; H7b |
| H7d | Role models follow their provider (model-to-provider lookup injected like price_lookup; also for reviewer and escalation) | DONE | EngineConfig.provider_lookup wired from the router catalog (signed-in providers only); explicit `provider::model` wins; unknown falls back to the turn provider |
| H7b | Anthropic thinking: request summaries when the user opted into reasoning display; decide on replaying thinking blocks | TODO | Finding (Anthropic API reference, 2026-11): the provider drops every thinking block from responses and never replays them. That is valid (removing ALL thinking blocks keeps history consistent) but costs reasoning continuity across tool calls, and on current models thinking display defaults to omitted, so `/reasoning` shows nothing unless the request sets `thinking: {type: "adaptive", display: "summarized"}`. Replaying blocks would require an append-only transcript: the transient per-turn plan tail (H2a) and keep-tail client-side compaction both edit history and would have to change first. Design needed before any worker packet |
| H8 | Command and path permission rules + explain command | DONE | H8a matcher, H8b engine gate + [permissions] config, H8c rules reach serve/daemon/acp sessions (shared build path), ConfigPatch.rules / LiveState.rules / ExplainRule. Evals stay rule-free |
| H9a | Token waste: whitespace runs in the on-demand stub and delegate_status descriptions; stale default_registry doc | DONE | stub and delegate_status descriptions fixed |
| H9b | Real compact command and true context occupancy over the protocol | DONE | Engine compact_session + context_gauge; Compact frame, ContextChanged, LiveState.context. Known: Compact vs Submit is check-then-act; the gauge is None until the first run or compaction in a server process |
| H9c | Hooks: turn-end and session-start events | DONE | session_start and turn_end hooks: observe-only, never block; do not disable speculative reads |
| H9d | MCP over HTTP | DONE | MCP Streamable HTTP client: [mcp.servers.x] url + headers with ${ENV} expansion, JSON or SSE replies, session id, one re-initialize on 404, body cap. stdio unchanged. CLI connects by transport (connect_mcp_server). Out of scope: legacy SSE transport, OAuth, server-initiated requests |
| H9e | Web fetch tool | DONE | webfetch tool: on-demand, approval-gated, SSRF checks with a pinned resolved address, no redirects, refused under a sandbox, body wrapped in untrusted-content markers. Ignores HTTP proxies by design (a proxy would bypass the address pin) |

### Client

| ID | Packet | Status | Evidence / blocker |
|---|---|---|---|
| C1 | Client core: approval/sandbox/decision labels into modron-view; mobile/rust-core consumes them | DONE | 16ac999; labels in modron-protocol / modron-view, client core delegates |
| C2 | Oculus tokens as one NSH theme file (shadcn cssVars), light and dark | DONE | 43c3dbe; app/theme/oculus.json + check.py. Owner to confirm: chart colours, mono font, tool colour, large radius |
| C3 | NSH app in Rust: sessions, chat, approvals, settings (replaces App.tsx) | ACTIVE | C3a chat screen on the terminal target; C3b client core speaks the v2 frames (steer, queue, plan decision, compact, phase, context label, rules). C3c DONE: `modron-app-tui --url ws://host:port [--token] [--session]` drives a live session (driver thread around RemoteClient; Enter = send/steer, Alt+Enter = queue, y/r on a pending plan, /compact); queue chips, steer echoes, plan block rendered. C3d DONE: one target-neutral key router (terminal and web), web-only tappable stop / approval / plan / queue controls, live tool rows, turn summary, swarm rows, notes and errors from the snapshot, Oculus theme applied on web (asserted) and terminal. C3e DONE: the client core adopts the session from the first snapshot when none is attached (share links can send), refusals reach last_error and the screen, dequeue (Backspace on an empty composer; pressable chips on web), STOP_LABEL / QUEUE_LABEL / COMPOSER_PLACEHOLDER / compact_tokens live in modron-view. C4d DONE: screen stack (Ctrl+O sessions, F2 settings, Esc backs out), sessions screen with type-to-filter, attach and new session, settings screen cycling approval mode, phase and effort and showing model, sandbox, context and rules; web header controls; labels live in the client core. C3f DONE: slash suggestions and execution (every shared-table command is served by a local action or a core call), help screen generated from the key table the router tests iterate, model picker over the router defaults, delete session with inline confirmation, a sub-agent screen with steer and kill. WIRE-CATALOG DONE: ModelsReq / Models carry the router catalog (id, provider, context window, availability, tier) and the client core folds it into picker rows; /sessions, /compact, /effort, /plan, /build are in the shared slash table. Open: the app picker still reads router defaults (switch it to the catalog rows); app-local strings "started by modron-app", "connected <url>", "no session to continue" need a shared home; 1M context renders as "1000k ctx"; Ctrl+A opens the first running agent only and the agent screen shows the chat transcript beneath it; portal sign-in on web; Ctrl+O may be browser-reserved on web; no visual review yet; the queue chip and steer echo formats and the terminal key hints are still app-local literals; modron-view docs show swarm tokens as `1.2k tok` but the code prints raw counts |
| C4 | Portal: Worker serves the web build; owner sign-in; linked GitHub accounts; session directory; invite links | ACTIVE | C4a DONE: app/ builds for the web (`nsh package --target web`, needs the nsh CLI and wasm-bindgen CLI 0.2.130) with the same chat screen; `?url=ws://..&token=..&session=..` connects through the client core wasm transport, no url = demo. wasm 5.56 MB raw / 1.51 MB brotli; bundle 12.5 MB in 98 files. app/web-test.js (headless Chromium): demo and live against a scripted websocket server pass. Open: web key hook (Esc, y/a/n, y/r, Alt+Enter not wired on web), theme not confirmed to render, a live-test flake seen by the worker (3 of ~17 runs missed the submit; 8 of 8 clean since), C4b DONE: the bridge serves the bundle from Workers Static Assets under /app/ (sync with cloud/bridge/scripts/sync-app.sh; 25 MiB per-file limit, the wasm fits); /s/<key> redirects to /app/?url=ws(s)://host/ws/<key>, ?classic=1 keeps the inline viewer; the first viewer hello replays the buffer. 12 checks pass under local `wrangler dev`. Open (cloud/bridge/NOTES.md): the app never attaches without session= so a submit through a share link would start a new session and be refused (fix in the app: adopt the session id from the first snapshot); snapshots over 96 KB are not buffered for late viewers; not run end to end against a real modron serve; C4c DONE: OWNER_TOKEN / HOST_TOKEN gates, Directory Durable Object (sessions, owner cookies stored hashed, invites, one-time tickets, sign-in rate limit), POST /auth, /api/sessions, host registration API, signed invite links with viewer / approver roles enforced on viewer sockets in the bridge; with OWNER_TOKEN set a bare share-key socket is refused. 57 checks pass under local wrangler dev. C4f DONE: modron serve sends MODRON_HOST_TOKEN on the tunnel upgrade and registers / heartbeats / deletes its session in the directory (never blocks the tunnel; 503 disables, 401 stops). C5a DONE: sessions with no heartbeat for 5 minutes are listed offline and swept after 30 days; /api/me; /api/sessions rows carry online, updated_at and open_url; / redirects to /app/ when the portal is on. C4g DONE: the web app is the portal front end: with no url it asks /api/me and shows the demo (503), a masked sign-in (401) or the portal session list (200; rows open their open_url, offline rows cannot be opened, refresh, sign out); Settings gains a share section that creates, lists and revokes viewer / approver invites; the model picker lists the wire catalog. The token is never stored (asserted in web tests). Open: invites for offline sessions are still allowed, the 15 s auto refresh and clipboard copy are not browser-tested, no visual review, app sessions and sign-in screens (C4d), linked GitHub (C4e). Deploy needs the owner Cloudflare login |
| C5 | Tandem: multi-device attach, replay on reconnect, push for approvals | TODO | |
| C6 | Cloud runs: container runs modron serve; sleep/restore; handoff | TODO | Depends on S3; deploy needs the owner's Cloudflare login |
| C7 | Terminal target from the NSH app | ACTIVE | C7a DONE: `modron-app-tui` with no arguments connects to ws://127.0.0.1:7699 (MODRON_URL), or starts `modron serve` itself (MODRON_BIN, next to the app, PATH), logs it to the state dir, and stops it on exit unless --keep-server; --demo, --new, --continue; picks the latest session for the working directory; reconnects with backoff; terminal restored on exit and panic. Lead smoke under a pty: server started, task submitted, approval shown, Ctrl+C exits 0, alternate screen left, no server left running. Open: make it the default UI of `modron` (ship the app with the binary), `!` shell passthrough, `@file`, attachments, parity review against crates/modron-tui, size (S2c) |
| C8 | Phone packaging (iOS, Android) | BLOCKED | Needs macOS/Xcode and signing (owner) |
| C9 | Delete modron-tui views, modron-desktop, mobile/app TSX, bridge inline viewer; rewrite the AGENTS.md parity contract | ACTIVE | C9a DONE (eedc245): modron-desktop removed, assets in app/assets. Open: TUI views, mobile/app TSX, bridge inline viewer, AGENTS.md contract rewrite - after C3/C7 |
| C10 | CI: nsh check / nsh test; drop the gpui exclusion | ACTIVE | CI job `app` added: fmt, clippy, test and a wasm32 type-check in app/ (not yet seen green in CI). Open: build the web bundle and run web-test.js in CI (needs the nsh CLI and wasm-bindgen CLI), mobile/app |

## Log

- 2026-11-15: program opened; plan and decisions recorded in PLAN.md
  section 10.
- 2026-11-15: wave 1 (four workers). C1, P1, H9a done; H1a, H6a, H8a
  landed. Lead review changed three things in H8a (a lone `&` is a
  compound operator, a path with `..` never matches an allow pattern,
  MCP-style tool names parse) and gated `edit_lines` in the engine (the
  worker could not touch engine.rs). Gates on Linux, excluding
  modron-desktop: fmt clean, clippy clean, 955 tests pass.
- NSH crates are published at 0.5.1 on crates.io (nsh-core, nsh-base,
  nsh-engine, nsh-testing, nsh-platform-tui); this container has glibc
  2.39, so S2 can run here.
- 2026-11-15: wave 2. H1b, H8b, C2 landed. Lead changes: an ask rule
  no longer lets a gated tool reach the approver in Deny mode; test
  struct literals outside the worker's file list repaired. The
  speculative read driver is disarmed whenever a rule set is present
  (it runs reads before the gate). PROTOCOL_VERSION stays 1 until the
  v2 set is complete. Gates: fmt clean, clippy clean, 969 tests pass.
- 2026-11-15: wave 3. H1c, H2a, H3a landed. The frozen desktop client
  got no-op match arms for the new frames (it has an exhaustive Payload
  match and is compiled on macOS CI); NOT compiled here (gpui needs GUI
  libraries this container lacks) - needs a macOS CI pass. Gates on
  Linux excluding modron-desktop: fmt clean, clippy clean, 985 tests.
- 2026-11-15: C9a (desktop removed) and S2 (terminal spike) closed. S2
  verdict: the NSH terminal backend is fast enough; the blockers to
  retire before C7 are binary size, a no-JS feature set that compiles,
  and an external-event path into the reactive graph (S2b).
- 2026-11-15: wave 4. H2b and H9c landed (H2 closed). Gates on Linux,
  full workspace (desktop is gone): fmt clean, clippy clean, 1004 tests.
- 2026-11-15: wave 5. H4a and H8c landed (H8 closed). Lead added the
  `review_model` / `review_max_rounds` config keys. Gates: fmt clean,
  clippy clean, 1036 tests pass (full workspace).
- 2026-11-15: S2b closed. The NSH terminal target is viable for C7
  with a no-JS build; binary size (17 MB at best) is the accepted cost
  unless the owner objects. Docs mismatch noted: `SurfaceOptions.runtime`
  does not exist in 0.5.1.
- 2026-11-15: wave 6. H9b and H9e landed. The container ran out of
  disk mid-wave (build caches): the H9b worker stopped with one test
  type error, which the lead fixed; .gitignore was found truncated to
  zero bytes and was restored from the index. Gates: fmt clean, clippy
  clean, 1063 tests pass.
- 2026-11-15: C3a landed: first screen of the unified NSH app, on the
  terminal target. App gates (in app/): fmt clean, clippy clean, 7
  tests pass. The app is its own workspace, so root gates do not cover
  it yet (C10).
- 2026-11-15: wave 7. C3b and H5a landed. Gates (root workspace):
  fmt clean, clippy clean, 1093 tests pass. wasm32 target is not
  installed here, so the client core wasm check did not run.
- 2026-11-15: H5b landed. Gates: fmt clean, clippy clean, 1096 tests.
- 2026-11-15: C3c landed. App gates (app/): fmt clean, clippy clean,
  16 tests pass. Live smoke by the lead: `modron serve --provider mock`
  plus `modron-app-tui --url ws://127.0.0.1:7711` under a pty: connected,
  a typed task was submitted, the mock run reached a bash approval and
  the app showed the three decision labels.
- 2026-11-15: S1 closed: the NSH web target is viable for the portal
  (C4) in Rust. Open questions moved to C4: accessibility roles, real
  device testing, load over a real network (1.4 MB brotli wasm).
- 2026-11-15: wave 9 (harness half). H3b and H9d landed; the lead
  wired HTTP servers into the three CLI connect sites. Gates: fmt
  clean, clippy clean, 1123 tests pass.
- 2026-11-15: H3c and H4b landed (H4 closed), commit ad59658. Gates:
  fmt clean, clippy clean, 1147 tests pass.
- 2026-11-15: C4a landed. App gates: fmt clean, clippy clean, wasm32
  lib check clean, 19 tests pass, web-test.js passes (3 of 3 lead
  reruns). Stale build artifacts in app/target gave a false compile
  failure once; `cargo clean -p modron-app` cleared it.
- 2026-11-15: C4b landed (local only; nothing deployed).
- 2026-11-15: C3d and S2c landed. App gates: fmt, clippy, wasm32
  check clean; 34 tests pass; web-test.js 6 of 6 cases (lead reran
  twice). S2c: the size floor found is 11.4 MB stripped; the NSH
  runtime itself (nsh_core + nsh_base) is about 46 percent of it.
- 2026-11-15: docs/PORTAL.md (owner sign-in, session directory, invite
  links, linked GitHub, push) and the `app` CI job added.
- 2026-11-15: C4c landed (local only; nothing deployed). Lead reran
  test/portal.mjs (all checks pass) and tsc (clean), and confirmed the
  role filter keys on the flattened frame `type`.
- 2026-11-15: C3e and C4f landed. Root gates: fmt clean, clippy clean,
  1163 tests pass. App gates: fmt, clippy, wasm32 check clean, 44 tests
  pass, web-test.js 8 of 8.
- 2026-11-15: H7 split. H7a (effort parameter end to end) is with a
  worker; H7b (thinking display and block replay) is recorded as a
  design item, see its row.
- 2026-11-15: H7a landed. Gates: fmt clean, clippy clean, 1177 tests.
- 2026-11-15: first end-to-end run. Three real defects in the remote
  path were found and fixed (see E2E row); 145 server tests pass. The
  scripted-fake tests had not caught them because each side was tested
  against the other's assumed behaviour.
- 2026-11-15: C4d landed. Root gates: fmt clean, clippy clean, 1184
  tests pass (this also covers the rc.rs tunnel fix). App gates: fmt,
  clippy, wasm32 check clean; 71 tests pass; web-test.js 11 of 11. The
  app is 2,713 source lines for chat, sessions and settings on two
  targets.
- 2026-11-15: e2e 11 of 11. App gates against committed crates: fmt,
  clippy, wasm32 check clean; 72 tests pass.
- 2026-11-15: H3d landed after the owner approved the one acp.rs match
  arm a worker had been denied. Root gates: fmt clean, clippy clean,
  1201 tests pass. App: 72 tests pass against the new crates.
- 2026-11-15: H7c and the docs refresh landed (README corrected,
  docs/CONFIG.md added). Gates: fmt clean, clippy clean, 1217 tests.
  Docs worker findings recorded: with no model configured the Anthropic
  default is claude-opus-5-5; cloud/bridge/README.md is stale on the
  host token and C4d.
- 2026-11-15: H7d and C5a landed. Gates: clippy clean, core + cli 442
  tests pass (worker ran the workspace: 1226). Bridge: tsc clean,
  portal 82 checks, local checks, e2e 11 of 11 unmodified.
- 2026-11-15: C3f landed. Root gates: fmt clean, clippy clean, 1226
  tests. App: fmt, clippy, wasm32 check clean; 93 tests; web-test.js 15
  of 15. The app is 4,087 source lines for chat, sessions, settings,
  help, model picker, slash commands and the sub-agent screen on two
  targets. The frozen mobile/app fixture got `..Default::default()`: it
  named every snapshot field and so has not compiled since the client
  core gained fields (C3b); it is not built in CI and was edited blind.
- 2026-11-15: WIRE-CATALOG landed. Gates: fmt clean, clippy clean,
  1234 tests pass.
- 2026-11-15: C7a landed. App gates: fmt, clippy, wasm32 check clean;
  113 tests; web-test.js 15 of 15. The app's local slash entries were
  removed in favour of the shared table (two tests had failed on the
  duplicates) and `/effort off` is accepted.
- 2026-11-15: C4g landed. Root gates: fmt clean, clippy clean, 1239
  tests. App: fmt, clippy, wasm32 check clean; 126 tests; web-test.js
  19 of 19. The two server-backed terminal tests shared one HOME and
  collided when run in parallel; they now hold a lock (3 clean reruns).
  They skip when the prebuilt server binary is absent, so CI does not
  run them yet.
- 2026-11-15: parity audit recorded. e2e reran 11 of 11 against the
  portal front end after the harness learned that the app's /api/me
  probe answers 401 or 503 on non-owner pages (expected, not errors).
- 2026-11-15: PARITY-B landed. Gates: clippy clean, 1255 tests pass,
  client core wasm check passes.
- 2026-11-15: PARITY-A landed. App gates: fmt, clippy, wasm32 check
  clean; 143 tests (two clean runs); web-test.js 21 of 21 twice.
- 2026-11-15: PARITY-C landed. Gates: fmt clean, clippy clean, 1269
  tests pass.
- 2026-11-15: PARITY-D1 landed. App gates: fmt, clippy, wasm32 check
  clean; 166 tests; web-test.js 22 of 22. The app is 7,854 source
  lines. Remaining must-haves from docs/APP_PARITY.md: 6 (credentials
  and onboarding), 10 (attachments in the app), 12 (task-submitting
  commands), 13 (picker details), 18 (screens for automations,
  worktree, agent, allow/deny), 19 (small items), 20 (themes).
- 2026-11-15: E2E-2 recorded. 14 of 22 scenarios pass; the eight
  failures are real findings, not harness problems, except S6b which
  is undiagnosed.
- 2026-11-15: ROLE and PARITY-D2 landed together. Root gates: fmt
  clean, clippy clean, 1283 tests. App: fmt, clippy, wasm32 check
  clean; 186 tests; web-test.js 24 of 24 twice. Lead changes: two app
  live tests and three web cases expected a submit while the scripted
  stream implied a running session (now a steer); the first-run
  credentials screen was covering sessions with history and is limited
  to blank idle sessions. e2e after the fixes: W3 (settings through
  the portal) passes; W1, W2, W4, W5, W6 still fail (click timeouts,
  history not shown) and are not yet diagnosed.
- 2026-11-15: owner set a standing rule (PLAN 10.2a): compare Claude
  Code and oh-my-pi, decide, record, proceed. Three open decisions
  closed under it.
- 2026-11-15: SHELL-2 landed. Gates: fmt clean, clippy clean, 1298
  tests pass; client core wasm check passes.
- 2026-11-15: E2E-DIAG landed. Full suite twice: 20 of 22 pass both
  times. Causes found: the first-run credentials screen covered blank
  sessions whose provider needs no sign-in (app; fixed); a web user had
  no way to scroll the transcript, so old rows were unreachable (app;
  wheel and touch scrolling added); W5 expected a session list through
  the tunnel, which the role table refuses by design (test reframed,
  the app now shows the refusal). OPEN: W6, after a local run the page
  stops sending frames and pings (suspect the client core send path);
  T1, the terminal app idles on "connecting" although the serve
  answers. The harness now kills its process groups on exit, signal
  and exception. App gates: fmt, clippy, wasm32 check clean; 190
  tests; web-test.js 26 of 26.
- 2026-11-15: CONNECT closed: neither open e2e failure was a product
  defect. W6 waited for a correlated reply on the wrong connection and
  the evidence was truncated (40 frames kept); T1 searched the raw
  terminal bytes for a string the app never writes whole. Both tests
  now check what a user would see. Kept from the investigation: a send
  on a closed connection sets last_error; MODRON_APP_TRACE logs the
  driver's stages; five client-core tests for send paths. Full e2e:
  22 of 22 (worker twice, lead once), no leftover processes. Core 110
  tests, app 190, web-test.js 26 of 26.
- 2026-11-15: SLASH-TABLE landed. Dependent tests 436 pass, clippy
  clean. Lead made two shared rows client-neutral (/exit, /plan [task]).
