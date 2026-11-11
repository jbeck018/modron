//! modron-protocol — the remote-control wire vocabulary (Phase 1).
//!
//! One [`Frame`] type in both directions: client commands and server
//! events ride the same envelope, tagged `type` in snake_case, with an
//! optional request `id` for correlation and a monotonic `seq` for server
//! stream ordering. Framing is transport-owned: NDJSON lines on the Unix
//! socket, one JSON document per WebSocket message remotely. JSON stays
//! the wire format for v1 (debuggable, adequate for text streams); a
//! binary codec can replace framing later without touching these types.
//!
//! Wire mirrors ([`WireDelta`], [`WireToolActivity`], [`WireRunSummary`])
//! exist because the frozen modron-core equivalents predate serde and must
//! not change; conversions are total except [`WireDelta`] (the engine
//! never lets `ToolCallReady` reach sinks, so it has no wire form).
//! Display-shaped payloads reuse [`modron_view`] types (`ToolEvent`) so
//! every client groups identically.

use modron_types::{
    Attachment, RunSummary, SandboxPolicy, SessionEvent, StreamDelta, ToolActivity, ToolCall, Usage,
};
use modron_view::ToolEvent;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Bumped on incompatible wire changes; the `hello`/`welcome` handshake
/// compares it.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default TCP port for the remote WebSocket listener (loopback by
/// default; TLS terminates at tailscale/cloudflared in front of it).
pub const DEFAULT_PORT: u16 = 7699;

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// One protocol frame. `id` correlates a client request with its direct
/// response (`pong`, `sessions`, `error`); server-initiated event frames
/// leave it `None`. `session` scopes the frame to a session when one
/// applies. `seq` is the server's per-session event counter: gaps mean the
/// client missed frames and should re-`attach` from its `last_seq`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Uuid>,
    #[serde(default)]
    pub seq: u64,
    #[serde(flatten)]
    pub payload: Payload,
}

impl Frame {
    /// A client request frame (correlated by id, seq always 0).
    pub fn request(id: u64, payload: Payload) -> Self {
        Self {
            id: Some(id),
            session: None,
            seq: 0,
            payload,
        }
    }

