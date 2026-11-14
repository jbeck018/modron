//! modron-core — frozen contracts for the Modron agent harness.
//!
//! This file is the single source of truth for cross-crate types and traits.
//! Worker agents implement *against* these contracts; they do not rewrite them.
//! See PLAN.md for architecture and LEDGER.md for the work ledger.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub mod auth;
pub mod b64;
pub mod bitmap_compact;
pub mod budget;
pub mod builtin_skills;
pub mod compact;
pub mod context;
pub mod engine;
pub mod goal;
pub mod hooks;
pub mod judge;
pub mod memory;
pub mod rewind;
pub mod sandbox;
/// Speculative mid-stream execution subsystem (private to the crate; the
/// engine's run loop is the only client).
pub(crate) mod speculative;
pub mod subagent;
pub mod testing;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config error: {0} — {1}")]
    Config(String, String),
    #[error("provider `{provider}` error: {message}")]
    Provider { provider: String, message: String },
    #[error("tool `{tool}` failed: {message}")]
    Tool { tool: String, message: String },
    #[error("budget exceeded: {0}")]
    Budget(String),
    #[error("session error: {0}")]
    Session(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

// ---------------------------------------------------------------------------
// Message / session data types
//
// The frozen data types (Role, Message, Attachment, ToolCall, StopReason,
// Usage, ToolSpec, ToolOutcome, SandboxPolicy, StreamDelta, SessionMeta,
// SessionEvent, ToolActivity, RunSummary) live in `modron-types` (split out
// 2026-11-10 so wasm targets can share the view stack without the
// native-only engine); re-exported here so every `modron_core::X` path
// keeps working.
// ---------------------------------------------------------------------------

pub use modron_types::{
    Attachment, Message, PlanItem, Role, RunSummary, SandboxPolicy, SessionEvent, SessionId,
    SessionMeta, StopReason, StreamDelta, ToolActivity, ToolCall, ToolOutcome, ToolSpec, Usage,
};

/// Context handed to a tool execution.
#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub working_dir: PathBuf,
    /// Sandbox policy the tool must honor for any subprocess it spawns
    /// (bash routes its shell through it; file tools don't spawn).
    pub sandbox: SandboxPolicy,
    /// Handle to the running engine (Phase 4): read-only subagent tools
    /// (`explore`) use it to spawn child agents. `None` in unit tests.
    pub engine: Option<Arc<Engine>>,
    /// The session the run executes in (2026-11-11): session-aware tools
    /// (`plan`) append events to the run's log. `None` in unit tests.
    pub session_id: Option<SessionId>,
}

impl ToolCtx {
    /// Unsandboxed context for a working dir (the common constructor).
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            sandbox: SandboxPolicy::Off,
            engine: None,
            session_id: None,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Execute the tool. Errors mean "the tool could not run" (bug/misuse);
    /// expected failures (e.g. grep no matches) are `ToolOutcome::err`.
    async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome>;
}

/// Files read this session (canonicalized paths), backing the hard
/// read-before-edit gate (Phase 13): `edit`/`write` refuse to touch an
/// existing file the run has not read first — blind edits are blocked.
/// Interior-mutable through `Arc` so every clone (and every `ToolCtx`
/// engine handle) shares one set.
#[derive(Clone, Default)]
pub struct ReadGate {
    read: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>,
}

