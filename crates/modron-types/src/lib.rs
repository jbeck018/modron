//! modron-types — the frozen session/transcript data types, split out of
//! `modron-core` (2026-11-10) so wasm targets (the mobile shell's web
//! build) can consume the shared view stack without the native-only
//! engine (tokio net/process/fs, reqwest). `modron-core` re-exports
//! every type here, so all `modron_core::X` paths keep working; new
//! client-side code should depend on this crate directly.

use serde::{Deserialize, Serialize};

/// Session identifier (uuid v4 at creation time).
pub type SessionId = uuid::Uuid;

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
    /// User-attached multimodal inputs (Phase 7): images and documents
    /// ride with the message; providers encode them per their wire format.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Provider-native blocks kept for replay (H7b-2): Anthropic thinking
    /// and redacted-thinking blocks, OpenAI Responses reasoning items. Only
    /// the provider and api that produced a block replays it; clients never
    /// see this field (the server strips it at the send boundary).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_blocks: Vec<ProviderBlock>,
}

/// One provider-native content block, stored verbatim (H7b-2). `payload` is
/// the provider's JSON exactly as received, replayed unchanged and never
/// interpreted by the harness. `index` is the block's position in the
/// provider's content list for the turn, so replay keeps the original order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderBlock {
    /// Provider id that produced the block (e.g. "anthropic", "openai").
    pub provider: String,
    /// Wire api of that provider (e.g. "messages", "responses").
    pub api: String,
    /// Provider block type (e.g. "thinking", "redacted_thinking", "reasoning").
    pub kind: String,
    pub index: u32,
    pub payload: serde_json::Value,
}

impl ProviderBlock {
    /// The one replay rule (H7b-2, PLAN 10.7 rule 3): a block goes back to a
    /// request only when the provider and api that produced it are the
    /// request's target. Every provider request builder filters through
    /// [`replayable_blocks`], which uses this predicate.
    pub fn replays_to(&self, provider: &str, api: &str) -> bool {
        self.provider == provider && self.api == api
    }
}

/// The blocks of `message` that may be replayed to `provider` / `api`, in
/// original order (ascending `index`, stable for equal indices).
pub fn replayable_blocks<'a>(
    message: &'a Message,
    provider: &str,
    api: &str,
) -> Vec<&'a ProviderBlock> {
    let mut blocks: Vec<&ProviderBlock> = message
        .provider_blocks
        .iter()
        .filter(|block| block.replays_to(provider, api))
        .collect();
    blocks.sort_by_key(|block| block.index);
    blocks
}

impl SessionEvent {
    /// Drop the provider-native blocks of a message before the event leaves
    /// the server (H7b-2, PLAN 10.7 rule 6). Clients never see
    /// `provider_blocks`: snapshots, live frames and ACP call this at the
    /// send boundary; the stored line keeps them.
    pub fn strip_provider_blocks(&mut self) {
        if let SessionEvent::Message { message, .. } = self {
            message.provider_blocks.clear();
        }
    }
}