    /// A server event frame (ordered by seq, uncorrelated).
    pub fn event(session: Option<Uuid>, seq: u64, payload: Payload) -> Self {
        Self {
            id: None,
            session,
            seq,
            payload,
        }
    }
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Payload {
    // ---- client -> server ----
    /// First frame on a connection. `token` authenticates remote
    /// connections (loopback/Unix-socket peers may omit it).
    Hello {
        client: String,
        protocol: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// Subscribe to a session's events. `last_seq` = the client's last
    /// seen seq (0 = full replay); the server answers with `snapshot` and
    /// then live frames.
    Attach {
        session_id: Uuid,
        #[serde(default)]
        last_seq: u64,
    },
    /// Stop receiving a session's events (the session keeps running).
    Detach {},
    /// Submit a task. `session_id: None` starts a new session; the
    /// server echoes the assigned id on subsequent frames.
    Submit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<Uuid>,
        task: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// Abort the session's active run (JoinHandle-abort parity with the
    /// TUI's Esc).
    Interrupt { session_id: Uuid },
    /// Answer a pending approval (first responder wins).
    Approve {
        request_id: String,
        decision: Decision,
    },
    /// Mutate session/engine config for future turns.
    SetConfig { patch: ConfigPatch },
    /// Fork a session: copy its log under a fresh id (engine `fork`).
    Fork { session_id: Uuid },
    /// Create an empty session without submitting a task.
    NewSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// List sessions (answered by `sessions`).
    ListSessions {},
    /// Permanently delete a session (store-level). Refused while the
    /// session's run is active; answered by a correlated `note`.
    DeleteSession { session_id: Uuid },
    /// Create a task: a new session bound to a fresh git worktree and
    /// branch off `base` (default HEAD of the server's repo). Answered by
    /// a correlated `task_created`.
    CreateTask {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
    /// Diff/branch status of a task session. Answered by `task_status`.
    GetTaskStatus { session_id: Uuid },
    /// Merge a task's branch back into the base it branched from
    /// (review-first: a conflicted merge aborts cleanly). Answered by
    /// `task_merged`.
    MergeTask {
        session_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// List automations (answered by `automations`).
    AutomationsList {},
    /// Add an automation: a prompt on an interval.
    AutomationAdd {
        name: String,
        prompt: String,
        interval_minutes: u32,
    },
    /// Remove an automation by name. Both mutations answer with the
    /// refreshed `automations` list.
    AutomationRemove { name: String },
    /// Open a remote-control tunnel for a session through a relay bridge
    /// (a deployed cloud/bridge worker). `bridge_url` is the relay's
    /// base (scheme + host). Answered by `remote_control_started` with
    /// the share URL to open on any device.
    RemoteControl {
        session_id: Uuid,
        bridge_url: String,
    },
    /// Publish an assistant answer as a shareable web page. Answered by
    /// `page_published` with the share URL.
    PublishPage {
        session_id: Uuid,
        title: String,
        content_md: String,
    },
    /// Provider auth status rows (answered by `auth_providers`).

    /// Persist router defaults into the server's config.toml and apply
    /// them to future sessions. Answered by a correlated `note`.
    PersistRouter {
        cheap: String,
        strong: String,
        cheap_fallback: Option<String>,
        strong_fallback: Option<String>,
    },
    /// Liveness check (answered by `pong`).
    Ping {},

    // ---- server -> client ----
    /// Handshake reply.
    Welcome {
        server_version: String,
        protocol: u32,
    },
    /// Full or incremental catch-up state for an `attach`: replayable
    /// persisted events plus the live (non-persisted) view.
    Snapshot {
        session_id: Uuid,
        last_seq: u64,
        events: Vec<SessionEvent>,
        live: LiveState,
    },
    /// One persisted-transcript event, replayed or appended live.
    Event { event: SessionEvent },
    /// A streaming delta (text or reasoning; never tool-call items).
    Delta { delta: WireDelta },
    /// Tool or subagent activity.
    Tool { activity: WireToolActivity },
    /// A mutating tool call awaits a decision; all attached clients see it.
    ApprovalRequested { request_id: String, call: ToolCall },
    /// The decision landed (echoed to every client).
    ApprovalResolved {
        request_id: String,
        decision: Decision,
    },
    /// Config mutated (echo of the applied patch).
    ConfigChanged { patch: ConfigPatch },
    /// Answer to `list_sessions`.
    Sessions { sessions: Vec<SessionMetaWire> },
    /// Answer to `create_task`: the new session runs inside a dedicated
    /// worktree on its own branch.
    TaskCreated {
        session_id: Uuid,
        worktree: String,
        branch: String,
    },
    /// Answer to `task_status`: per-task diff/branch state.
    TaskStatus {
        session_id: Uuid,
        worktree: String,
        branch: String,
        running: bool,
        ahead: u64,
        files: Vec<TaskFileStat>,
    },
    /// Answer to `merge_task`: conflicted merges abort cleanly (ok=false
    /// with the conflicting paths) and the base stays untouched.
    TaskMerged {
        session_id: Uuid,
        ok: bool,
        summary: String,
    },
    /// Answer to `remote_control`: the tunnel is up; open `url` (web
    /// viewer) on any device to watch and drive the session.
    RemoteControlStarted { url: String },
    /// Answer to `automations_list` and both mutations.
    Automations { rows: Vec<AutomationWire> },
    /// Answer to `publish_page`: the share URL of the rendered page.
    PagePublished { url: String },
    /// Answer to `auth_providers`: one row per registry provider.
    AuthProviders { rows: Vec<AuthProviderWire> },
    /// A run completed.
    TurnDone { summary: WireRunSummary },
    /// A run errored.
    TurnFailed { error: String },
    /// A run was aborted (interrupt).
    TurnAborted {},
    /// OAuth sign-in progress (server-side flows stream status to remote
    /// clients: waiting URL, device code, outcome).
    OAuthProgress {
        provider: String,
        stage: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_code: Option<String>,
    },
    /// An informational toast/scrollback note (slash-command results).
    Note { text: String },
    /// Answer to `ping`.
    Pong {},
    /// A request failed (echoes the request `id`).
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Shared payload types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Execute this call once.
    AllowOnce,
    /// Execute this call and every later mutating call this run (the
    /// TUI's sticky `a`).
    AllowAll,
    Deny,
}

/// Config mutations a client may request (all optional; `None` = leave).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxPolicy>,
    /// `ask` / `yolo` / `deny` (the /mode command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<ApprovalModeWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
    /// Persona switch (the /agent command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// Standing goal prepended to submitted tasks (the /goal command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
}

/// The approval-mode knob (`ask`/`yolo`/`deny`), mirrored because the core
/// enum's serde names are fixed by config files (`approve_all`, not
/// `yolo`) and the wire uses the TUI's user-facing vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalModeWire {
    Deny,
    Ask,
    ApproveAll,
}

/// Session list row (mirror of core `SessionMeta`, which is serde-stable —
/// re-declared so the protocol owns its wire shape).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMetaWire {
    pub id: Uuid,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u32,
    /// Set when the session is a worktree task: the task branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_branch: Option<String>,
    /// Task state as the server sees it (list-time snapshot): "running"
    /// or "ready" (has commits ahead of base).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_state: Option<String>,
    /// The folder the session runs in (the server's working dir, or a
    /// task's worktree). Clients group the session list by it; absent on
    /// older servers (pre-grouping wire snapshots).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// One changed file in a task's diff (from `git diff --numstat`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFileStat {
    pub path: String,
    pub added: u64,
    pub deleted: u64,
}

/// One scheduled automation (wire row).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationWire {
    pub name: String,
    pub prompt: String,
    pub interval_minutes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<String>,
}