impl ReadGate {
    /// Record a path as read (best-effort canonicalization).
    pub fn mark(&self, path: &std::path::Path) {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.read.lock().unwrap().insert(canonical);
    }
    /// Has this exact file been read this session?
    pub fn seen(&self, path: &std::path::Path) -> bool {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.read.lock().unwrap().contains(&canonical)
    }
    pub fn len(&self) -> usize {
        self.read.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.read.lock().unwrap().is_empty()
    }
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// Shared per-session read ledger (see [`ReadGate`]).
    pub read_gate: ReadGate,
    /// Tools hidden from the system prompt and surfaced on demand via
    /// `tool_search` (Phase 13 dynamic discovery): registered and callable
    /// as usual, just not paid for on every turn.
    on_demand: std::collections::HashSet<String>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let spec = tool.spec();
        self.tools.insert(spec.name.clone(), tool);
    }
    /// Mark a tool on-demand: hidden from the prompt, discoverable through
    /// `tool_search`. Unknown names are ignored (registration order races).
    pub fn mark_on_demand(&mut self, name: &str) {
        self.on_demand.insert(name.to_string());
    }
    /// Whether `name` is on-demand.
    pub fn is_on_demand(&self, name: &str) -> bool {
        self.on_demand.contains(name)
    }
    /// Specs for the SYSTEM PROMPT: everything except on-demand tools.
    pub fn specs_prompt(&self) -> Vec<ToolSpec> {
        // On-demand tools ride the wire schema as STUBS: declared (so
        // structured tool-calling providers accept calls to them) at
        // ~10 tokens each, with the full schema discoverable via
        // `tool_search`. Dropping them entirely would make them
        // uncallable on strict providers; full schemas would pay their
        // cost on every request.
        let mut v: Vec<_> = self
            .tools
            .values()
            .map(|t| {
                let spec = t.spec();
                if self.on_demand.contains(&spec.name) {
                    ToolSpec {
                        description: "on-demand tool — call tool_search for its full                                       schema and usage"
                            .into(),
                        parameters: serde_json::json!({"type": "object"}),
                        ..spec
                    }
                } else {
                    spec
                }
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut v: Vec<_> = self.tools.values().map(|t| t.spec()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

// ---------------------------------------------------------------------------
// Provider types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TurnRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

#[derive(Clone, Debug)]
pub struct TurnResponse {
    /// Concatenated assistant text for this turn ("" if only tool calls).
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub stop_reason: StopReason,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Stable identifier, e.g. "anthropic", "openai-compat", "mock".
    fn id(&self) -> &str;
    /// One model turn. Non-streaming for MVP; streaming is a later crate feature.
    /// Requests are shared by Arc: retries and escalation re-send the
    /// same turn without re-cloning the transcript (the request is
    /// immutable once built).
    async fn turn(&self, req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse>;
    /// Streaming variant (Phase-1b S1 lift, additive): sends `StreamDelta`s
    /// on `tx` while the turn runs and still returns the complete
    /// `TurnResponse`. Default: run [`Self::turn`] and emit one `Text` delta
    /// (mock providers and any provider without an SSE implementation get
    /// streaming-shaped behavior for free). No auto-retry — a retried stream
    /// would re-emit deltas.
    async fn turn_streaming(
        &self,
        req: std::sync::Arc<TurnRequest>,
        tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
    ) -> Result<TurnResponse> {
        let _ = &tx;
        let response = self.turn(req).await?;
        let _ = tx.send(StreamDelta::Text(response.text.clone()));
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Session store types
// ---------------------------------------------------------------------------

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, title: &str) -> Result<SessionMeta>;
    async fn append(&self, id: SessionId, event: &SessionEvent) -> Result<()>;
    /// Full event log for a session.
    async fn load(&self, id: SessionId) -> Result<Vec<SessionEvent>>;
    async fn list(&self) -> Result<Vec<SessionMeta>>;
    /// Remove a session entirely (Phase 6a: vacuum/restart support).
    /// Returns whether it existed. Default: unsupported.
    async fn delete(&self, id: SessionId) -> Result<bool> {
        let _ = id;
        Ok(false)
    }
    /// Fork a session: copy the full event log under a fresh id and return
    /// it (Phase 14 /fork — branch a conversation). The source is untouched;
    /// both branches then evolve independently. Default: event-by-event
    /// copy through `load` + `append`, which every store already supports.
    async fn fork(&self, from: SessionId) -> Result<SessionId> {
        let events = self.load(from).await?;
        let new = SessionId::new_v4();
        for event in &events {
            self.append(new, event).await?;
        }
        Ok(new)
    }
    /// Append many events under ONE meta write (fork of a large session:
    /// per-event appends meant O(n) tiny meta rewrites - cycle-2 audit
    /// H5). Default: loop [`Self::append`].
    async fn append_batch(&self, id: SessionId, events: &[SessionEvent]) -> Result<()> {
        for event in events {
            self.append(id, event).await?;
        }
        Ok(())
    }
    /// Load events while HOLDING the store's per-session append lock,
    /// returning the guard. `modron serve`'s attach needs "no append can
    /// interleave between reading the disk and reading the seq
    /// watermark" - under this guard the watermark read is exact.
    /// Default: plain `load` under an unlocked guard (stores whose loads
    /// are inherently serialized against appends need nothing more).
    async fn load_snapshot_locked(
        &self,
        id: SessionId,
    ) -> Result<(tokio::sync::OwnedMutexGuard<()>, Vec<SessionEvent>)> {
        let guard = Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
        let events = self.load(id).await?;
        Ok((guard, events))
    }
    /// Number of parsed events in a session, cheaply. Default: a full
    /// `load` (correct everywhere). Stores with an on-disk log override
    /// this when callers ask repeatedly — the engine's checkpoint path
    /// asks once per mutating tool call, and a full parse there taxes
    /// every write-heavy turn (2026-11-12 audit perf fix).
    async fn event_count(&self, id: SessionId) -> Result<u64> {
        Ok(self.load(id).await?.len() as u64)
    }
}

// ---------------------------------------------------------------------------
// Model selection (routing)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Chat,
    Search,
    Summarize,
    Plan,
    Edit,
}

#[derive(Clone, Debug)]
pub struct SelectRequest {
    pub task: String,
    pub action: ActionKind,
    /// Estimated context size for the coming turn.
    pub context_tokens: u64,
    /// Remaining USD budget for the run (Phase-3a R0, additive): set when a
    /// spend cap is configured; selectors may downgrade to stay inside it.
    pub budget_remaining_usd: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct ModelChoice {
    pub model: String,
    pub provider: String,
    /// Human-readable rationale; always populated (routing is auditable).
    pub reason: String,
    /// Whether the selector trusts this decision (Phase-3b M0, additive):
    /// `false` marks ambiguous calls that an async decision model (LLM
    /// judge, Kev/Jev) may second-guess. Default intent: `true`.
    pub confident: bool,
    /// The chosen model's context window (Phase 5i, additive): selectors
    /// fill it from the catalog so the engine can compact against the
    /// MODEL's window, not just the global cap. `None` = unknown.
    pub context_window: Option<u64>,
}

/// Synchronous, allocation-light, no I/O, <1ms. This is the seam where the
/// embedder router (Phase 3) and the Jev adapter plug in.
pub trait ModelSelector: Send + Sync {
    fn select(&self, req: &SelectRequest) -> ModelChoice;
}

/// Verdict of an async decision model (Phase-3b M0): which tier should take
/// the task. Router-not-judge — it classifies complexity, never quality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteVerdict {
    /// Mechanical work: search, read, list, summarize, look up.
    Mechanical,
    /// Judgment work: implement, fix, refactor, plan, design.
    Judgment,
}

/// When an async decision model is consulted (Phase-3b M0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeMode {
    /// Only when the sync selector marks its choice unconfident.
    #[default]
    Ambiguous,
    /// Every decision (decision models are ~100-500ms; still far cheaper
    /// than a wrong strong-tier call).
    Always,
}

/// A named agent persona (Phase 5c A2): reusable delegate configuration.
/// The CLI resolves `provider` from the model catalog at parse time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Async decision-model seam (Phase-3b M0): consulted by the engine AFTER
/// the sync selector. Implementations: an LLM judge over any
/// OpenAI-compatible provider, and the Kev/Jev SystemOne client — both in
/// `modron-providers`. Return `None` to keep the sync choice (parse
/// failure, low confidence, transport error).
#[async_trait]
pub trait AsyncRouter: Send + Sync {
    async fn route(&self, task: &str, action: ActionKind) -> Option<RouteVerdict>;
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Autocompaction policy (PLAN.md §4.10, LEDGER X0/X1). Additive amendment:
/// new struct plus a new `EngineConfig` field that defaults to these values.
///
/// When `enabled`, the engine may compact the transcript before a turn (never
/// the first): once the estimated context exceeds
/// `threshold_frac × max_input_tokens` and at least `min_interval_turns`
/// turns have passed since the last compaction, the older transcript is
/// summarized by a routed cheap-model call (`ActionKind::Summarize`), capped
/// at `summary_max_bytes`, while the `tail_messages` most recent transcript
/// items stay verbatim. Compact once, not continuously.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionPolicy {
    pub enabled: bool,
    /// Compaction trigger: estimated context > this fraction of
    /// `max_input_tokens`.
    pub threshold_frac: f64,
    /// Minimum main turns between compactions.
    pub min_interval_turns: u32,
    /// Recent transcript items kept verbatim after a compaction.
    pub tail_messages: usize,
    /// Hard byte cap on the stored summary (char-boundary truncation).
    pub summary_max_bytes: usize,
    /// Attach the pruned transcript as a pixel-font bitmap image alongside
    /// the summary (bitmap compaction, `bitmap_compact.rs`): vision-capable
    /// models read the full older conversation back from one image instead
    /// of a lossy re-summary. Kill-switch for configs that want summary
    /// text only; serde default keeps pre-field configs loading as `true`.
    #[serde(default = "default_bitmap_compaction")]
    pub bitmap_compaction: bool,
}

/// Serde default for [`CompactionPolicy::bitmap_compaction`] — on by design
/// (the bitmap is additive; the text summary is always kept either way).
fn default_bitmap_compaction() -> bool {
    true
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_frac: 0.8,
            min_interval_turns: 4,
            tail_messages: 6,
            summary_max_bytes: 2048,
            bitmap_compaction: true,
        }
    }
}

/// Tool-approval posture (Phase-1b, LEDGER A1). Gates the *mutating* tool
/// class (`bash`, `write`, `edit`) before execution; read-only tools
/// (`read`, `glob`, `grep`, `remember`) are never gated.
///
/// v1 semantics (interactive approve UI arrives with the TUI): [`ApprovalMode::Ask`]
/// behaves as Deny-with-message — in print mode there is no one to ask, so
/// the tool is blocked with an actionable hint naming `--approve-all`, and
/// the model adapts. Only an explicit override (`--approve-all`) executes
/// mutating tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Mutating tools are always blocked (the model sees the block and can
    /// adapt, e.g. propose read-only work).
    Deny,
    /// Block mutating tools with an actionable "how to allow" message.
    /// Default: safe in non-interactive print mode. Interactive asking is
    /// deferred to the TUI tranche.
    #[default]
    Ask,
    /// Mutating tools execute without prompting.
    ApproveAll,
}