/// A multimodal attachment on a user message (Phase 7).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attachment {
    /// An image, base64-encoded with its MIME type.
    Image {
        media_type: String,
        /// Raw base64 (no data: prefix).
        data: String,
    },
    /// A text document, extracted and inlined.
    Document {
        name: String,
        /// Extracted text (PDFs via pdftotext at the CLI layer, etc.).
        text: String,
    },
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            attachments: Vec::new(),
            provider_blocks: Vec::new(),
        }
    }
    /// A user message with multimodal attachments (Phase 7).
    pub fn user_with_attachments(content: impl Into<String>, attachments: Vec<Attachment>) -> Self {
        Self {
            attachments,
            ..Message::user(content)
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            attachments: Vec::new(),
            provider_blocks: Vec::new(),
        }
    }
    pub fn tool_result(call_id: impl Into<String>, outcome: &ToolOutcome) -> Self {
        Self {
            role: Role::Tool,
            content: outcome.content.clone(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            attachments: Vec::new(),
            provider_blocks: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
///
/// One meaning for every provider (E3):
/// - `input_tokens` is ALL prompt tokens billed for the request, INCLUDING
///   cache reads and cache writes. Providers whose wire format reports
///   cache counts separately (Anthropic: `input_tokens` +
///   `cache_read_input_tokens` + `cache_creation_input_tokens`) sum them
///   before filling this field.
/// - `cached_input_tokens` is the cache-READ portion of `input_tokens`
///   (always `<= input_tokens` for well-formed provider data).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// All prompt tokens for the request, including cache reads and writes.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The cache-read subset of `input_tokens`.
    pub cached_input_tokens: u64,
}

impl Usage {
    /// Budget-facing total: prompt (including cache) plus output tokens.
    /// Saturates instead of overflowing on hostile or corrupt counts.
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

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
    /// Structured extras the engine consumes (Phase 13 self-improving
    /// memory): `memory_search` reports `{"memory_hits": [id, …]}` and the
    /// engine folds the run's outcome back into those memories' ledger at
    /// turn end. Tools that have nothing to report leave it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            usage: None,
            meta: None,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            usage: None,
            meta: None,
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
    /// limited to the working directory + system temp dirs.
    Seatbelt,
    /// Linux bubblewrap (`bwrap`): same posture — network denied via
    /// unshare-net, writes limited to the workspace + /tmp via tmpfs
    /// overlays and read-only binds (Phase 9).
    Bubblewrap,
}

/// Streaming progress from a model turn (Phase-1b S1 lift, additive).
/// Deltas are strictly ordered; `Done` is final and sent only on success.
/// Defined here so the frozen `ModelProvider` trait can carry the streaming
/// method; `modron-providers` re-exports this type.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamDelta {
    /// A fragment of assistant text.
    Text(String),
    /// A fragment of extended-thinking / reasoning trace (never part of
    /// `TurnResponse`).
    Reasoning(String),
    /// A COMPLETE tool call item emitted mid-stream, before the turn's
    /// remaining items finish (multi-item streams complete items one at
    /// a time). The engine uses these to speculatively execute idempotent
    /// read tools while the rest of the turn still streams (oh-my-pi
    /// pattern); sinks never see this variant — the engine driver
    /// intercepts it.
    ToolCallReady(ToolCall),
    /// The stream terminated cleanly.
    Done,
}

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
    /// A shell command the user ran with `!` or `!!` (not a model tool
    /// call). A non-private entry joins the model context as a labelled user
    /// message. A `private` entry (`!!`) is shown in the transcript but never
    /// reaches the model, the compaction summary, or any other context
    /// builder. Additive variant: older logs carry such commands as `Task`
    /// text and load unchanged.
    UserShell {
        command: String,
        output: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        private: bool,
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
    /// Harness observability record (2026-11, additive): one per model
    /// turn (verifier/failure_label None), one per finish-gate run
    /// (verifier Some), and one terminal record carrying the run's stop
    /// label. Fields follow the "minimal trace record" literature (run
    /// id = the session, task id = the Task event, step index = turn):
    /// versions, prompt/config hash for prefix-stability auditing
    /// (cache discipline), latency, cost, verifier verdict, failure
    /// label. `None`-heavy records stay small; serde defaults keep old
    /// logs loadable.
    TurnTrace {
        turn: u32,
        harness_version: String,
        provider: String,
        model: String,
        /// sha256 over (system prompt bytes + tool-spec wire bytes).
        #[serde(default)]
        prompt_hash: Option<String>,
        #[serde(default)]
        latency_ms: Option<u64>,
        #[serde(default)]
        cost_usd: Option<f64>,
        #[serde(default)]
        usage: Option<Usage>,
        /// "pass" | "fail" — the finish gate's verification verdict.
        #[serde(default)]
        verifier: Option<String>,
        /// Terminal stop label for the run's final record:
        /// "max_turns", "spend_cap", "budget_exhausted", "end_turn".
        #[serde(default)]
        failure_label: Option<String>,
    },
    /// The standing plan (2026-11-11, the measurable-subtask primitive):
    /// the model decomposes a goal into subtasks with measurable done
    /// criteria via the `plan` tool; each write replaces the whole plan.
    /// Every turn's context re-injects the latest plan so the run stays
    /// on track; the finish gate and UIs read it too. Additive variant:
    /// older logs load unchanged.
    Plan {
        items: Vec<PlanItem>,
    },
    /// Pre-mutation file snapshot (Phase-1b C1, additive): captured before
    /// `write`/`edit` execute. Three states (D8):
    /// - `prior_content_b64: None, uncaptured: false` = the file did not
    ///   exist; rewind deletes it.
    /// - `prior_content_b64: Some(b64)` = the prior bytes; rewind restores.
    /// - `prior_content_b64: None, uncaptured: true` = the original was not
    ///   a regular file, exceeded the 256KB snapshot cap, or was unreadable;
    ///   rewind refuses and changes nothing.
    ///
    /// Bash mutations are a documented v1 limitation — use git.
    Checkpoint {
        path: String,
        prior_content_b64: Option<String>,
        event_index: u64,
        /// Absent in older logs: `#[serde(default)]` reads them as `false`.
        #[serde(default)]
        uncaptured: bool,
    },
    /// Independent reviewer verdict (H4a), appended once per reviewer round
    /// at normal run completion. `Skipped` records that no review ran
    /// (no diff, provider error, unparsable output). Additive variant: older
    /// logs without it load unchanged.
    Review {
        verdict: ReviewVerdict,
        #[serde(default)]
        findings: Vec<Finding>,
        model: String,
    },
    /// Ownership fact (C6c, additive): this host handed the session to another
    /// host with `modron handoff export`. Appended as the LAST line of the
    /// exporting host's log only; the bundle never carries it. While it is
    /// the last event the host refuses runs, compaction and `!` commands on
    /// the session (see `modron_core::handoff`). An import of a longer log
    /// that continues past this line clears it. `to` is the optional target
    /// host name; `carrier_ref` is the git ref the bundle was pushed to.
    /// Clients fold it as a transcript note and nothing else.
    HandedOff {
        #[serde(default)]
        to: Option<String>,
        at: chrono::DateTime<chrono::Utc>,
        carrier_ref: String,
    },
}

/// The reviewer's verdict for one review round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Pass,
    NeedsFix,
    Skipped,
}