/// One provider row for the onboarding/settings surfaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthProviderWire {
    pub id: String,
    pub label: String,
    /// "api key" | "subscription (oauth)" | "not configured"
    pub status: String,
    pub oauth_capable: bool,
}

/// The live (non-persisted) half of a snapshot: what a fresh attach needs
/// to render the in-flight state a late joiner would otherwise miss.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveState {
    /// A run is executing right now.
    pub running: bool,
    /// In-flight assistant text buffer (committed on turn end).
    pub streaming_text: String,
    /// In-flight reasoning buffer (rendered only when shown).
    pub streaming_reasoning: String,
    /// Grouped per-turn tool view (the expandable panel's content).
    pub turn_tools: Vec<ToolEvent>,
    /// Live swarm rows.
    pub subagents: Vec<WireSwarmRow>,
    /// The approval awaiting a decision, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<PendingApproval>,
    /// Cumulative usage across the session.
    pub cumulative_usage: Usage,
    /// Current model/provider labels.
    pub model: String,
    pub provider: String,
    /// Standing goal (the /goal command).
    pub goal: String,
}

/// An approval awaiting a decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub request_id: String,
    pub call: ToolCall,
}

// ---------------------------------------------------------------------------
// Wire mirrors of non-serde core types
// ---------------------------------------------------------------------------

/// Mirror of core `StreamDelta` minus `ToolCallReady` (engine-internal;
/// sinks never see it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireDelta {
    Text { text: String },
    Reasoning { text: String },
    Done,
}

impl WireDelta {
    /// The wire form of a sink-visible delta; `ToolCallReady` -> `None`.
    pub fn from_core(delta: &StreamDelta) -> Option<Self> {
        match delta {
            StreamDelta::Text(text) => Some(Self::Text { text: text.clone() }),
            StreamDelta::Reasoning(text) => Some(Self::Reasoning { text: text.clone() }),
            StreamDelta::ToolCallReady(_) => None,
            StreamDelta::Done => Some(Self::Done),
        }
    }

    /// Back into a core delta (total).
    pub fn to_core(&self) -> StreamDelta {
        match self {
            Self::Text { text } => StreamDelta::Text(text.clone()),
            Self::Reasoning { text } => StreamDelta::Reasoning(text.clone()),
            Self::Done => StreamDelta::Done,
        }
    }
}

/// Mirror of core `ToolActivity`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireToolActivity {
    Start {
        name: String,
    },
    Done {
        name: String,
        is_error: bool,
        bytes: u64,
    },
    SubagentStarted {
        id: String,
        label: String,
    },
    SubagentProgress {
        id: String,
        label: String,
        turns: u32,
        tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_tool: Option<String>,
    },
    SubagentFinished {
        id: String,
        label: String,
        tokens: u64,
        turns: u32,
        is_error: bool,
    },
}