/// Interactive approver for Ask-mode mutating tools (Phase-1c I2): given the
/// pending call, return `true` to execute it. The CLI wires a `/dev/tty`
/// y/N/a prompt; `None` keeps the block-and-explain behavior.
pub type Approver = dyn Fn(&ToolCall) -> bool + Send + Sync;

/// Live-delta sink for streaming turns (Phase-1c I1): called for each
/// [`StreamDelta`] as the provider emits it. Never blocks the engine; the
/// sink runs on a Tokio task, so keep it cheap and non-panicking.
pub type DeltaSink = dyn Fn(StreamDelta) + Send + Sync;

/// Sink for [`ToolActivity`] (see `EngineConfig::on_tool_event`).
pub type ToolActivitySink = dyn Fn(ToolActivity) + Send + Sync;

/// How the engine gates mutating tools ([`ApprovalMode`]) plus the
/// non-interactivity flag (set by the CLI so hooks/logic can tell a TTY run
/// from a scripted one; v1: informational).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPolicy {
    pub mode: ApprovalMode,
    pub non_interactive: bool,
    /// Tools explicitly allowed (bypass Ask mode; Phase 4). Deny wins.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tools always blocked, whatever the mode says (any tool class).
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Pricing hook for USD spend caps (Phase-1b A3): given a model name, return
/// `(input_usd_per_m, output_usd_per_m)`. The CLI wires the router catalog;
/// `None` from the lookup means "no price known" — that turn counts as $0
/// with a warning, never a made-up number.
pub type PriceLookup = dyn Fn(&str) -> Option<(f64, f64)> + Send + Sync;