/// Reviewer finding severity (P0 = most severe).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    P0,
    P1,
    P2,
    P3,
}

impl Severity {
    /// Upper-case wire label (`P0`..`P3`) for model-facing text.
    pub fn code(self) -> &'static str {
        match self {
            Severity::P0 => "P0",
            Severity::P1 => "P1",
            Severity::P2 => "P2",
            Severity::P3 => "P3",
        }
    }
}

/// One reviewer finding: a real defect the diff introduces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    pub text: String,
}

/// One measurable subtask of the standing plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    /// The subtask in one line (imperative).
    pub text: String,
    /// The measurable done criterion ("tests X pass", "curl returns 200").
    pub done_criteria: String,
    /// "pending" | "active" | "done" | "blocked".
    #[serde(default = "default_plan_status")]
    pub status: String,
}

fn default_plan_status() -> String {
    "pending".to_string()
}

/// The run's phase (H2a). `Build` is the normal mode. `Plan` is an
/// engine-enforced read-only phase: every approval-gated tool is refused
/// (even under ApproveAll or an allow rule), ungated read-only tools still
/// run, the finish gate is skipped, and every turn carries the current plan
/// plus a phase line as a transient tail message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Build,
    Plan,
}

/// Reasoning effort (H7a): how hard the model should think on a turn.
/// `None` on a request means the provider default (no field is sent).
/// Providers map the five levels onto their own wire vocabulary (see each
/// provider module); `label()` is the canonical user-facing word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    /// The canonical word: `low`, `medium`, `high`, `xhigh` or `max`.
    pub const fn label(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

impl std::str::FromStr for Effort {
    /// The message names the five accepted values.
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "xhigh" => Ok(Effort::Xhigh),
            "max" => Ok(Effort::Max),
            _ => Err(format!(
                "unknown effort `{raw}` (use low, medium, high, xhigh or max)"
            )),
        }
    }
}