impl From<&ToolActivity> for WireToolActivity {
    fn from(a: &ToolActivity) -> Self {
        match a {
            ToolActivity::Start { name } => Self::Start { name: name.clone() },
            ToolActivity::Done {
                name,
                is_error,
                bytes,
            } => Self::Done {
                name: name.clone(),
                is_error: *is_error,
                bytes: *bytes as u64,
            },
            ToolActivity::SubagentStarted { id, label } => Self::SubagentStarted {
                id: id.clone(),
                label: label.clone(),
            },
            ToolActivity::SubagentProgress {
                id,
                label,
                turns,
                tokens,
                last_tool,
            } => Self::SubagentProgress {
                id: id.clone(),
                label: label.clone(),
                turns: *turns,
                tokens: *tokens,
                last_tool: last_tool.clone(),
            },
            ToolActivity::SubagentFinished {
                id,
                label,
                tokens,
                turns,
                is_error,
            } => Self::SubagentFinished {
                id: id.clone(),
                label: label.clone(),
                tokens: *tokens,
                turns: *turns,
                is_error: *is_error,
            },
        }
    }
}

impl From<WireToolActivity> for ToolActivity {
    fn from(w: WireToolActivity) -> Self {
        match w {
            WireToolActivity::Start { name } => Self::Start { name },
            WireToolActivity::Done {
                name,
                is_error,
                bytes,
            } => Self::Done {
                name,
                is_error,
                bytes: bytes as usize,
            },
            WireToolActivity::SubagentStarted { id, label } => Self::SubagentStarted { id, label },
            WireToolActivity::SubagentProgress {
                id,
                label,
                turns,
                tokens,
                last_tool,
            } => Self::SubagentProgress {
                id,
                label,
                turns,
                tokens,
                last_tool,
            },
            WireToolActivity::SubagentFinished {
                id,
                label,
                tokens,
                turns,
                is_error,
            } => Self::SubagentFinished {
                id,
                label,
                tokens,
                turns,
                is_error,
            },
        }
    }
}

/// Mirror of core `RunSummary` (non-serde in core).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireRunSummary {
    pub session_id: Uuid,
    pub answer: String,
    pub turns: u32,
    pub tool_calls: u32,
    pub usage: Usage,
    pub models_used: Vec<String>,
}

impl From<&RunSummary> for WireRunSummary {
    fn from(s: &RunSummary) -> Self {
        Self {
            session_id: s.session_id,
            answer: s.answer.clone(),
            turns: s.turns,
            tool_calls: s.tool_calls,
            usage: s.usage,
            models_used: s.models_used.clone(),
        }
    }
}

impl From<WireRunSummary> for RunSummary {
    fn from(w: WireRunSummary) -> Self {
        Self {
            session_id: w.session_id,
            answer: w.answer,
            turns: w.turns,
            tool_calls: w.tool_calls,
            usage: w.usage,
            models_used: w.models_used,
        }
    }
}