#[derive(Clone)]
pub struct EngineConfig {
    pub working_dir: PathBuf,
    pub system: String,
    pub default_model: String,
    pub default_provider: String,
    pub max_turns: u32,
    /// Per-turn assembled-context cap (pre-flight estimate).
    pub max_input_tokens: u64,
    /// Cumulative token budget for a whole task run.
    pub max_total_tokens: u64,
    /// Autocompaction policy (defaults to enabled at 80% of the input cap).
    pub compaction: CompactionPolicy,
    /// Root of the per-workspace user memory (`~/.modron/memory`), Phase-1a
    /// lead amendment (additive, `None` by default). When set, the engine
    /// appends the workspace's user-memory section to the system prompt (see
    /// `context::load_user_memory`); `None` keeps runs memory-free (tests,
    /// embedding uses).
    pub memory_root: Option<std::path::PathBuf>,
    /// Tool-approval gate for mutating tools (Phase-1b A1, additive:
    /// default `Ask` + `non_interactive: false`, which in this tranche
    /// blocks-and-explains exactly like `Deny` — see [`ApprovalMode`]).
    pub approvals: ApprovalPolicy,
    /// USD spend cap for the run (Phase-1b A3, additive). Cost is computed
    /// per turn from provider-reported usage via [`Self::price_lookup`].
    /// `None` (default) = unlimited.
    pub max_cost_usd: Option<f64>,
    /// Per-model pricing source for [`Self::max_cost_usd`] (in/out $/M
    /// tokens). `None` = no pricing known; caps cannot trip (with a warning
    /// if a cap was requested).
    pub price_lookup: Option<Arc<PriceLookup>>,
    /// Lifecycle hooks around every tool call (Phase-1b H1, additive).
    /// `None` commands = no hooks, no behavior change.
    pub hooks: hooks::HookConfig,
    /// Resume transcript reconstruction from this event index onward
    /// (Phase-1b C2, additive): used by `modron rewind` — events before it
    /// are history, events after it stay in the append-only log but never
    /// re-enter context. `None` = full-log resume.
    pub resume_from_event: Option<u64>,
    /// End (exclusive) of the range a rewind abandoned, paired with
    /// `resume_from_event` as its start: events in `[cut, end)` never
    /// re-enter context, while events appended AFTER the rewind (index
    /// `>= end`) do. `None` with a cut set keeps the old meaning (everything
    /// from the cut onward is dropped).
    pub resume_skip_end: Option<u64>,
    /// Force one self-check turn before ending when no verify tool exists.
    pub self_check_on_finish: bool,
    /// Verify-failure escalation (additive, `None` by default): when the
    /// finish gate exhausts its feedback rounds (`MAX_VERIFY_ROUNDS` in
    /// `engine.rs` — verification still failing), the run gets ONE extra
    /// model turn on this model id (selector provider kept, the
    /// `resolve_provider` fallback ladder unchanged — the persona
    /// ForcedSelector path). The escalated answer replaces the run answer on
    /// success; any failure keeps the pre-escalation answer. Bound: one per
    /// run (a resume grants a fresh bound, like the gate rounds).
    pub escalate_on_check_failure: Option<String>,
    /// OS sandbox applied to mutating tool subprocesses (Phase-1b A2,
    /// additive; default [`SandboxPolicy::Off`]).
    pub sandbox: SandboxPolicy,
    /// Interactive approver for Ask mode (Phase-1c I2, additive). When set,
    /// each mutating tool call is offered to it before the block decision;
    /// `None` (default) keeps block-and-explain.
    pub approver: Option<Arc<Approver>>,
    /// Live streaming sink (Phase-1c I1, additive). When set, the engine
    /// drives providers through `turn_streaming` and forwards every delta;
    /// `None` uses the plain non-streaming path.
    pub on_delta: Option<Arc<DeltaSink>>,
    /// Tool-activity sink (Phase-1d T0, additive): fired around every tool
    /// execution. `None` = silent.
    pub on_tool_event: Option<Arc<ToolActivitySink>>,
    /// Async decision model (Phase-3b M0, additive): consulted per the
    /// JudgeMode when set. `None` = sync routing only.
    pub async_router: Option<(Arc<dyn AsyncRouter>, JudgeMode)>,
    /// Named personas available to the `delegate` tool (Phase 5c A2,
    /// additive). The CLI fills these from `[agents.*]` with catalog-resolved
    /// providers.
    pub personas: std::collections::BTreeMap<String, PersonaSpec>,
    /// Ordered per-tier fallback candidates (remote-control hardening,
    /// additive; `None` by default). Index 0 of each list is the tier's
    /// PRIMARY model; the rest are same-tier alternates on other
    /// providers, most-preferred first. When a turn fails with an
    /// AUTH-class provider error on a tier's primary (dead token, revoked
    /// grant), the engine retries the turn ONCE on the first candidate
    /// whose provider is registered — and keeps that model for the rest
    /// of the run. Set by the CLI from `derive_tier_candidates` in
    /// auto-routing mode only; explicit pins (`[router]` overrides,
    /// `--model`, `--provider`) leave it `None`, so an explicit choice is
    /// never silently swapped.
    pub tier_fallbacks: Option<Arc<TierFallbacks>>,
    /// Background-delegate registry (Phase 5i I2, additive): the `delegate`
    /// tool's detached children report into this map, and `delegate_status`
    /// reads it. Engine-held so it survives across chat turns.
    pub background_subagents:
        Arc<std::sync::Mutex<std::collections::HashMap<String, BackgroundDelegate>>>,
}

