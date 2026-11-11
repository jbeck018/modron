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
        }
    }
    pub fn tool_result(call_id: impl Into<String>, outcome: &ToolOutcome) -> Self {
        Self {
            role: Role::Tool,
            content: outcome.content.clone(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            attachments: Vec::new(),
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
    /// `write`/`edit` execute. `prior_content_b64: None` means the file was
    /// new (rewind deletes it) or exceeded the 256KB snapshot cap. Bash
    /// mutations are a documented v1 limitation — use git.
    Checkpoint {
        path: String,
        prior_content_b64: Option<String>,
        event_index: u64,
    },
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
}