/// Named model roles (H7c): one optional model id per kind of work, so a
/// run can pin different work to different models. `None` = the role is
/// unset and the router decides as before. Precedence (see the engine):
/// a pin (`--provider`, a persona's forced model) > a role > the router.
/// Model ids resolve through the run's selector provider, the same way the
/// escalation and reviewer turns do; an unresolvable id falls back to the
/// router with a warning.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRoles {
    /// Main-loop turns while the run phase is `plan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Default model for `delegate` children without a persona model.
    /// `explore` is not affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
    /// Independent reviewer pass model. Setting it enables one review round
    /// when no explicit reviewer is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critic: Option<String>,
    /// Compaction summary model (instead of the cheap-tier route).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl ModelRoles {
    /// True when no role is set (the router decides everything).
    pub fn is_empty(&self) -> bool {
        self.plan.is_none()
            && self.subagent.is_none()
            && self.critic.is_none()
            && self.summary.is_none()
    }
}

/// Lifecycle state of one sub-agent (PROTOCOL_V2 section 4, H3d). Snake case
/// on the wire. `Running` is the default, so older rows without the field
/// read as running until their finish arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    #[default]
    Running,
    /// The child's approval ask is pending with the parent's approver.
    WaitingApproval,
    Done,
    Failed,
    /// Stopped by a user `KillAgent`. Sticky: a later finish keeps it.
    Killed,
}