/// Per-tier ordered fallback candidates for [`EngineConfig::tier_fallbacks`].
#[derive(Clone, Debug)]
pub struct TierFallbacks {
    /// `(model, provider)` pairs, the tier's primary first.
    pub cheap: Vec<(String, String)>,
    /// `(model, provider)` pairs, the tier's primary first.
    pub strong: Vec<(String, String)>,
}

/// A detached subagent's state (Phase 5i I2).
#[derive(Clone, Debug)]
pub struct BackgroundDelegate {
    pub label: String,
    pub status: BackgroundStatus,
}

#[derive(Clone, Debug, Default)]
pub enum BackgroundStatus {
    #[default]
    Running,
    Done {
        brief: String,
        tokens: u64,
        turns: u32,
        is_error: bool,
    },
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            working_dir: PathBuf::from("."),
            system: String::new(),
            default_model: "claude-sonnet-5-5".to_string(),
            default_provider: "anthropic".to_string(),
            max_turns: 40,
            max_input_tokens: 180_000,
            max_total_tokens: 2_000_000,
            compaction: CompactionPolicy::default(),
            memory_root: None,
            approvals: ApprovalPolicy::default(),
            max_cost_usd: None,
            price_lookup: None,
            hooks: hooks::HookConfig::default(),
            resume_from_event: None,
            resume_skip_end: None,
            self_check_on_finish: false,
            escalate_on_check_failure: None,
            sandbox: SandboxPolicy::Off,
            approver: None,
            on_delta: None,
            on_tool_event: None,
            async_router: None,
            personas: Default::default(),
            tier_fallbacks: None,
            background_subagents: Arc::new(std::sync::Mutex::new(Default::default())),
        }
    }
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `price_lookup` is an unnameable closure type — summarize it rather
        // than derive Debug (which cannot hold `Arc<dyn Fn>`.
        f.debug_struct("EngineConfig")
            .field("working_dir", &self.working_dir)
            .field("default_model", &self.default_model)
            .field("default_provider", &self.default_provider)
            .field("max_turns", &self.max_turns)
            .field("max_input_tokens", &self.max_input_tokens)
            .field("max_total_tokens", &self.max_total_tokens)
            .field("compaction", &self.compaction)
            .field("memory_root", &self.memory_root)
            .field("approvals", &self.approvals)
            .field("max_cost_usd", &self.max_cost_usd)
            .field("price_lookup", &self.price_lookup.is_some())
            .field("hooks", &self.hooks)
            .field("resume_from_event", &self.resume_from_event)
            .field("resume_skip_end", &self.resume_skip_end)
            .field("escalate_on_check_failure", &self.escalate_on_check_failure)
            .field("sandbox", &self.sandbox)
            .field("approver", &self.approver.is_some())
            .field("on_delta", &self.on_delta.is_some())
            .field("on_tool_event", &self.on_tool_event.is_some())
            .field("async_router", &self.async_router.is_some())
            .field("personas", &self.personas.keys().collect::<Vec<_>>())
            .field("tier_fallbacks", &self.tier_fallbacks.is_some())
            .field(
                "background_subagents",
                &self.background_subagents.lock().unwrap().len(),
            )
            .finish()
    }
}