/// Mirror of the TUI's swarm row (no `Instant`; wire-safe).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSwarmRow {
    pub id: String,
    pub label: String,
    pub turns: u32,
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    /// `None` = still running.
    pub done: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_types::Message;

    fn roundtrip(frame: &Frame) -> Frame {
        let json = serde_json::to_string(frame).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    /// Equality via the wire: core payload types do not implement
    /// PartialEq, so roundtrip checks compare serialized JSON.
    fn assert_roundtrip(frame: &Frame) {
        let back = roundtrip(frame);
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(frame).unwrap(),
            "roundtrip: {:?}",
            frame.payload
        );
    }

    #[test]
    fn handshake_frames_roundtrip_with_tags() {
        let hello = Frame::request(
            1,
            Payload::Hello {
                client: "modron-tui".into(),
                protocol: PROTOCOL_VERSION,
                token: None,
            },
        );
        let json = serde_json::to_string(&hello).unwrap();
        assert!(
            json.contains("\"type\":\"hello\""),
            "tag must be snake_case type: {json}"
        );
        assert_roundtrip(&hello);

        let welcome = Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "0.1.2".into(),
                protocol: PROTOCOL_VERSION,
            },
        );
        assert_roundtrip(&welcome);
    }

    #[test]
    fn client_commands_roundtrip() {
        let session = Uuid::new_v4();
        let frames = vec![
            Frame::request(
                2,
                Payload::Attach {
                    session_id: session,
                    last_seq: 42,
                },
            ),
            Frame::request(3, Payload::Detach {}),
            Frame::request(
                4,
                Payload::Submit {
                    session_id: Some(session),
                    task: "fix the bug".into(),
                    attachments: vec![Attachment::Image {
                        media_type: "image/png".into(),
                        data: "aGk=".into(),
                    }],
                },
            ),
            Frame::request(
                5,
                Payload::Interrupt {
                    session_id: session,
                },
            ),
            Frame::request(
                6,
                Payload::Approve {
                    request_id: "req-1".into(),
                    decision: Decision::AllowAll,
                },
            ),
            Frame::request(
                7,
                Payload::SetConfig {
                    patch: ConfigPatch {
                        model: Some("gpt-5.6-sol".into()),
                        ..ConfigPatch::default()
                    },
                },
            ),
            Frame::request(
                8,
                Payload::Fork {
                    session_id: session,
                },
            ),
            Frame::request(9, Payload::NewSession { title: None }),
            Frame::request(10, Payload::ListSessions {}),
            Frame::request(11, Payload::Ping {}),
        ];
        for frame in &frames {
            let back = roundtrip(frame);
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                serde_json::to_value(frame).unwrap(),
                "roundtrip: {:?}",
                frame.payload
            );
        }
    }

    #[test]
    fn server_events_roundtrip() {
        let session = Uuid::new_v4();
        let frames = vec![
            Frame::event(
                Some(session),
                1,
                Payload::Snapshot {
                    session_id: session,
                    last_seq: 1,
                    events: vec![SessionEvent::Task {
                        text: "hello".into(),
                    }],
                    live: LiveState {
                        running: true,
                        streaming_text: "partial".into(),
                        ..LiveState::default()
                    },
                },
            ),
            Frame::event(
                Some(session),
                2,
                Payload::Delta {
                    delta: WireDelta::Text {
                        text: "chunk".into(),
                    },
                },
            ),
            Frame::event(
                Some(session),
                3,
                Payload::Tool {
                    activity: WireToolActivity::Done {
                        name: "bash".into(),
                        is_error: false,
                        bytes: 120,
                    },
                },
            ),
            Frame::event(
                Some(session),
                4,
                Payload::ApprovalRequested {
                    request_id: "req-9".into(),
                    call: ToolCall {
                        id: "c1".into(),
                        name: "edit".into(),
                        arguments: serde_json::json!({"path": "a.rs"}),
                    },
                },
            ),
            Frame::event(
                Some(session),
                5,
                Payload::ApprovalResolved {
                    request_id: "req-9".into(),
                    decision: Decision::Deny,
                },
            ),
            Frame::event(
                Some(session),
                6,
                Payload::ConfigChanged {
                    patch: ConfigPatch {
                        approval_mode: Some(ApprovalModeWire::ApproveAll),
                        ..ConfigPatch::default()
                    },
                },
            ),
            Frame::event(Some(session), 7, Payload::Sessions { sessions: vec![] }),
            Frame::event(
                Some(session),
                8,
                Payload::TurnDone {
                    summary: WireRunSummary {
                        session_id: session,
                        answer: "done".into(),
                        turns: 3,
                        tool_calls: 5,
                        usage: Usage {
                            input_tokens: 10,
                            output_tokens: 2,
                            cached_input_tokens: 0,
                        },
                        models_used: vec!["m".into()],
                    },
                },
            ),
            Frame::event(
                Some(session),
                9,
                Payload::TurnFailed {
                    error: "boom".into(),
                },
            ),
            Frame::event(Some(session), 10, Payload::TurnAborted {}),
            Frame::event(
                Some(session),
                11,
                Payload::OAuthProgress {
                    provider: "chatgpt".into(),
                    stage: "waiting".into(),
                    message: "enter the code".into(),
                    url: Some("https://auth.openai.com".into()),
                    user_code: Some("ABCD-1234".into()),
                },
            ),
            Frame::event(
                Some(session),
                12,
                Payload::Note {
                    text: "model switched".into(),
                },
            ),
            Frame::event(None, 0, Payload::Pong {}),
            Frame {
                id: Some(4),
                session: None,
                seq: 0,
                payload: Payload::Error {
                    message: "no such session".into(),
                },
            },
        ];
        for frame in &frames {
            let back = roundtrip(frame);
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                serde_json::to_value(frame).unwrap(),
                "roundtrip: {:?}",
                frame.payload
            );
        }
    }

    #[test]
    fn delta_mirror_skips_tool_call_ready() {
        assert!(WireDelta::from_core(&StreamDelta::ToolCallReady(ToolCall {
            id: "c".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
        }))
        .is_none());
        let text = StreamDelta::Text("hi".into());
        let wire = WireDelta::from_core(&text).unwrap();
        assert_eq!(wire.to_core(), text);
        let done = StreamDelta::Done;
        assert_eq!(WireDelta::from_core(&done).unwrap().to_core(), done);
    }

    #[test]
    fn tool_activity_mirror_roundtrips_every_variant() {
        let variants = vec![
            ToolActivity::Start {
                name: "read".into(),
            },
            ToolActivity::Done {
                name: "bash".into(),
                is_error: true,
                bytes: 4_000_000_000usize,
            },
            ToolActivity::SubagentStarted {
                id: "a".into(),
                label: "explore".into(),
            },
            ToolActivity::SubagentProgress {
                id: "a".into(),
                label: "explore".into(),
                turns: 3,
                tokens: 900,
                last_tool: Some("grep".into()),
            },
            ToolActivity::SubagentFinished {
                id: "a".into(),
                label: "explore".into(),
                tokens: 900,
                turns: 3,
                is_error: false,
            },
        ];
        for v in &variants {
            let wire = WireToolActivity::from(v);
            let back: ToolActivity = wire.clone().into();
            assert_eq!(&back, v);
            let json = serde_json::to_string(&wire).unwrap();
            let parsed: WireToolActivity = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, wire);
        }
    }

    #[test]
    fn run_summary_mirror_roundtrips() {
        let summary = RunSummary {
            session_id: Uuid::new_v4(),
            answer: "ok".into(),
            turns: 2,
            tool_calls: 1,
            usage: Usage::default(),
            models_used: vec!["m".into()],
        };
        let wire = WireRunSummary::from(&summary);
        let back: RunSummary = wire.into();
        assert_eq!(back.session_id, summary.session_id);
        assert_eq!(back.answer, summary.answer);
        assert_eq!(back.turns, summary.turns);
    }

    #[test]
    fn snapshot_reuses_view_tool_events() {
        let frame = Frame::event(
            None,
            1,
            Payload::Snapshot {
                session_id: Uuid::new_v4(),
                last_seq: 0,
                events: vec![],
                live: LiveState {
                    turn_tools: vec![ToolEvent {
                        name: "read".into(),
                        bytes: Some(64),
                        is_error: false,
                        done: true,
                    }],
                    ..LiveState::default()
                },
            },
        );
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"turn_tools\":[{\"name\":\"read\""));
        assert_roundtrip(&frame);
    }

    #[test]
    fn partial_live_state_deserializes_with_defaults() {
        // Forward/backward tolerance: a peer that omits newer LiveState
        // fields still deserializes.
        let json = format!(
            r#"{{"seq":1,"type":"snapshot","session_id":"{u}","last_seq":0,"events":[],"live":{{"running":true}}}}"#,
            u = Uuid::new_v4()
        );
        let frame: Frame = serde_json::from_str(&json).expect("partial live state loads");
        match frame.payload {
            Payload::Snapshot { live, .. } => {
                assert!(live.running);
                assert_eq!(live.streaming_text, "");
                assert_eq!(live.model, "");
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[test]
    fn message_helpers_exist_for_attachment_path() {
        // Guard that Submit's attachment type matches the core Message
        // attachment path (compile-time pairing).
        let attachments = vec![Attachment::Document {
            name: "notes.md".into(),
            text: "# notes".into(),
        }];
        let message = Message::user_with_attachments("see attached", attachments);
        assert_eq!(message.attachments.len(), 1);
    }
}