/// Tool-activity notification (Phase-1d T0, additive): fired around every
/// tool execution so UIs can render `● Bash …` lines live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolActivity {
    /// A tool call is about to execute. `call_id` is the model's tool-call
    /// id (`ToolCall.id`), `None` when the peer or provider supplied none;
    /// consumers pair a `Done` with its `Start` by `call_id` when present.
    Start {
        name: String,
        call_id: Option<String>,
    },
    /// The tool finished; `bytes` is the (already reduced) result size.
    /// `call_id` mirrors the matching `Start` (see above).
    Done {
        name: String,
        is_error: bool,
        bytes: usize,
        call_id: Option<String>,
    },
    /// A subagent began (Phase 5h): `label` is the persona or "explore".
    SubagentStarted { id: String, label: String },
    /// Subagent heartbeat (Phase 5h): emitted after each child turn so UIs
    /// can render live per-agent progress rows.
    SubagentProgress {
        id: String,
        label: String,
        turns: u32,
        tokens: u64,
        last_tool: Option<String>,
    },
    /// A subagent settled.
    SubagentFinished {
        id: String,
        label: String,
        tokens: u64,
        turns: u32,
        is_error: bool,
    },
    /// A subagent's state changed (H3d): `WaitingApproval` while its ask is
    /// pending, `Running` once answered, `Killed` just before a user stop's
    /// finish. Row UI effect: the swarm row's state word updates in place.
    SubagentState {
        id: String,
        label: String,
        state: AgentState,
    },
    /// A BACKGROUND subagent settled (H3d): `ok` is false for a failure and
    /// for a kill. `brief` is the first 200 chars of its result. Drives the
    /// `AgentFinished` push and its `Notify`; a foreground child never sends it.
    BackgroundFinished {
        id: String,
        label: String,
        ok: bool,
        brief: String,
    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thinking_block(provider: &str, api: &str, index: u32) -> ProviderBlock {
        ProviderBlock {
            provider: provider.into(),
            api: api.into(),
            kind: "thinking".into(),
            index,
            payload: json!({"type": "thinking", "thinking": "t", "signature": "S/+=="}),
        }
    }

    #[test]
    fn replay_keeps_only_the_producing_provider_and_api_in_order() {
        let mut message = Message::assistant("done");
        message.provider_blocks = vec![
            thinking_block("anthropic", "messages", 2),
            thinking_block("chatgpt", "responses", 0),
            thinking_block("anthropic", "messages", 0),
            thinking_block("anthropic", "legacy", 1),
        ];
        let kept = replayable_blocks(&message, "anthropic", "messages");
        assert_eq!(kept.iter().map(|b| b.index).collect::<Vec<_>>(), vec![0, 2]);
        assert!(replayable_blocks(&message, "openai-compat", "chat").is_empty());
        assert_eq!(replayable_blocks(&message, "chatgpt", "responses").len(), 1);
    }

    #[test]
    fn provider_blocks_are_stored_but_absent_when_empty() {
        let plain = serde_json::to_string(&Message::assistant("hi")).unwrap();
        assert!(!plain.contains("provider_blocks"), "{plain}");
        let mut with = Message::assistant("hi");
        with.provider_blocks = vec![thinking_block("anthropic", "messages", 0)];
        let line = serde_json::to_string(&SessionEvent::Message {
            parent_id: None,
            message: with,
        })
        .unwrap();
        assert!(line.contains("\"provider_blocks\""), "{line}");
        assert!(line.contains("S/+=="), "signature bytes stored: {line}");
    }

    /// The Message shape as it was before H7b-2 (no provider_blocks field).
    #[derive(Deserialize)]
    struct OldMessage {
        role: Role,
        content: String,
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
        #[serde(default)]
        tool_call_id: Option<String>,
        #[serde(default)]
        attachments: Vec<Attachment>,
    }

    #[test]
    fn a_line_written_with_blocks_parses_in_a_build_without_the_field() {
        // Forward compatibility: an old binary reads a new line, ignoring the
        // unknown field, and loses only the blocks.
        let mut with = Message::assistant("answer");
        with.provider_blocks = vec![thinking_block("anthropic", "messages", 0)];
        let line = serde_json::to_string(&with).unwrap();
        let old: OldMessage = serde_json::from_str(&line).expect("old shape reads new line");
        assert_eq!(old.content, "answer");
        assert_eq!(old.role, Role::Assistant);
        assert!(old.tool_calls.is_empty() && old.tool_call_id.is_none());
        assert!(old.attachments.is_empty());
    }

    #[test]
    fn strip_removes_blocks_from_the_wire_form_only() {
        let mut with = Message::assistant("answer");
        with.provider_blocks = vec![thinking_block("anthropic", "messages", 0)];
        let stored = SessionEvent::Message {
            parent_id: None,
            message: with,
        };
        let mut wire = stored.clone();
        wire.strip_provider_blocks();
        let wire_line = serde_json::to_string(&wire).unwrap();
        assert!(!wire_line.contains("provider_blocks"), "{wire_line}");
        let stored_line = serde_json::to_string(&stored).unwrap();
        assert!(stored_line.contains("provider_blocks"), "{stored_line}");
    }

    #[test]
    fn an_old_format_line_loads_with_no_blocks() {
        let line = r#"{"role":"assistant","content":"hi","tool_calls":[]}"#;
        let message: Message = serde_json::from_str(line).unwrap();
        assert!(message.provider_blocks.is_empty());
        let old_line =
            r#"{"type":"message","parent_id":null,"message":{"role":"assistant","content":"hi"}}"#;
        match serde_json::from_str::<SessionEvent>(old_line).unwrap() {
            SessionEvent::Message { message, .. } => assert!(message.provider_blocks.is_empty()),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// The session log format is the wire/replay contract: an event must
    /// round-trip without change (serde defaults keep old logs loadable).
    #[test]
    fn session_event_round_trips() {
        let event = SessionEvent::Message {
            parent_id: None,
            message: Message::assistant("hi"),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    #[test]
    fn usage_totals() {
        assert_eq!(Usage::default().total(), 0);
        assert_eq!(
            Usage {
                input_tokens: 2,
                output_tokens: 3,
                cached_input_tokens: 5,
            }
            .total(),
            5
        );
    }

    #[test]
    fn usage_total_saturates_instead_of_overflowing() {
        let usage = Usage {
            input_tokens: u64::MAX,
            output_tokens: 10,
            cached_input_tokens: 0,
        };
        assert_eq!(usage.total(), u64::MAX);
        let normal = Usage {
            input_tokens: 3,
            output_tokens: 4,
            cached_input_tokens: 1,
        };
        assert_eq!(normal.total(), 7);
    }
}
