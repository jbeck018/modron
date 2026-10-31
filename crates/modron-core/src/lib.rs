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
use uuid::Uuid;

pub mod b64;
pub mod budget;
pub mod compact;
pub mod context;
pub mod engine;
pub mod hooks;
pub mod judge;
pub mod memory;
pub mod rewind;
pub mod sandbox;
pub mod subagent;
pub mod testing;

pub type Result<T> = std::result::Result<T, Error>;

pub type SessionId = Uuid;

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
// Message types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// One transcript message. Assistant messages may carry tool calls; tool
/// results are their own message with `tool_call_id` set and role `Tool`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn tool_result(call_id: impl Into<String>, outcome: &ToolOutcome) -> Self {
        Self {
            role: Role::Tool,
            content: outcome.content.clone(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments as parsed JSON (providers return a string; we parse eagerly).
    pub arguments: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

/// Provider-reported usage is ground truth for all accounting. Never replace
/// it with estimates in recorded data; estimates are pre-flight only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

// ---------------------------------------------------------------------------
// Tool types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the `parameters` the model should send.
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
    /// Usage spent BEHIND the tool's interface (Phase 5c A2: subagent
    /// delegates, explore). The engine folds this into the budget governor
    /// and usage events so delegated work never escapes honest accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            usage: None,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            usage: None,
        }
    }
}

/// Filesystem/network sandbox for tool execution (Phase-1b A2, additive).
/// The approval gate (A1) decides *whether* a tool may run; the sandbox
/// constrains *what a run can touch* even when approved. Defense in depth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPolicy {
    /// No OS sandbox; approval gates are the only control (default).
    #[default]
    Off,
    /// macOS Seatbelt via `sandbox-exec`: network denied, file writes
    /// limited to the working directory + system temp dirs. The Linux
    /// (bubblewrap/seccomp) port is Phase-1 residual work.
    Seatbelt,
}

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
}

impl ToolCtx {
    /// Unsandboxed context for a working dir (the common constructor).
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            sandbox: SandboxPolicy::Off,
            engine: None,
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

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let spec = tool.spec();
        self.tools.insert(spec.name.clone(), tool);
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

/// Streaming progress from a model turn (Phase-1b S1 lift, additive).
/// Deltas are strictly ordered; `Done` is final and sent only on success.
/// Defined here so the frozen `ModelProvider` trait can carry the streaming
/// method; `modron-providers` re-exports this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamDelta {
    /// A fragment of assistant text.
    Text(String),
    /// A fragment of extended-thinking / reasoning trace (never part of
    /// `TurnResponse`).
    Reasoning(String),
    /// The stream terminated cleanly.
    Done,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Stable identifier, e.g. "anthropic", "openai-compat", "mock".
    fn id(&self) -> &str;
    /// One model turn. Non-streaming for MVP; streaming is a later crate feature.
    async fn turn(&self, req: TurnRequest) -> Result<TurnResponse>;
    /// Streaming variant (Phase-1b S1 lift, additive): sends `StreamDelta`s
    /// on `tx` while the turn runs and still returns the complete
    /// `TurnResponse`. Default: run [`Self::turn`] and emit one `Text` delta
    /// (mock providers and any provider without an SSE implementation get
    /// streaming-shaped behavior for free). No auto-retry — a retried stream
    /// would re-emit deltas.
    async fn turn_streaming(
        &self,
        req: TurnRequest,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub title: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub message_count: u32,
}

/// Append-only session events. `parent_id` (index of the event this one
/// branches from) enables session trees; MVP may always use `None`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    Task {
        text: String,
    },
    Message {
        parent_id: Option<u64>,
        message: Message,
    },
    ToolResult {
        call_id: String,
        outcome: ToolOutcome,
    },
    ModelSelected {
        model: String,
        reason: String,
        /// Action kind the selector saw (Phase-3a R5, additive; serde default
        /// keeps Phase-0 logs loadable).
        #[serde(default)]
        action: Option<String>,
        /// Estimated context tokens at decision time (Phase-3a R5).
        #[serde(default)]
        context_tokens: Option<u64>,
    },
    Usage {
        model: String,
        usage: Usage,
    },
    /// Emitted when the engine compacts the transcript (PLAN.md §4.10).
    /// `summary` is the compact state that replaces everything older than the
    /// recent tail; `summarized_events` counts the transcript items
    /// (`Task`/`Message`/`ToolResult`) that went into the summary. Additive
    /// variant: Phase-0 logs without it still load unchanged.
    Compaction {
        summary: String,
        summarized_events: u32,
    },
    /// Pre-mutation file snapshot (Phase-1b C1, additive): captured before
    /// `write`/`edit` execute. `prior_content_b64: None` means the file was
    /// new (rewind deletes it) or exceeded the 256KB snapshot cap. Bash
    /// mutations are a documented v1 limitation — use git.
    Checkpoint {
        path: String,
        prior_content_b64: Option<String>,
        event_index: u64,
    },
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, title: &str) -> Result<SessionMeta>;
    async fn append(&self, id: SessionId, event: &SessionEvent) -> Result<()>;
    /// Full event log for a session.
    async fn load(&self, id: SessionId) -> Result<Vec<SessionEvent>>;
    async fn list(&self) -> Result<Vec<SessionMeta>>;
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
#[derive(Clone, Debug, PartialEq)]
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
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_frac: 0.8,
            min_interval_turns: 4,
            tail_messages: 6,
            summary_max_bytes: 2048,
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

/// Tool-activity notification (Phase-1d T0, additive): fired around every
/// tool execution so UIs can render `● Bash …` lines live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolActivity {
    /// A tool call is about to execute.
    Start { name: String },
    /// The tool finished; `bytes` is the (already reduced) result size.
    Done {
        name: String,
        is_error: bool,
        bytes: usize,
    },
}

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
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            working_dir: PathBuf::from("."),
            system: String::new(),
            default_model: "claude-sonnet-4-5".to_string(),
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
            sandbox: SandboxPolicy::Off,
            approver: None,
            on_delta: None,
            on_tool_event: None,
            async_router: None,
            personas: Default::default(),
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
            .field("sandbox", &self.sandbox)
            .field("approver", &self.approver.is_some())
            .field("on_delta", &self.on_delta.is_some())
            .field("on_tool_event", &self.on_tool_event.is_some())
            .field("async_router", &self.async_router.is_some())
            .field("personas", &self.personas.keys().collect::<Vec<_>>())
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

#[derive(Clone, Debug)]
pub struct RunSummary {
    pub session_id: SessionId,
    pub answer: String,
    pub turns: u32,
    pub tool_calls: u32,
    pub usage: Usage,
    pub models_used: Vec<String>,
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
}