#[derive(Clone)]
pub struct Engine {
    pub selector: Arc<dyn ModelSelector>,
    pub providers: HashMap<String, Arc<dyn ModelProvider>>,
    pub tools: Arc<ToolRegistry>,
    pub store: Arc<dyn SessionStore>,
    pub config: EngineConfig,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("tools", &self.tools.specs().len())
            .field("config", &self.config)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Token estimation (pre-flight only; provider usage is ground truth)
// ---------------------------------------------------------------------------

/// ~4 chars/token heuristic. Good enough for budget pre-flight checks.
pub fn estimate_tokens(s: &str) -> u64 {
    (s.len() as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_is_sane() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens(&"x".repeat(4000)), 1000);
    }

    #[test]
    fn registry_roundtrip() {
        #[derive(Default)]
        struct Dummy;
        #[async_trait]
        impl Tool for Dummy {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "dummy".into(),
                    description: "d".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolCtx,
            ) -> Result<ToolOutcome> {
                Ok(ToolOutcome::ok("hi"))
            }
        }
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Dummy));
        assert_eq!(reg.specs().len(), 1);
        assert!(reg.get("dummy").is_some());
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn prompt_prefix_is_wire_stable_across_registries() {
        // Cache discipline: provider prompt caches key on the byte prefix
        // of (system + tools). Tool specs must serialize IDENTICALLY
        // regardless of registration order — this test pins that property
        // (a HashMap-iteration ordering regression would bust every
        // provider's cache on every turn).
        #[derive(Default, Clone)]
        struct Named(&'static str);
        #[async_trait]
        impl Tool for Named {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: self.0.into(),
                    description: format!("tool {}", self.0),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {"x": {"type": "string"}}
                    }),
                }
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolCtx,
            ) -> Result<ToolOutcome> {
                Ok(ToolOutcome::ok("hi"))
            }
        }
        let names = ["zeta", "alpha", "mid", "beta"];
        let mut a = ToolRegistry::new();
        for n in names {
            a.register(Arc::new(Named(n)));
        }
        let mut b = ToolRegistry::new();
        for n in names.iter().rev() {
            b.register(Arc::new(Named(n)));
        }
        let wire_a = serde_json::to_string(&a.specs_prompt()).unwrap();
        let wire_b = serde_json::to_string(&b.specs_prompt()).unwrap();
        assert_eq!(wire_a, wire_b, "tool spec wire order must be deterministic");
        let specs: Vec<String> = a.specs_prompt().iter().map(|s| s.name.clone()).collect();
        assert_eq!(specs, vec!["alpha", "beta", "mid", "zeta"]);
    }
}
